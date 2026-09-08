use std::cell::RefCell;
use std::hash::Hash;
use std::rc::Rc;


use crate::driver::SlotId;
use crate::signal::{Cx, SignalVec};

/// How a value is captured into a `live_view!` binding closure. A binding closure is
/// `'static` and outlives the setup that built it, so every value it reads must be
/// captured by *value* — the macro can't move a capture the surrounding code may still
/// use. This blanket clones any `Clone` value; a [`MutableSignal`](crate::MutableSignal)
/// instead resolves to its inherent [`view_capture`](crate::MutableSignal::view_capture)
/// (a **reader**, winning
/// over this blanket by method-resolution priority), so a writer in scope is captured
/// as a reader — the writer stays for the loop. Not called directly; the macro emits it.
pub trait ViewCapture {
    type Captured;
    fn view_capture(&self) -> Self::Captured;
}

impl<T: Clone> ViewCapture for T {
    type Captured = T;
    fn view_capture(&self) -> T {
        self.clone()
    }
}

// ── Render: what a child-position `(expr)` places ─────────────────────────────

/// The template leaf a `(expr)` interpolation occupies — chosen per type, at
/// compile time, so text keeps its `TextSlot` (a direct `SetText`) and a view its
/// `AnchorSlot` (a mount), with no runtime type-dispatch and no anchor tax on text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotKind {
    Text,
    View,
}

/// The template-leaf kind a one-shot `(expr)` occupies — text (any `Display`) or a
/// spliced `LiveView`. **Deliberately not generic over `M`**: a `LiveView<M>` is a
/// view for every `M`, so the template node the macro emits never has to pin `M`,
/// which is what keeps a text-only template a single `&'static` (the `Display`
/// blanket would otherwise leave `M` ambiguous).
///
/// The blanket and the `LiveView` impl cohere: the bound excludes the local
/// non-`Display` `LiveView`.
pub trait RenderKind {
    const KIND: SlotKind;
}

impl<T: std::fmt::Display> RenderKind for T {
    const KIND: SlotKind = SlotKind::Text;
}

impl<M> RenderKind for LiveView<M> {
    const KIND: SlotKind = SlotKind::View;
}

/// A slot placed in child position is a mount, like a view — it occupies an `AnchorSlot`.
/// Placing it spawns the instance into the executor; this side only anchors it.
impl RenderKind for crate::slot::Slot {
    const KIND: SlotKind = SlotKind::View;
}

/// Content in child position mounts at an anchor: resolved paint, spliced as a unit.
/// The consumer of a view-shaped value places it blind — whether a static paint or a
/// live subtree stands behind `(expr)` is the constructor's business, never the
/// placement's.
impl RenderKind for crate::template::View {
    const KIND: SlotKind = SlotKind::View;
}

/// A reactive content source in child position: the splice follows the signal,
/// replacing wholesale on change — the store-content door (a `Content` field read, a
/// navigation's new paint).
impl RenderKind for crate::Signal<crate::template::View> {
    const KIND: SlotKind = SlotKind::View;
}

impl RenderKind for crate::Computed<crate::template::View> {
    const KIND: SlotKind = SlotKind::View;
}

/// What a one-shot `(expr)` turned into. The fill matches on *this* rather than on
/// [`RenderKind::KIND`], so each impl produces only the shape it has and neither has
/// to claim the other is unreachable. `KIND` still picks the template leaf — that is
/// a fact about the type, needed before any value exists; this is a fact about the
/// value, and the two are no longer asked to stand in for one another.
pub enum Rendered<M: 'static> {
    Text(String),
    View(LiveView<M>),
    /// A slot placement: the receiver end of a slot, to anchor here and subscribe. Not a
    /// `LiveView` — the instance is the parent's, built and driven there.
    Slot(crate::slot::Slot),
    /// Resolved content, mounted once.
    Content(crate::template::View),
    /// A reactive content source: the splice follows it, replacing as a unit.
    ContentSource(Rc<dyn Fn(&Cx) -> crate::template::View>),
}

/// The fill for a one-shot `(expr)`. `M` comes from the `LiveView` being built.
/// Reactive text is `$sig` and never routes through here — it stays the devirtualised
/// binding.
pub trait RenderInto<M: 'static>: RenderKind {
    fn render(self) -> Rendered<M>;
}

impl<M: 'static, T: std::fmt::Display> RenderInto<M> for T {
    fn render(self) -> Rendered<M> {
        Rendered::Text(self.to_string())
    }
}

impl<M: 'static> RenderInto<M> for LiveView<M> {
    fn render(self) -> Rendered<M> {
        Rendered::View(self)
    }
}

impl<M: 'static> RenderInto<M> for crate::slot::Slot {
    fn render(self) -> Rendered<M> {
        Rendered::Slot(self)
    }
}

impl<M: 'static> RenderInto<M> for crate::template::View {
    fn render(self) -> Rendered<M> {
        Rendered::Content(self)
    }
}

impl<M: 'static> RenderInto<M> for crate::Signal<crate::template::View> {
    fn render(self) -> Rendered<M> {
        Rendered::ContentSource(Rc::new(move |cx| self.get(cx)))
    }
}

impl<M: 'static> RenderInto<M> for crate::Computed<crate::template::View> {
    fn render(self) -> Rendered<M> {
        Rendered::ContentSource(Rc::new(move |cx| self.get(cx)))
    }
}

/// The template-slot kind for a value — the runtime read of [`RenderKind::KIND`], for
/// the runtime-built template form (the `const` form reads `KIND` directly). Emitted
/// by the macro; not called by hand.
pub fn render_kind_of<T: RenderKind>(_: &T) -> SlotKind {
    <T as RenderKind>::KIND
}

// ── Event ─────────────────────────────────────────────────────────────────────

/// A laid-out element's rectangle, in CSS pixels **relative to the mount root** — the frame
/// a component composes in (the same choice [`Ctx::resizes`](crate::Ctx::resizes) makes for
/// width). So an SVG overlay positioned over the root aligns to measured HTML without the
/// component restating any layout rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A normalised browser event. Wraps the raw platform event and exposes the
/// fields components most commonly need.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Event {
    pub target_value: Option<String>,
    pub key: Option<String>,
    /// Set on tick events (`ctx.every` / `ctx.frames`): the delta since the previous
    /// tick, in milliseconds.
    pub timestamp: Option<f64>,
    /// Set on `measure` events: the element's post-layout rect, root-relative. A
    /// `measure=>(|e| …)` handler reads `e.rect`.
    pub rect: Option<Rect>,
}

impl Event {
    pub fn value(&self) -> String {
        self.target_value.clone().unwrap_or_default()
    }
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
    /// The measured rect, on a `measure` event.
    pub fn rect(&self) -> Option<Rect> {
        self.rect
    }
}

pub mod key {
    use super::Event;

    /// Wrap a handler so it only fires on Enter key.
    pub fn enter<M>(f: impl Fn(Event) -> M + 'static) -> impl Fn(Event) -> Option<M> {
        move |e| (e.key.as_deref() == Some("Enter")).then(|| f(e))
    }
}

// ── Block ─────────────────────────────────────────────────────────────────────

/// One view generator's reactive leaves — text/attr/style/bool bindings and event
/// mappers — behind **one dispatch each**, indexed by position. The `live_view!` macro
/// emits `run`/`event` as a single monomorphic `match` over the block's arms (the
/// devirtualised form: one erased seam per block, plain code per leaf); the slot
/// tables are the data the runtime needs to resolve nodes and register listeners.
/// One block per *generator*: the top-level template is one, and each `@if`/`@match`
/// arm, `@for` row, and slot recipe — its own nested generator — carries its own.
///
/// Both dispatches are **pure**: a binding arm receives a [`Cx`] (its body opts into
/// reactivity via `.get(cx)`) and returns the patch it wants as data — the runtime
/// owns the per-index last-emission table and drops a returned op equal to the last
/// one, so "the op stream carries only real changes" is enforced at one seam, not in
/// every arm. Event arms are `Event -> Option<M>` — no `Client`, no effects, no
/// async. Effects (and the `Client` capability) live only in the message loop /
/// `Ctx::client_effect`, so every state transition flows through a message and the
/// message log is a complete, replayable history.
/// What an event slot subscribes to. Decided where it is parsed — the `live_view!`
/// grammar — and typed the rest of the way to the wire: DOM events ride
/// `AddEventListener`, a measurement rides `WatchMeasure` (a ResizeObserver
/// delivering the element's root-relative rect). The DOM event *type* stays a
/// string because that set is genuinely open (any event name an app listens for);
/// the two-way split is the closed set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventBinding {
    /// A DOM event, delivered through the delegated listener path.
    Dom(&'static str),
    /// The element's post-layout rectangle, on mount and on resize.
    Measure,
}

pub struct Block<M: 'static> {
    /// Binding position → the slot whose node its arm patches.
    pub binding_slots: Vec<SlotId>,
    /// The canvases this block draws (`painting=(…)`): the slot each is on, and its
    /// layers. A picture has no HTML — the served `<canvas>` is blank — so it must be
    /// drawn in the browser, and a view that draws one cannot be adopted as a static
    /// paint.
    pub paintings: Vec<PaintingDecl>,
    /// Event position → the slot it listens on and what it subscribes to.
    pub event_slots: Vec<(SlotId, EventBinding)>,
    /// The block's structural fragments, as data; their code lives in `fragment`.
    pub fragments: Vec<FragmentDecl<M>>,
    /// The binding dispatch: `run(cx, nodes, idx)` returns the arm's patch (`nodes`
    /// parallel to `binding_slots`), or `None` to leave the DOM untouched.
    pub run: Rc<dyn Fn(&Cx, &[crate::driver::NodeId], u32) -> Option<crate::driver::DomOp>>,
    /// The event dispatch: `Some(msg)` to deliver, `None` to swallow.
    pub event: Rc<dyn Fn(u32, Event) -> Option<M>>,
    /// The fragment dispatch: selectors, branch/row builders, content sources.
    pub fragment: Rc<dyn Fn(&Cx, FragmentOp) -> FragmentOut<M>>,
}

/// Autoref-specialized event-mapper dispatch. A `=>` mapper may yield `M` (every event
/// delivers) or `Option<M>` (`None` swallows); which one is a fact about the mapper's
/// *type*, so it is decided here by method resolution, never by inspecting its spelling.
/// The macro emits `(&mapper(f)).deliver(event)`: one autoref reaches the
/// [`DeliverOption`] impl on `MapperCall<F>` when `F` returns an option; otherwise
/// resolution takes the second autoref to the [`DeliverMsg`] impl on `&MapperCall<F>`
/// and wraps in `Some`.
pub struct MapperCall<F>(F);

/// Wrap a mapper for [`DeliverOption`]/[`DeliverMsg`] dispatch. The `impl Fn(Event)`
/// parameter types a closure literal's argument as [`Event`] by expectation while
/// leaving its return free — the two facts the dispatch needs.
pub fn mapper<R>(f: impl Fn(Event) -> R) -> MapperCall<impl Fn(Event) -> R> {
    MapperCall(f)
}

pub trait DeliverOption<M> {
    fn deliver(&self, event: Event) -> Option<M>;
}

impl<M, F: Fn(Event) -> Option<M>> DeliverOption<M> for MapperCall<F> {
    fn deliver(&self, event: Event) -> Option<M> {
        (self.0)(event)
    }
}

pub trait DeliverMsg<M> {
    fn deliver(&self, event: Event) -> Option<M>;
}

impl<M, F: Fn(Event) -> M> DeliverMsg<M> for &MapperCall<F> {
    fn deliver(&self, event: Event) -> Option<M> {
        Some((self.0)(event))
    }
}

fn no_events<M>() -> Rc<dyn Fn(u32, Event) -> Option<M>> {
    Rc::new(|_, _| None)
}

fn no_bindings() -> Rc<dyn Fn(&Cx, &[crate::driver::NodeId], u32) -> Option<crate::driver::DomOp>>
{
    Rc::new(|_, _, _| None)
}

fn no_fragments<M>() -> Rc<dyn Fn(&Cx, FragmentOp) -> FragmentOut<M>> {
    Rc::new(|_, _| FragmentOut::None)
}

// A view-embedded child is spawned into the flat executor (see `crate::component::mount_child`);
// its render splices through a `child_id`-keyed sink (`crate::runtime::child_render_sink`), and its
// cancel-guard rides this view's `placement_guards`. The template records only its anchor
// (`LiveView::child_anchor`).

/// A canvas declared by a [`Block`], as data: the slot its element is on, and the
/// picture it draws. Like a `@for`'s source, the layers are taken at view construction
/// — a live list is a value, not a value read per run.
pub struct PaintingDecl {
    pub slot: SlotId,
    pub layers: std::rc::Rc<dyn crate::canvas::Layers>,
}

// ── Fragments: the structural protocol ────────────────────────────────────────

/// A fragment declared by a [`Block`], as data: where it anchors and what shape it
/// is. All of its *code* — conditions, selectors, branch and row bodies, content
/// sources — lives in the block's one fragment dispatch.
pub struct FragmentDecl<M: 'static> {
    pub slot: SlotId,
    pub kind: FragmentSource<M>,
}

/// A fragment's shape, before or after it has a scope to live in. A source that owns
/// reactive work (a keyed list's sync) cannot be built until there is an owner to root
/// that work in, so it arrives as a builder and [`resolve`](FragmentSource::resolve)s
/// at wiring — where the mount's owner exists. Past that point the deferred case is
/// gone from the type, so nothing downstream can encounter one.
pub enum FragmentSource<M: 'static> {
    Ready(FragmentKind<M>),
    Deferred(Box<dyn FnOnce(&crate::owner::Owner) -> FragmentKind<M>>),
}

impl<M: 'static> FragmentSource<M> {
    pub(crate) fn resolve(self, owner: &crate::owner::Owner) -> FragmentKind<M> {
        match self {
            FragmentSource::Ready(kind) => kind,
            FragmentSource::Deferred(build) => build(owner),
        }
    }
}

pub enum FragmentKind<M: 'static> {
    /// `@if` (one or two arms) and `@match` (n arms) — one shape: a tracked
    /// selector chooses an arm (`None` = nothing shown), the dispatch builds it.
    Branch { keep: bool, arms: u32 },
    /// `@for` over a live list: the structural half of the source (order snapshot,
    /// op-stream, tracking) as data, plus the one stored row builder — the composed
    /// typed read + body, the single closure a `@for` call site keeps (a stored arm
    /// cannot speak a fixed source's borrowed item type, so rows do not ride the
    /// dispatch).
    List { structure: Rc<dyn ForStructure>, rebuild: Rc<dyn Fn(crate::Row) -> Option<LiveView<M>>> },
    /// `@for` over a plain iterator: rows built once at view construction — no
    /// changes, no subscription, nothing minted, nothing stored.
    FixedList(RefCell<Vec<(crate::Row, LiveView<M>)>>),
    /// A content splice — a placed `View`/`Signal<View>`: a **server-rendered component**
    /// ([`View`](crate::template::View)) spliced as content, tracked like an
    /// `@if` condition. Nothing to wire inside — the IR is resolved output; its
    /// live surface at mount.
    Content,
    /// A `(view)` interpolation: a pre-built `LiveView<M>` mounted **once** at its
    /// anchor, its own block routing its events to the shared reducer. Taken at
    /// wire time (never rebuilt), so an `Option` behind a cell.
    Slot(RefCell<Option<LiveView<M>>>),
}

/// A structural request into a block's fragment dispatch. Fragments are numbered by
/// position in the block's `fragments`, the same indexing discipline as leaves.
#[derive(Clone, Copy)]
pub enum FragmentOp {
    /// Which arm should a branch show? (Tracked: the answer's reads re-run it.)
    Select(u32),
    /// Build one branch arm's view.
    Arm { fragment: u32, arm: u32 },
    /// Resolve a content splice's view. (Tracked like `Select`.)
    Content(u32),
}

pub enum FragmentOut<M: 'static> {
    Selected(Option<usize>),
    LiveView(LiveView<M>),
    None,
    Content(crate::template::View),
}

/// The structural half of a live `@for` source — pure data plane: what
/// [`SignalVec`] provides minus the typed row read. The runtime holds this to
/// snapshot order, follow the op-stream, and track structure; every row *view*
/// builds through the block dispatch.
pub trait ForStructure: 'static {
    fn snapshot_order(&self) -> Vec<crate::Row>;
    fn consume(&self) -> crate::signal::vec::Consumer;
    fn observe(&self, cx: &Cx);
}

impl<V: SignalVec<T>, T: 'static> ForStructure for Structure<V, T> {
    fn snapshot_order(&self) -> Vec<crate::Row> {
        self.source.snapshot_order()
    }
    fn consume(&self) -> crate::signal::vec::Consumer {
        self.source.consume()
    }
    fn observe(&self, cx: &Cx) {
        self.source.observe(cx);
    }
}

/// A [`SignalVec`] viewed as pure structure (the `T` pinned so the impl is
/// coherent).
pub struct Structure<V, T: 'static> {
    source: V,
    _row: std::marker::PhantomData<fn() -> T>,
}

/// What `@for` iterates, and **what its pattern binds** — the one list form's
/// dispatch. The binding shape is *determined by the source* (an associated type,
/// which is also what lets a row body typecheck against a concrete binding the
/// moment the source's type is known): a [`KeyedVec`](crate::KeyedVec) binds
/// `(key, Signal<T>)` (the key is row identity, sampled untracked at build — a key
/// change arrives as remove+insert, never an update), a
/// [`MutableVec`](crate::MutableVec) binds `(Row, Signal<T>)`, any plain
/// `IntoIterator` binds its item by value. A live source composes `build` with its
/// typed row read into the fragment's stored `rebuild`; a fixed source invokes it
/// eagerly for its one-time rows and stores nothing. (The reactive types don't
/// implement `IntoIterator`, so the impls are coherent.)
pub trait ForFragmentSource<M: 'static>: Sized {
    type Binding;
    /// Build the fragment's rows. A row body's component children ride the row view's
    /// `placement_guards`, so a row removal (the source's `SpliceOp::Remove`) drops the row
    /// view and reaps its subtree — no separate child collection.
    fn into_parts(self, build: impl Fn(Self::Binding) -> LiveView<M> + 'static) -> FragmentKind<M>;
}

impl<T: 'static, M: 'static> ForFragmentSource<M> for crate::MutableVec<T> {
    type Binding = (crate::Row, crate::Signal<T>);
    fn into_parts(self, build: impl Fn(Self::Binding) -> LiveView<M> + 'static) -> FragmentKind<M> {
        let structure = Structure { source: self.clone(), _row: std::marker::PhantomData };
        FragmentKind::List {
            structure: Rc::new(structure),
            rebuild: Rc::new(move |row| {
                SignalVec::read(&self, row).map(|cell| build((row, cell)))
            }),
        }
    }
}

impl<T: 'static, K: 'static, M: 'static> ForFragmentSource<M> for crate::KeyedVec<T, K> {
    type Binding = (K, crate::Signal<T>);
    fn into_parts(self, build: impl Fn(Self::Binding) -> LiveView<M> + 'static) -> FragmentKind<M> {
        let structure = Structure { source: self.clone(), _row: std::marker::PhantomData };
        FragmentKind::List {
            structure: Rc::new(structure),
            rebuild: Rc::new(move |row| {
                let cell = SignalVec::read(&self, row)?;
                let key = (self.key_fn())(&cell.peek());
                Some(build((key, cell)))
            }),
        }
    }
}

impl<I, M: 'static> ForFragmentSource<M> for I
where
    I: IntoIterator,
{
    type Binding = I::Item;
    fn into_parts(self, build: impl Fn(I::Item) -> LiveView<M> + 'static) -> FragmentKind<M> {
        // Rows are identity bookkeeping for the mount; no cells are ever minted.
        let rows: Vec<(crate::Row, LiveView<M>)> = self
            .into_iter()
            .map(|item| (crate::Row::fresh(), build(item)))
            .collect();
        FragmentKind::FixedList(RefCell::new(rows))
    }
}

// ── LiveView ──────────────────────────────────────────────────────────────────────

/// The product of `live_view! { ... }`. Generic over the component's message type
/// `M` so that event mappers remain type-safe until `ctx.render()` wires them
/// to the inbox. After mounting, this value is consumed; the DOM lives on
/// through signal subscriptions.
/// The reactive plane's view: typed template IR plus the blocks that make it live.
///
/// `live_view!` is the idiomatic spelling, **not a requirement** — the macro lowers
/// onto this public surface, and a view built by hand meets the runtime at exactly
/// the same seam (the JSX ↔ `createElement` relationship):
///
/// ```rust,no_run
/// use std::borrow::Cow;
/// use idyll::driver::SlotId;
/// use idyll::template::{Template, TplNode};
/// use idyll::{Ctx, LiveView, Result, Setup};
///
/// #[derive(Debug)]
/// enum Msg { Inc }
///
/// async fn counter(ctx: Ctx<Setup, Msg>) -> Result {
///     let n = ctx.mutable_signal(0i64);
///     let count = n.read();
///     let template: Template = vec![
///         TplNode::Element {
///             tag: Cow::Borrowed("button"),
///             attrs: Cow::Borrowed(&[]),
///             slot: Some(SlotId(0)),
///             children: 1,
///         },
///         TplNode::TextSlot(SlotId(1)),
///     ]
///     .into();
///     let mut ctx = ctx.render(|_| async move {
///         Ok::<_, idyll::Fault>(
///             LiveView::new(template)
///                 .event(0, "click", |_| Some(Msg::Inc))
///                 .text(1, move |cx| count.get(cx).to_string()),
///         )
///     }).await?;
///     loop {
///         let (Msg::Inc, reducer) = ctx.recv().await?;
///         n.update(&reducer, |v| *v += 1);
///     }
/// }
/// ```
pub struct LiveView<M: 'static = ()> {
    /// The template as **typed IR** (see [`crate::template`]) — slots are first-class
    /// nodes, never markers in markup. HTML exists only as a serialization *output*.
    pub template: crate::template::Template,
    pub blocks: Vec<Block<M>>,
    pub child_anchors: Vec<(SlotId, crate::runtime::ChildId)>,
    /// Owners (or other handles) whose lifetime this view extends. A server
    /// render resolves the component future — dropping its locals — *before*
    /// serializing the view, so a view built from a locally-minted [`Owner`]
    /// must carry that owner here to keep its reactive cells alive through
    /// serialization. See [`LiveView::own`].
    pub keepalives: Vec<Rc<dyn std::any::Any>>,
    /// One-shot text fills — a `(expr)` interpolation whose value is text. Applied
    /// once at mount (`SetText`), never a block: one-shot text stays out of the
    /// devirtualised binding, so the view keeps its single `Block`.
    pub oneshot: Vec<(SlotId, String)>,
    /// Cancel-guards for the view-embedded children spawned to build this view — a component
    /// splice, an `@if`/`@match`/`@for` child. Folded into this view's `cleanup_guards` at
    /// [`wire_view`](crate::ctx), so each drops when this view — a component's whole view, or
    /// the `@if` arm / `@for` row it sits in — unmounts, reaping the child's executor task
    /// (and its subtree).
    pub placement_guards: Vec<crate::runtime::MountGuard>,
    /// Slot receivers placed in this view, awaiting their frame: subscription happens at
    /// [`wire_view`](crate::ctx), with the frame of the view that places them — context
    /// follows tree.
    pub placements: Vec<(crate::runtime::ChildId, crate::slot::Slot)>,
    /// The `(name, key)` of every live marker in this view's template — derived
    /// from the IR at construction (one source of truth). The runtime accumulates
    /// them per render so the host knows what to mount.
    pub live: Vec<(String, Option<String>)>,
}

impl<M: 'static> LiveView<M> {
    /// The vacuous live view: how the **content plane mounts** — the engine behind
    /// [`Ctx::render_content`](crate::Ctx::render_content) and the `Never` `From`.
    /// Crate-private on purpose: content enters a live tree only through the placement
    /// doors — `(expr)` over a `View`/`Signal<View>`, or `render_content` as a static
    /// component's whole view.
    pub(crate) fn from_content(content: crate::template::View) -> Self {
        LiveView::new(content.into_template())
    }

    pub fn new(template: impl Into<crate::template::Template>) -> Self {
        let template = template.into();
        let live = crate::template::islands_of(&template.nodes);
        LiveView {
            template,
            blocks: Vec::new(),
            child_anchors: Vec::new(),
            keepalives: Vec::new(),
            oneshot: Vec::new(),
            placement_guards: Vec::new(),
            placements: Vec::new(),
            live,
        }
    }

    /// Mark this view's nodes as living in the SVG namespace — emitted by `live_view!`
    /// from the lexical nesting at the point the template was written. The runtime
    /// cannot derive it for a fragment: a `@for` row is built while its anchor is still
    /// parked off-tree, and positioned only afterwards.
    pub fn in_svg(mut self) -> Self {
        self.template.svg = true;
        self
    }

    pub fn empty() -> Self {
        LiveView::new(crate::template::Template::EMPTY)
    }

    /// Tie an [`Owner`](crate::Owner)'s lifetime to this view, so the cells it
    /// roots are not disposed until the view itself is dropped. Needed when a
    /// server component mints its own owner and returns a view that outlives the
    /// render future.
    pub fn own(mut self, owner: &crate::Owner) -> Self {
        self.keepalives.push(owner.keepalive());
        self
    }

    /// Concatenate two views as siblings. `other`'s slot ids are **rebased** past this
    /// view's, so independently-built views (each numbering slots from 0) compose without
    /// collision — slots are typed IR, so the rebase is mechanical and total.
    pub fn append(mut self, mut other: LiveView<M>) -> Self {
        let offset = self.next_slot_id();
        other.rebase_slots(offset);
        self.template
            .nodes
            .to_mut()
            .extend(other.template.nodes.iter().cloned());
        if !other.template.styles.is_empty() {
            let mut union = std::mem::take(&mut self.template.styles).into_owned();
            crate::template::union_styles(&mut union, &other.template.styles);
            self.template.styles = std::borrow::Cow::Owned(union);
        }
        self.blocks.append(&mut other.blocks);
        self.child_anchors.append(&mut other.child_anchors);
        self.keepalives.append(&mut other.keepalives);
        self.oneshot.append(&mut other.oneshot);
        self.placement_guards.append(&mut other.placement_guards);
        self.placements.append(&mut other.placements);
        self.live.append(&mut other.live);
        self
    }

    /// One past the highest slot id this view's template declares.
    fn next_slot_id(&self) -> u32 {
        use crate::template::TplNode;
        self.template
            .nodes
            .iter()
            .filter_map(|node| match node {
                TplNode::Element { slot, .. } => slot.map(|s| s.0),
                TplNode::TextSlot(slot) | TplNode::AnchorSlot(slot) => Some(slot.0),
                TplNode::Text(_) | TplNode::Live { .. } => None,
            })
            .max()
            .map(|max| max + 1)
            .unwrap_or(0)
    }

    /// Shift every slot id in this view (template + blocks + fragments + children)
    /// by `offset`.
    fn rebase_slots(&mut self, offset: u32) {
        use crate::template::TplNode;
        if offset == 0 {
            return;
        }
        for node in self.template.nodes.to_mut() {
            match node {
                TplNode::Element { slot: Some(slot), .. } => slot.0 += offset,
                TplNode::TextSlot(slot) | TplNode::AnchorSlot(slot) => slot.0 += offset,
                _ => {}
            }
        }
        for block in &mut self.blocks {
            for slot in &mut block.binding_slots {
                slot.0 += offset;
            }
            for (slot, _) in &mut block.event_slots {
                slot.0 += offset;
            }
            for fragment in &mut block.fragments {
                fragment.slot.0 += offset;
            }
        }
        for (slot, _) in &mut self.child_anchors {
            slot.0 += offset;
        }
        for (slot, _) in &mut self.oneshot {
            slot.0 += offset;
        }
    }

    /// Wrap this view's whole content in a single container element — the sibling of
    /// [`append`](LiveView::append) for *nesting* (e.g. markdown content inside an
    /// `<article>`).
    pub fn wrap(mut self, tag: &'static str) -> Self {
        use crate::template::TplNode;
        let nodes = self.template.nodes.to_mut();
        let roots = crate::template::root_count(nodes);
        nodes.insert(
            0,
            TplNode::Element {
                tag: std::borrow::Cow::Borrowed(tag),
                attrs: std::borrow::Cow::Borrowed(&[]),
                slot: None,
                children: roots,
            },
        );
        self
    }

    /// Attach style rules to this view's template (`css=[…]` in `live_view!` routes its
    /// merged rules here; rules union by name).
    pub fn with_styles(mut self, styles: Vec<crate::template::StyleRule>) -> Self {
        self.template = std::mem::replace(&mut self.template, crate::template::Template::EMPTY)
            .with_styles(styles);
        self
    }

    // ── Builder methods ───────────────────────────────────────────────────────
    //
    // `live_view!` emits one [`Block`] per invocation through [`block`](Self::block);
    // these per-leaf builders are the primitive form for hand-assembled views (a
    // markdown pipeline, tests): each is a one-arm block, so both forms mount and
    // dispatch identically.

    /// Append a `live_view!`-emitted dispatch block.
    pub fn block(mut self, block: Block<M>) -> Self {
        self.blocks.push(block);
        self
    }

    pub fn text(self, slot: u32, f: impl Fn(&Cx) -> String + 'static) -> Self {
        self.one_binding(slot, move |cx, node_id| crate::driver::DomOp::SetText {
            node_id,
            text: f(cx),
        })
    }

    pub fn attr(
        self,
        slot: u32,
        name: &'static str,
        f: impl Fn(&Cx) -> String + 'static,
    ) -> Self {
        self.one_binding(slot, move |cx, node_id| crate::driver::DomOp::SetAttr {
            node_id,
            name,
            value: f(cx),
        })
    }

    /// One CSS declaration (`style:prop=(…)` in `live_view!`): updates write a single
    /// property on the element's inline style instead of replacing the whole
    /// `style` attribute — the moving-parts binding for per-frame animation.
    pub fn style_prop(
        self,
        slot: u32,
        name: &'static str,
        f: impl Fn(&Cx) -> String + 'static,
    ) -> Self {
        self.one_binding(slot, move |cx, node_id| crate::driver::DomOp::SetStyleProp {
            node_id,
            name,
            value: f(cx),
        })
    }

    pub fn bool_attr(
        self,
        slot: u32,
        name: &'static str,
        f: impl Fn(&Cx) -> bool + 'static,
    ) -> Self {
        self.one_binding(slot, move |cx, node_id| crate::driver::DomOp::SetBoolAttr {
            node_id,
            name,
            value: f(cx),
        })
    }

    fn one_binding(
        mut self,
        slot: u32,
        arm: impl Fn(&Cx, crate::driver::NodeId) -> crate::driver::DomOp + 'static,
    ) -> Self {
        self.blocks.push(Block {
            binding_slots: vec![SlotId(slot)],
            paintings: Vec::new(),
            event_slots: Vec::new(),
            fragments: Vec::new(),
            run: Rc::new(move |cx, nodes, _| Some(arm(cx, nodes[0]))),
            event: no_events(),
            fragment: no_fragments(),
        });
        self
    }

    /// A one-shot `(expr)` interpolation: text is a one-time `SetText`, a view mounts
    /// at the anchor. Per monomorphisation `render` yields one variant, so the match
    /// folds and the value moves into exactly one channel.
    pub fn place<T: RenderInto<M>>(self, slot: u32, value: T) -> Self {
        match value.render() {
            Rendered::Text(text) => self.oneshot_text(slot, text),
            Rendered::View(view) => self.slot(slot, view),
            Rendered::Slot(receiver) => self.place_slot(slot, receiver),
            Rendered::Content(content) => self.view_fragment(slot, move |_| content.clone()),
            Rendered::ContentSource(source) => self.view_fragment(slot, move |cx| source(cx)),
        }
    }

    /// Place a slot's receiver at an anchor: mint the instance's `child_id` and anchor it
    /// here. Subscription waits for the frame — `wire_view` places the receiver under the
    /// frame of the view being wired (context follows tree), and the unsubscribe guard rides
    /// this view's cleanup, so the instance unmounts when this view (or the `@if` arm /
    /// `@for` row it sits in) does.
    fn place_slot(mut self, slot: u32, receiver: crate::slot::Slot) -> Self {
        let child_id = crate::runtime::fresh_child_id();
        self.placements.push((child_id, receiver));
        self.child_anchor(slot, child_id)
    }

    /// Fill a text slot once (a one-shot `(expr)`) — applied at mount, never a
    /// block, so the view's single dispatch is untouched. Reactive text is
    /// [`text`](Self::text) (a `$sig` binding).
    pub fn oneshot_text(mut self, slot: u32, text: String) -> Self {
        self.oneshot.push((SlotId(slot), text));
        self
    }

    /// Place a pre-built `LiveView<M>` at an anchor slot — the hand-built form of a
    /// `(view)` interpolation. It mounts once; its own block routes its events to
    /// this view's reducer (the block is merged at wire time).
    pub fn slot(self, slot: u32, view: LiveView<M>) -> Self {
        self.one_fragment(
            slot,
            FragmentKind::Slot(RefCell::new(Some(view))),
            |_, _| FragmentOut::None,
        )
    }

    /// A fragment whose source owns reactive work, so it is built at wiring against
    /// the mount's owner rather than here, where there is no scope yet.
    fn deferred_fragment(
        mut self,
        slot: u32,
        build: Box<dyn FnOnce(&crate::owner::Owner) -> FragmentKind<M>>,
    ) -> Self {
        self.blocks.push(Block {
            binding_slots: Vec::new(),
            paintings: Vec::new(),
            event_slots: Vec::new(),
            fragments: vec![FragmentDecl {
                slot: SlotId(slot),
                kind: FragmentSource::Deferred(build),
            }],
            run: no_bindings(),
            event: no_events(),
            fragment: Rc::new(|_cx, _op| FragmentOut::None),
        });
        self
    }

    fn one_fragment(
        mut self,
        slot: u32,
        kind: FragmentKind<M>,
        fragment: impl Fn(&Cx, FragmentOp) -> FragmentOut<M> + 'static,
    ) -> Self {
        self.blocks.push(Block {
            binding_slots: Vec::new(),
            paintings: Vec::new(),
            event_slots: Vec::new(),
            fragments: vec![FragmentDecl { slot: SlotId(slot), kind: FragmentSource::Ready(kind) }],
            run: no_bindings(),
            event: no_events(),
            fragment: Rc::new(fragment),
        });
        self
    }

    /// Register an event mapper. `mapper` returns `Some(M)` to deliver or
    /// `None` to swallow (used by `key::enter` etc.).
    pub fn event(
        mut self,
        slot: u32,
        event_type: &'static str,
        mapper: impl Fn(Event) -> Option<M> + 'static,
    ) -> Self {
        self.blocks.push(Block {
            binding_slots: Vec::new(),
            paintings: Vec::new(),
            event_slots: vec![(SlotId(slot), EventBinding::Dom(event_type))],
            fragments: Vec::new(),
            run: no_bindings(),
            event: Rc::new(move |_, e| mapper(e)),
            fragment: no_fragments(),
        });
        self
    }

    /// Shorthand for events that always produce a message.
    pub fn event_map(
        self,
        slot: u32,
        event_type: &'static str,
        mapper: impl Fn(Event) -> M + 'static,
    ) -> Self {
        self.event(slot, event_type, move |e| Some(mapper(e)))
    }

    /// Record that `slot` is the anchor of a view-embedded child identified by `child_id`. The
    /// child lives in the flat executor (its cancel-guard rides `placement_guards`), not this
    /// view; here we only mark where it splices. The runtime resolves `child_id` to this slot's
    /// node at mount and mounts the child's render there (`runtime::child_render_sink` /
    /// `resolve_child_anchor`).
    pub fn child_anchor(mut self, slot: u32, child_id: crate::runtime::ChildId) -> Self {
        self.child_anchors.push((SlotId(slot), child_id));
        self
    }

    /// Attach a view-embedded child's cancel-guard to this view. It rides `placement_guards`
    /// (folded into cleanup by `wire_view`), so the child unmounts when this view does — how a
    /// `@if`/`@match` arm, a `@for` row, or a slot placement reaps its `spawn_child` subtree.
    pub fn placement_guard(mut self, guard: crate::runtime::MountGuard) -> Self {
        self.placement_guards.push(guard);
        self
    }

    /// A one-anchor view placing a single view-embedded child (a boundary's caught child, or any
    /// hand-built mount). The child lives in the flat executor; this view only marks where it
    /// splices.
    pub fn child_slot(child_id: crate::runtime::ChildId) -> Self {
        LiveView::new(vec![crate::template::TplNode::AnchorSlot(SlotId(0))]).child_anchor(0, child_id)
    }


    pub fn if_fragment(
        self,
        slot: u32,
        condition: impl Fn(&Cx) -> bool + 'static,
        then_view: impl Fn(&Cx) -> LiveView<M> + 'static,
        else_view: Option<impl Fn(&Cx) -> LiveView<M> + 'static>,
        keep: bool,
    ) -> Self {
        let arms = 1 + else_view.is_some() as u32;
        self.one_fragment(
            slot,
            FragmentKind::Branch { keep, arms },
            move |cx, op| match op {
                FragmentOp::Select(_) => FragmentOut::Selected(if condition(cx) {
                    Some(0)
                } else if else_view.is_some() {
                    Some(1)
                } else {
                    None
                }),
                FragmentOp::Arm { arm: 0, .. } => FragmentOut::LiveView(then_view(cx)),
                FragmentOp::Arm { arm: 1, .. } => match &else_view {
                    Some(else_view) => FragmentOut::LiveView(else_view(cx)),
                    None => FragmentOut::None,
                },
                _ => FragmentOut::None,
            },
        )
    }

    pub fn match_fragment(
        self,
        slot: u32,
        selector: impl Fn(&Cx) -> usize + 'static,
        arms: Vec<Box<dyn Fn(&Cx) -> LiveView<M>>>,
        keep: bool,
    ) -> Self {
        let count = arms.len();
        self.one_fragment(
            slot,
            FragmentKind::Branch { keep, arms: count as u32 },
            move |cx, op| match op {
                FragmentOp::Select(_) => {
                    let active = selector(cx);
                    FragmentOut::Selected((active < count).then_some(active))
                }
                FragmentOp::Arm { arm, .. } => match arms.get(arm as usize) {
                    Some(build) => FragmentOut::LiveView(build(cx)),
                    None => FragmentOut::None,
                },
                _ => FragmentOut::None,
            },
        )
    }

    /// `@for pat in (source) { … }` — the one list form. **The binding shape comes
    /// from the source** (see [`ForFragmentSource`]): a [`KeyedVec`](crate::KeyedVec)
    /// binds `(key, Signal<T>)`, a [`MutableVec`](crate::MutableVec) binds
    /// `(Row, Signal<T>)` (reactive rows, read with `.get(cx)`); any plain
    /// `IntoIterator` binds `T` by value (fixed rows, built once — nothing minted, so
    /// it is legal in inert server views).
    /// Reactivity stays visible at the pattern, not hidden in the keyword.
    pub fn for_source<S>(
        self,
        slot: u32,
        source: S,
        row_view: impl Fn(S::Binding) -> LiveView<M> + 'static,
    ) -> Self
    where
        S: ForFragmentSource<M>,
    {
        let kind = source.into_parts(row_view);
        self.one_fragment(slot, kind, |_cx, _op| FragmentOut::None)
    }

    /// The content splice's mechanism — what a `(expr)` placement of a
    /// [`View`](crate::template::View)/`Signal<View>` lowers to, as the hand-built form.
    /// The source is tracked: read the view out of a signal and the content replaces
    /// when it changes (the navigation shape: new query response commits to the cache
    /// → new body splices in).
    pub fn view_fragment(
        self,
        slot: u32,
        source: impl Fn(&Cx) -> crate::template::View + 'static,
    ) -> Self {
        self.one_fragment(slot, FragmentKind::Content, move |cx, op| match op {
            FragmentOp::Content(_) => FragmentOut::Content(source(cx)),
            _ => FragmentOut::None,
        })
    }

    /// `@for pat in (values) [key = …] { … }` — sugar for a [`KeyedVec`](crate::KeyedVec)
    /// over the signal.
    pub fn for_keyed_signal<T, K>(
        self,
        slot: u32,
        values: crate::Signal<Vec<T>>,
        key: impl Fn(&T) -> K + 'static,
        row_view: impl Fn(K, crate::Signal<T>) -> LiveView<M> + 'static,
    ) -> Self
    where
        T: Clone + PartialEq + 'static,
        K: Eq + Hash + 'static,
    {
        self.deferred_fragment(
            slot,
            Box::new(move |owner| {
                let list = crate::KeyedVec::derived(owner, move |cx| values.get(cx), key);
                list.into_parts(move |(key, cell)| row_view(key, cell))
            }),
        )
    }

}

/// Content converts implicitly only at `M = Never` — the message type only static
/// components have — so a page's `ctx.render(view! { … })` stays ergonomic. Inside a
/// live tree, content is *placed* (`(expr)` over a `View`/`Signal<View>`), never
/// converted.
impl From<crate::template::View> for LiveView<crate::Never> {
    fn from(content: crate::template::View) -> Self {
        LiveView::from_content(content)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::LiveView;
    use crate::driver::SlotId;
    use crate::template::TplNode;

    fn el(tag: &'static str, slot: Option<u32>, children: u32) -> TplNode {
        TplNode::Element {
            tag: Cow::Borrowed(tag),
            attrs: Cow::Borrowed(&[]),
            slot: slot.map(SlotId),
            children,
        }
    }

    fn text(s: &'static str) -> TplNode {
        TplNode::Text(Cow::Borrowed(s))
    }

    #[test]
    fn append_concatenates_template_ir() {
        let left: LiveView<()> = LiveView::new(vec![el("p", None, 1), text("left")]);
        let right: LiveView<()> = LiveView::new(vec![el("p", None, 1), text("right")]);

        let view = left.append(right);

        assert_eq!(
            &view.template.nodes[..],
            &[el("p", None, 1), text("left"), el("p", None, 1), text("right")]
        );
    }

    /// The ONE serialization path: view → mount (command stream) → HTML fold. This is
    /// what `render_view` + `view_html` compose; tests drive it directly so any M
    /// (including handler-bearing views, whose handlers must vanish) is exercisable.
    fn fold_of<M: 'static>(view: LiveView<M>) -> String {
        let mut rt = crate::runtime::Runtime::new();
        let mut driver = crate::driver::CommandBufferDriver::new();
        let ctx = rt.ctx::<M>();
        rt.spawn(async move {
            let _ = ctx
                .render(|_| async move { ::std::result::Result::<_, crate::Fault>::Ok(view) })
                .await;
        });
        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.flush(&mut driver);
        crate::html::fold_html(&driver.take_commands()).into_string()
    }

    #[test]
    fn the_fold_expands_text_and_attributes() {
        let rt = crate::Runtime::new();
        let owner = crate::Owner::new(rt.core());
        let label = owner.mutable_signal("A&B".to_string());
        let title = owner.mutable_signal("\"quoted\"".to_string());
        let disabled = owner.mutable_signal(true);
        let view: LiveView<()> =
            LiveView::new(vec![el("button", Some(0), 1), TplNode::TextSlot(SlotId(1))])
                .attr(0, "title", move |_cx| title.peek().clone())
                .bool_attr(0, "disabled", move |_cx| *disabled.peek())
                .text(1, move |_cx| label.peek().clone());

        assert_eq!(
            fold_of(view),
            "<button title=\"&quot;quoted&quot;\" disabled>A&amp;B</button>"
        );
    }

    #[test]
    fn the_fold_expands_initial_fragments_and_lists() {
        let rt = crate::Runtime::new();
        let owner = crate::Owner::new(rt.core());
        let show = owner.mutable_signal(true);
        let items = owner.mutable_vec();
        items.push(&crate::Turn::for_test(), "one".to_string());
        items.push(&crate::Turn::for_test(), "two".to_string());

        let view: LiveView<()> = LiveView::new(vec![
            el("section", None, 2),
            TplNode::AnchorSlot(SlotId(0)),
            TplNode::AnchorSlot(SlotId(1)),
        ])
        .if_fragment(
            0,
            move |_cx: &crate::Cx| *show.peek(),
            |_cx: &crate::Cx| LiveView::new(vec![el("h1", None, 1), text("shown")]),
            Some(|_cx: &crate::Cx| LiveView::new(vec![el("h1", None, 1), text("hidden")])),
            false,
        )
        .for_source(1, items, |(_row, item): (crate::Row, crate::Signal<String>)| {
            LiveView::new(vec![el("p", None, 1), TplNode::TextSlot(SlotId(0))])
                .text(0, move |_cx: &crate::Cx| item.peek().clone())
        });

        assert_eq!(
            fold_of(view),
            "<section><h1>shown</h1><p>one</p><p>two</p></section>"
        );
    }

    #[test]
    fn the_fold_emits_no_client_scaffolding() {
        // An element slot (events) and a child anchor: neither leaves a trace in the
        // static output — slots are IR, not markup, so there is nothing to strip.
        let view: LiveView<()> = LiveView::new(vec![el("main", Some(0), 1), TplNode::AnchorSlot(SlotId(1))])
            .event_map(0, "click", |_| ());

        assert_eq!(fold_of(view), "<main></main>");
    }
}
