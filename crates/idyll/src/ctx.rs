use std::cell::RefCell;
use std::error::Error;
use std::future::Future;
use std::marker::PhantomData;
use std::rc::Rc;

use type_map::TypeMap;

use crate::callback::{callback_from_sender, Callback};
use crate::capability::Client;
use crate::dev::{MessageLog, ReplayInputError, ReplayInputs};
use crate::driver::SlotId;
use crate::inbox::{Inbox, InboxSender};
use crate::runtime::MountGuard;
use crate::owner::Owner;
use crate::signal::{ListenGuard, MutableSignal, Signal};
use crate::live_view::LiveView;

// ── Typestate markers ─────────────────────────────────────────────────────────

pub struct Setup;
pub struct Live;

pub(crate) type ContextMap = Rc<ContextScope>;

/// A lexically-scoped context frame. Each component gets its **own** frame whose parent is
/// the spawning component's frame; [`provide`](Ctx::provide) inserts into this frame, and
/// [`use_context`](Ctx::use_context) walks the parent chain. So a `provide` is visible to
/// descendants and shadows ancestors, but never leaks up to the parent or across to
/// siblings — the scoping React's context has, and what lets error/suspense boundaries nest.
///
/// The *type* is public only because it flows through public plumbing (`ContextMap` in
/// `WiredView`/`LiveView::child`); its fields and constructors stay crate-private.
pub struct ContextScope {
    map: RefCell<TypeMap>,
    parent: Option<ContextMap>,
    /// The runtime this frame's tree runs on. The frame chain is the capillary every
    /// mount-plane handle flows through — a child mount, a slot instance, a spawned
    /// task all reach their runtime here.
    rt: Rc<crate::runtime::RuntimeCore>,
}

impl ContextScope {
    /// The root frame (the top of the tree; no parent), on `rt`.
    pub(crate) fn root(rt: &Rc<crate::runtime::RuntimeCore>) -> ContextMap {
        Rc::new(ContextScope {
            map: RefCell::new(TypeMap::new()),
            parent: None,
            rt: Rc::clone(rt),
        })
    }

    /// A fresh child frame that inherits `parent` by lookup but shadows/adds in isolation.
    pub(crate) fn child(parent: &ContextMap) -> ContextMap {
        Rc::new(ContextScope {
            map: RefCell::new(TypeMap::new()),
            parent: Some(Rc::clone(parent)),
            rt: Rc::clone(&parent.rt),
        })
    }

    pub(crate) fn is_root(&self) -> bool {
        self.parent.is_none()
    }

    /// The runtime this frame's tree runs on.
    pub(crate) fn runtime(&self) -> &Rc<crate::runtime::RuntimeCore> {
        &self.rt
    }

    pub(crate) fn insert<T: 'static>(&self, value: Rc<T>) {
        self.map.borrow_mut().insert::<Rc<T>>(value);
    }

    /// Look up `T` in this frame, then up the parent chain (nearest wins).
    pub(crate) fn get<T: 'static>(&self) -> Option<Rc<T>> {
        if let Some(value) = self.map.borrow().get::<Rc<T>>().cloned() {
            return Some(value);
        }
        self.parent.as_ref().and_then(|parent| parent.get::<T>())
    }
}

/// A per-island **static-paint probe** — the compute-once posture. An island root
/// publishes one into its own frame ([`Ctx::assemble`], root position only);
/// every component in that island — the root and the view-embedded children that inherit
/// the frame — shares it and *disqualifies* it the moment the island stops being a pure
/// function of its seed: it wires client work (a listener, effect, subscription, or
/// `on_unmount`), declares a nested live, or reads a context its own frame cannot satisfy.
///
/// An island whose probe survives its mount has an SSR paint that is the whole story: the
/// browser adopts the served DOM as-is and never re-runs the component. The absent-context
/// clear is what makes deciding this on the *server* sound — a mount there is isolated
/// (`parent: None`), so a `use_context` that resolves to `None` is exactly one the client
/// would satisfy from an ancestor and then re-render on. Disqualifying on that miss keeps
/// the server strictly conservative: it can only ever see fewer contexts than the client,
/// never more.
pub(crate) struct StaticProbe(std::cell::Cell<bool>);

impl StaticProbe {
    fn eligible() -> Self {
        StaticProbe(std::cell::Cell::new(true))
    }

    /// This island is not a pure adopted paint — it has client work, a nested live, or a
    /// dependency its isolated frame cannot resolve.
    pub(crate) fn disqualify(&self) {
        self.0.set(false);
    }

    pub(crate) fn is_static(&self) -> bool {
        self.0.get()
    }
}

/// One `ctx.every`/`ctx.frames` registration, pre-render (message type still known).
struct TickSubSpec<M> {
    /// `None` = animation frames (rAF); `Some(ms)` = a fixed interval.
    interval_ms: Option<f64>,
    /// The Elm-style gate: ticks flow while this signal is true. Run/pause IS the
    /// signal — no start/stop calls exist anywhere.
    gate: Signal<bool>,
    /// delta-ms → message.
    map: Box<dyn Fn(f64) -> M>,
}

/// A tick subscription after the inbox sender is bound (message type erased). The
/// runtime registers the handler and follows the gate, emitting `StartTicks`/`StopTicks`.
pub struct WiredTickSub {
    pub interval_ms: Option<f64>,
    pub gate: Signal<bool>,
    pub handler: Rc<dyn Fn(crate::live_view::Event)>,
}

/// A navigation subscription after the inbox sender is bound (message type erased).
/// The runtime registers the handler and emits `WatchNavigation`; each intent
/// dispatches an event whose `target_value` is the path.
pub struct WiredNavSub {
    pub handler: Rc<dyn Fn(crate::live_view::Event)>,
}

/// A size subscription after the inbox sender is bound (message type erased). The
/// runtime registers the handler and emits `WatchSize`; each measurement dispatches
/// an event whose `target_value` is the width in CSS pixels.
pub struct WiredSizeSub {
    pub handler: Rc<dyn Fn(crate::live_view::Event)>,
}

/// A boxed future produced by a client effect.
type ClientEffectFut = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;
/// A registered client effect, before its inbox sender is bound (held on
/// `Ctx<Setup>` until `render`).
type ClientEffect<M> = Box<dyn FnOnce(Client, InboxSender<M>) -> ClientEffectFut>;
/// A client effect after the sender is bound (message type erased). Drained and
/// spawned as a scoped task by the runtime once the view is mounted.
pub type WiredClientEffect = Box<dyn FnOnce(Client) -> ClientEffectFut>;

// ── Wired view ────────────────────────────────────────────────────────────────

/// A dispatch block after its event arms have been bound to the inbox (message type
/// erased): the block's slot tables as data, its two dispatches as the one erased
/// seam per `live_view!` block.
pub struct WiredBlock {
    pub(crate) binding_slots: Vec<SlotId>,
    /// See [`Block::paintings`](crate::live_view::Block::paintings).
    pub(crate) paintings: Vec<crate::live_view::PaintingDecl>,
    pub(crate) event_slots: Vec<(SlotId, crate::live_view::EventBinding)>,
    pub(crate) fragments: Vec<WiredFragmentDecl>,
    pub(crate) run: Rc<
        dyn Fn(&crate::signal::Cx, &[crate::driver::NodeId], u32) -> Option<crate::driver::DomOp>,
    >,
    pub(crate) on_event: Rc<dyn Fn(u32, crate::live_view::Event)>,
    pub(crate) on_fragment:
        Rc<dyn Fn(&crate::signal::Cx, crate::live_view::FragmentOp) -> WiredFragmentOut>,
}

/// A view after event mappers have been bound to the inbox. Stored in the runtime's
/// `pending_view` slot for it to process after `ctx.render()` returns.
pub struct WiredView {
    pub(crate) template: crate::template::Template,
    pub(crate) blocks: Vec<WiredBlock>,
    pub(crate) child_anchors: Vec<(SlotId, crate::runtime::ChildId)>,
    pub(crate) cleanup_guards: Vec<MountGuard>,
    pub(crate) contexts: ContextMap,
    /// The disposal scope every binding/fragment effect in this view roots in — a
    /// child of the component's owner, so a spliced subtree (an `@if` branch, a
    /// `@for` row) can be reclaimed on its own when it unmounts. The runtime spawns
    /// the view's binding [`Reaction`](crate::Reaction)s here.
    pub(crate) owner: crate::owner::Owner,
    /// Mount-time effects (from `Ctx::client_effect`), spawned post-mount.
    pub(crate) client_effects: Vec<WiredClientEffect>,
    /// Tick subscriptions (from `Ctx::every`/`Ctx::frames`), mounted with the view.
    pub(crate) tick_subs: Vec<WiredTickSub>,
    /// Navigation subscriptions (from `Ctx::navigation`), mounted with the view.
    pub(crate) nav_subs: Vec<WiredNavSub>,
    /// Size subscriptions (from `Ctx::resizes`), mounted with the view.
    pub(crate) size_subs: Vec<WiredSizeSub>,
    /// Live names declared by this view (see [`crate::live`]).
    pub(crate) live: Vec<(String, Option<String>)>,
    /// One-shot text fills (a `(expr)` text interpolation), applied at mount.
    pub(crate) oneshot: Vec<(SlotId, String)>,
}

/// A block's fragment after wiring (message type erased): the declared shape as
/// data, its code reachable only through the block's `on_fragment` dispatch.
pub struct WiredFragmentDecl {
    pub(crate) slot: SlotId,
    pub(crate) kind: WiredFragmentKind,
}

pub(crate) enum WiredFragmentKind {
    Branch { keep: bool, arms: u32 },
    List {
        structure: Rc<dyn crate::live_view::ForStructure>,
        rebuild: Rc<dyn Fn(crate::Row) -> Option<WiredView>>,
    },
    /// Rows built at view construction, wired here — never rebuilt.
    FixedList(Vec<(crate::Row, WiredView)>),
    Content,
    /// A `(view)` slot: its pre-built view wired at construction, mounted once.
    Slot(RefCell<Option<WiredView>>),
}

pub(crate) enum WiredFragmentOut {
    Selected(Option<usize>),
    LiveView(WiredView),
    None,
    Content(crate::template::View),
}

// The message type is already erased field by field (`WiredBlock`'s dispatches,
// the client effects), so the runtime holds `WiredView` directly — there is no
// second implementor to erase over.
impl WiredView {
    /// The view's template (cheap clone — const-emitted IR is a borrowed `Cow`).
    pub(crate) fn template(&self) -> crate::template::Template {
        self.template.clone()
    }
    pub(crate) fn take_blocks(&mut self) -> Vec<WiredBlock> {
        std::mem::take(&mut self.blocks)
    }
    pub(crate) fn take_child_anchors(&mut self) -> Vec<(SlotId, crate::runtime::ChildId)> {
        std::mem::take(&mut self.child_anchors)
    }
    pub(crate) fn take_cleanup_guards(&mut self) -> Vec<MountGuard> {
        std::mem::take(&mut self.cleanup_guards)
    }
    pub(crate) fn take_client_effects(&mut self) -> Vec<WiredClientEffect> {
        std::mem::take(&mut self.client_effects)
    }
    pub(crate) fn take_tick_subs(&mut self) -> Vec<WiredTickSub> {
        std::mem::take(&mut self.tick_subs)
    }
    pub(crate) fn take_nav_subs(&mut self) -> Vec<WiredNavSub> {
        std::mem::take(&mut self.nav_subs)
    }
    pub(crate) fn take_size_subs(&mut self) -> Vec<WiredSizeSub> {
        std::mem::take(&mut self.size_subs)
    }
    pub(crate) fn take_islands(&mut self) -> Vec<(String, Option<String>)> {
        std::mem::take(&mut self.live)
    }
    /// One-shot text fills — applied once at mount.
    pub(crate) fn take_oneshot(&mut self) -> Vec<(SlotId, String)> {
        std::mem::take(&mut self.oneshot)
    }
    pub(crate) fn contexts(&self) -> ContextMap {
        Rc::clone(&self.contexts)
    }
    /// The disposal scope this view's binding/fragment effects root in.
    pub(crate) fn owner(&self) -> crate::owner::Owner {
        self.owner.clone()
    }
    /// Whether this view registers anything that keeps its island from being a pure
    /// adopted paint: a DOM listener, a canvas display list, a client effect, a
    /// tick/nav/size subscription, an `on_unmount` cleanup, or a nested live. Peeked
    /// (non-draining) at mount so it can retire the island's [`StaticProbe`]; runs per
    /// view, so a listener buried in an `@if` branch or `@for` row disqualifies as
    /// surely as one at the top.
    pub(crate) fn blocks_static_paint(&self) -> bool {
        !self.client_effects.is_empty()
            || !self.tick_subs.is_empty()
            || !self.nav_subs.is_empty()
            || !self.size_subs.is_empty()
            || !self.live.is_empty()
            || self
                .blocks
                .iter()
                .any(|block| !block.event_slots.is_empty() || !block.paintings.is_empty())
            || self.cleanup_guards.iter().any(MountGuard::is_cleanup)
    }
}

fn wire_view<M: 'static>(
    view: LiveView<M>,
    sender: InboxSender<M>,
    contexts: ContextMap,
    owner: crate::owner::Owner,
    mut cleanup_guards: Vec<MountGuard>,
) -> WiredView {
    use crate::live_view::{FragmentKind, FragmentOut};

    // A `(slot)` placement's unsubscribe guard rides this view's cleanup, so it drops — and
    // removes the parent's instance — when this view (or the `@if` arm / `@for` row it is)
    // unmounts. Folded in here, at the one place a view becomes mount-ready.
    cleanup_guards.extend(view.placement_guards);
    // Slot placements subscribe now, under THIS view's frame — the tree parent's: the
    // instance's `use_context` resolves against where it is placed, while its callbacks
    // stay bound to the recipe parent's inbox.
    for (child_id, receiver) in view.placements {
        cleanup_guards.push(MountGuard::new(receiver.place(child_id, &contexts)));
    }

    let wired_blocks: Vec<WiredBlock> = view
        .blocks
        .into_iter()
        .map(|block| {
            let sender2 = sender.clone();
            let event = Rc::clone(&block.event);
            let fragments = block
                .fragments
                .into_iter()
                .map(|decl| WiredFragmentDecl {
                    slot: decl.slot,
                    // A deferred source builds here because here is where the scope
                    // is: its reactive work roots in this mount's owner.
                    kind: match decl.kind.resolve(&owner) {
                        FragmentKind::Branch { keep, arms } => {
                            WiredFragmentKind::Branch { keep, arms }
                        }
                        FragmentKind::List { structure, rebuild } => {
                            let row_sender = sender.clone();
                            let row_contexts = Rc::clone(&contexts);
                            let row_owner = owner.downgrade();
                            WiredFragmentKind::List {
                                structure,
                                rebuild: Rc::new(move |row| {
                                    let row_owner = row_owner.upgrade()?;
                                    rebuild(row).map(|row_view| {
                                        wire_view(
                                            row_view,
                                            row_sender.clone(),
                                            Rc::clone(&row_contexts),
                                            row_owner.child(),
                                            Vec::new(),
                                        )
                                    })
                                }),
                            }
                        }
                        // Rows built at construction wire now, each in its own child
                        // scope — nothing ever rebuilds them.
                        FragmentKind::FixedList(rows) => WiredFragmentKind::FixedList(
                            rows.take()
                                .into_iter()
                                .map(|(row, row_view)| {
                                    (
                                        row,
                                        wire_view(
                                            row_view,
                                            sender.clone(),
                                            Rc::clone(&contexts),
                                            owner.child(),
                                            Vec::new(),
                                        ),
                                    )
                                })
                                .collect(),
                        ),
                        FragmentKind::Content => WiredFragmentKind::Content,
                        // The slotted view wires now, in its own child scope — its
                        // block joins the mount so its events reach this reducer.
                        FragmentKind::Slot(cell) => WiredFragmentKind::Slot(RefCell::new(
                            cell.take().map(|slot_view| {
                                wire_view(
                                    slot_view,
                                    sender.clone(),
                                    Rc::clone(&contexts),
                                    owner.child(),
                                    Vec::new(),
                                )
                            }),
                        )),
                    },
                })
                .collect();
            // Every structural build the runtime requests wires here, in a fresh
            // child of the component owner — so swapping a branch or removing a row
            // reclaims exactly its subtree.
            let dispatch = Rc::clone(&block.fragment);
            let fragment_sender = sender.clone();
            let fragment_contexts = Rc::clone(&contexts);
            let fragment_owner = owner.downgrade();
            WiredBlock {
                binding_slots: block.binding_slots,
                paintings: block.paintings,
                event_slots: block.event_slots,
                fragments,
                run: Rc::clone(&block.run),
                on_event: Rc::new(move |idx, e| {
                    if let Some(msg) = event(idx, e) {
                        sender2.send(msg);
                    }
                }),
                on_fragment: Rc::new(move |cx, op| match dispatch(cx, op) {
                    FragmentOut::Selected(arm) => WiredFragmentOut::Selected(arm),
                    FragmentOut::LiveView(built) => match fragment_owner.upgrade() {
                        Some(owner) => WiredFragmentOut::LiveView(wire_view(
                            built,
                            fragment_sender.clone(),
                            Rc::clone(&fragment_contexts),
                            owner.child(),
                            Vec::new(),
                        )),
                        None => WiredFragmentOut::None,
                    },
                    FragmentOut::None => WiredFragmentOut::None,
                    FragmentOut::Content(rendered) => WiredFragmentOut::Content(rendered),
                }),
            }
        })
        .collect();

    let child_anchors = view.child_anchors;

    WiredView {
        template: view.template,
        blocks: wired_blocks,
        child_anchors,
        cleanup_guards,
        contexts,
        owner,
        client_effects: Vec::new(),
        tick_subs: Vec::new(),
        nav_subs: Vec::new(),
        size_subs: Vec::new(),
        live: view.live,
        oneshot: view.oneshot,
    }
}

/// Build a slot's receiver from the recipe parent's sender — the core of [`Ctx::slot`] and the
/// macro's children-block sugar. A placement's subscribe closure spawns the instance as its own
/// executor task (post-render, like a fragment): it runs the `recipe` — an ordinary render
/// closure — resolving the instance's own embedded children, wires the result under a child of
/// the **placing view's frame** (context follows tree; the instance's callbacks still route to
/// `sender` — messages follow recipe), and splices it at the placement's anchor. The returned
/// [`SlotGuard`](crate::slot::SlotGuard) holds the task's cancel-guard, so dropping it unmounts
/// the whole instance. The one `dyn` is this subscribe closure — the `Callback` seam.
pub fn build_slot<M, F, Fut>(sender: InboxSender<M>, recipe: F) -> crate::slot::Slot
where
    M: 'static,
    F: Fn(RenderScope<M>) -> Fut + 'static,
    Fut: std::future::Future<Output = std::result::Result<LiveView<M>, crate::Fault>>,
{
    let recipe = Rc::new(recipe);
    let subscribe = Rc::new(move |child_id: crate::runtime::ChildId, frame: &ContextMap| {
        let contexts = ContextScope::child(frame);
        let rt = Rc::clone(contexts.runtime());
        let sender = sender.clone();
        let recipe = Rc::clone(&recipe);
        let task = rt.spawn_pending(async move {
            let scope = RenderScope::new(sender.clone(), Rc::clone(&contexts));
            let view = match recipe(scope).await {
                Ok(built) => built,
                // The instance failed before rendering: route to the nearest error boundary and
                // hold the (empty) task open so the placement guard still reaps it.
                Err(error) => {
                    FaultRoute::from_frame(&contexts).route(error);
                    std::future::pending::<()>().await;
                    return;
                }
            };
            // A fresh owner per instance, kept alive by riding the instance's own cleanup — so
            // the cells live exactly as long as the instance. Its embedded children ride the
            // view's `placement_guards`, folded into cleanup by `wire_view`.
            let owner = Owner::new(contexts.runtime());
            let cleanup = vec![MountGuard::new(owner.clone())];
            let (guards, sink) = contexts.runtime().child_render_sink(child_id);
            let wired = wire_view(view, sender, Rc::clone(&contexts), owner, cleanup);
            sink(wired);
            // Hold the instance's DOM guards for the task's life; cancelling the task (dropping
            // the SlotGuard) drops them, unmounting the subtree.
            let _dom = guards;
            std::future::pending::<()>().await;
        });
        crate::slot::SlotGuard::new(move || drop(task))
    });
    crate::slot::Slot::new(subscribe)
}


// ── Ctx ───────────────────────────────────────────────────────────────────────

/// The component's **fault line**: infrastructure that broke a promise made on this
/// component's behalf (a persisted mutation the server failed to honor) fails the
/// component through here, exactly as if its own future returned `Err` — the spawner
/// races it against the component and propagates the resulting [`crate::Fault`] to
/// whatever boundary catches it. One cell per component, minted with the `Ctx`.
#[derive(Clone)]
pub(crate) struct FaultCell(Rc<RefCell<FaultState>>);

#[derive(Default)]
struct FaultState {
    error: Option<Box<dyn Error>>,
    waker: Option<std::task::Waker>,
}

impl FaultCell {
    fn new() -> Self {
        FaultCell(Rc::new(RefCell::new(FaultState::default())))
    }

    /// Fail the owning component. First fault wins; later ones are dropped (the
    /// component is already dying).
    pub(crate) fn fail(&self, error: impl Into<Box<dyn Error>>) {
        let mut state = self.0.borrow_mut();
        if state.error.is_none() {
            state.error = Some(error.into());
        }
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }

    /// Poll for a fault, registering the task's waker — the spawner's side of the race.
    pub(crate) fn poll_fault(&self, cx: &mut std::task::Context<'_>) -> Option<Box<dyn Error>> {
        let mut state = self.0.borrow_mut();
        match state.error.take() {
            Some(error) => Some(error),
            None => {
                state.waker = Some(cx.waker().clone());
                None
            }
        }
    }
}

/// A sink an **error boundary** installs into its child frame ([`Ctx::provide`]-style, in
/// [`crate::boundary`]): a view-embedded child's *terminal* fault is routed here instead of
/// climbing a drive tree, because in the flat executor there is no parent awaiting the child.
/// The boundary's sink writes the error signal it `@match`es to overlay a fallback.
#[derive(Clone)]
pub(crate) struct FaultSink(pub(crate) Rc<dyn Fn(Box<dyn Error>)>);

/// Where a view-embedded child's terminal fault goes: the nearest enclosing [`FaultSink`] in
/// its frame, or — with none watching — the top-level report. Resolved from the child's frame
/// at mount ([`crate::component::mount_child`]) so it is fixed for the child's lifetime.
#[derive(Clone)]
pub struct FaultRoute(Option<FaultSink>);

impl FaultRoute {
    pub(crate) fn from_frame(frame: &ContextMap) -> Self {
        FaultRoute(frame.get::<FaultSink>().map(|sink| (*sink).clone()))
    }

    /// Route a terminal fault to the nearest boundary, or report it if none is watching.
    pub fn route(self, error: Box<dyn Error>) {
        match self.0 {
            Some(sink) => (sink.0)(error),
            None => crate::component::report_to_log(error),
        }
    }

    /// A child frame of `parent` whose fault resolution is **this** route, shadowing
    /// whatever `parent` would resolve. An error boundary mounts its fallback under
    /// one, captured from the frame *before* the boundary installed its own sink —
    /// so a faulting fallback routes past the boundary to the enclosing one
    /// (React's semantics: an error while showing the fallback belongs to the next
    /// boundary up, never back to the boundary that is already showing it).
    pub(crate) fn shadowing_frame(self, parent: &ContextMap) -> ContextMap {
        let frame = ContextScope::child(parent);
        frame.insert::<FaultSink>(Rc::new(FaultSink(Rc::new(move |error| {
            self.clone().route(error);
        }))));
        frame
    }
}

pub struct Ctx<State, M: 'static> {
    pub(crate) inbox: Inbox<M>,
    pub(crate) sender: InboxSender<M>,
    pub(crate) fault: FaultCell,
    /// This component's write-authority node in the owner tree. Minted when the
    /// `Ctx` is created (one owner per component), it backs `ctx.mutable_signal(..)` and
    /// is what `&Ctx` presents to gated `set`/`update` — so the cells a component
    /// creates can only be written by that component's own reducer/setup.
    owner: Owner,
    contexts: ContextMap,
    render_sink: Option<Rc<dyn Fn(WiredView)>>,
    cleanup_guards: Rc<RefCell<Vec<MountGuard>>>,
    /// Mount-time effects registered via `client_effect`; consumed by `render`.
    client_effects: Vec<ClientEffect<M>>,
    /// Tick subscriptions registered via `every`/`frames`; consumed by `render`.
    tick_subs: RefCell<Vec<TickSubSpec<M>>>,
    nav_subs: RefCell<Vec<Box<dyn Fn(String) -> M>>>,
    /// Size subscriptions registered via `resizes`; consumed by `render`.
    size_subs: RefCell<Vec<Box<dyn Fn(f64) -> M>>>,
    _state: PhantomData<State>,
}

/// What a `render` view closure builds against: this mount's **sender** (the inbox its callbacks
/// bind to) and its **frame** (the scope a view-embedded child roots in, passed to
/// [`mount_child`](crate::component::mount_child) / [`spawn_child`](crate::component::spawn_child)).
/// Threaded explicitly — the child demands its frame rather than reading a thread-local.
pub struct RenderScope<M: 'static> {
    sender: InboxSender<M>,
    frame: ContextMap,
}

impl<M: 'static> RenderScope<M> {
    pub(crate) fn new(sender: InboxSender<M>, frame: ContextMap) -> Self {
        RenderScope { sender, frame }
    }
    /// The inbox this view's callbacks and view-embedded children bind to.
    pub fn sender(&self) -> &InboxSender<M> {
        &self.sender
    }
    /// The scope a view-embedded child roots in ([`mount_child`](crate::mount_child) /
    /// [`spawn_child`](crate::spawn_child) child it).
    pub fn frame(&self) -> &ContextMap {
        &self.frame
    }
}

impl<M: 'static> Ctx<Setup, M> {
    /// Assemble a fresh setup context.
    fn assemble(
        contexts: ContextMap,
        render_sink: Option<Rc<dyn Fn(WiredView)>>,
    ) -> Self {
        let inbox = Inbox::new();
        let sender = inbox.sender();
        // An island root — no render sink, so it posts to the runtime's `pending_view` rather than a
        // parent's splice — publishes the static-paint probe its whole component subtree
        // shares. View-embedded children (which carry a render sink) inherit it through the
        // frame chain rather than minting their own, so one island holds exactly one probe.
        if render_sink.is_none() {
            contexts.insert::<StaticProbe>(Rc::new(StaticProbe::eligible()));
        }
        Ctx {
            inbox,
            sender,
            fault: FaultCell::new(),
            owner: Owner::new(contexts.runtime()),
            contexts,
            render_sink,
            cleanup_guards: Rc::new(RefCell::new(Vec::new())),
            client_effects: Vec::new(),
            tick_subs: RefCell::new(Vec::new()),
            nav_subs: RefCell::new(Vec::new()),
            size_subs: RefCell::new(Vec::new()),
            _state: PhantomData,
        }
    }

    /// The root authority on one runtime — the same line `Owner::new` sits behind.
    /// Reached through [`Runtime::ctx`](crate::Runtime::ctx) and [`for_mount`](Self::for_mount):
    /// a mount spawns its children through [`spawned`](Self::spawned), and a component
    /// receives its `Ctx` as its first parameter, so nothing else starts a tree.
    pub(crate) fn root_in(rt: &Rc<crate::runtime::RuntimeCore>) -> Self {
        Self::assemble(ContextScope::root(rt), None)
    }

    /// The context a mount runs in: rooted on `rt`, or parented to the mount that
    /// encloses it (inheriting its runtime).
    ///
    /// **The one public way to start a context tree**, and it is public for a reason
    /// worth stating plainly: `guest!` expands the mount glue *into the app crate*, so
    /// visibility cannot separate generated framework code from hand-written app code.
    /// Nothing else needs it — a component receives its `Ctx` as its first parameter —
    /// so this is a seam, not a constructor to reach for.
    pub fn for_mount(rt: &crate::Runtime, parent: Option<&ContextHandle>) -> Self {
        match parent {
            Some(parent) => Self::new_under(parent),
            None => Self::root_in(rt.core()),
        }
    }

    /// Create a context whose scope is a **child frame of another mount's** — the
    /// spine of cross-mount context inheritance: a live mounted inside a
    /// store-root live parents its scope here, so the root's `provide`s are
    /// visible (and shadowable) below without anything leaking back up. Document
    /// order guarantees the provider mounted first. Reached through
    /// [`for_mount`](Self::for_mount).
    pub(crate) fn new_under(parent: &ContextHandle) -> Self {
        Self::new_with_contexts(ContextScope::child(&parent.0))
    }

    pub(crate) fn new_with_contexts(contexts: ContextMap) -> Self {
        Self::assemble(contexts, None)
    }

    /// The context a **fire-and-forget** view-embedded child runs under (a reactive `@if`/`@match`
    /// arm, a `@for` row, a slot instance): it carries the render sink its view posts to (the
    /// parent's splice, keyed by `child_id`). Its render is not awaited, so nothing witnesses it.
    pub(crate) fn spawned(
        contexts: ContextMap,
        render_sink: Rc<dyn Fn(WiredView)>,
    ) -> Self {
        Self::assemble(contexts, Some(render_sink))
    }

    /// The context an **initial-tree** view-embedded child runs under, whose render is *awaited*
    /// ([`mount_child`](crate::mount_child)). Mints the [`RenderWitness`](crate::lifecycle::RenderWitness)
    /// and wraps `render_sink` to mark it the instant the child posts its view, returning both. This
    /// is the single site that binds a witness to the sink that marks it — so the witness a caller
    /// hands to [`run_to_render`](crate::lifecycle::run_to_render) is, by construction, the one this
    /// ctx's render fires. Pairing the future with any other witness is unreachable: a witness is
    /// only ever born here, next to its sink.
    pub(crate) fn resolving(
        contexts: ContextMap,
        render_sink: Rc<dyn Fn(WiredView)>,
    ) -> (Self, crate::lifecycle::RenderWitness) {
        let witness = crate::lifecycle::RenderWitness::new();
        let marking = {
            let witness = witness.clone();
            Rc::new(move |view| {
                witness.mark();
                render_sink(view);
            }) as Rc<dyn Fn(WiredView)>
        };
        (Self::assemble(contexts, Some(marking)), witness)
    }

    /// Mount the view and transition to the live phase.
    ///
    /// The view closure receives a [`RenderScope`] (this mount's sender and frame) and returns
    /// the view IR. Each view-embedded child it builds — a top-level `Name(props)`, an
    /// `@if`/`@match` arm, a `@for` row, a slot placement — is mounted into the flat executor and
    /// its cancel-guard rides the view's `placement_guards`. Those fold into this mount's cleanup
    /// (in `wire_view`), so the whole subtree unmounts when this view does; the returned live
    /// `Ctx` has no child tree, and `recv`/`finish` drive only the inbox. This wires all event
    /// mappers, then posts the `WiredView` to a parent's splice sink (a view-embedded child) or
    /// the runtime's `pending_view` (an island root). Inert content goes through
    /// [`render_content`](Self::render_content) instead.
    pub async fn render<F, Fut>(
        mut self,
        view: F,
    ) -> std::result::Result<Ctx<Live, M>, crate::Fault>
    where
        F: FnOnce(RenderScope<M>) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<LiveView<M>, crate::Fault>>,
    {
        let scope = RenderScope::new(self.sender.clone(), Rc::clone(&self.contexts));
        // A child failing *before* it renders resolves the closure's `Err`, which is this
        // mount's own render failure — propagate it (up to the nearest error boundary).
        let view = view(scope).await?;
        let mut cleanup_guards = std::mem::take(&mut *self.cleanup_guards.borrow_mut());
        // Anchor this component's owner to the *mount*, not its setup future. A render-once
        // component returns (dropping its `Ctx`, and with it the owner) right after `render`,
        // but the DOM it produced lives on and reads these cells. Carrying an owner handle in
        // the mount guards keeps the cells alive until the component unmounts. The view-embedded
        // children ride the view's own `placement_guards` (folded in by `wire_view`).
        cleanup_guards.push(MountGuard::new(self.owner.clone()));
        let mut wired = wire_view(
            view,
            self.sender.clone(),
            Rc::clone(&self.contexts),
            self.owner.clone(),
            cleanup_guards,
        );
        self.bind_subscriptions(&mut wired);
        if let Some(render_sink) = self.render_sink.as_ref() {
            render_sink(wired);
        } else {
            self.contexts.runtime().post_pending_view(wired);
        }
        Ok(Ctx {
            inbox: self.inbox,
            sender: self.sender,
            fault: self.fault,
            owner: self.owner,
            contexts: self.contexts,
            render_sink: self.render_sink,
            cleanup_guards: self.cleanup_guards,
            client_effects: Vec::new(),
            tick_subs: RefCell::new(Vec::new()),
            nav_subs: RefCell::new(Vec::new()),
            size_subs: RefCell::new(Vec::new()),
            _state: PhantomData,
        })
    }

    /// Render inert **content** — the `view!` plane.
    /// Content is a plain [`View`](crate::View) value (no closures, so nothing in it reads a
    /// signal or takes an event), mounted as the vacuous live view; a nested `live::Name()`
    /// marker inside it still declares its island. The counterpart to [`render`](Self::render)
    /// for the content plane — no reactive closure, no view-embedded children.
    pub async fn render_content(
        self,
        content: crate::View,
    ) -> std::result::Result<Ctx<Live, M>, crate::Fault> {
        self.render(move |_| async move { Ok(LiveView::from_content(content)) }).await
    }

    /// Bind this component's registered subscriptions — client effects and tick/nav/size subs —
    /// to its inbox, erasing the message type into the wired view.
    fn bind_subscriptions(&mut self, wired: &mut WiredView) {
        let sender = self.sender.clone();
        wired.client_effects = std::mem::take(&mut self.client_effects)
            .into_iter()
            .map(|effect| {
                let sender = sender.clone();
                Box::new(move |client: Client| effect(client, sender.clone())) as WiredClientEffect
            })
            .collect();
        wired.tick_subs = self
            .tick_subs
            .take()
            .into_iter()
            .map(|spec| {
                let sender = self.sender.clone();
                let TickSubSpec { interval_ms, gate, map } = spec;
                WiredTickSub {
                    interval_ms,
                    gate,
                    handler: Rc::new(move |event: crate::live_view::Event| {
                        // A tick without its delta is a malformed dispatch — dropped
                        // like the size handler's, never a fabricated zero (which
                        // would corrupt any integrator downstream).
                        if let Some(delta) = event.timestamp {
                            sender.send(map(delta));
                        }
                    }),
                }
            })
            .collect();
        wired.nav_subs = self
            .nav_subs
            .take()
            .into_iter()
            .map(|map| {
                let sender = self.sender.clone();
                WiredNavSub {
                    handler: Rc::new(move |event: crate::live_view::Event| {
                        // A navigation intent without its path is malformed — dropped,
                        // never a fabricated empty path.
                        if let Some(path) = event.target_value.clone() {
                            sender.send(map(path));
                        }
                    }),
                }
            })
            .collect();
        wired.size_subs = self
            .size_subs
            .take()
            .into_iter()
            .map(|map| {
                let sender = self.sender.clone();
                WiredSizeSub {
                    handler: Rc::new(move |event: crate::live_view::Event| {
                        if let Some(width) =
                            event.target_value.as_deref().and_then(|v| v.parse().ok())
                        {
                            sender.send(map(width));
                        }
                    }),
                }
            })
            .collect();
    }

    /// Build a **slot** — the sender end of a view-typed prop a component places 0-to-many.
    ///
    /// `recipe` builds a fresh instance each time a placement subscribes: a view whose `=>`
    /// callbacks reach this inbox (the recipe is written in this component's `live_view!`, so its
    /// senders are already bound here). Each instance is spawned fire-and-forget into the executor
    /// (post-render, like a fragment); the placement's [`SlotGuard`](crate::SlotGuard) holds its
    /// cancel-guard, so dropping the guard unmounts it. Returns the receiver [`Slot`] to pass as
    /// the prop. The one `dyn` is the receiver's subscribe closure — the `Callback` seam.
    pub fn slot<F, Fut>(&self, recipe: F) -> crate::slot::Slot
    where
        F: Fn(RenderScope<M>) -> Fut + 'static,
        Fut: std::future::Future<Output = std::result::Result<LiveView<M>, crate::Fault>>,
    {
        build_slot(self.sender.clone(), recipe)
    }

    /// Register a mount-time effect (builder method; chain before `render`).
    ///
    /// The closure runs **once, after the view is mounted** (client-side,
    /// post-hydrate), receiving a [`Client`] capability token and this
    /// component's [`InboxSender`]. Do client-only work (read `localStorage`,
    /// focus a node, start a subscription) and **emit results as messages** —
    /// the loop stays pure, and replay reconstructs from the message log. The
    /// effect is a scoped task, cancelled if the component unmounts.
    ///
    /// On the server this never runs: an SSR mount states `client: false` across
    /// the membrane, and the mount pass spawns client effects only on client
    /// mounts (the same code re-runs in the browser, where they fire once).
    pub fn client_effect<F, Fut>(mut self, effect: F) -> Self
    where
        F: FnOnce(Client, InboxSender<M>) -> Fut + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        self.client_effects.push(Box::new(move |client, sender| {
            Box::pin(effect(client, sender)) as ClientEffectFut
        }));
        self
    }

    /// Subscribe to **timer ticks** while `gate` is true — Elm's `Time.every` as a
    /// signal-gated subscription. Each tick delivers `map(delta_ms)` to this
    /// component's inbox; flipping the gate starts/stops the browser timer. Run/pause
    /// IS the signal — no handles, no cancellation bookkeeping. Server mounts never
    /// tick (the commands are client concerns); tick-0 state paints via ordinary
    /// bindings.
    pub fn every(
        &self,
        gate: &Signal<bool>,
        interval: std::time::Duration,
        map: impl Fn(f64) -> M + 'static,
    ) {
        self.tick_subs.borrow_mut().push(TickSubSpec {
            interval_ms: Some(interval.as_secs_f64() * 1000.0),
            gate: gate.clone(),
            map: Box::new(map),
        });
    }

    /// Subscribe to **animation frames** while `gate` is true — Elm's
    /// `onAnimationFrameDelta`. See [`every`](Ctx::every); `map` receives the delta
    /// since the previous frame, in milliseconds.
    pub fn frames(&self, gate: &Signal<bool>, map: impl Fn(f64) -> M + 'static) {
        self.tick_subs.borrow_mut().push(TickSubSpec {
            interval_ms: None,
            gate: gate.clone(),
            map: Box::new(map),
        });
    }

    /// Subscribe to **navigation intents** — the SPA-live shape's one wiring. The
    /// browser intercepts same-origin link clicks (pushing the history entry first)
    /// and `popstate` (history already moved), and delivers `map(path)` to this
    /// inbox; the loop answers with [`Turn::navigate`](Ctx::navigate), which refires
    /// the route query and replays the response into the store. URL semantics live in
    /// the browser; data semantics live here — one message arm covers both
    /// directions. Server mounts never subscribe (client concern, like ticks).
    pub fn navigation(&self, map: impl Fn(String) -> M + 'static) {
        self.nav_subs.borrow_mut().push(Box::new(map));
    }

    /// Subscribe to **this component's own width**, in CSS pixels — a `ResizeObserver`
    /// on its mount root, delivering `map(width)` on mount and on every change. A
    /// component that lays itself out in its own coordinates (a diagram whose paths are
    /// computed here, not by CSS) can only do so once it knows how much room it has;
    /// the width is a message like every other input, so a resize replays from the log.
    ///
    /// The root is measured, never the viewport: what a component can use is what its
    /// own box gives it, and no layout rule has to be restated to work that out. A
    /// server mount never subscribes — there is nothing to measure (client concern,
    /// like ticks), so the first measurement arrives on hydration.
    pub fn resizes(&self, map: impl Fn(f64) -> M + 'static) {
        self.size_subs.borrow_mut().push(Box::new(map));
    }

    /// Create a `Callback<T>` that sends `mapper(value)` into this inbox.
    pub fn callback<T: 'static>(&self, mapper: impl Fn(T) -> M + 'static) -> Callback<T> {
        callback_from_sender(self.sender.clone(), mapper)
    }

    /// Deliver `mapper(&value)` to the inbox whenever `signal` changes (not on the
    /// initial value). A graph effect under the hood; drop the returned
    /// `ListenGuard` to stop.
    pub fn listen<T: Clone + 'static>(
        &self,
        signal: &Signal<T>,
        mapper: impl Fn(&T) -> Option<M> + 'static,
    ) -> ListenGuard {
        let sender = self.sender.clone();
        let signal = signal.clone();
        let first = std::cell::Cell::new(true);
        // A subscription finer-grained than a scope: the guard holds the effect's only
        // strong reference, so dropping it stops the effect.
        let effect = crate::signal::reaction::Reaction::spawn_guarded(
            self.contexts.runtime(),
            move |cx| {
                let value = signal.get(cx);
                if first.replace(false) {
                    return; // a change subscription, not the initial value
                }
                if let Some(msg) = mapper(&value) {
                    sender.send(msg);
                }
            },
        );
        ListenGuard::new(effect)
    }

    /// Get a cloneable sender for passing to callbacks and closures.
    pub fn inbox_sender(&self) -> InboxSender<M> {
        self.sender.clone()
    }

    /// Install this component as the **error boundary** for its subtree: a view-embedded child's
    /// terminal fault routes to `map(error)` in *this* inbox (see [`FaultRoute`]). It arrives as
    /// a message — so it lands in this component's log and replay reconstructs it — which the
    /// reducer turns into a rendered fallback. Installed into this frame, so every descendant
    /// mounted under it (until a nearer error boundary shadows this one) routes here.
    pub fn fault_sink(&self, map: impl Fn(Rc<dyn Error>) -> M + 'static) {
        let sender = self.sender.clone();
        self.contexts.insert::<FaultSink>(Rc::new(FaultSink(Rc::new(move |error: Box<dyn Error>| {
            sender.send(map(Rc::from(error)));
        }))));
    }
}

impl<M: 'static> Ctx<Live, M> {
    /// Await the next message and enter its **turn**. Yields the message together
    /// with a [`Reducer`] — the per-turn handle that mints the [`Turn`](crate::Turn)
    /// capability (for `signal.now`/`set`/`update`) and carries the turn's effects
    /// (`emit`/`mutate`/`client`). The `Reducer` borrows `&mut self`, so it cannot
    /// outlive the turn or be held across the next `recv`, and a `'static` effect
    /// can never capture it. This `recv` is the sole mint site of the turn.
    ///
    /// A fault on this component's own fault line (a mutation's broken promise, a
    /// store absorb the owner cannot apply) resolves here as the `Err` — the loop's
    /// `?` carries it out of the body, up to whatever boundary catches it.
    ///
    /// Leaf reactivity runs off the reactive flush, not here; this poll fires only on message
    /// delivery. View-embedded children are not driven here — each lives in the flat executor
    /// (see [`mount_child`](crate::component::mount_child)), reaped when this mount's cleanup
    /// guards drop. A child's fault routes to the nearest error boundary through the frame's
    /// [`FaultSink`], not up a drive tree.
    pub async fn recv(&mut self) -> std::result::Result<(M, Reducer<'_, M>), crate::Fault> {
        let msg = std::future::poll_fn(|cx| {
            // Fault first, so the waker is registered before an absorb (applied
            // inside `poll_recv`) can fail the component and need to wake it.
            if let Some(error) = self.fault.poll_fault(cx) {
                return std::task::Poll::Ready(Err(error));
            }
            match self.inbox.poll_recv(cx) {
                std::task::Poll::Ready(msg) => {
                    // An absorb applied inside that poll may have faulted; the fault
                    // outranks the queued message — no turn runs on a component that
                    // is already dying (first fault wins, and it wins *now*).
                    if let Some(error) = self.fault.poll_fault(cx) {
                        return std::task::Poll::Ready(Err(error));
                    }
                    std::task::Poll::Ready(Ok(msg))
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        })
        .await?;
        Ok((msg, Reducer { ctx: self }))
    }

    /// A cloneable sender for passing to callbacks from the live loop.
    pub fn inbox_sender(&self) -> InboxSender<M> {
        self.sender.clone()
    }
}

impl Ctx<Live, crate::Never> {
    /// The terminal of a **message-less** component: rendering is done, so hold the mount
    /// for its lifetime. A `Never` inbox can never yield a message, so this diverges; it
    /// appears once, at the end of the body:
    ///
    /// ```ignore
    /// ctx.render(view).finish().await
    /// ```
    ///
    /// Divergence is the point: never returning keeps this `Ctx` (and the owner behind its
    /// cells) alive as long as the DOM it produced. Its children do not ride this future —
    /// they live in the flat executor, reaped when this mount's guards drop.
    ///
    /// It yields [`crate::Result`] (whose `Ok` never occurs — a `Never` inbox can't deliver
    /// a message) so a **fault** raised on this component's own fault line can still leave:
    /// `recv().await?` propagates it as the body's `Err`, on up to an error boundary.
    pub async fn finish(mut self) -> crate::Result {
        loop {
            let (never, _) = self.recv().await?;
            match never {}
        }
    }
}

/// The **turn handle** yielded by [`Ctx::recv`]: the app's capability for the
/// current message. It mints the [`Turn`](crate::Turn) proof for `signal.now`/`set`/
/// `update` (via `&reducer`) and carries the turn's effects (`emit`, `mutate`,
/// `client`). Borrows the live `Ctx` for the turn's duration only — it cannot be
/// stored, held across the next `recv`, or captured by a `'static` effect.
pub struct Reducer<'a, M: 'static> {
    ctx: &'a mut Ctx<Live, M>,
}

/// `&reducer` **is** the turn capability: the sole discharge of [`Turn::mint`],
/// safe because a `Reducer` exists only inside a `recv`-driven turn.
impl<'a, 'r, M: 'static> From<&'r Reducer<'a, M>> for crate::signal::Turn<'r> {
    fn from(_reducer: &'r Reducer<'a, M>) -> Self {
        crate::signal::Turn::mint()
    }
}

impl<'a, M: 'static> Reducer<'a, M> {
    /// Send a message back into this component's inbox (handled on a later turn).
    pub fn emit(&self, msg: M) {
        self.ctx.sender.send(msg);
    }

    /// Mint a [`Client`] capability token — client-only effects (DOM, `localStorage`)
    /// scoped to this turn. Absent on `Ctx<Setup>`, so they cannot run during the
    /// render that produces SSR HTML.
    pub fn client(&self) -> Client {
        Client::new()
    }

    /// Fire a persisted server [`Mutation`](crate::Mutation) with its typed variables.
    /// **Success is data**: the refresh seed riding the response absorbs into the
    /// store, and every projection updates — no message arrives, because there is
    /// nothing to say that the data doesn't. **Failure is a fault**: transport,
    /// decode, or the server handler's `Err` fails this component through its fault
    /// line — the app declared a boot-validated op, and the infrastructure broke its
    /// promise, which is a component failure for the nearest boundary, never a
    /// console whisper. Only the operation's [`OpHash`] crosses the wire.
    ///
    /// (A variant delivering the typed outcome as a message — optimistic flows,
    /// domain rejections with UI — is deliberately unbuilt until its first real
    /// user: every observer in the tree today ignored the success value.)
    ///
    /// Live-only on purpose: a mutation is a client effect. When a live's pre-`recv`
    /// code runs during the server paint, the emitted request is discarded by the HTML
    /// fold — the same code re-runs on the client mount and fires the request for real.
    ///
    /// [`OpHash`]: crate::OpHash
    pub fn mutate<Mut: crate::Mutation>(&self, vars: Mut::Vars) {
        let fault = self.ctx.fault.clone();
        let sink = self.ctx.use_context::<SeedSink>();
        let args = match serde_json::to_vec(&vars) {
            Ok(args) => args,
            Err(err) => {
                fault.fail(crate::MutationError::Vars(err.to_string()));
                return;
            }
        };
        self.ctx.contexts.runtime().enqueue_server_request(
            Mut::op_hash(),
            args,
            Box::new(move |response| {
                // The wire is an envelope: `{ outcome, seed }`. The seed — the route
                // query re-executed after the mutation — replays into the context
                // store (value-equality absorption makes it a delta: unchanged
                // records don't fire). The outcome itself is dropped: the store is
                // the one channel data arrives through.
                // `RawValue` keeps the seed's original bytes: the envelope is opened,
                // never re-derived — no parse-then-reserialize round trip.
                #[derive(serde::Deserialize)]
                struct Envelope {
                    seed: Option<Box<serde_json::value::RawValue>>,
                }
                match response {
                    Ok(bytes) => match serde_json::from_slice::<Envelope>(&bytes) {
                        Ok(Envelope { seed }) => {
                            // `Option<RawValue>` keeps an explicit `null` as raw bytes
                            // (serde's documented raw-value behaviour), so absent and
                            // null both mean "no refresh rode along".
                            if let (Some(sink), Some(seed)) = (&sink, seed) {
                                if seed.get() != "null" {
                                    (sink.0)(seed.get().as_bytes());
                                }
                            }
                        }
                        Err(err) => fault.fail(crate::MutationError::Decode(err.to_string())),
                    },
                    Err(err) => fault.fail(crate::MutationError::Request(err)),
                }
            }),
        );
    }

    /// Re-execute the persisted route query for `path` — **navigation as data**, the
    /// SPA live's answer to a [`navigation`](Ctx::navigation) intent. Success is
    /// silent and arrives as state: the response (the executed `Preloaded` payload)
    /// replays into the context store, the live's route projection swaps its arm.
    /// URL semantics (history, scroll) are the browser's, handled where the intent
    /// was raised. Failure faults this live to its boundary — and the browser has
    /// already fallen back to a document navigation for a path the route refused.
    pub fn navigate<Q: crate::Query>(&self, path: impl Into<String>) {
        let fault = self.ctx.fault.clone();
        let sink = self.ctx.use_context::<SeedSink>();
        self.ctx.contexts.runtime().enqueue_navigate(
            Q::op_hash(),
            path.into(),
            Box::new(move |response| match response {
                // The response IS the seed (the executed route query, whole).
                Ok(bytes) => {
                    if let Some(sink) = &sink {
                        (sink.0)(&bytes);
                    }
                }
                Err(err) => fault.fail(crate::NavigateError(err)),
            }),
        );
    }
}

/// Reactive minting. Every `Ctx` state may create cells — the inert-content guarantee
/// needs no typestate: content is not a component at all (`view!` builds plain
/// [`View`](crate::View) IR with no closures, so nothing in it can read a
/// cell).
impl<State, M: 'static> Ctx<State, M> {
    /// Create a signal owned by this component. The returned [`MutableSignal`] can only
    /// be written by presenting this `Ctx` (or its [`Owner`]) — so a spawned
    /// effect, which cannot capture `&Ctx`, can never mutate it behind the loop.
    pub fn mutable_signal<T: 'static>(&self, value: T) -> MutableSignal<T> {
        self.owner.mutable_signal(value)
    }

    /// A signal that never changes. No writer is kept, so "never changes" is a fact
    /// about the value rather than a promise about the code — which is what a component
    /// asking for a `Signal<T>` needs when this caller's value happens to be fixed.
    pub fn constant<T: 'static>(&self, value: T) -> Signal<T> {
        self.mutable_signal(value).read()
    }

    /// Create a [`MutableVec`](crate::MutableVec) owned by this component, so its
    /// row cells dispose with the component.
    pub fn mutable_vec<T: 'static>(&self) -> crate::MutableVec<T> {
        self.owner.mutable_vec()
    }

    /// The same, holding `values` from the start — the rows a component knows about
    /// before it renders, which is the only fill that needs no turn to witness it.
    pub fn mutable_vec_of<T: 'static>(&self, values: Vec<T>) -> crate::MutableVec<T> {
        self.owner.mutable_vec_of(values)
    }

    /// A keyed live projection: `source` reads reactive cells (through the read
    /// capability) and yields a `Vec<T>`; the returned [`KeyedVec`](crate::KeyedVec)
    /// reconciles it by `key` (rows keep identity, unchanged rows do nothing).
    /// `source` re-runs — re-tracking its exact dependency set — whenever anything it
    /// read changes. The derivation is a graph effect rooted in this component and
    /// disposes with it.
    pub fn synced<T, Key>(
        &self,
        source: impl Fn(&crate::Cx) -> Vec<T> + 'static,
        key: impl Fn(&T) -> Key + 'static,
    ) -> crate::KeyedVec<T, Key>
    where
        T: Clone + PartialEq + 'static,
        Key: Eq + std::hash::Hash + 'static,
    {
        crate::KeyedVec::derived(&self.owner, source, key)
    }

    /// Derive a [`Computed`](crate::Computed) owned by this component. `f` reads
    /// reactive cells through the capability; its exact read set (re-collected each
    /// run) is the computed's dependencies. The derived cell disposes with the
    /// component that created it — not with its dependencies — so a computed built
    /// from a parent's prop signal does not outlive a remounted child.
    pub fn computed<T, F>(&self, f: F) -> crate::Computed<T>
    where
        T: Clone + PartialEq + 'static,
        F: Fn(&crate::Cx) -> T + 'static,
    {
        self.owner.computed(f)
    }

    /// Derive a **deferred** view of `source` owned by this component: a read-only
    /// [`Signal`](crate::Signal) that trails `source`, re-committing on the idle
    /// lane so an expensive subtree reading it yields to urgent input work
    /// (fine-grained `useDeferredValue`). Disposed when the component unmounts.
    pub fn deferred<T, F>(&self, source: F) -> crate::Signal<T>
    where
        T: Clone + PartialEq + 'static,
        F: Fn(&crate::Cx) -> T + 'static,
    {
        self.owner.deferred(source)
    }

    /// Spawn an effect owned by this component: `f` runs now and re-runs whenever a
    /// reactive cell it read changes. Detached when the component unmounts. Effects
    /// are `'static`, so they cannot hold a [`Turn`](crate::Turn) — they read
    /// (tracked) and emit messages, never write state directly.
    pub fn effect<F>(&self, f: F) -> crate::Reaction
    where
        F: Fn(&crate::Cx) + 'static,
    {
        self.owner.effect(f)
    }

    /// This component's owner node — its write-authority handle. Hand it to a
    /// provider (e.g. a router or data cache) so that provider's cells are
    /// rooted in this component rather than minted from thin air.
    pub fn owner(&self) -> Owner {
        self.owner.clone()
    }
}

/// An opaque handle to a component's live context frame, held so a **later mount**
/// can parent its scope to it ([`Ctx::new_under`]). Deliberately opaque: a holder can
/// hang children off the frame, never read or write it — provide/consume stay the
/// component's own (`Ctx`-gated) verbs.
#[derive(Clone)]
pub struct ContextHandle(pub(crate) ContextMap);

impl ContextHandle {
    /// Whether the island rooted at this frame is a **static paint**: it wired no client
    /// work, declared no nested live, and read no context outside its own frame, so its
    /// SSR paint is the whole story and the browser can adopt it without re-running the
    /// component. Read once, after the mount's initial flush has settled (see
    /// [`StaticProbe`]); absent probe (a non-root frame) reads as not-static.
    pub fn paint_is_static(&self) -> bool {
        self.0.get::<StaticProbe>().is_some_and(|probe| probe.is_static())
    }
}

/// The store-absorb capability a store-root provides through context. Mutation
/// responses carry a refresh seed alongside their outcome ([`Ctx::mutate`]); when a
/// sink is in scope, the requesting component's wrapper hands the seed's bytes here.
/// A sink built by [`Ctx::absorber`] queues the bytes for the **owning component's
/// turn** — the absorb is a message in the owner's log, never a delivery-callback
/// side effect, so a totally-ordered tree log reconstructs the store on replay.
#[derive(Clone)]
pub struct SeedSink(pub Rc<dyn Fn(&[u8])>);

impl<M: 'static> Ctx<Setup, M> {
    /// Register this component as a store owner: `apply` runs in this component's
    /// turn (and lands in its message log) for every absorb the returned sink
    /// receives. `apply` fails this component through its fault line — bytes the
    /// store cannot use are the same broken promise [`Reducer::mutate`] faults on,
    /// reaching the same boundary, and the sink that delivered them has no turn of
    /// its own to report against.
    ///
    /// Setup-only, once: a store root declares itself before rendering, and a second
    /// absorber would silently orphan every sink the first one handed out.
    pub fn absorber<E: Into<Box<dyn Error>>>(
        &self,
        apply: impl Fn(&crate::Turn, &[u8]) -> std::result::Result<(), E> + 'static,
    ) -> SeedSink {
        let fault = self.fault.clone();
        let installed = self.inbox.set_absorber(Rc::new(move |turn: &crate::Turn, bytes: &[u8]| {
            if let Err(error) = apply(turn, bytes) {
                fault.fail(error);
            }
        }));
        if !installed {
            // The same contract-violation class as completing unrendered: fault the
            // one component (guests are panic=abort — a panic here would kill every
            // island on the page). The dead sink matches the dying component.
            let error: Box<dyn Error> =
                "one absorber per component — a second would orphan the first's sinks".into();
            self.fault.fail(error);
            return SeedSink(Rc::new(|_| {}));
        }
        let sender = self.sender.clone();
        SeedSink(Rc::new(move |bytes: &[u8]| sender.send_absorb(bytes.to_vec())))
    }

    /// Provide a value into this component's context frame, visible to descendants
    /// (and shadowing ancestors). Setup-only: `use_context` is a one-shot read, so a
    /// post-render provide would be visible or invisible depending on when each
    /// descendant happened to read — time-of-read semantics the tree never promises.
    pub fn provide<T: 'static>(&self, value: T) {
        self.contexts.insert::<T>(Rc::new(value));
    }

    pub fn record_replay_inputs(&self) -> ReplayInputs {
        let inputs = ReplayInputs::recording();
        self.provide(inputs.clone());
        inputs
    }

    pub fn replay_inputs(&self, inputs: ReplayInputs) {
        self.provide(inputs);
    }
}

impl<State, M: 'static> Ctx<State, M> {
    /// This component's context frame, as a handle another mount can inherit from.
    /// The guest's live glue keeps one per mounted live; a child live's mount
    /// passes it to [`Ctx::new_under`].
    pub fn context_handle(&self) -> ContextHandle {
        ContextHandle(Rc::clone(&self.contexts))
    }

    /// Whether this mount is in **owner position**: its context frame has no parent —
    /// the server's isolated per-mount paint, or the browser's page root. A mount
    /// with a parent frame inherits its provides from above; one without IS the top
    /// of its tree, so building root-scoped state (a store) here is legitimate where
    /// doing so under a parent would silently shadow the real owner.
    pub fn is_root_mount(&self) -> bool {
        self.contexts.is_root()
    }

    pub fn on_unmount(&self, fut: impl Future<Output = ()> + 'static) {
        self.cleanup_guards
            .borrow_mut()
            .push(MountGuard::cleanup(self.contexts.runtime(), fut));
    }

    pub fn use_context<T: 'static>(&self) -> Option<Rc<T>> {
        let value = self.contexts.get::<T>();
        if value.is_none() {
            // A context this frame cannot satisfy is a reach for an ancestor's provide:
            // harmless-looking on the isolated server (it resolves to `None`), but on the
            // client it binds a reactive edge to that ancestor and re-renders when the
            // ancestor writes. Either way the island is no longer a pure paint the browser
            // can adopt untouched — retire its static probe.
            if let Some(probe) = self.contexts.get::<StaticProbe>() {
                probe.disqualify();
            }
        }
        value
    }

    pub fn now_millis(&self) -> std::result::Result<u64, ReplayInputError> {
        let rt = Rc::clone(self.contexts.runtime());
        if let Some(inputs) = self.use_context::<ReplayInputs>() {
            inputs.now_millis(move || platform_now_millis(&rt))
        } else {
            Ok(platform_now_millis(&rt))
        }
    }

    pub fn random_u64(&self) -> std::result::Result<u64, ReplayInputError> {
        let rt = Rc::clone(self.contexts.runtime());
        if let Some(inputs) = self.use_context::<ReplayInputs>() {
            inputs.random_u64(move || platform_random_u64(&rt))
        } else {
            Ok(platform_random_u64(&rt))
        }
    }
}

impl<State, M> Ctx<State, M>
where
    M: serde::Serialize + 'static,
{
    pub fn record_messages(&self) -> MessageLog {
        let log = MessageLog::new();
        self.inbox.set_recorder(Some(log.recorder::<M>(self.contexts.runtime())));
        self.inbox.set_absorb_recorder(Some(log.absorb_recorder(self.contexts.runtime())));
        log
    }

    pub fn stop_recording_messages(&self) {
        self.inbox.set_recorder(None);
        self.inbox.set_absorb_recorder(None);
    }
}

// Randomness enters the guest through exactly one door: the WASI insecure-seed, which
// the SSR host and the browser both set to the server's **unguessable per-request
// seed**. `random_u64` seeds a per-runtime splitmix64 from it once, then advances
// deterministically — so the same seed yields the same sequence, and an app using
// randomness stays replayable (the seed is the only nondeterministic input, and it is
// recorded with the page). `now_millis` reads the monotonic clock, whose browser shim
// is a deterministic virtual clock; real time reaches components only as tick `dt`
// carried on messages. Both live on the runtime core, so each guest instance (a fresh
// SSR render, or the browser page) seeds from its own delivered entropy.

fn platform_now_millis(rt: &crate::runtime::RuntimeCore) -> u64 {
    // Monotonic milliseconds since this runtime's first read — a virtual timeline on
    // the browser (the deterministic monotonic-clock shim), not the wall clock, so no
    // real time leaks into replayable state.
    let origin = match rt.time_origin.get() {
        Some(origin) => origin,
        None => {
            let origin = std::time::Instant::now();
            rt.time_origin.set(Some(origin));
            origin
        }
    };
    origin.elapsed().as_millis() as u64
}

fn platform_random_u64(rt: &crate::runtime::RuntimeCore) -> u64 {
    // splitmix64: advance the state by the golden gamma, output the mixed state.
    let state = rt.prng.get().unwrap_or_else(seed_from_platform);
    let next = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    rt.prng.set(Some(next));
    mix64(next)
}

/// Draw the initial PRNG seed from `std`'s `RandomState`, which pulls
/// `wasi:random/insecure-seed` — on the browser the server-delivered seed, in the SSR
/// host the per-request seed the host installs, natively the OS. Salted so the state is
/// not the raw seed. This is the single point host entropy enters; all draws after are
/// deterministic from it.
fn seed_from_platform() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0x1DEA_5EED_5A17_C0DE); // salt
    hasher.finish()
}

fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
