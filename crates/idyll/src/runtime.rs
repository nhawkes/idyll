use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use crate::signal::FlushStep;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushBudget {
    pub lane_items: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushStatus {
    pub complete: bool,
}

// ── The per-runtime core ──────────────────────────────────────────────────────

type MountFn = Box<dyn FnOnce(&mut Runtime, &mut dyn crate::driver::DomDriver) -> Vec<MountGuard>>;

/// The state every handle-holder of one [`Runtime`] reaches: the reactive flush's
/// queues, the mount plane's, the host round-trip table, and the guest-instance
/// inputs. Nothing here is ambient — the `Owner`/`Ctx`/frame chain carries the
/// handle, teardown guards hold it weakly, and a second runtime on the thread
/// shares none of it. `Runtime` owns its tasks and its core; a root context is
/// minted *from* a runtime ([`Runtime::ctx`]), so the pairing is spelled at the one
/// place it exists.
pub struct RuntimeCore {
    signals: crate::signal::SignalState,
    /// Platform callback to schedule a flush microtask.
    flush_callback: RefCell<Option<Box<dyn Fn()>>>,
    /// Pending DOM ops queued by signal subscribers, drained on flush.
    pending_ops: RefCell<Vec<crate::driver::DomOp>>,
    /// Pending mount work that needs access to the DOM driver during flush.
    pending_mounts: RefCell<Vec<MountFn>>,
    /// LiveView posted by `ctx.render()` at an island root; the runtime picks it up
    /// after polling.
    pending_view: RefCell<Option<crate::ctx::WiredView>>,
    pending_cleanups: RefCell<Vec<LocalFuture>>,
    /// Live component futures handed off at render: a component that reaches its `recv` loop
    /// pushes its still-live future here with a cancel flag; the owning runtime adopts them
    /// into its task set before every drive.
    pending_tasks: RefCell<Vec<(Rc<Cell<bool>>, LocalFuture)>>,
    /// Node ids whose views have unmounted, awaiting the frame's one `FreeNodes` —
    /// drained at `flush_inner`'s settled exit, after every teardown op of the same
    /// cascade has been emitted (see [`free_nodes_guard`]).
    pending_frees: RefCell<Vec<crate::driver::NodeId>>,
    /// Resolved child anchors, keyed by child id — filled when a parent view mounts,
    /// released by that view's [`child_anchor_guard`] when it unmounts.
    child_anchors: RefCell<HashMap<ChildId, crate::driver::NodeId>>,
    /// Child renders that arrived before their anchor was known — drained at parent mount.
    child_renders: RefCell<HashMap<ChildId, StashedChildRender>>,
    /// In-flight server requests: request id → the continuation that turns the response
    /// into a message in the requesting component's inbox. An entry whose live has since
    /// unmounted resolves into a dropped inbox — a no-op by construction.
    pending_requests: RefCell<HashMap<u32, Box<dyn FnOnce(Result<Vec<u8>, crate::driver::RequestError>)>>>,
    next_request_id: Cell<u32>,
    /// Whether this runtime is replaying a recorded message log (see [`replay_mode`]).
    replay: Cell<bool>,
    /// Whether mounts on this runtime are client mounts (a real DOM lands, client
    /// effects run) — the SSR host marks its per-mount store `false`; everything
    /// else (browser, native tests) defaults `true`. Stated by the caller across
    /// the membrane: it is the one environment fact the guest cannot sense.
    client_mount: Cell<bool>,
    /// This runtime's splitmix64 state — seeded lazily from the platform, advanced per
    /// draw (see `ctx::platform_random_u64`).
    pub(crate) prng: Cell<Option<u64>>,
    /// The monotonic-clock origin of this runtime's virtual timeline.
    pub(crate) time_origin: Cell<Option<std::time::Instant>>,
    /// The message-log delivery counter — dequeue order is execution order, so this
    /// stamps a total order across the runtime's live tree (see `dev`).
    delivery_sequence: Cell<u64>,
}

impl RuntimeCore {
    fn new() -> Rc<Self> {
        Rc::new(RuntimeCore {
            signals: crate::signal::SignalState::default(),
            flush_callback: RefCell::new(None),
            pending_ops: RefCell::new(Vec::new()),
            pending_mounts: RefCell::new(Vec::new()),
            pending_view: RefCell::new(None),
            pending_cleanups: RefCell::new(Vec::new()),
            pending_tasks: RefCell::new(Vec::new()),
            pending_frees: RefCell::new(Vec::new()),
            child_anchors: RefCell::new(HashMap::new()),
            child_renders: RefCell::new(HashMap::new()),
            pending_requests: RefCell::new(HashMap::new()),
            next_request_id: Cell::new(1),
            replay: Cell::new(false),
            client_mount: Cell::new(true),
            prng: Cell::new(None),
            time_origin: Cell::new(None),
            delivery_sequence: Cell::new(0),
        })
    }

    pub(crate) fn signals(&self) -> &crate::signal::SignalState {
        &self.signals
    }

    pub(crate) fn next_delivery_sequence(&self) -> u64 {
        let sequence = self.delivery_sequence.get();
        self.delivery_sequence.set(sequence + 1);
        sequence
    }

    pub(crate) fn effects(&self) -> &crate::signal::graph::EffectQueues {
        &self.signals.effects
    }

    /// Schedule this runtime's flush microtask, once per turn.
    pub(crate) fn schedule_flush(&self) {
        if !self.signals.flush_scheduled.replace(true) {
            if let Some(f) = self.flush_callback.borrow().as_ref() {
                f();
            }
        }
    }

    /// Apply everything enqueued so far, in order, ahead of the caller's next `driver.apply`.
    ///
    /// The flush loop drains [`pending_ops`](Self::pending_ops) once per turn, *before* the mount
    /// pass — so an op a mount pass enqueues lands a turn later, behind every `driver.apply` that
    /// pass makes. That is the wrong order for a teardown emitted from a value's `Drop` in the
    /// middle of a splice: the row is gone from the model, and the ops that follow it are
    /// positioned against siblings. Draining here puts the drop's ops where the drop happened.
    pub(crate) fn drain_dom_ops(&self, driver: &mut dyn crate::driver::DomDriver) {
        let ops = std::mem::take(&mut *self.pending_ops.borrow_mut());
        if !ops.is_empty() {
            driver.apply(ops);
        }
    }

    /// Queue work for the flush's deferred pass — after the turn's effects have run,
    /// before their ops are applied, with the driver in hand. Structural splices run
    /// here because they need the driver; a canvas's one command per frame runs here
    /// because it needs *every* effect of the turn to have marked what it moved first.
    pub(crate) fn defer(&self, work: MountFn) {
        self.pending_mounts.borrow_mut().push(work);
    }

    pub(crate) fn enqueue_dom_op(&self, op: crate::driver::DomOp) {
        // The queue is deliberately dumb: ops are last-wins per target when applied in
        // order, so duplicates are correct, merely wire-fat — and bindings memoize their
        // last emission, so a duplicate only exists when one turn genuinely writes a
        // target twice (a multi-dep binding whose deps are written in the same turn).
        // No per-enqueue coalescing scan: it would be O(batch²) — a hot-path killer at
        // ~2k ops/turn. Storm control belongs ABOVE this layer: continuous input
        // (pointermove-class events) should be pull-batched per frame and coalesced
        // latest-wins at the message level, where it also saves the binding re-runs, not
        // just the wire entries.
        self.pending_ops.borrow_mut().push(op);
    }

    pub(crate) fn post_pending_view(&self, view: crate::ctx::WiredView) {
        *self.pending_view.borrow_mut() = Some(view);
    }

    /// Whether this runtime is **replaying** a recorded message log
    /// ([`crate::dev::replay_component`]). In replay the world is a fold of
    /// `(seed, messages)`: side effects are inert — `client_effect` tasks are not
    /// spawned, tick subscriptions and server-mutation requests are not emitted —
    /// because their observable results were recorded as messages and are re-fed from
    /// the log, not re-produced.
    pub fn replay_mode(&self) -> bool {
        self.replay.get()
    }

    /// Mark this runtime's mounts as a **server paint**: client effects will not
    /// spawn (see the gate in `mount_wired_view`). Called by the guest glue when the
    /// mount's caller says `client: false`; there is no unmarking — a server store
    /// lives for exactly one paint.
    pub fn mark_server_paint(&self) {
        self.client_mount.set(false);
    }
}

/// Enter replay mode on one runtime for the guard's lifetime, restoring the prior
/// state on drop.
pub(crate) struct ReplayModeGuard {
    core: Rc<RuntimeCore>,
    prev: bool,
}

impl ReplayModeGuard {
    pub(crate) fn enter(core: &Rc<RuntimeCore>) -> Self {
        ReplayModeGuard { core: Rc::clone(core), prev: core.replay.replace(true) }
    }
}

impl Drop for ReplayModeGuard {
    fn drop(&mut self) {
        self.core.replay.set(self.prev);
    }
}

impl RuntimeCore {
    /// Register a host round-trip's continuation and enqueue its request op —
    /// mutations and navigations share the pending registry and are resolved by the
    /// same [`Runtime::deliver_response`].
    fn enqueue_host_request(
        &self,
        on_response: Box<dyn FnOnce(Result<Vec<u8>, crate::driver::RequestError>)>,
        request: impl FnOnce(crate::driver::RequestId) -> crate::driver::DomOp,
    ) {
        // In replay the response is already a recorded message re-fed from the log; do
        // not re-request or register a continuation that will never resolve.
        if self.replay_mode() {
            return;
        }
        let id = self.next_request_id.get();
        self.next_request_id.set(id + 1);
        self.pending_requests.borrow_mut().insert(id, on_response);
        self.enqueue_dom_op(request(crate::driver::RequestId(id)));
        self.schedule_flush();
    }

    /// Queue a [`crate::Mutation`]-shaped server request: the `ServerRequest` command
    /// joins the ordinary output stream on the next flush, and `on_response` runs when
    /// the host environment delivers this request's response.
    pub(crate) fn enqueue_server_request(
        &self,
        op: idyll_schema::OpHash,
        args: Vec<u8>,
        on_response: Box<dyn FnOnce(Result<Vec<u8>, crate::driver::RequestError>)>,
    ) {
        self.enqueue_host_request(on_response, |request_id| crate::driver::DomOp::ServerRequest {
            request_id,
            op,
            args,
        });
    }

    /// Enqueue a navigation's route re-execution (see
    /// [`DomOp::Navigate`](crate::driver::DomOp::Navigate)).
    pub(crate) fn enqueue_navigate(
        &self,
        op: idyll_schema::OpHash,
        path: String,
        on_response: Box<dyn FnOnce(Result<Vec<u8>, crate::driver::RequestError>)>,
    ) {
        self.enqueue_host_request(on_response, |request_id| crate::driver::DomOp::Navigate {
            request_id,
            op,
            path,
        });
    }
}

// ── View-embedded child render protocol ────────────────────────────────────────
//
// A view-embedded child is a **driven future** (a field of its parent's mount), not a
// spawned task. It renders through a sink keyed by [`ChildId`]; the runtime resolves that
// id to an anchor node when the parent view mounts, and splices there. The two orders both
// work: a child that renders synchronously (before its parent mounts) stashes its render in
// `CHILD_RENDERS`, drained at parent mount; a child that renders later (a suspended child
// whose data has now resolved) finds its anchor already in `CHILD_ANCHORS` and splices
// immediately. Teardown is `Drop` of the child future, which owns its DOM guards.

/// Identity of a view-embedded child mount, minted at the parent's `render` and shared
/// between the child's anchor (in the parent template) and the child's render sink.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ChildId(u64);

static CHILD_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Mint a fresh child identity (called by the `live_view!` macro at each component call).
pub fn fresh_child_id() -> ChildId {
    ChildId(CHILD_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// A child render awaiting its anchor: the parts `enqueue_child_render` needs, held until
/// the parent mount resolves the slot.
struct StashedChildRender {
    view: crate::ctx::WiredView,
    guards: Rc<RefCell<Vec<MountGuard>>>,
}

impl RuntimeCore {
    /// The render sink a view-embedded child mounts through, paired with the DOM-guard cell it
    /// fills. The sink splices the child's view at its anchor if the parent has mounted, else
    /// stashes it until the anchor lands. A child renders exactly once (its live loop patches
    /// the DOM through bindings, never re-splicing), so this fires once — a `MountFragment`,
    /// never a replace. The returned `guards` cell receives the mounted subtree's DOM guards;
    /// the child's executor task holds it, so cancelling the task tears the subtree down. Cell
    /// and sink are minted together so the two are never wired to different children.
    pub(crate) fn child_render_sink(
        self: &Rc<Self>,
        child_id: ChildId,
    ) -> (Rc<RefCell<Vec<MountGuard>>>, Rc<dyn Fn(crate::ctx::WiredView)>) {
        let guards = Rc::new(RefCell::new(Vec::<MountGuard>::new()));
        let sink = {
            let guards = Rc::clone(&guards);
            let core = Rc::clone(self);
            Rc::new(move |view| {
                match core.child_anchors.borrow().get(&child_id).copied() {
                    Some(anchor) => core.enqueue_child_render(anchor, view, Rc::clone(&guards)),
                    None => {
                        core.child_renders.borrow_mut().insert(
                            child_id,
                            StashedChildRender { view, guards: Rc::clone(&guards) },
                        );
                    }
                }
            }) as Rc<dyn Fn(crate::ctx::WiredView)>
        };
        (guards, sink)
    }

    /// Resolve a child anchor at parent mount: record the node, and splice any render that
    /// already arrived for it.
    fn resolve_child_anchor(self: &Rc<Self>, child_id: ChildId, anchor: crate::driver::NodeId) {
        self.child_anchors.borrow_mut().insert(child_id, anchor);
        if let Some(stashed) = self.child_renders.borrow_mut().remove(&child_id) {
            self.enqueue_child_render(anchor, stashed.view, stashed.guards);
        }
    }

    fn enqueue_child_render(
        self: &Rc<Self>,
        anchor_id: crate::driver::NodeId,
        mut view: crate::ctx::WiredView,
        guards: Rc<RefCell<Vec<MountGuard>>>,
    ) {
        let core = Rc::clone(self);
        self.pending_mounts.borrow_mut().push(Box::new(move |runtime, driver| {
            let template = driver.register_template(view.template());
            driver.apply(vec![crate::driver::DomOp::MountFragment { anchor_id, template }]);
            let mut mounted = mount_wired_view(runtime, &mut view, driver);
            // Reclaim the spliced subtree when the child unmounts: `MountFragment` created it at
            // this anchor, so `RemoveFragment` there tears it down — including static structure the
            // guest never minted node ids for.
            mounted.push(remove_fragment_guard(&core, anchor_id));
            *guards.borrow_mut() = mounted;
            Vec::new()
        }));
    }

    /// Hand a live component future to the executor at render hand-off. Queues it with a
    /// cancel flag for the owning runtime to adopt; the returned guard is the parent's —
    /// dropping it cancels this future, and the child guards it holds cascade the unmount.
    /// The task-side seam a component reaches through its handles, mirroring
    /// `child_render_sink` on the view side.
    pub(crate) fn spawn_pending(&self, fut: impl Future<Output = ()> + 'static) -> MountGuard {
        let cancelled = Rc::new(Cell::new(false));
        self.pending_tasks.borrow_mut().push((Rc::clone(&cancelled), Box::pin(fut)));
        MountGuard::new(CancelOnDrop { cancelled })
    }
}

// ── Task waker ────────────────────────────────────────────────────────────────

type LocalFuture = Pin<Box<dyn Future<Output = ()>>>;

/// Opaque task identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TaskId(u64);

static TASK_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_task_id() -> TaskId {
    TaskId(TASK_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

struct Task {
    id: TaskId,
    fut: LocalFuture,
    cancelled: Rc<Cell<bool>>,
}

/// What a guard means to the static-paint verdict — stamped at construction, never
/// recovered by downcast: a new cleanup-shaped guard routed through the wrong
/// constructor would otherwise misclassify silently and skip its teardown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GuardKind {
    /// An `on_unmount` cleanup — the one obligation a static-paint island cannot
    /// honour once its mount is skipped, so it disqualifies the paint.
    Cleanup,
    /// Everything else: owner anchors, listener/measure removals, id releases,
    /// fragment teardown, task cancels. Skippable when the paint is adopted whole.
    Structural,
}

pub struct MountGuard {
    _inner: Box<dyn std::any::Any>,
    kind: GuardKind,
}

impl MountGuard {
    pub(crate) fn new(inner: impl std::any::Any + 'static) -> Self {
        MountGuard {
            _inner: Box::new(inner),
            kind: GuardKind::Structural,
        }
    }

    pub(crate) fn cleanup(core: &Rc<RuntimeCore>, fut: impl Future<Output = ()> + 'static) -> Self {
        MountGuard {
            _inner: Box::new(CleanupOnDrop {
                core: Rc::downgrade(core),
                fut: Some(Box::pin(fut)),
            }),
            kind: GuardKind::Cleanup,
        }
    }

    pub(crate) fn is_cleanup(&self) -> bool {
        self.kind == GuardKind::Cleanup
    }
}

impl From<crate::signal::ListenGuard> for MountGuard {
    fn from(value: crate::signal::ListenGuard) -> Self {
        MountGuard::new(value)
    }
}

struct CancelOnDrop {
    cancelled: Rc<Cell<bool>>,
}

struct CleanupOnDrop {
    core: Weak<RuntimeCore>,
    fut: Option<LocalFuture>,
}

struct KeptBranch {
    anchor_id: crate::driver::NodeId,
    _guards: Vec<MountGuard>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.cancelled.set(true);
    }
}

impl Drop for CleanupOnDrop {
    fn drop(&mut self) {
        // A dead runtime has nothing left to run the cleanup against — skip, exactly
        // as guards dropping during teardown always have.
        if let (Some(fut), Some(core)) = (self.fut.take(), self.core.upgrade()) {
            core.pending_cleanups.borrow_mut().push(fut);
        }
    }
}

/// Safe waker: pushes the task ID into the shared ready queue when woken.
/// Uses `Arc<Mutex<_>>` so `Wake` is satisfied (it requires `Arc<Self>`).
/// The Mutex will never actually contend since Idyll is single-threaded.
struct TaskWaker {
    id: TaskId,
    queue: Arc<Mutex<VecDeque<TaskId>>>,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.queue.lock().unwrap().push_back(self.id);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.queue.lock().unwrap().push_back(self.id);
    }
}

// ── Runtime ───────────────────────────────────────────────────────────────────

/// Single-threaded async runtime for Idyll components. Owns its tasks and its
/// [`RuntimeCore`]; every handle in the tree reaches the same core, so a second
/// runtime on the thread shares nothing.
pub struct Runtime {
    tasks: Vec<Task>,
    ready: Arc<Mutex<VecDeque<TaskId>>>,
    fragment_guards: Vec<MountGuard>,
    /// Live names declared by views mounted in this runtime (see [`crate::live`]).
    live: Vec<(String, Option<String>)>,
    core: Rc<RuntimeCore>,
}

/// A dying runtime's queues die with its core — nothing is shared, so there is
/// nothing to scrub. Pending tasks are drained explicitly because a queued future
/// can hold a `Ctx` holding the core (a transient cycle until adoption); dropping
/// them here breaks it.
impl Drop for Runtime {
    fn drop(&mut self) {
        self.tasks.clear();
        self.fragment_guards.clear();
        self.core.pending_tasks.borrow_mut().clear();
        self.core.pending_mounts.borrow_mut().clear();
        self.core.pending_view.borrow_mut().take();
    }
}

impl Runtime {
    pub fn new() -> Self {
        Runtime {
            tasks: Vec::new(),
            ready: Arc::new(Mutex::new(VecDeque::new())),
            fragment_guards: Vec::new(),
            live: Vec::new(),
            core: RuntimeCore::new(),
        }
    }

    /// This runtime's core — the handle the `Owner`/`Ctx`/frame chain carries.
    pub(crate) fn core(&self) -> &Rc<RuntimeCore> {
        &self.core
    }

    /// Mint a **root** context on this runtime — the composition seam: `guest!` glue
    /// and test harnesses start a tree here, naming the runtime it belongs to.
    /// Mark this runtime's mounts as a server paint (see
    /// [`RuntimeCore::mark_server_paint`]) — called by the `guest!` glue when the
    /// mount's caller states `client: false`.
    pub fn mark_server_paint(&self) {
        self.core.mark_server_paint();
    }

    pub fn ctx<M: 'static>(&self) -> crate::Ctx<crate::Setup, M> {
        crate::Ctx::root_in(&self.core)
    }

    /// Resolve an in-flight server request with the far side's response. Called by the
    /// component's `deliver` export (browser: after the runtime's fetch completes); the
    /// registered continuation sends the typed result into the requesting component's
    /// inbox. Returns whether the id was known — an unknown id (component unmounted
    /// mid-flight, or a duplicate delivery) is a no-op, not an error.
    pub fn deliver_response(
        &self,
        request_id: crate::driver::RequestId,
        response: Result<Vec<u8>, crate::driver::RequestError>,
    ) -> bool {
        let handler = self.core.pending_requests.borrow_mut().remove(&request_id.0);
        match handler {
            Some(handler) => {
                handler(response);
                true
            }
            None => false,
        }
    }

    /// The highest-priority [`Lane`](crate::Lane) with pending effect work, or `None`
    /// once settled — what an interruptible host reports between slices.
    pub fn pending_lane(&self) -> Option<crate::Lane> {
        crate::signal::graph::pending_lane(&self.core)
    }

    /// Settle the reactive graph synchronously: run every stale effect root to
    /// completion. Tests and non-interruptible paths use this; an interruptible host
    /// drives budgeted flushes instead.
    pub fn run_pending_effects(&self) {
        while let FlushStep::Ran { .. } = crate::signal::graph::flush_step(&self.core) {}
    }

    /// Drain the live names collected while mounting. The page renderer reports these
    /// so the host can skip the client runtime entirely for live-free pages.
    pub fn take_islands(&mut self) -> Vec<(String, Option<String>)> {
        std::mem::take(&mut self.live)
    }

    pub fn bind_flush_scheduler<D>(runtime: Rc<RefCell<Self>>, driver: Rc<RefCell<D>>)
    where
        D: crate::driver::DomDriver + 'static,
    {
        let core = Rc::clone(&runtime.borrow().core);
        *core.flush_callback.borrow_mut() = Some(Box::new(move || {
            let runtime = Rc::clone(&runtime);
            let driver_for_task = Rc::clone(&driver);
            driver.borrow().schedule_microtask(Box::new(move || {
                runtime
                    .borrow_mut()
                    .flush(&mut *driver_for_task.borrow_mut());
            }));
        }));
    }

    /// The number of executor tasks alive right now. In the flat executor every
    /// view-embedded component is its own task: it resolves to its render point owned by
    /// its parent, then hands its still-live future here at the render seam
    /// ([`spawn_pending`]). So a mounted tree of N components is ~N tasks, plus the root
    /// live, client effects, and cleanups — the count tests assert against.
    #[cfg(any(test, feature = "testing"))]
    pub fn live_task_count(&self) -> usize {
        self.tasks.len()
    }

    /// Spawn a local future as a component task. Returns its task ID.
    pub fn spawn(&mut self, fut: impl Future<Output = ()> + 'static) -> TaskId {
        let id = next_task_id();
        let cancelled = Rc::new(Cell::new(false));
        self.tasks.push(Task {
            id,
            fut: Box::pin(fut),
            cancelled,
        });
        self.ready.lock().unwrap().push_back(id);
        id
    }

    pub fn spawn_scoped(&mut self, fut: impl Future<Output = ()> + 'static) -> MountGuard {
        let id = next_task_id();
        let cancelled = Rc::new(Cell::new(false));
        self.tasks.push(Task {
            id,
            fut: Box::pin(fut),
            cancelled: Rc::clone(&cancelled),
        });
        self.ready.lock().unwrap().push_back(id);
        MountGuard::new(CancelOnDrop { cancelled })
    }

    /// Adopt live futures handed off at render (`spawn_pending`) into this runtime's task set,
    /// each scheduled for a first poll. Called before every drive so a component spawned during
    /// a poll (a child reaching its `recv` loop) runs on the next.
    fn adopt_pending_tasks(&mut self) {
        let pending: Vec<(Rc<Cell<bool>>, LocalFuture)> =
            std::mem::take(&mut *self.core.pending_tasks.borrow_mut());
        for (cancelled, fut) in pending {
            let id = next_task_id();
            self.tasks.push(Task { id, fut, cancelled });
            self.ready.lock().unwrap().push_back(id);
        }
    }

    /// Poll all currently-ready tasks once.
    pub fn run_once(&mut self) {
        self.adopt_pending_tasks();
        // Reap tasks whose scope guard was dropped: dropping a `spawn_scoped` guard
        // sets `cancelled` but cannot itself re-schedule the task, and a cancelled
        // task may never be woken again. Removing it here drops its future — and the
        // component `Owner` that future holds — so an unmounted subtree's reactive
        // nodes (its bindings) dispose and detach from the graph before the next turn.
        self.tasks.retain(|t| !t.cancelled.get());
        let ready: Vec<TaskId> = self.ready.lock().unwrap().drain(..).collect();
        for id in ready {
            if let Some(pos) = self.tasks.iter().position(|t| t.id == id) {
                if self.tasks[pos].cancelled.get() {
                    self.tasks.remove(pos);
                    continue;
                }
                let waker = Waker::from(Arc::new(TaskWaker {
                    id,
                    queue: Arc::clone(&self.ready),
                }));
                let mut cx = Context::from_waker(&waker);
                match self.tasks[pos].fut.as_mut().poll(&mut cx) {
                    Poll::Ready(()) => {
                        self.tasks.remove(pos);
                    }
                    Poll::Pending => {}
                }
            }
        }
    }

    /// Run until no tasks are ready **and** none are waiting to be adopted. A task may spawn
    /// another (a view-embedded child hands its live future to the executor via
    /// [`spawn_pending`], which queues into `PENDING_TASKS`), and that queue is only drained at
    /// the next `run_once`; following it here lets the whole mount cascade settle. Suitable for
    /// tests. (The browser executor is woken per queued task, so it drains the same cascade.)
    pub fn run_to_quiescence(&mut self) {
        loop {
            let ready = !self.ready.lock().unwrap().is_empty();
            let pending = !self.core.pending_tasks.borrow().is_empty();
            if !ready && !pending {
                break;
            }
            self.run_once();
        }
    }

    /// Drain any pending DOM ops and apply them to the driver.
    pub fn flush(&mut self, driver: &mut dyn crate::driver::DomDriver) {
        self.flush_inner(driver, None);
    }

    pub fn flush_budgeted(
        &mut self,
        driver: &mut dyn crate::driver::DomDriver,
        budget: FlushBudget,
    ) -> FlushStatus {
        self.flush_inner(driver, Some(budget))
    }

    fn flush_inner(
        &mut self,
        driver: &mut dyn crate::driver::DomDriver,
        mut budget: Option<FlushBudget>,
    ) -> FlushStatus {
        self.core.signals().flush_scheduled.set(false);
        loop {
            let cleanups = std::mem::take(&mut *self.core.pending_cleanups.borrow_mut());
            let had_cleanups = !cleanups.is_empty();
            for cleanup in cleanups {
                self.spawn(cleanup);
            }

            let ops = std::mem::take(&mut *self.core.pending_ops.borrow_mut());
            if !ops.is_empty() {
                driver.apply(ops);
            }

            let mounts = std::mem::take(&mut *self.core.pending_mounts.borrow_mut());
            let had_mounts = !mounts.is_empty();

            for mount in mounts {
                let guards = mount(self, driver);
                self.fragment_guards.extend(guards);
            }

            // Settle the reactive graph: run stale effect roots (the view's
            // bindings and control-flow fragments). Each enqueues DOM ops (applied
            // on the next turn of this loop) and may push more mounts. A budget
            // caps how many run before we yield — L5's host loops this with event
            // delivery between slices; an unbudgeted flush drains to quiescence.
            let mut ran_effects = false;
            loop {
                if matches!(budget, Some(FlushBudget { lane_items: 0 })) {
                    break;
                }
                match crate::signal::graph::flush_step(&self.core) {
                    FlushStep::Ran { .. } => {
                        ran_effects = true;
                        if let Some(budget) = budget.as_mut() {
                            budget.lane_items = budget.lane_items.saturating_sub(1);
                        }
                    }
                    FlushStep::Done => break,
                }
            }
            let effects_remain = crate::signal::graph::has_pending_effects(&self.core);

            if matches!(budget, Some(FlushBudget { lane_items: 0 })) && effects_remain {
                self.core.schedule_flush();
                return FlushStatus { complete: false };
            }

            // Complete any **unmount cascade** this frame's effects started: an `@if`/`@for` arm
            // switch (a reactive effect above) cancels a view-embedded child's executor task, but
            // the task — which holds the child's DOM guards and `on_unmount` cleanups — is only
            // reaped by a poll. Drive one here so the reap drops those guards, whose
            // `PENDING_CLEANUPS` this loop's next iteration runs. A view-embedded child spawned by
            // this frame likewise adopts and reaches its `recv` park here.
            let had_task_work = self.tasks.iter().any(|t| t.cancelled.get())
                || !self.ready.lock().unwrap().is_empty()
                || !self.core.pending_tasks.borrow().is_empty();
            if had_task_work {
                self.run_once();
            }

            if !had_mounts && !ran_effects && !had_cleanups && !had_task_work {
                // Fully settled for this frame: every teardown op the cascade produced
                // has been applied, so the frame's id releases go last — the strict
                // folds refuse ops on freed ids, which makes this position load-bearing.
                let frees = std::mem::take(&mut *self.core.pending_frees.borrow_mut());
                if !frees.is_empty() {
                    driver.apply(vec![crate::driver::DomOp::FreeNodes { node_ids: frees }]);
                }
                // Removed list cells reclaim on their own — a cell lives exactly until
                // its last consumer drops it.
                return FlushStatus {
                    complete: !effects_remain,
                };
            }
        }
    }

    /// Mount the view most recently posted to `pending_view` by `ctx.render()`, then
    /// settle the initial frame. Call this immediately after polling a component that
    /// just rendered. Mounting spawns the view's binding/fragment effects (which
    /// enqueue the first paint and any initial `@if`/`@for` content); the flush
    /// applies it — so after this call the driver holds the mounted DOM.
    pub fn process_pending_view(&mut self, driver: &mut dyn crate::driver::DomDriver) {
        let guards = self.take_pending_view(driver);
        self.fragment_guards.extend(guards);
        self.flush(driver);
    }

    /// Mount the pending root view (posted by the component's `ctx.render`) and hand
    /// the mount's guards to the **caller** — the many-roots form of
    /// [`process_pending_view`](Self::process_pending_view), for one runtime hosting
    /// independently-unmountable roots (the guest's live). Dropping the returned
    /// guards unmounts that root: its task cancels via its own guard, its listeners
    /// unregister, its owner's cells dispose, and `free-nodes` for its minted ids
    /// rides the next flush.
    pub fn mount_root(&mut self, driver: &mut dyn crate::driver::DomDriver) -> Vec<MountGuard> {
        self.take_pending_view(driver)
    }

    pub(crate) fn take_pending_view(
        &mut self,
        driver: &mut dyn crate::driver::DomDriver,
    ) -> Vec<MountGuard> {
        let maybe = self.core.pending_view.borrow_mut().take();
        if let Some(mut view) = maybe {
            // The root mount: register the template, then ANNOUNCE it as this
            // mount's root. Explicit — registration alone can't signal a root,
            // because templates are content-addressed (a remount's root is already
            // registered and emits nothing).
            let template = driver.register_template(view.template());
            driver.apply(vec![crate::driver::DomOp::MountRoot { template }]);
            mount_wired_view(self, &mut view, driver)
        } else {
            Vec::new()
        }
    }

}

/// Mount a wired view's dynamics. **Registration/placement of its template is the
/// caller's job** (root registration, or a structural op at an anchor) — this binds
/// slots, wires events, and mounts fragments/children against the current slot scope.
fn mount_wired_view(
    runtime: &mut Runtime,
    view: &mut crate::ctx::WiredView,
    driver: &mut dyn crate::driver::DomDriver,
) -> Vec<MountGuard> {
    let mut guards = Vec::new();
    // Retire this island's static-paint probe the moment a view — the root or any nested
    // `@if`/`@for` sub-view, each of which mounts through here — registers client work or a
    // nested live. The probe is shared across the island's frame chain, so one disqualifying
    // view settles the whole island.
    if view.blocks_static_paint() {
        if let Some(probe) = view.contexts().get::<crate::ctx::StaticProbe>() {
            probe.disqualify();
        }
    }
    runtime.live.extend(view.take_islands());

    // Every node id this view instance mints is freed when its mount guards drop
    // (`free-nodes` on the wire): ids are guest-allocated, so the guest ends them.
    // Nested fragments' rows mint through their own mount_wired_view and carry
    // their own guard, so coverage is recursive.
    let mut minted: Vec<crate::driver::NodeId> = Vec::new();

    let (block_guards, block_fragments) = mount_blocks(&runtime.core, view, driver, &mut minted);
    guards.extend(block_guards);

    // One-shot text (`(expr)` interpolation): a single `SetText` at mount, resolved
    // against this template's slot scope — no block, no subscription.
    for (slot, text) in view.take_oneshot() {
        let node_id = driver.alloc_slot_node_id(slot);
        minted.push(node_id);
        driver.apply(vec![crate::driver::DomOp::SetText { node_id, text }]);
    }

    // Resolve child-component anchors and fragment anchors **before** any fragment
    // mounts: the first row/branch instantiation replaces the driver's transient slot
    // scope, so every anchor must bind while this template's slots are still current.
    let child_anchors: Vec<(crate::driver::NodeId, crate::runtime::ChildId)> = view
        .take_child_anchors()
        .into_iter()
        .map(|(slot, child_id)| (driver.alloc_slot_node_id(slot), child_id))
        .collect();
    type FragmentDispatch =
        Rc<dyn Fn(&crate::signal::Cx, crate::live_view::FragmentOp) -> crate::ctx::WiredFragmentOut>;
    let fragments: Vec<(crate::driver::NodeId, FragmentDispatch, u32, crate::ctx::WiredFragmentDecl)> =
        block_fragments
            .into_iter()
            .flat_map(|block| {
                let dispatch = block.dispatch;
                block.decls.into_iter().map(move |(index, decl)| {
                    (decl.slot, Rc::clone(&dispatch), index, decl)
                })
            })
            .map(|(slot, dispatch, index, decl)| {
                (driver.alloc_slot_node_id(slot), dispatch, index, decl)
            })
            .collect();
    minted.extend(child_anchors.iter().map(|(id, _)| *id));
    minted.extend(fragments.iter().map(|(id, _, _, _)| *id));

    let view_owner = view.owner();
    for (anchor_id, dispatch, index, decl) in fragments {
        guards.extend(mount_fragment(
            runtime,
            dispatch,
            index,
            decl.kind,
            &view_owner,
            anchor_id,
            driver,
        ));
    }

    // Resolve each view-embedded child's anchor: record it, and splice any render that
    // already arrived (a child that rendered synchronously before this parent mounted).
    let core = Rc::clone(&runtime.core);
    for (anchor_id, child_id) in child_anchors {
        core.resolve_child_anchor(child_id, anchor_id);
        guards.push(child_anchor_guard(&core, child_id));
    }

    // Mount-time effects: now that the DOM nodes exist, mint a client-only
    // capability token and spawn each effect as a scoped task (cancelled if the
    // component unmounts). Gated on the caller-stated client fact — a server paint
    // runs the same mount path, and an effect fired there would fire AGAIN on the
    // browser mount, the exact hydration desync `Client` exists to forbid. Inert
    // under replay too: a client effect is a real side effect, not a message-loop
    // transition.
    if runtime.core.client_mount.get() && !runtime.core.replay_mode() {
        for effect in view.take_client_effects() {
            guards.push(runtime.spawn_scoped(effect(crate::capability::Client::new())));
        }
    }

    guards.extend(view.take_cleanup_guards());

    // The id release: dropped guards only *queue* their ids — the frame's one
    // `FreeNodes` drains at `flush_inner`'s settled exit, after every teardown op of
    // the same cascade (including an embedded child's `RemoveFragment`, which waits
    // for its task's reap). Free-last across task seams, by construction.
    guards.push(free_nodes_guard(&runtime.core, minted));

    guards
}

fn event_listener_guard(
    core: &Rc<RuntimeCore>,
    node_id: crate::driver::NodeId,
    event_type: &'static str,
    handler_id: crate::driver::HandlerId,
) -> MountGuard {
    struct EventListenerGuard {
        core: Weak<RuntimeCore>,
        node_id: crate::driver::NodeId,
        event_type: &'static str,
        handler_id: crate::driver::HandlerId,
    }

    impl Drop for EventListenerGuard {
        fn drop(&mut self) {
            if let Some(core) = self.core.upgrade() {
                core.enqueue_dom_op(crate::driver::DomOp::RemoveEventListener {
                    node_id: self.node_id,
                    event_type: self.event_type,
                    handler_id: self.handler_id,
                });
            }
        }
    }

    MountGuard::new(EventListenerGuard {
        core: Rc::downgrade(core),
        node_id,
        event_type,
        handler_id,
    })
}

/// Disconnect a measurement observer when its element's view unmounts — the
/// `UnwatchMeasure` twin of [`event_listener_guard`].
fn measure_guard(
    core: &Rc<RuntimeCore>,
    node_id: crate::driver::NodeId,
    handler_id: crate::driver::HandlerId,
) -> MountGuard {
    struct MeasureGuard {
        core: Weak<RuntimeCore>,
        node_id: crate::driver::NodeId,
        handler_id: crate::driver::HandlerId,
    }

    impl Drop for MeasureGuard {
        fn drop(&mut self) {
            if let Some(core) = self.core.upgrade() {
                core.enqueue_dom_op(crate::driver::DomOp::UnwatchMeasure {
                    node_id: self.node_id,
                    handler_id: self.handler_id,
                });
            }
        }
    }

    MountGuard::new(MeasureGuard { core: Rc::downgrade(core), node_id, handler_id })
}

/// Release a child's anchor registration when the parent's view unmounts: the anchor
/// node is gone, so a render arriving after this must stash, not splice — and a stash
/// that never found its anchor will never splice, so it drops here too.
fn child_anchor_guard(core: &Rc<RuntimeCore>, child_id: ChildId) -> MountGuard {
    struct ChildAnchorGuard {
        core: Weak<RuntimeCore>,
        child_id: ChildId,
    }

    impl Drop for ChildAnchorGuard {
        fn drop(&mut self) {
            if let Some(core) = self.core.upgrade() {
                core.child_anchors.borrow_mut().remove(&self.child_id);
                core.child_renders.borrow_mut().remove(&self.child_id);
            }
        }
    }

    MountGuard::new(ChildAnchorGuard { core: Rc::downgrade(core), child_id })
}

/// Mount the view's tick subscriptions (`ctx.every` / `ctx.frames`): register the
/// handler, emit the initial `StartTicks` if the gate is already true, and follow the
/// gate — Elm's `Sub`, on the wire as a Start/Stop command pair (the exact pattern of
/// Add/RemoveEventListener).
fn mount_tick_subs(
    core: &Rc<RuntimeCore>,
    view: &mut crate::ctx::WiredView,
    driver: &mut dyn crate::driver::DomDriver,
) -> Vec<MountGuard> {
    // Inert under replay: real ticks are suppressed and re-fed from the message log
    // (each tick was recorded as a `dispatch` carrying its delta).
    if core.replay_mode() {
        return Vec::new();
    }
    // A tick subscription is a graph effect gated by a signal: it enqueues
    // `Start`/`Stop` when the gate flips (its first run starts ticks if the gate is
    // already true). Rooted in the view's scope, so it stops watching on unmount.
    let owner = view.owner();
    let owner = &owner;
    for sub in view.take_tick_subs() {
        let handler_id = driver.alloc_handler_id();
        driver
            .register_event_handler(handler_id, crate::driver::EventHandler::single(Rc::clone(&sub.handler)));
        let interval_ms = sub.interval_ms;
        let gate = sub.gate;
        let active = Cell::new(false);
        let tick_core = Rc::downgrade(core);
        crate::signal::reaction::Reaction::spawn_in(owner, move |cx| {
            let now = gate.get(cx);
            if now == active.get() {
                return;
            }
            active.set(now);
            if let Some(core) = tick_core.upgrade() {
                core.enqueue_dom_op(if now {
                    crate::driver::DomOp::StartTicks { handler_id, interval_ms }
                } else {
                    crate::driver::DomOp::StopTicks { handler_id }
                });
            }
        });
    }
    Vec::new()
}

/// Register each watcher's handler and emit its one-shot watch op — the subscription
/// lives as long as the mount. Inert under replay: the intents were recorded as
/// `dispatch`es and re-feed from the message log, so no observer is armed.
fn mount_watchers(
    core: &Rc<RuntimeCore>,
    driver: &mut dyn crate::driver::DomDriver,
    handlers: Vec<Rc<dyn Fn(crate::live_view::Event)>>,
    op: impl Fn(crate::driver::HandlerId) -> crate::driver::DomOp,
) {
    if core.replay_mode() {
        return;
    }
    for handler in handlers {
        let handler_id = driver.alloc_handler_id();
        driver.register_event_handler(handler_id, crate::driver::EventHandler::single(handler));
        core.enqueue_dom_op(op(handler_id));
    }
}

/// Mount the view's navigation subscriptions (`ctx.navigation`).
fn mount_nav_subs(
    core: &Rc<RuntimeCore>,
    view: &mut crate::ctx::WiredView,
    driver: &mut dyn crate::driver::DomDriver,
) {
    let handlers = view.take_nav_subs().into_iter().map(|s| s.handler).collect();
    mount_watchers(core, driver, handlers, |handler_id| {
        crate::driver::DomOp::WatchNavigation { handler_id }
    });
}

/// Mount the view's size subscriptions (`ctx.resizes`).
fn mount_size_subs(
    core: &Rc<RuntimeCore>,
    view: &mut crate::ctx::WiredView,
    driver: &mut dyn crate::driver::DomDriver,
) {
    let handlers = view.take_size_subs().into_iter().map(|s| s.handler).collect();
    mount_watchers(core, driver, handlers, |handler_id| {
        crate::driver::DomOp::WatchSize { handler_id }
    });
}

/// Frees a view instance's minted node ids when its mount is torn down. A dead
/// runtime has no fold left to free anything from — the weak upgrade fails and the
/// drop is a no-op, exactly as teardown wants.
fn free_nodes_guard(core: &Rc<RuntimeCore>, node_ids: Vec<crate::driver::NodeId>) -> MountGuard {
    struct FreeOnDrop {
        core: Weak<RuntimeCore>,
        node_ids: Vec<crate::driver::NodeId>,
    }
    impl Drop for FreeOnDrop {
        fn drop(&mut self) {
            if self.node_ids.is_empty() {
                return;
            }
            if let Some(core) = self.core.upgrade() {
                // Queued, not emitted: an unmount cascade crosses task seams (a parent's
                // guards drop synchronously; an embedded child's RemoveFragment waits for
                // its task's reap), so a free emitted here could precede a teardown op
                // that names the same id. The frame's frees drain once, after the cascade
                // settles (`flush_inner`'s settled exit) — free-last by construction, at
                // the one place that can see the whole frame.
                core.pending_frees.borrow_mut().append(&mut self.node_ids);
                core.schedule_flush();
            }
        }
    }
    MountGuard::new(FreeOnDrop { core: Rc::downgrade(core), node_ids })
}

/// Tears down the DOM a view-embedded child spliced at `anchor_id` when its mount guards drop —
/// the child-unmount counterpart of a `@for` row's [`SpliceOp::Remove`](crate::SpliceOp). A child
/// mounts through `MountFragment { anchor_id, .. }`, so its whole subtree (static structure
/// included, which `free_nodes_guard` cannot reach — those nodes are the driver's, not
/// guest-minted) is reclaimed by `RemoveFragment` on that anchor. The anchor is still live when
/// this fires — an ancestor tearing the child down frees its ids only at the frame's settled
/// exit, after this guard's op — so a cascaded child and one removed on its own both land clean
/// under the strict folds.
fn remove_fragment_guard(core: &Rc<RuntimeCore>, anchor_id: crate::driver::NodeId) -> MountGuard {
    struct RemoveOnDrop {
        core: Weak<RuntimeCore>,
        anchor_id: crate::driver::NodeId,
    }
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            if let Some(core) = self.core.upgrade() {
                core.enqueue_dom_op(crate::driver::DomOp::RemoveFragment {
                    anchor_id: self.anchor_id,
                });
                core.schedule_flush();
            }
        }
    }
    MountGuard::new(RemoveOnDrop { core: Rc::downgrade(core), anchor_id })
}

/// A mounted `@for` row: its anchor, its disposal scope (a child of the `@for`'s
/// scope), and its binding guards — so removing a row reclaims exactly its subtree.
struct MountedRow {
    anchor_id: crate::driver::NodeId,
    owner: crate::owner::Owner,
    _guards: Vec<MountGuard>,
}

/// Mount a list's rows in order after `list_anchor`, recording each into `mounted`.
fn mount_rows(
    runtime: &mut Runtime,
    driver: &mut dyn crate::driver::DomDriver,
    rows: Vec<(crate::Row, crate::ctx::WiredView)>,
    list_anchor: crate::driver::NodeId,
    mounted: &Rc<RefCell<std::collections::HashMap<crate::Row, MountedRow>>>,
) {
    let mut previous_anchor = list_anchor;
    for (row, mut row_view) in rows {
        let row_owner = row_view.owner.clone();
        let anchor_id = driver.alloc_node_id();
        let template = driver.register_template(row_view.template.clone());
        driver.apply(vec![crate::driver::DomOp::MountFragment { anchor_id, template }]);
        driver.apply(vec![crate::driver::DomOp::MoveFragment {
            anchor_id,
            after_anchor: previous_anchor,
        }]);
        let mut row_guards = mount_wired_view(runtime, &mut row_view, driver);
        // The row's anchor is the list machinery's mint, not the row view's, so the row's own
        // guards do not cover it. One guard reclaims both halves: `RemoveFragment` takes the
        // subtree the anchor carries *and* the anchor's fold entries, so the row's DOM and its
        // ids go together, whenever the row goes — spliced away, or dropped with the region
        // around it.
        row_guards.push(remove_fragment_guard(&runtime.core, anchor_id));
        mounted
            .borrow_mut()
            .insert(row, MountedRow { anchor_id, owner: row_owner, _guards: row_guards });
        previous_anchor = anchor_id;
    }
}

/// One block's structural work, carried out of [`mount_blocks`] so fragment anchors
/// can resolve in the same slot-scope window as child anchors — before any fragment
/// mounts (the first row/branch instantiation replaces the slot scope).
struct BlockFragments {
    dispatch: Rc<dyn Fn(&crate::signal::Cx, crate::live_view::FragmentOp) -> crate::ctx::WiredFragmentOut>,
    decls: Vec<(u32, crate::ctx::WiredFragmentDecl)>,
}

/// Mount a view's dispatch blocks: resolve each block's slots to node ids, register
/// its event arms (one [`EventHandler`](crate::driver::EventHandler) *index* per
/// listener — the handler table shares the block's dispatch), install its binding
/// dispatch into the view's [`ViewScope`](crate::signal::scope::ViewScope), and run
/// the initial paint. A binding arm returns its patch as data; the wrapper here owns
/// the per-index last-emission table, so only a differing op joins the stream —
/// first run included, which is the initial paint. The scope re-tracks each index's
/// dependency set per run and is retained by the view's owner: unmounting drops it,
/// which kills its edges and its pending-queue entry.
fn mount_blocks(
    core: &Rc<RuntimeCore>,
    view: &mut crate::ctx::WiredView,
    driver: &mut dyn crate::driver::DomDriver,
    minted: &mut Vec<crate::driver::NodeId>,
) -> (Vec<MountGuard>, Vec<BlockFragments>) {
    let owner = view.owner();
    let mut guards = mount_tick_subs(core, view, driver);
    mount_nav_subs(core, view, driver);
    mount_size_subs(core, view, driver);
    let mut fragments = Vec::new();
    let scope = crate::signal::scope::ViewScope::new();
    for block in view.take_blocks() {
        if !block.fragments.is_empty() {
            fragments.push(BlockFragments {
                dispatch: Rc::clone(&block.on_fragment),
                decls: block
                    .fragments
                    .into_iter()
                    .enumerate()
                    .map(|(index, decl)| (index as u32, decl))
                    .collect(),
            });
        }
        for (idx, (slot, binding)) in block.event_slots.iter().enumerate() {
            let node_id = driver.alloc_slot_node_id(*slot);
            minted.push(node_id);
            let handler_id = driver.alloc_handler_id();
            driver.register_event_handler(
                handler_id,
                crate::driver::EventHandler::arm(Rc::clone(&block.on_event), idx as u32),
            );
            match binding {
                crate::live_view::EventBinding::Dom(event_type) => {
                    driver.apply(vec![crate::driver::DomOp::AddEventListener {
                        node_id,
                        event_type,
                        handler_id,
                    }]);
                    guards.push(event_listener_guard(core, node_id, event_type, handler_id));
                }
                crate::live_view::EventBinding::Measure => {
                    driver.apply(vec![crate::driver::DomOp::WatchMeasure { node_id, handler_id }]);
                    guards.push(measure_guard(core, node_id, handler_id));
                }
            }
        }

        for painting in block.paintings {
            let node_id = driver.alloc_slot_node_id(painting.slot);
            minted.push(node_id);
            crate::canvas::install(&owner, core, node_id, painting.layers);
        }

        let nodes: Vec<crate::driver::NodeId> = block
            .binding_slots
            .iter()
            .map(|slot| {
                let node_id = driver.alloc_slot_node_id(*slot);
                minted.push(node_id);
                node_id
            })
            .collect();
        let count = nodes.len() as u32;
        let run = block.run;
        let last: RefCell<Vec<Option<crate::driver::DomOp>>> =
            RefCell::new(vec![None; count as usize]);
        let binding_core = Rc::downgrade(core);
        scope.install(
            count,
            Rc::new(move |cx, idx| {
                let Some(op) = run(cx, &nodes, idx) else { return };
                let mut last = last.borrow_mut();
                if last[idx as usize].as_ref() == Some(&op) {
                    return;
                }
                last[idx as usize] = Some(op.clone());
                if let Some(core) = binding_core.upgrade() {
                    core.enqueue_dom_op(op);
                }
            }),
        );
    }
    scope.paint();
    owner.retain(scope);
    (guards, fragments)
}

/// A `keep` branch fragment (`@if keep`/`@match keep`): each branch, once shown, is
/// mounted **once** and kept — switching detaches the old and attaches the new
/// (preserving its DOM and component state) rather than tearing down. A graph effect
/// on the selector drives the detach/attach; the first appearance of a branch mounts
/// it. All kept branches dispose together with the enclosing scope.
fn mount_kept_branch_fragment(
    owner: &crate::owner::Owner,
    slot_anchor: crate::driver::NodeId,
    selector: impl Fn(&crate::signal::Cx) -> Option<usize> + 'static,
    build: impl Fn(&crate::signal::Cx, usize) -> Option<crate::ctx::WiredView> + 'static,
) {
    let mount_core = Rc::downgrade(&owner.runtime());
    let branches = Rc::new(RefCell::new(
        std::collections::HashMap::<usize, KeptBranch>::new(),
    ));
    // `None` = never run; `Some(active)` = the branch shown last run.
    let active_branch = Rc::new(Cell::new(None::<Option<usize>>));
    crate::signal::reaction::Reaction::spawn_in(owner, move |cx| {
        let next = selector(cx);
        if active_branch.get() == Some(next) {
            return;
        }
        let prev = active_branch.get().flatten();
        active_branch.set(Some(next));
        // Build the incoming branch's view now (first appearance only); the swap
        // itself needs the driver, so it runs in the deferred mount pass.
        let next_view = match next {
            Some(n) if !branches.borrow().contains_key(&n) => build(cx, n),
            _ => None,
        };
        // The queued swap holds the region weakly: if the enclosing scope disposes
        // before the mount pass runs, the region's guards are already emitting its
        // teardown, and running a stale swap would splice zombie DOM at a dead anchor.
        let branches = Rc::downgrade(&branches);
        let Some(core) = mount_core.upgrade() else { return };
        core.pending_mounts.borrow_mut().push(Box::new(move |runtime, driver| {
                let Some(branches) = branches.upgrade() else { return Vec::new() };
                if let Some(prev) = prev {
                    if let Some(branch) = branches.borrow().get(&prev) {
                        driver.apply(vec![crate::driver::DomOp::DetachFragment {
                            anchor_id: branch.anchor_id,
                        }]);
                    }
                }
                if let Some(n) = next {
                    let mounted = branches.borrow().get(&n).map(|b| b.anchor_id);
                    if let Some(anchor_id) = mounted {
                        driver.apply(vec![crate::driver::DomOp::AttachFragment { anchor_id }]);
                    } else if let Some(mut view) = next_view {
                        let anchor_id = driver.alloc_node_id();
                        let template = driver.register_template(view.template.clone());
                        driver.apply(vec![
                            crate::driver::DomOp::MountFragment { anchor_id, template },
                            crate::driver::DomOp::MoveFragment {
                                anchor_id,
                                after_anchor: slot_anchor,
                            },
                        ]);
                        let mut branch_guards = mount_wired_view(runtime, &mut view, driver);
                        branch_guards.push(free_nodes_guard(&runtime.core, vec![anchor_id]));
                        branches.borrow_mut().insert(
                            n,
                            KeptBranch { anchor_id, _guards: branch_guards },
                        );
                    }
                }
                Vec::new()
            }));
    });
}

/// A non-`keep` branch fragment (`@if`/`@match`/a content splice): a graph effect rooted
/// in the enclosing view's scope. `select` yields a cheap branch identity (tracked)
/// so we swap only on a real branch change; `build` produces the active branch's
/// wired view (also tracked). On a swap the effect disposes the old branch's child
/// scope and mounts the new one in a fresh child scope, in the deferred mount pass
/// (registration needs the driver). The effect and the last branch dispose together
/// when the enclosing scope does, so this returns no guards of its own.
fn mount_branch_fragment(
    owner: &crate::owner::Owner,
    anchor_id: crate::driver::NodeId,
    select: impl Fn(&crate::signal::Cx) -> Option<usize> + 'static,
    build: impl Fn(&crate::signal::Cx) -> Option<crate::ctx::WiredView> + 'static,
) {
    let mount_core = Rc::downgrade(&owner.runtime());
    let branch_guards = Rc::new(RefCell::new(Vec::<MountGuard>::new()));
    let branch_owner = Rc::new(RefCell::new(None::<crate::owner::Owner>));
    // `Some(identity)` once evaluated; the initial `None` forces the first mount.
    let current = Rc::new(Cell::new(None::<Option<usize>>));
    // First application is a `MountFragment` (the initial branch), later ones
    // `ReplaceFragment` — so the initial paint shows the branch directly, with no
    // empty intermediate frame.
    let mounted = Rc::new(Cell::new(false));
    crate::signal::reaction::Reaction::spawn_in(owner, move |cx| {
        let next = select(cx);
        if current.get() == Some(next) {
            return;
        }
        current.set(Some(next));
        let view = build(cx);
        // Weak for the same reason as the kept-branch swap: a region disposed before
        // the mount pass must not have a stale swap splice into its dead anchor.
        let branch_guards = Rc::downgrade(&branch_guards);
        let branch_owner = Rc::downgrade(&branch_owner);
        let mounted = Rc::clone(&mounted);
        let Some(core) = mount_core.upgrade() else { return };
        core.pending_mounts.borrow_mut().push(Box::new(move |runtime, driver| {
                let (Some(branch_guards), Some(branch_owner)) =
                    (branch_guards.upgrade(), branch_owner.upgrade())
                else {
                    return Vec::new();
                };
                let mut view = view;
                let template = driver.register_template(
                    view.as_ref()
                        .map(|v| v.template.clone())
                        .unwrap_or(crate::template::Template::EMPTY),
                );
                let op = if mounted.replace(true) {
                    crate::driver::DomOp::ReplaceFragment { anchor_id, template }
                } else {
                    crate::driver::DomOp::MountFragment { anchor_id, template }
                };
                driver.apply(vec![op]);
                // Reclaim the outgoing branch: drop its mount guards, then dispose
                // its child scope (freeing its bindings/nested components).
                branch_guards.borrow_mut().clear();
                if let Some(old) = branch_owner.borrow_mut().take() {
                    old.dispose();
                }
                if let Some(mounted) = view.as_mut() {
                    *branch_owner.borrow_mut() = Some(mounted.owner.clone());
                    *branch_guards.borrow_mut() = mount_wired_view(runtime, mounted, driver);
                }
                Vec::new()
            }));
    });
}

fn mount_fragment(
    runtime: &mut Runtime,
    dispatch: Rc<dyn Fn(&crate::signal::Cx, crate::live_view::FragmentOp) -> crate::ctx::WiredFragmentOut>,
    index: u32,
    kind: crate::ctx::WiredFragmentKind,
    owner: &crate::owner::Owner,
    anchor_id: crate::driver::NodeId,
    driver: &mut dyn crate::driver::DomDriver,
) -> Vec<MountGuard> {
    use crate::ctx::{WiredFragmentKind, WiredFragmentOut};
    use crate::live_view::FragmentOp;

    // The three requests this mount makes of the block's dispatch, as adapters the
    // branch/list machinery below calls. Their closure types are this function's —
    // O(1) for the whole program, not per app call site.
    let select = {
        let dispatch = Rc::clone(&dispatch);
        move |cx: &crate::signal::Cx| match dispatch(cx, FragmentOp::Select(index)) {
            WiredFragmentOut::Selected(arm) => arm,
            _ => None,
        }
    };
    let build_arm = {
        let dispatch = Rc::clone(&dispatch);
        move |cx: &crate::signal::Cx, arm: u32| match dispatch(
            cx,
            FragmentOp::Arm { fragment: index, arm },
        ) {
            WiredFragmentOut::LiveView(view) => Some(view),
            _ => None,
        }
    };
    let mut guards = Vec::new();
    match kind {
        WiredFragmentKind::Branch { keep, arms } => {
            let selector = {
                let select = select.clone();
                move |cx: &crate::signal::Cx| select(cx).filter(|&active| (active as u32) < arms)
            };
            if keep {
                mount_kept_branch_fragment(owner, anchor_id, selector, move |cx, arm: usize| {
                    build_arm(cx, arm as u32)
                });
                return guards;
            }
            mount_branch_fragment(owner, anchor_id, selector.clone(), move |cx| {
                selector(cx).and_then(|active| build_arm(cx, active as u32))
            });
        }
        WiredFragmentKind::Content => {
            // Server-rendered content: an effect that re-mounts the resolved template
            // whenever its source changes. The first application is a `MountFragment`
            // (so the initial paint shows the content directly, with no empty frame),
            // later ones `ReplaceFragment`. Nothing inside to wire; its live surface
            // at mount so the enclosing page render reports them to the host.
            let mounted = Cell::new(false);
            let mount_core = Rc::downgrade(&owner.runtime());
            crate::signal::reaction::Reaction::spawn_in(owner, move |cx| {
                let WiredFragmentOut::Content(rendered) =
                    dispatch(cx, FragmentOp::Content(index))
                else {
                    return;
                };
                let first = !mounted.replace(true);
                let Some(core) = mount_core.upgrade() else { return };
                core.pending_mounts.borrow_mut().push(Box::new(move |runtime, driver| {
                    runtime.live.extend(rendered.live());
                    let template = driver.register_template(rendered.template().clone());
                    let op = if first {
                        crate::driver::DomOp::MountFragment { anchor_id, template }
                    } else {
                        crate::driver::DomOp::ReplaceFragment { anchor_id, template }
                    };
                    driver.apply(vec![op]);
                    Vec::new()
                }));
            });
        }
        WiredFragmentKind::FixedList(rows) => {
            // Rows built once at view construction: mount them and keep their
            // guards; no subscription, nothing ever splices.
            let mounted_rows = Rc::new(RefCell::new(std::collections::HashMap::<
                crate::Row,
                MountedRow,
            >::new()));
            mount_rows(runtime, driver, rows, anchor_id, &mounted_rows);
            guards.push(MountGuard::new(mounted_rows));
        }
        WiredFragmentKind::Slot(cell) => {
            // A `(view)` slot: one pre-built view mounted once at its anchor. No
            // subscription, nothing ever re-selects — its block came across at wire
            // time, so its events already route to this reducer.
            let slot_guards = Rc::new(RefCell::new(Vec::<MountGuard>::new()));
            if let Some(mut view) = cell.borrow_mut().take() {
                let slot_guards_eff = Rc::clone(&slot_guards);
                owner.runtime().pending_mounts.borrow_mut().push(Box::new(
                    move |runtime, driver| {
                        let template = driver.register_template(view.template.clone());
                        driver.apply(vec![crate::driver::DomOp::MountFragment {
                            anchor_id,
                            template,
                        }]);
                        *slot_guards_eff.borrow_mut() =
                            mount_wired_view(runtime, &mut view, driver);
                        Vec::new()
                    },
                ));
            }
            guards.push(MountGuard::new(slot_guards));
        }
        WiredFragmentKind::List { structure, rebuild } => {
            // Each mounted row keeps its anchor, its disposal scope (a child of the
            // `@for`'s scope), and its binding guards — so removing a row reclaims
            // exactly its subtree.
            let mounted_rows = Rc::new(RefCell::new(std::collections::HashMap::<
                crate::Row,
                MountedRow,
            >::new()));
            let list_anchor = anchor_id;

            // Subscribe the op-stream *before* snapshotting, so nothing is missed;
            // then mount the current rows.
            let consumer = structure.consume();
            let initial: Vec<(crate::Row, crate::ctx::WiredView)> = structure
                .snapshot_order()
                .into_iter()
                .filter_map(|row| rebuild(row).map(|view| (row, view)))
                .collect();
            mount_rows(runtime, driver, initial, list_anchor, &mounted_rows);

            // The `@for` is a graph effect: it observes the list's structure and, on
            // a structural change, drains the op-stream and applies the splice —
            // O(op) DOM work per op — in the deferred mount pass (needs the driver).
            let mounted_rows_eff = Rc::clone(&mounted_rows);
            let mount_core = Rc::downgrade(&owner.runtime());
            crate::signal::reaction::Reaction::spawn_in(owner, move |cx| {
                structure.observe(cx);
                let ops = consumer.drain();
                if ops.is_empty() {
                    return;
                }
                let rebuild = Rc::clone(&rebuild);
                // Weak for the same reason as the branch swaps: a list region disposed
                // before the mount pass must not have a stale splice touch its rows.
                let mounted = Rc::downgrade(&mounted_rows_eff);
                let Some(core) = mount_core.upgrade() else { return };
                core.pending_mounts.borrow_mut().push(Box::new(move |runtime, driver| {
                        let Some(mounted) = mounted.upgrade() else { return Vec::new() };
                        for op in ops {
                            match op {
                                crate::SpliceOp::Insert { row, after } => {
                                    let Some(mut view) = rebuild(row) else {
                                        continue;
                                    };
                                    let row_owner = view.owner.clone();
                                    let anchor_id = driver.alloc_node_id();
                                    let after_anchor = after
                                        .and_then(|r| {
                                            mounted.borrow().get(&r).map(|m| m.anchor_id)
                                        })
                                        .unwrap_or(list_anchor);
                                    let template =
                                        driver.register_template(view.template.clone());
                                    driver.apply(vec![crate::driver::DomOp::MountFragment {
                                        anchor_id,
                                        template,
                                    }]);
                                    driver.apply(vec![crate::driver::DomOp::MoveFragment {
                                        anchor_id,
                                        after_anchor,
                                    }]);
                                    let mut row_guards =
                                        mount_wired_view(runtime, &mut view, driver);
                                    row_guards
                                        .push(remove_fragment_guard(&runtime.core, anchor_id));
                                    mounted.borrow_mut().insert(
                                        row,
                                        MountedRow {
                                            anchor_id,
                                            owner: row_owner,
                                            _guards: row_guards,
                                        },
                                    );
                                }
                                // Removing a row is forgetting it: the `MountedRow` owns its
                                // subtree, so letting it drop is what reclaims the DOM. The
                                // drop's ops are drained here rather than left for the next
                                // turn, so they land in splice order — the moves that follow
                                // position against siblings this one has already vacated.
                                crate::SpliceOp::Remove { row } => {
                                    let gone = mounted.borrow_mut().remove(&row);
                                    if let Some(m) = gone {
                                        m.owner.dispose();
                                        drop(m);
                                        runtime.core.drain_dom_ops(driver);
                                    }
                                }
                                crate::SpliceOp::Move { row, after } => {
                                    let anchor_id =
                                        mounted.borrow().get(&row).map(|m| m.anchor_id);
                                    if let Some(anchor_id) = anchor_id {
                                        let after_anchor = after
                                            .and_then(|r| {
                                                mounted.borrow().get(&r).map(|m| m.anchor_id)
                                            })
                                            .unwrap_or(list_anchor);
                                        driver.apply(vec![crate::driver::DomOp::MoveFragment {
                                            anchor_id,
                                            after_anchor,
                                        }]);
                                    }
                                }
                                crate::SpliceOp::Clear => {
                                    let gone: Vec<_> =
                                        mounted.borrow_mut().drain().map(|(_, m)| m).collect();
                                    for m in gone {
                                        m.owner.dispose();
                                    }
                                    runtime.core.drain_dom_ops(driver);
                                }
                            }
                        }
                        Vec::new()
                    }));
            });
            // Keep the initial rows' guards alive for the life of the `@for` mount.
            guards.push(MountGuard::new(mounted_rows));
        }
    }
    guards
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_complete() {
        let mut rt = Runtime::new();
        let done = Arc::new(Mutex::new(false));
        let done2 = Arc::clone(&done);
        rt.spawn(async move {
            *done2.lock().unwrap() = true;
        });
        rt.run_to_quiescence();
        assert!(*done.lock().unwrap());
    }

    /// The render hand-off seam: a component reaches its `recv` loop and `spawn_pending`s its
    /// live future without a `&mut Runtime`; the active runtime adopts and drives it. Dropping
    /// the parent's guard cancels it — the reap drops the future on the next drive.
    #[test]
    fn spawn_pending_is_adopted_and_cancel_guard_reaps_it() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let polls = Arc::new(AtomicU32::new(0));
        let polls2 = Arc::clone(&polls);

        // A parked live future: counts each poll, never completes.
        struct Parked(Arc<AtomicU32>);
        impl Future for Parked {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Poll::Pending
            }
        }

        let mut rt = Runtime::new();
        let guard = rt.core().spawn_pending(Parked(polls2));
        rt.run_once(); // adopt + first poll
        assert_eq!(polls.load(Ordering::Relaxed), 1, "the handed-off future was adopted and driven");
        assert_eq!(rt.live_task_count(), 1);

        drop(guard); // parent unmounts the child
        rt.run_once(); // reap the cancelled task before polling
        assert_eq!(rt.live_task_count(), 0, "dropping the guard reaps the future");
        assert_eq!(polls.load(Ordering::Relaxed), 1, "a cancelled future is never polled again");
    }

    #[test]
    fn task_yields_and_resumes() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let count = Arc::new(AtomicU32::new(0));
        let count2 = Arc::clone(&count);
        let mut rt = Runtime::new();
        rt.spawn(async move {
            struct Yield(bool);
            impl Future for Yield {
                type Output = ();
                fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                    if self.0 {
                        Poll::Ready(())
                    } else {
                        self.0 = true;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
            }
            count2.fetch_add(1, Ordering::SeqCst);
            Yield(false).await;
            count2.fetch_add(1, Ordering::SeqCst);
        });
        rt.run_to_quiescence();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

}
