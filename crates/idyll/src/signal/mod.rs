//! # The reactive core
//!
//! Fine-grained reactivity with **dynamically-tracked dependencies** and a
//! **borrowed read capability**, so that application code is reactively correct
//! *if it compiles*. Three kinds of node, one mechanism:
//!
//! - **Sources** — [`MutableSignal<T>`] / [`Signal<T>`]: a writable cell and its
//!   read-only view. A read registers a dependency; a write marks observers and
//!   schedules a flush.
//! - **Derived nodes** — [`Computed<T>`](computed::Computed): a cached value
//!   recomputed from whatever it read last time.
//! - **Effects** — the view bindings and control-flow fragments (built on
//!   [`Reaction`](reaction::Reaction)): the pull *roots* that touch the DOM.
//!
//! Every node is an [`Rc`] cell in the reactive [`graph`]; the dependency graph and
//! propagation live there.
//!
//! ## Writer and reader
//!
//! Write authority is a **linear resource**. [`MutableSignal<T>`] is the writer,
//! held where the writing happens — a reducer loop, a store owner. [`read`] hands
//! out [`Signal<T>`], the reader: `Clone`, flowing freely into view bindings and
//! derived nodes. A reader keeps the cell alive, so [`Signal::get`] is infallible.
//!
//! ## The safety model
//!
//! Dynamic tracking is only correct if a read (a) happens inside a running
//! computation and (b) is attributed to *that* computation. Both are structural:
//!
//! - The read capability [`Cx`] **carries the current observer** by borrow. A
//!   tracked read ([`MutableSignal::get`]) demands a `&Cx`, and `Cx` is minted only
//!   by the [`graph`] while a computation is on the stack, so a read cannot happen
//!   outside a reactive scope and the borrow cannot be smuggled into a `'static`
//!   closure (an effect, a subscriber, a spawned task). *App code can never
//!   construct a `Cx`.*
//! - **Untracked reads and writes** are confined to the reducer turn by a second
//!   capability, [`Turn`]: [`now`](MutableSignal::now) (sample) and
//!   [`set`](MutableSignal::set)/[`update`](MutableSignal::update) (write) each take
//!   one. A `Turn` is minted only by the driver's `recv` (through the
//!   [`Reducer`](crate::ctx::Reducer)) and is borrowed/`!'static`, so a `'static`
//!   effect holds none — it can only emit messages. *Which* cells a turn may write
//!   is decided by the `MutableSignal`/`Signal` split; the token decides *when*.
//!
//! ## Glitch-free by construction
//!
//! Propagation is **mark-and-pull** ([`graph`]): a write marks observers stale but
//! recomputes nothing; recomputation happens lazily when a value is pulled, after
//! the graph has settled, so no observer ever reads a half-updated frame. A
//! **derived** value that did not change (compared with `PartialEq`) never marks its
//! observers, so a diamond recomputes its apex exactly once. (A source write always
//! wakes — `set` has no `PartialEq` bound; the binding layer's output-equality gate
//! is what keeps the DOM quiet, not the cell.)
//!
//! ## Single-threaded
//!
//! The graph and the capability are thread-local; handles are `!Send`/`!Sync`. This
//! matches the runtime (one guest instance per thread) and lets the capability be a
//! bare borrow rather than a branded generative lifetime.

pub(crate) mod graph;
pub(crate) mod scope;
pub mod computed;
pub mod reaction;
pub mod vec;

use std::cell::{Cell, Ref, RefCell};
use std::marker::PhantomData;
use std::rc::{Rc, Weak};

use serde::{Deserialize, Serialize};

use crate::owner::Owner;
use crate::runtime::RuntimeCore;

pub use computed::Computed;
pub use graph::Lane;
use graph::NodeCore;
pub use vec::{Row, SignalVec};

// ── ID generation ─────────────────────────────────────────────────────────────

/// Opaque identifier for a signal instance. The key that lines a server render up
/// with the client that hydrates it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct SignalId(u64);

impl SignalId {
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// The read capability. It **carries the current observer**: a tracked read
/// ([`MutableSignal::get`]/[`Computed::get`](computed::Computed::get)) registers the
/// cell it reads against that observer. Minted only by the [`graph`] while a
/// computation runs, and — deliberately — not `Copy`/`Clone`/`'static`, so it is
/// threaded as `&Cx` and never stored.
pub struct Cx<'a> {
    /// What reads attribute to: a graph node, a view scope's running binding index,
    /// or nothing (an untracked framework read seeding a derived cell's first value).
    observer: CxObserver<'a>,
    _not_send: PhantomData<*mut ()>,
}

/// The observer a `Cx` carries. A binding observer carries its index — the proof the
/// minting run holds, riding the capability instead of being re-derived downstream.
pub(crate) enum CxObserver<'a> {
    None,
    Node(&'a Rc<NodeCore>),
    Binding(&'a Rc<scope::ViewScope>, u32),
}

impl<'a> Cx<'a> {
    pub(crate) fn tracking(observer: &'a Rc<NodeCore>) -> Self {
        Cx {
            observer: CxObserver::Node(observer),
            _not_send: PhantomData,
        }
    }

    /// The capability a view scope mints for one binding run: reads attribute to
    /// `idx`, the binding being run.
    pub(crate) fn binding(scope: &'a Rc<scope::ViewScope>, idx: u32) -> Self {
        Cx {
            observer: CxObserver::Binding(scope, idx),
            _not_send: PhantomData,
        }
    }

    /// A capability that registers nowhere — for framework-internal seeding, never
    /// handed to app code (which only ever receives a tracking `&Cx`).
    pub(crate) fn untracked() -> Self {
        Cx {
            observer: CxObserver::None,
            _not_send: PhantomData,
        }
    }

    pub(crate) fn observer(&self) -> &CxObserver<'a> {
        &self.observer
    }
}

/// The **message-turn capability**. Its existence proves the component is handling a
/// message (render already happened), so an untracked read ([`MutableSignal::now`])
/// and a write ([`MutableSignal::set`]) both take one: reads sample state to compute
/// the *next* frame, never the current paint, and a `'static` effect (which cannot
/// hold the borrow) can only emit messages.
///
/// A **unique** token: neither `Copy` nor `Clone`, and invariant in `'a`, so it can
/// be neither duplicated into a longer-lived place nor widened to `'static`. App code
/// obtains one only by converting the [`Reducer`](crate::ctx::Reducer) borrow that
/// `recv` yields — which is itself lifetime-bound to the turn — so the proof cannot
/// outlive the thing it proves.
pub struct Turn<'a> {
    _scope: PhantomData<&'a mut &'a ()>,
}

impl<'a> Turn<'a> {
    pub(crate) fn mint() -> Self {
        Turn { _scope: PhantomData }
    }
}

/// Proof that the caller is inside a message turn — what every untracked read and
/// every write asks for. Implemented for the [`Reducer`](crate::ctx::Reducer) borrow
/// `recv` yields and for a borrow of the [`Turn`] derived from it, so the proof is
/// always *borrowed* from something whose lifetime is the turn: it can be shown
/// repeatedly and never moved somewhere that outlives it.
///
/// Sealed. The set of things that count as proof is closed, so no downstream type can
/// declare itself in a turn.
pub trait InTurn: sealed::Sealed {}

mod sealed {
    pub trait Sealed {}
    impl<M: 'static> Sealed for &crate::ctx::Reducer<'_, M> {}
    impl Sealed for &super::Turn<'_> {}
}

impl<M: 'static> InTurn for &crate::ctx::Reducer<'_, M> {}
impl InTurn for &Turn<'_> {}

#[cfg(any(test, feature = "testing"))]
impl Turn<'static> {
    /// Mint a turn outside a component — test harnesses only, and gated behind the
    /// `testing` feature so an app cannot reach it. In an app the only turns that
    /// exist come from a reducer, which is the point of the capability: without this
    /// gate the proof could simply be fabricated, and it would prove nothing.
    pub fn for_test() -> Self {
        Turn::mint()
    }
}


// ── Per-runtime signal state ────────────────────────────────────────────────────

/// The reactive half of a [`RuntimeCore`]: the effect queues, the flush-scheduled
/// latch, and the signal-id allocation scope. One per runtime — nothing reactive is
/// ambient.
#[derive(Default)]
pub(crate) struct SignalState {
    render_scope: RefCell<RenderScopeState>,
    /// Whether a microtask-flush has already been scheduled this turn.
    pub(crate) flush_scheduled: Cell<bool>,
    pub(crate) effects: graph::EffectQueues,
}

impl SignalState {
    /// Allocate the next deterministic signal id in the current render scope.
    pub(crate) fn next_signal_id(&self) -> SignalId {
        let mut scope = self.render_scope.borrow_mut();
        if scope.counter == 0 {
            // Ids start at 1: 0 is reserved as a "never allocated" sentinel.
            scope.counter = 1;
        }
        let id = scope.counter;
        scope.counter += 1;
        SignalId(id)
    }
}

/// The signal-id allocation scope. Signal ids line a server render up with the
/// client that hydrates it, so they must be **deterministic**, not a process-global
/// running counter. Each render (the shell, and every deferred boundary) runs inside
/// a fresh scope whose counter starts at 1, so the same tree always allocates the
/// same ids.
#[derive(Clone, Debug, Default)]
struct RenderScopeState {
    boundary: Option<String>,
    counter: u64,
}

impl RenderScopeState {
    fn fresh(boundary: Option<String>) -> Self {
        RenderScopeState { boundary, counter: 1 }
    }
}

/// RAII guard that enters a fresh signal-id scope on one runtime for one
/// render/boundary and restores the previous scope on drop.
#[must_use = "the scope is only active while the guard is held"]
pub struct RenderScope {
    core: Rc<RuntimeCore>,
    prev: RenderScopeState,
}

impl RenderScope {
    /// Enter a fresh scope for a render. `boundary` is `Some(id)` for a deferred
    /// boundary render, `None` for the shell / a standalone render.
    pub fn enter(core: &Rc<RuntimeCore>, boundary: Option<String>) -> Self {
        let prev = std::mem::replace(
            &mut *core.signals().render_scope.borrow_mut(),
            RenderScopeState::fresh(boundary),
        );
        RenderScope { core: Rc::clone(core), prev }
    }

    /// The boundary id of the currently active scope, if any.
    pub fn current_boundary(core: &RuntimeCore) -> Option<String> {
        core.signals().render_scope.borrow().boundary.clone()
    }
}

impl Drop for RenderScope {
    fn drop(&mut self) {
        *self.core.signals().render_scope.borrow_mut() = std::mem::take(&mut self.prev);
    }
}

/// The result of one flush step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushStep {
    /// A unit of work ran. `pending` is the highest-priority [`Lane`] still holding
    /// work (`None` if the step drained the last of it).
    Ran { pending: Option<Lane> },
    /// The graph is settled; nothing left to do.
    Done,
}

/// A **deferred** read-only view of a tracked value: a [`Signal`] that trails
/// `source`, re-committing on the low-priority [`Lane::Idle`] rather than urgently.
/// Under an interruptible flush this is fine-grained `useDeferredValue` — while the
/// `Input` lane churns, an expensive subtree reading the deferred value keeps
/// rendering the *previous* value and only catches up once the idle lane drains.
/// Retained by `owner`, so it disposes with the component.
pub fn deferred<T>(owner: &Owner, source: impl Fn(&Cx) -> T + 'static) -> Signal<T>
where
    T: Clone + PartialEq + 'static,
{
    // A plain source cell, committed by an idle-lane effect that re-reads `source`
    // and writes the cell only on a real change. A consumer reads the *cell* (a
    // source: reading it never recomputes), so it sees the previous value until the
    // idle lane drains and the effect commits — that lag *is* the deferral.
    let seed = source(&Cx::untracked());
    let cell = MutableSignal::new_in(owner, seed);
    let reader = cell.read();
    // The cell's sole writer moves into its re-commit effect.
    reaction::Reaction::spawn_in_lane(owner, Lane::Idle, move |cx| {
        let next = source(cx);
        if *cell.peek() != next {
            cell.write(next);
        }
    });
    reader
}

// ── The node cell ────────────────────────────────────────────────────────────────

/// The `Rc` cell behind a source or derived scalar node: its value plus its
/// reactive-graph record, plus the handle to the runtime whose queues its writes
/// mark. A source leaves the record's derived fields empty; a [`Computed`]/
/// [`deferred`] fills in a recompute.
/// The value cell holds its graph record as `Rc<NodeCore>` — a **separate** allocation
/// so the graph can carry `Rc<NodeCore>` directly (no trait object) without dragging the
/// typed value through the reactive machinery. The cell keeps its core alive; readers
/// and the owner keep the cell alive; the graph reaches other nodes' cores through the
/// edge lists. The runtime handle is weak — cells never keep a runtime alive.
pub(crate) struct SignalCell<T> {
    value: RefCell<T>,
    core: Rc<NodeCore>,
    id: SignalId,
    rt: Weak<RuntimeCore>,
}

impl<T: 'static> SignalCell<T> {
    pub(crate) fn new_source(rt: &Rc<RuntimeCore>, value: T) -> Rc<Self> {
        Rc::new(SignalCell {
            value: RefCell::new(value),
            core: Rc::new(NodeCore::source()),
            id: rt.signals().next_signal_id(),
            rt: Rc::downgrade(rt),
        })
    }

    /// Assemble a cell with a given core — for derived nodes, whose recompute must be
    /// captured before the cell exists (via `Rc::new_cyclic`).
    pub(crate) fn build(rt: &Rc<RuntimeCore>, value: T, core: Rc<NodeCore>) -> Self {
        SignalCell {
            value: RefCell::new(value),
            core,
            id: rt.signals().next_signal_id(),
            rt: Rc::downgrade(rt),
        }
    }

    pub(crate) fn value(&self) -> &RefCell<T> {
        &self.value
    }

    /// This cell's graph node — a clone of its `Rc<NodeCore>`.
    pub(crate) fn node(&self) -> Rc<NodeCore> {
        self.core.clone()
    }

    pub(crate) fn id(&self) -> SignalId {
        self.id
    }

    /// Mark this cell's observers stale in its runtime's queues. A cell outliving its
    /// runtime (teardown) has no observers left to wake — a no-op, not an error.
    pub(crate) fn wake(&self) {
        if let Some(rt) = self.rt.upgrade() {
            graph::source_changed(&rt, &self.core);
        }
    }
}

fn as_node<T: 'static>(cell: &Rc<SignalCell<T>>) -> Rc<NodeCore> {
    cell.core.clone()
}

// ── MutableSignal ──────────────────────────────────────────────────────────────

/// A reactive source cell — the **writer**. Held where the writing happens (a
/// reducer loop, a store owner); write authority does not spread. [`read`](Self::read)
/// hands out [`Signal`] readers. A read inside a computation registers a dependency;
/// a write marks observers stale and schedules a flush.
pub struct MutableSignal<T: 'static> {
    cell: Rc<SignalCell<T>>,
}

// The writer is deliberately **not `Clone`** (nor `Copy`): write authority is linear.
// It is created once, moved to where the writing happens (a reducer loop, a producer),
// and cannot be duplicated — so a cell has exactly one writer. Consumers are handed
// readers via [`read`](Self::read); the reader is what flows into views and derivations.

impl<T: 'static> MutableSignal<T> {
    /// Create a source cell on `rt`. The caller holds the returned writer; nothing
    /// else keeps the cell alive, so a producer that owns rows (a `SignalVec`) keeps
    /// its writers and reclaims a row by dropping it.
    pub(crate) fn new(rt: &Rc<RuntimeCore>, value: T) -> Self {
        MutableSignal {
            cell: SignalCell::new_source(rt, value),
        }
    }

    /// Create a source retained by `owner`, so it disposes with that scope.
    pub(crate) fn new_in(owner: &Owner, value: T) -> Self {
        let this = Self::new(&owner.runtime(), value);
        // Retain the value cell (not just its core): disposing the scope must free the
        // value, and the graph reaches the core through the cell.
        owner.retain(this.cell.clone());
        this
    }

    fn node(&self) -> Rc<NodeCore> {
        as_node(&self.cell)
    }

    pub fn id(&self) -> SignalId {
        self.cell.id()
    }

    /// A read-only view of this cell — the reader half. Carries read + subscribe but
    /// not mutation, so handing one out cannot create a second writer.
    pub fn read(&self) -> Signal<T> {
        Signal {
            cell: self.cell.clone(),
        }
    }

    /// Tracked read — registers this cell as a dependency of `cx`'s observer and
    /// returns a clone.
    pub fn get(&self, cx: &Cx) -> T
    where
        T: Clone,
    {
        graph::track(cx, &self.node());
        self.cell.value.borrow().clone()
    }

    /// The current value, in this component's live turn. The [`Turn`] only exists
    /// after `render()`, so an untracked read can never feed the painted frame — it
    /// samples state to compute the *next* one.
    ///
    /// Returns the value, not a borrow of it: app code never holds a cell's borrow,
    /// so no sequence of app calls can make one conflict. The cell's exclusion is the
    /// framework's business, and it is kept where it can be reasoned about — inside
    /// this module, across statements no caller can interleave with.
    pub fn now(&self, _turn: impl InTurn) -> T
    where
        T: Clone,
    {
        self.cell.value.borrow().clone()
    }

    /// Untracked read — framework internals only.
    pub(crate) fn peek(&self) -> Ref<'_, T> {
        self.cell.value.borrow()
    }

    /// Replace the value, confined to the reducer turn.
    pub fn set(&self, _turn: impl InTurn, value: T) {
        self.write(value);
    }

    /// Write that defers marking to an explicit [`notify`](Self::notify) — for batch
    /// appliers: write every cell first, notify once, so no observer sees a
    /// half-applied batch.
    pub fn set_silent(&self, _turn: impl InTurn, value: T) {
        *self.cell.value.borrow_mut() = value;
    }

    /// Mark this cell's observers stale (the second half of [`set_silent`](Self::set_silent)).
    /// Turn-gated like its first half — the pair's whole point is that no observer
    /// sees a half-applied batch, which only holds if both halves happen in a turn.
    pub fn notify(&self, _turn: impl InTurn) {
        self.cell.wake();
    }

    /// Mutate the value, turn-gated like [`set`](Self::set). `f` runs against a value
    /// this call owns, with no borrow of the cell outstanding — so `f` may read or
    /// write any signal, including this one, without the cell being live-borrowed
    /// underneath it. (A write to *this* signal from inside `f` is then overwritten by
    /// the value `f` produced, which is a plain last-write-wins, not a panic.)
    pub fn update(&self, _turn: impl InTurn, f: impl FnOnce(&mut T))
    where
        T: Clone,
    {
        let mut next = self.cell.value.borrow().clone();
        f(&mut next);
        *self.cell.value.borrow_mut() = next;
        self.cell.wake();
    }

    /// How a writer is captured into a view binding: as its **reader**. This is the
    /// inherent method the `live_view!` macro's capture resolves to (winning over the
    /// blanket [`ViewCapture`](crate::ViewCapture) `clone`), so `(signal.get(cx))`
    /// works with a writer in scope — the closure captures a reader, the writer stays
    /// for the loop. The capture protocol `live_view!` emits; hand-built views call
    /// [`read`](Self::read) directly.
    pub fn view_capture(&self) -> Signal<T> {
        self.read()
    }

    /// Internal write for framework code that legitimately owns the cell (a
    /// `SignalVec` row, a `deferred` re-commit).
    pub(crate) fn write(&self, value: T) {
        *self.cell.value.borrow_mut() = value;
        self.cell.wake();
    }
}

// `MutableSignal` deliberately does *not* implement `Display`: rendering goes
// through `.get(cx)` (tracked) — a bare `(signal)` in a view is a compile error.

// ── Signal ────────────────────────────────────────────────────────────────────

/// A read-only handle to a source cell — the **reader**. `Clone`, so it flows freely
/// into view bindings and derived nodes; each clone keeps the cell alive, which is
/// why [`get`](Self::get) is infallible. Exposes only reads: possessing one cannot
/// make you a second writer.
pub struct Signal<T: 'static> {
    cell: Rc<SignalCell<T>>,
}

impl<T: 'static> Clone for Signal<T> {
    fn clone(&self) -> Self {
        Signal {
            cell: self.cell.clone(),
        }
    }
}

impl<T: 'static> Signal<T> {
    pub(crate) fn from_cell(cell: Rc<SignalCell<T>>) -> Self {
        Signal { cell }
    }

    fn node(&self) -> Rc<NodeCore> {
        as_node(&self.cell)
    }

    pub fn id(&self) -> SignalId {
        self.cell.id()
    }

    /// Tracked read — registers this cell as a dependency of `cx`'s observer. Pulls
    /// current first, so a reader over a [`Computed`] reflects settled state; a no-op
    /// for a source cell (always clean).
    pub fn get(&self, cx: &Cx) -> T
    where
        T: Clone,
    {
        let node = self.node();
        graph::update_if_necessary(&node);
        graph::track(cx, &node);
        self.cell.value.borrow().clone()
    }

    /// The current value, in this component's live turn (see [`MutableSignal::now`]).
    /// Pulls current first, so a reader over a [`Computed`] reflects settled state.
    pub fn now(&self, _turn: impl InTurn) -> T
    where
        T: Clone,
    {
        graph::update_if_necessary(&self.node());
        self.cell.value.borrow().clone()
    }

    /// The value as of this mount — a plain value, not a subscription. `Setup` runs
    /// exactly once per mount, so what this feeds is painted once and only changes by
    /// re-mounting. The form for decisions the mount's own contract fixes: a page's
    /// route (transitions re-mount), store data rendered as static content inside a
    /// live view.
    pub fn at_mount<M>(&self, _ctx: &crate::Ctx<crate::Setup, M>) -> T
    where
        T: Clone,
    {
        graph::update_if_necessary(&self.node());
        self.cell.value.borrow().clone()
    }

    /// Untracked read — framework internals only.
    pub(crate) fn peek(&self) -> Ref<'_, T> {
        self.cell.value.borrow()
    }
}

// Like `MutableSignal`, `Signal` has no `Display`: render through `.get(cx)`.

// ── ListenGuard ───────────────────────────────────────────────────────────────

/// Dropping this releases whatever it holds (a structural-list subscription).
pub struct ListenGuard {
    _inner: Rc<dyn std::any::Any>,
}

impl ListenGuard {
    pub(crate) fn new(inner: Rc<dyn std::any::Any>) -> Self {
        ListenGuard { _inner: inner }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn scoped() -> (crate::Runtime, Owner) {
        let rt = crate::Runtime::new();
        let owner = Owner::new(rt.core());
        (rt, owner)
    }

    #[test]
    fn signal_get_set() {
        let (_rt, owner) = scoped();
        let s = owner.mutable_signal(42i32);
        assert_eq!(*s.peek(), 42);
        s.set(&Turn::mint(), 99);
        assert_eq!(*s.peek(), 99);
    }

    /// The cell's borrow never leaves this module, so no sequence of app calls can
    /// make two of them overlap — each of these shapes exercises a path that could.
    #[test]
    fn app_code_cannot_overlap_a_cells_borrows() {
        let (_rt, owner) = scoped();
        let turn = Turn::mint();

        // A read feeding a `match` whose arm writes the same signal: the read's value
        // is still in scope across the arms.
        let draft = owner.mutable_signal(String::from("  hi  "));
        match draft.now(&turn).trim() {
            "" => unreachable!("draft is not empty"),
            text => {
                let text = text.to_string();
                draft.set(&turn, text);
            }
        }
        assert_eq!(draft.now(&turn), "hi");

        // A read of the very signal being updated, from inside its own closure.
        let n = owner.mutable_signal(1i32);
        n.update(&turn, |v| *v += n.now(&turn));
        assert_eq!(n.now(&turn), 2);

        // And a write to another signal from inside one's update.
        let other = owner.mutable_signal(0i32);
        n.update(&turn, |v| {
            *v += 1;
            other.set(&turn, 7);
        });
        assert_eq!((n.now(&turn), other.now(&turn)), (3, 7));
    }

    #[test]
    fn render_scope_makes_signal_ids_deterministic() {
        let (rt, owner) = scoped();
        let first = {
            let _scope = RenderScope::enter(rt.core(), None);
            (
                owner.mutable_signal(0u8).id(),
                owner.mutable_signal(0u8).id(),
            )
        };
        let second = {
            let _scope = RenderScope::enter(rt.core(), None);
            (
                owner.mutable_signal(0u8).id(),
                owner.mutable_signal(0u8).id(),
            )
        };
        assert_eq!(first, second);
        assert_eq!(first.0.as_u64(), 1);
        assert_eq!(first.1.as_u64(), 2);
    }

    #[test]
    fn flush_drains_the_input_lane_before_the_idle_lane() {
        use std::cell::RefCell;
        let (rt, owner) = scoped();
        let src = owner.mutable_signal(0i32);
        let order = Rc::new(RefCell::new(Vec::<&'static str>::new()));

        let src_r = src.read();
        let o1 = order.clone();
        let _input = reaction::Reaction::spawn_in_lane(&owner, Lane::Input, move |cx| {
            let _ = src_r.get(cx);
            o1.borrow_mut().push("input");
        });
        let src_r2 = src.read();
        let o2 = order.clone();
        let _idle = reaction::Reaction::spawn_in_lane(&owner, Lane::Idle, move |cx| {
            let _ = src_r2.get(cx);
            o2.borrow_mut().push("idle");
        });
        rt.run_pending_effects();
        order.borrow_mut().clear();

        src.set(&Turn::mint(), 1);
        rt.run_pending_effects();
        assert_eq!(*order.borrow(), vec!["input", "idle"]);
    }

    #[test]
    fn deferred_trails_the_idle_lane_and_coalesces_to_the_latest() {
        use std::cell::RefCell;
        let (rt, owner) = scoped();
        let src = owner.mutable_signal(0i32);
        let src_r = src.read();
        let d = deferred(&owner, move |cx| src_r.get(cx));
        assert_eq!(*d.peek(), 0, "deferred seeds with the source's current value");

        let urgent_seen = Rc::new(RefCell::new(Vec::<i32>::new()));
        let deferred_seen = Rc::new(RefCell::new(Vec::<i32>::new()));
        let src_r2 = src.read();
        let u = urgent_seen.clone();
        let _urgent = reaction::Reaction::spawn_in_lane(&owner, Lane::Input, move |cx| {
            u.borrow_mut().push(src_r2.get(cx));
        });
        let d2 = d.clone();
        let ds = deferred_seen.clone();
        let _watch = owner.effect(move |cx| ds.borrow_mut().push(d2.get(cx)));
        rt.run_pending_effects();
        assert_eq!(*urgent_seen.borrow(), vec![0]);
        assert_eq!(*deferred_seen.borrow(), vec![0]);

        src.set(&Turn::mint(), 1);
        src.set(&Turn::mint(), 2);
        loop {
            match graph::flush_step(rt.core()) {
                FlushStep::Ran { pending: Some(Lane::Idle) } => break,
                FlushStep::Ran { .. } => continue,
                FlushStep::Done => break,
            }
        }
        assert_eq!(urgent_seen.borrow().last(), Some(&2));
        assert_eq!(*d.peek(), 0, "deferred trails while only idle work is pending");
        assert_eq!(*deferred_seen.borrow(), vec![0], "expensive subtree untouched");

        rt.run_pending_effects();
        assert_eq!(*d.peek(), 2);
        assert_eq!(*deferred_seen.borrow(), vec![0, 2], "coalesced: 1 was never seen");
    }
}
