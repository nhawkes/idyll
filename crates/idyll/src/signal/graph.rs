//! The reactive graph: dynamic dependency tracking, mark-and-pull propagation,
//! glitch-freedom. This is the single mechanism every reactive primitive rides.
//!
//! ## The algorithm (tri-color, lazy pull)
//!
//! Each node is `Clean`, `Check`, or `Dirty`. A **write** to a source does not
//! recompute anything: it marks its direct observers `Dirty` and their transitive
//! observers `Check`, then schedules a flush. A **read through a derived node**
//! ([`update_if_necessary`]) is where recomputation happens, and only if needed:
//!
//! - `Dirty` → recompute (re-tracking dependencies), and *only if the new value
//!   differs* mark this node's own observers `Dirty`.
//! - `Check` → pull each source current first; if none actually changed, stay
//!   cached and go `Clean` without recomputing.
//!
//! Because nothing recomputes until marking has fully settled, and a node is only
//! computed when pulled (after its sources are current), there are **no glitches**
//! — an observer never sees a half-updated graph.
//!
//! ## Ownership of edges
//!
//! Every node is an [`Rc`]`<`[`NodeCore`]`>` cell, held concretely — no trait object.
//! Edges are stored both
//! ways — a node's `sources` (what it reads) and each source's `observers` (who
//! reads it) — but the two directions own differently, and that is what keeps the
//! graph leak-free:
//!
//! - **Down-links (`sources`) are strong.** A derived node keeps the inputs it
//!   reads alive for as long as it can recompute from them.
//! - **Up-links (`observers`) are [`Weak`].** A source never keeps its observers
//!   alive; propagation upgrades each and skips the dead.
//!
//! Strong references therefore only ever point observer→source — the reads
//! direction — which is a DAG by construction (a dependency cycle would be a
//! reactive infinite loop). Strong refs following a DAG cannot form a cycle, so a
//! node is reclaimed the instant its last handle, scope retention, and observing
//! down-link are gone. There is no disposal pass; RAII is the whole story.
//!
//! ## Single-threaded
//!
//! The graph is thread-local and its handles are `!Send`/`!Sync`. The read
//! capability [`Cx`] carries the current observer by borrow, so a tracked read is
//! only expressible while a computation is on the stack — see [`super`].

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use super::scope::ViewScope;
use super::{Cx, CxObserver};

/// Scheduling priority for an effect root. [`flush_step`] always drains the
/// highest-priority non-empty lane first, so input-driven work runs ahead of
/// deferred work. Variants are ordered highest-priority first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Lane {
    /// Input-driven effects: view bindings and event-handler reactions. Urgent —
    /// this is where a click's DOM updates land.
    Input,
    /// Deferred effects: a [`deferred`](super::deferred) value's re-commit. Yields
    /// to `Input`, so an interruptible host keeps taking events while it waits.
    Idle,
}

impl Lane {
    /// Every lane, highest priority first — the order [`flush_step`] drains.
    const ALL: [Lane; 2] = [Lane::Input, Lane::Idle];
}

/// A node's staleness. Ordered `Clean < Check < Dirty` so a mark only ever raises.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum NodeState {
    /// Value is current; cache is authoritative.
    Clean,
    /// A transitive source *might* have changed — verify sources before deciding.
    Check,
    /// A direct source definitely changed — recompute on next pull.
    Dirty,
}

/// Who reads a node: another node (a derived cell, an effect), or a **binding** — an
/// index into a mounted view's [`ViewScope`], addressed as pure data. Both halves are
/// weak, so a source never pins what observes it.
#[derive(Clone)]
pub(crate) enum Obs {
    Node(Weak<NodeCore>),
    Binding(Weak<ViewScope>, u32),
}

/// The reactive-graph record carried by every node cell. A pure source uses only
/// `state`/`observers` (and stays `Clean` forever); a derived node also carries its
/// `sources` and its `recompute` action.
pub(crate) struct NodeCore {
    state: Cell<NodeState>,
    /// Nodes this one reads (derived nodes only) — **strong on the core**, rebuilt on
    /// every recompute. Value liveness rides the recompute closure's captured readers
    /// (it read them, so it holds them); this strong link only keeps the *core*
    /// reachable for state checks during a `Check` pull.
    sources: RefCell<Vec<Rc<NodeCore>>>,
    /// What reads this one — **weak**, so a source never pins its observers.
    observers: RefCell<Vec<Obs>>,
    /// Recompute the payload and report whether it changed. `None` for a source.
    /// Runs with this node installed as the current observer (see [`run_update`]).
    recompute: RefCell<Option<Rc<dyn Fn(&Cx) -> bool>>>,
    /// An effect root's scheduling lane, or `None` for an interior node. Effects are
    /// the pull *roots*: on mark they are enqueued in this lane and drained by the
    /// flush. Interior nodes (computeds) are pulled lazily by their observers.
    effect_lane: Cell<Option<Lane>>,
}

impl NodeCore {
    /// Filter this node's **binding** observers — how a scope detaches one index's
    /// edge before it re-runs. Node observers and live entries pass through; dead
    /// weaks prune opportunistically.
    pub(crate) fn retain_observers(&self, keep: impl Fn(&Rc<ViewScope>, u32) -> bool) {
        self.observers.borrow_mut().retain(|obs| match obs {
            Obs::Node(weak) => weak.upgrade().is_some(),
            Obs::Binding(weak, idx) => {
                weak.upgrade().is_some_and(|scope| keep(&scope, *idx))
            }
        });
    }

    /// A pure source: no recompute, `Clean` forever.
    pub(crate) fn source() -> Self {
        NodeCore {
            state: Cell::new(NodeState::Clean),
            sources: RefCell::new(Vec::new()),
            observers: RefCell::new(Vec::new()),
            recompute: RefCell::new(None),
            effect_lane: Cell::new(None),
        }
    }

    /// A derived node, born `Dirty` so its first pull establishes edges and value.
    pub(crate) fn derived(recompute: Rc<dyn Fn(&Cx) -> bool>, effect_lane: Option<Lane>) -> Self {
        NodeCore {
            state: Cell::new(NodeState::Dirty),
            sources: RefCell::new(Vec::new()),
            observers: RefCell::new(Vec::new()),
            recompute: RefCell::new(Some(recompute)),
            effect_lane: Cell::new(effect_lane),
        }
    }
}

/// The graph operates on `Rc<NodeCore>` directly — no trait object. A concrete cell
/// (`SignalCell<T>`, `VecCell<T>`) owns its core as `Rc<NodeCore>` and hands out clones;
/// typed value access stays on the cell, which the graph never touches. Identity is the
/// core's address.
fn same(a: &Rc<NodeCore>, b: &Rc<NodeCore>) -> bool {
    Rc::ptr_eq(a, b)
}

// ── Effect scheduling ────────────────────────────────────────────────────────────

/// A pull root awaiting the flush: a graph effect, or a view scope with dirty
/// binding indices. Held **weakly**: a root dropped while enqueued leaves a dead
/// `Weak` that the flush skips.
enum Root {
    Effect(Weak<NodeCore>),
    Scope(Weak<ViewScope>),
}

/// The pending roots, bucketed by [`Lane`].
#[derive(Default)]
struct LaneQueues([VecDeque<Root>; 2]);

impl LaneQueues {
    fn push(&mut self, lane: Lane, root: Root) {
        self.0[lane as usize].push_back(root);
    }

    /// Highest-priority non-empty lane, or `None` once fully drained.
    fn highest(&self) -> Option<Lane> {
        Lane::ALL.into_iter().find(|&lane| !self.0[lane as usize].is_empty())
    }

    fn pop_highest(&mut self) -> Option<Root> {
        let lane = self.highest()?;
        self.0[lane as usize].pop_front()
    }

    fn is_empty(&self) -> bool {
        self.0.iter().all(VecDeque::is_empty)
    }
}

/// Effect roots marked stale this turn, awaiting the flush — bucketed by lane. One
/// per [`RuntimeCore`](crate::runtime::RuntimeCore): marking threads the owning
/// runtime's queues down from the written cell, so a second runtime on the thread
/// shares nothing.
#[derive(Default)]
pub(crate) struct EffectQueues(RefCell<LaneQueues>);

// ── Reading: dependency tracking ────────────────────────────────────────────────

/// Register that the observer carried by `cx` read `source`. Adds the edge both
/// ways — a strong down-link on the observer, a weak up-link on the source —
/// deduplicated, and never a self-edge. A no-op when `cx` carries no observer (an
/// untracked framework read).
pub(crate) fn track(cx: &Cx, source: &Rc<NodeCore>) {
    match cx.observer() {
        CxObserver::None => {}
        CxObserver::Node(observer) => {
            if same(observer, source) {
                return;
            }
            // The dedup is O(sources): a node reading k *distinct* sources tracks in O(k²).
            // Fine for the ordinary handful; a cliff only for a high-fan-in node — a status
            // rollup `computed` reading every child's status over a large list under a
            // boundary. No such node exists in the dogfood (the fleet's aggregates are a plain
            // `.sum()` at build, not a reactive rollup), so this stays a scan by decision, not
            // debt: for the small-k common path the linear scan beats hashing, and swapping in
            // a blanket `HashSet` would regress every real computed to serve a node that isn't
            // there. If one ever is, the fix is a per-run epoch marker on the *source* (a
            // `Cell<(run_id, observer_ptr)>` set on read, run_id carried by the `Cx`), which is
            // O(1) per track and keeps the guard below — not a set, and not a bare push. That
            // guard is the opposite pathology: a node re-reading one signal in a loop, which
            // without the dedup bloats that source's observer edges.
            let mut sources = observer.sources.borrow_mut();
            if !sources.iter().any(|s| same(s, source)) {
                sources.push(Rc::clone(source));
                drop(sources);
                push_observer(source, Obs::Node(Rc::downgrade(observer)));
            }
        }
        CxObserver::Binding(scope, idx) => {
            if scope.note_read(*idx, source) {
                push_observer(source, Obs::Binding(Rc::downgrade(scope), *idx));
            }
        }
    }
}

/// Register an observer edge, sweeping dead `Weak`s at capacity. A written source
/// prunes as its observers re-run; a **never-written** source (a `Ctx::constant`
/// read by churning rows) has no such moment — this sweep is its only reclamation.
/// Amortized O(1): a sweep that frees less than half the list forces growth
/// instead, so the next sweep is a doubling away (a steady one-dead-per-push churn
/// cannot re-trigger a full scan every push).
fn push_observer(source: &Rc<NodeCore>, obs: Obs) {
    let mut observers = source.observers.borrow_mut();
    if observers.len() >= 8 && observers.len() == observers.capacity() {
        let before = observers.len();
        observers.retain(|o| match o {
            Obs::Node(weak) => weak.strong_count() > 0,
            Obs::Binding(weak, _) => weak.strong_count() > 0,
        });
        if observers.len() > before / 2 {
            observers.reserve(before);
        }
    }
    observers.push(obs);
}

// ── Writing: marking ────────────────────────────────────────────────────────────

/// A source changed: mark its direct observers `Dirty` (and, transitively, their
/// observers `Check`) into `core`'s queues and schedule its flush. The source itself
/// does not change state — sources are always `Clean`.
pub(crate) fn source_changed(core: &crate::runtime::RuntimeCore, source: &Rc<NodeCore>) {
    mark_observers(core, source, NodeState::Dirty);
    core.schedule_flush();
}

/// Mark each of `node`'s observers: nodes to at least `state`, bindings dirty
/// outright — a binding is a leaf whose arm gates its own output, so any upstream
/// mark means "re-run" (there is no `Check` verification to save it).
fn mark_observers(core: &crate::runtime::RuntimeCore, node: &Rc<NodeCore>, state: NodeState) {
    // Iterate the observer list under a shared borrow rather than cloning it. Marking never
    // mutates any observer list — it sets `state`/`effect_lane` cells and enqueues effects;
    // the only `observers` mutations (`track`, `run_update`) belong to the recompute phase,
    // which `schedule_flush` defers. And the up-link graph is acyclic (a cycle would be a
    // reactive infinite loop), with `mark` short-circuiting on already-marked nodes, so no
    // reentrant borrow of *this* node's list occurs. That removes a per-mark Vec allocation
    // on every write — the reactive-graph hot path.
    for obs in node.observers.borrow().iter() {
        match obs {
            Obs::Node(weak) => {
                if let Some(observer) = weak.upgrade() {
                    mark(core, &observer, state);
                }
            }
            Obs::Binding(weak, idx) => {
                if let Some(scope) = weak.upgrade() {
                    if scope.mark(*idx) {
                        core.effects().0.borrow_mut().push(Lane::Input, Root::Scope(weak.clone()));
                    }
                }
            }
        }
    }
}

/// Raise `node` to at least `state`, propagating `Check` to observers the first time
/// it leaves `Clean`, and enqueuing it if it is an effect root.
fn mark(core: &crate::runtime::RuntimeCore, node: &Rc<NodeCore>, state: NodeState) {
    if node.state.get() >= state {
        return;
    }
    let first = node.state.get() == NodeState::Clean;
    node.state.set(state);
    if !first {
        // A later Check→Dirty upgrade needs no re-propagation — observers are
        // already at least Check from the first pass.
        return;
    }
    if let Some(lane) = node.effect_lane.get() {
        core.effects().0.borrow_mut().push(lane, Root::Effect(Rc::downgrade(node)));
    }
    mark_observers(core, node, NodeState::Check);
}

// ── Reading a derived node: the pull ────────────────────────────────────────────

/// Ensure `node`'s cached value is current, recomputing only if a source actually
/// changed. The glitch-free heart: a `Check` node verifies its sources first, and
/// recomputes only when one is confirmed changed.
pub(crate) fn update_if_necessary(node: &Rc<NodeCore>) {
    if node.state.get() == NodeState::Clean {
        return;
    }

    if node.state.get() == NodeState::Check {
        // Pull each source current; a source whose value moved will have marked us
        // Dirty (via `run_update`), so re-read our own state after each.
        let sources: Vec<Rc<NodeCore>> = node.sources.borrow().clone();
        for source in sources {
            update_if_necessary(&source);
            if node.state.get() == NodeState::Dirty {
                break;
            }
        }
    }

    if node.state.get() == NodeState::Dirty {
        run_update(node);
    }
    node.state.set(NodeState::Clean);
}

/// Recompute a derived node: detach its old dependency edges, run its recompute with
/// itself installed as the current observer (so reads re-track), and — only if the
/// value changed — mark its observers `Dirty`.
fn run_update(node: &Rc<NodeCore>) {
    // Drop stale edges: remove this node from each old source's observer list and
    // clear its own source list. Reads during the recompute rebuild both.
    let old_sources: Vec<Rc<NodeCore>> = std::mem::take(&mut *node.sources.borrow_mut());
    for source in old_sources {
        source.observers.borrow_mut().retain(|obs| match obs {
            Obs::Node(weak) => weak.upgrade().is_some_and(|o| !same(&o, node)),
            Obs::Binding(weak, _) => weak.upgrade().is_some(),
        });
    }

    let recompute = node.recompute.borrow().clone();
    let Some(recompute) = recompute else {
        return; // a pure source has no recompute (should not reach here)
    };

    let cx = Cx::tracking(node);
    let changed = recompute(&cx);

    if changed {
        // Our value moved, so our observers are genuinely stale. They are already
        // `Check` (marked when we first went stale); raise them to `Dirty` without
        // re-propagating — their observers were reached in the same first pass.
        // Binding observers were marked dirty in that pass too — nothing to raise.
        let observers: Vec<Obs> = node.observers.borrow().clone();
        for obs in observers {
            if let Obs::Node(weak) = obs {
                if let Some(observer) = weak.upgrade() {
                    if observer.state.get() < NodeState::Dirty {
                        observer.state.set(NodeState::Dirty);
                    }
                }
            }
        }
    }
}

/// Establish a derived node's initial value and edges by running its recompute once,
/// now, with the node installed as the current observer. Called by `Computed`/effect
/// constructors after the cell exists. Settles to `Clean`.
pub(crate) fn init_derived(node: &Rc<NodeCore>) {
    run_update(node);
    node.state.set(NodeState::Clean);
}

// ── Flushing effect roots ────────────────────────────────────────────────────────

/// Whether any effect root is still marked stale (the flush is not yet settled).
pub(crate) fn has_pending_effects(core: &crate::runtime::RuntimeCore) -> bool {
    !core.effects().0.borrow().is_empty()
}

/// The highest-priority lane with pending work, or `None` if fully settled.
pub(crate) fn pending_lane(core: &crate::runtime::RuntimeCore) -> Option<Lane> {
    core.effects().0.borrow().highest()
}

/// Run at most **one** pending effect root — the highest-priority lane's next —
/// pulling the values it reads current on demand, and report the highest lane still
/// pending afterward. The bounded, resumable seam the flush is built on.
pub(crate) fn flush_step(core: &crate::runtime::RuntimeCore) -> super::FlushStep {
    let next = core.effects().0.borrow_mut().pop_highest();
    match next {
        Some(Root::Effect(weak)) => {
            if let Some(effect) = weak.upgrade() {
                update_if_necessary(&effect);
            }
            // Running the effect may have enqueued more (e.g. a deferred re-commit
            // waking its observers), so re-read the pending lane after.
            super::FlushStep::Ran { pending: pending_lane(core) }
        }
        Some(Root::Scope(weak)) => {
            if let Some(scope) = weak.upgrade() {
                scope.flush();
            }
            super::FlushStep::Ran { pending: pending_lane(core) }
        }
        None => super::FlushStep::Done,
    }
}
