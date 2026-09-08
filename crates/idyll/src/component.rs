use std::cell::RefCell;
use std::error::Error;
use std::future::Future;
use std::rc::Rc;

use crate::ctx::{ContextMap, Ctx, FaultRoute, Setup};
use crate::lifecycle::{run_to_render, Rendering, Resolved};
use crate::runtime::{ChildId, MountGuard};

/// The parts every view-embedded child needs, whichever way it mounts: a fresh context frame, the
/// fault route resolved from it (fixed for the child's life), the DOM-guard cell its executor task
/// holds (cancelling the task tears the subtree down), and the base render sink that splices the
/// child's view at its anchor — stashing until the anchor lands, since the parent records it only
/// when it mounts its own view. `mount_child` hands the sink to [`Ctx::resolving`], which wraps it
/// to mark a render witness; the fire-and-forget mounts pass it to `Ctx::spawned` as-is.
struct ChildScaffold {
    contexts: ContextMap,
    route: FaultRoute,
    guards: Rc<RefCell<Vec<MountGuard>>>,
    sink: Rc<dyn Fn(crate::ctx::WiredView)>,
}

fn child_scaffold(frame: &ContextMap, child_id: ChildId) -> ChildScaffold {
    let contexts = crate::ctx::ContextScope::child(frame);
    let route = FaultRoute::from_frame(&contexts);
    let (guards, sink) = contexts.runtime().child_render_sink(child_id);
    ChildScaffold { contexts, route, guards, sink }
}

// Suspense is not a status but a **fact about a future**: a component that has not yet posted
// its view is `Pending`. The parent's `render` awaits each child to its render point, so the
// whole initial tree resolves together and a still-pending descendant keeps the nearest
// suspense boundary showing its fallback — no status enum, no rollup. The chain is the
// **witnessed mount chain**: a child driven fire-and-forget (an error boundary's) resolves
// outside it, so pending stops there by design — see the composition rule in `boundary.rs`.
// A fault is likewise a plain value: pre-render it resolves the future's `Err` (propagating
// up the render-await chain); post-render it fires a message to the nearest error boundary's
// mailbox (see [`crate::ctx::FaultRoute`]).

/// Mount a view-embedded child in the **initial render tree**: build it concretely against a
/// child of `frame`, drive it to its **render point**, then hand its still-live future to the
/// flat executor and return a cancel-guard. The parent's `render` `await`s this, so the whole
/// initial subtree resolves together.
///
/// - `Ok(guard)` — it rendered (or a render-once completed); it now lives in the executor.
///   Dropping the guard unmounts it and its whole subtree (the DOM guards and owner ride the
///   task the guard cancels). The guard rides the enclosing view's `placement_guards`.
/// - `Err(fault)` — it failed **before** rendering. That is a plain value the parent's `render`
///   propagates on up the render-await chain to the nearest error boundary (which drives its
///   child to render and catches the `Err`) or, failing that, the top-level report.
///
/// After it renders, a fault from its own live loop is **post-render**: it fires a message to
/// the nearest error boundary's mailbox (see [`crate::ctx::FaultRoute`]) rather than propagating
/// — the parent is no longer awaiting it. Suspension after render just delays the child's own
/// updates; nothing rolls up.
///
/// The `C::run` call is concrete here (island-splitting attribution lives at these sites); the
/// coercion to `dyn` happens once, when [`spawn_pending`](crate::runtime::spawn_pending) boxes
/// the still-live future for the executor.
pub async fn mount_child<C: Component>(
    frame: &ContextMap,
    required: C::Required,
    optional: C::Optional,
    child_id: ChildId,
) -> std::result::Result<MountGuard, crate::Fault> {
    let ChildScaffold { contexts, route, guards, sink } = child_scaffold(frame, child_id);
    // `resolving` mints the witness *with* the sink that marks it, so the one we hand to
    // `run_to_render` is by construction the one this child's render fires. The view itself flows
    // to the DOM through the base sink (stash-until-anchor); the witness carries no payload.
    let (ctx, witness) = Ctx::<Setup, C::Msg>::resolving(contexts, sink);
    let run = race_fault::<C>(ctx, required, optional);

    let rt = Rc::clone(frame.runtime());
    match run_to_render(Rendering::new(run, witness)).await {
        // Rendered and parked at `recv`: hand the still-live future to the executor.
        Resolved::Live(future) => Ok(park_child(&rt, guards, route, future)),
        // Rendered, then completed — a render-once mount, or a post-render fault: park it too, so
        // its `Ok` owner (or routed `Err`) is handled by the same tail that drives a live child.
        Resolved::Rendered(outcome) => {
            Ok(park_child(&rt, guards, route, std::future::ready(outcome)))
        }
        // Failed **before** rendering: the parent's own render failure, propagated up the
        // render-await chain.
        Resolved::Unrendered(Err(error)) => Err(error),
        // Completing `Ok` without rendering means the component body escaped the
        // render discipline (e.g. it returned another mount's `Ctx<Live>`). A usage
        // convention, not a type-held fact — so it faults like any component error
        // (guests are panic=abort; a panic here would kill every island on the page).
        Resolved::Unrendered(Ok(_)) => Err("component completed without rendering".into()),
    }
}

/// Park a resolved child in the executor: hold its DOM guards for the task's life, drive an `Ok`
/// mount's `finish` (its owner backs the DOM's cells, and its fault line stays live — a
/// render-once child's post-return fault reaches the boundary exactly as a live child's does) or
/// route an `Err` to the nearest boundary, then suspend forever so the subtree outlives the
/// resolve. Dropping the returned guard cancels the task and its guards, unmounting the subtree.
/// The shared tail of every child mount — a live child, a render-once one, and [`spawn_child`]'s
/// fire-and-forget alike.
fn park_child(
    rt: &Rc<crate::runtime::RuntimeCore>,
    guards: Rc<RefCell<Vec<MountGuard>>>,
    route: FaultRoute,
    run: impl Future<Output = crate::Result> + 'static,
) -> MountGuard {
    rt.spawn_pending(async move {
        let _dom = guards;
        let held = match run.await {
            Ok(mount) => mount.finish().await,
            Err(error) => Err(error),
        };
        if let Err(error) = held {
            route.route(error);
        }
        std::future::pending::<()>().await
    })
}

/// Mount a view-embedded child from a **reactive, post-render** site — an `@if`/`@match` arm, a
/// `@for` row, a slot placement, all (re)built during the synchronous flush, which cannot
/// `await`. Fire-and-forget: the whole `run` future goes to the executor at once and a
/// cancel-guard comes back synchronously (riding the arm/row/placement view's `placement_guards`,
/// so it unmounts with that view).
///
/// These are post-render by construction, so both phases follow the post-render rules: a
/// suspension just delays the child's appearance, and **any** fault fires a message to the
/// nearest error boundary's mailbox — there is no render-await above to propagate a pre-render
/// `Err` to.
pub fn spawn_child<C: Component>(
    frame: &ContextMap,
    required: C::Required,
    optional: C::Optional,
    child_id: ChildId,
) -> MountGuard {
    let ChildScaffold { contexts, route, guards, sink } = child_scaffold(frame, child_id);
    let rt = Rc::clone(contexts.runtime());
    let ctx = Ctx::<Setup, C::Msg>::spawned(contexts, sink);
    // Fire-and-forget: the whole `run` goes to the executor at once. A live child never returns;
    // a render-once one returns its mount; a fault — pre- or post-render, indistinguishable here —
    // routes to the nearest boundary. `park_child` handles all three, holding the DOM either way.
    park_child(&rt, guards, route, race_fault::<C>(ctx, required, optional))
}

/// A component: its message type, its props, and how to run it.
///
/// Props come in two halves because that is how a call site reads them. The required half
/// is a plain struct, so omitting a field is a compile error; the optional half defaults,
/// and is marked `?name=(…)` at the call site so the two never have to be told apart by
/// looking up which struct owns a field name.
///
/// The success value is the **live mount**. A message-less component (`M = Never`) renders
/// and returns its `Ctx` — the keep-alive token; one with messages can never construct a
/// `Ctx<Live, Never>`, so it satisfies the signature only by looping on `recv` forever
/// (divergence coerces). Ending a mount by accident is unrepresentable.
///
/// Written with `#[component]`, which emits both prop structs and this impl together, so
/// the types exist by construction rather than by being remembered.
pub trait Component: 'static {
    type Msg: 'static;
    type Required: 'static;
    type Optional: Default + 'static;

    /// The future is anonymous and **concrete** here, so the whole resolve call graph
    /// [`mount_child`] drives to the render point stays monomorphic — the seam the island
    /// splitter reads. Only at the hand-off to the flat executor does it coerce to `dyn`
    /// (boxed by [`spawn_pending`](crate::runtime::spawn_pending)), where a mount's remaining
    /// life is its message loop and the vtable indirection is a rounding error. That coercion
    /// is why this is not a `type Future` the caller names.
    ///
    /// The lint wants `-> impl Future + Send` so callers can bound the future; there is
    /// no such bound to want. A mount owns `Rc` signals and a `RefCell` runtime, so it is
    /// `!Send` by construction and always runs on the thread that made it.
    #[allow(async_fn_in_trait)]
    async fn run(
        ctx: Ctx<Setup, Self::Msg>,
        required: Self::Required,
        optional: Self::Optional,
    ) -> crate::Result;
}

// A message-less component ends by holding its mount for its lifetime: `Ctx::finish`
// (ctx.rs) — `ctx.render(view).await.finish().await` diverges on a `Never` inbox, so it never
// returns the mount. A view-embedded child is not driven by its parent; it lives in the flat
// executor (see `mount_child`), reaped when the parent drops its cancel-guard.

/// Run a component raced against its **fault line** (infrastructure failing a
/// promise made on its behalf — see [`Ctx`]'s fault cell). A fault ends the
/// component exactly as `Err` from its own body would.
async fn race_fault<C: Component>(
    ctx: Ctx<Setup, C::Msg>,
    required: C::Required,
    optional: C::Optional,
) -> crate::Result {
    let fault = ctx.fault.clone();
    // The component's future is concrete now; pin it in this frame instead of boxing.
    let mut fut = std::pin::pin!(C::run(ctx, required, optional));
    race_fault_future(fault, fut.as_mut()).await
}

/// The mount, unless the frame faults first. A fault is the membrane's typed refusal, and
/// it wins the race — a component that cannot be given its world never renders.
async fn race_fault_future(
    fault: crate::ctx::FaultCell,
    mut fut: std::pin::Pin<&mut impl Future<Output = crate::Result>>,
) -> crate::Result {
    std::future::poll_fn(|cx| {
        if let Some(error) = fault.poll_fault(cx) {
            return std::task::Poll::Ready(Err(error));
        }
        fut.as_mut().poll(cx)
    })
    .await
}

/// A harness's answer to a failed top-level component: say so. Nothing is above it to
/// refuse the mount or render a fallback, so the log is the only honest place left.
pub fn report_to_log(error: Box<dyn Error>) {
    eprintln!("component error: {error}");
}

/// Spawn a **live**'s entry function: `async fn(Ctx<Setup, M>, …) -> Result`.
///
/// A live is not a [`Component`] — it is mounted by key from a marker, not called with
/// props from a view — so it stays an ordinary function. The closure carries whatever
/// arguments it takes, which is why one spelling serves every arity.
pub fn spawn_live<M, Fut>(
    f: impl FnOnce(Ctx<Setup, M>) -> Fut + 'static,
    ctx: Ctx<Setup, M>,
    report: impl FnOnce(Box<dyn Error>) + 'static,
) -> impl Future<Output = ()>
where
    M: 'static,
    Fut: Future<Output = crate::Result> + 'static,
{
    let fault = ctx.fault.clone();
    async move {
        let mut fut = std::pin::pin!(f(ctx));
        match race_fault_future(fault, fut.as_mut()).await {
            // No boundary sits above the root, so a fault that reaches here — a setup
            // failure, or one bubbling up out of `finish` from a driven child — is the
            // membrane's last resort: report it.
            Ok(mount) => {
                if let Err(error) = mount.finish().await {
                    report(error);
                }
            }
            Err(error) => report(error),
        }
    }
}


#[cfg(test)]
mod tests {
    //! The two-phase mount path: `mount_child` resolves a child to render and hands it to the
    //! executor; a pre-render fault is the future's `Err`; the top-level report is the last
    //! resort. Boundary behaviour (fault mailbox, suspense fallback) is pinned in
    //! `boundary.rs`.
    use crate::component::{mount_child, report_to_log, spawn_live};
    use crate::runtime::fresh_child_id;
    use crate::{component, live_view, Ctx, InboxSender, MockDriver, Result, Runtime, Setup};
    use std::cell::RefCell;
    use std::rc::Rc;

    type SenderSlot = Rc<RefCell<Option<InboxSender<LeafMsg>>>>;
    type GuardSlot = Rc<RefCell<Option<crate::runtime::MountGuard>>>;

    #[derive(Debug, Clone)]
    enum LeafMsg {
        Bump,
    }

    #[component]
    async fn Leaf(ctx: Ctx<Setup, LeafMsg>, sender_slot: SenderSlot) -> Result {
        let n = ctx.mutable_signal(0i64);
        let count = n.read();
        let mut ctx = ctx.render(live_view! { span { $count } }).await?;
        *sender_slot.borrow_mut() = Some(ctx.inbox_sender());
        loop {
            let (LeafMsg::Bump, turn) = ctx.recv().await?;
            n.update(&turn, |v| *v += 1);
        }
    }

    /// `mount_child` resolves a real leaf to render (its view stashed until the parent's anchor
    /// lands), hands its live future to the executor, and returns a cancel-guard. The leaf then
    /// drives from the executor (a message patches its DOM), and dropping the guard reaps it.
    #[test]
    fn mount_child_resolves_drives_and_unmounts() {
        #[derive(Debug)]
        enum PMsg {}

        async fn parent(ctx: Ctx<Setup, PMsg>, sender_slot: SenderSlot, guard_slot: GuardSlot) -> Result {
            let cid = fresh_child_id();
            let mut ctx = ctx
                .render(move |scope| {
                    let frame = scope.frame().clone();
                    async move {
                        let guard = mount_child::<Leaf>(
                            &frame,
                            LeafRequired { sender_slot },
                            LeafOptional::default(),
                            cid,
                        )
                        .await?;
                        *guard_slot.borrow_mut() = Some(guard); // held outside so the test can drop it
                        Ok(crate::LiveView::child_slot(cid))
                    }
                })
                .await?;
            loop {
                let _ = ctx.recv().await?;
            }
        }

        let sender_slot: SenderSlot = Rc::new(RefCell::new(None));
        let guard_slot: GuardSlot = Rc::new(RefCell::new(None));
        let (s, g) = (Rc::clone(&sender_slot), Rc::clone(&guard_slot));

        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx = rt.ctx::<PMsg>();
        rt.spawn(spawn_live(move |ctx| parent(ctx, s, g), ctx, report_to_log));
        rt.run_to_quiescence();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        assert_eq!(driver.set_texts(), vec!["0"], "the leaf resolved and spliced at the anchor");

        sender_slot.borrow().as_ref().unwrap().send(LeafMsg::Bump);
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        assert_eq!(driver.set_texts(), vec!["0", "1"], "the leaf drives from the executor");

        let before = rt.live_task_count();
        guard_slot.borrow_mut().take(); // drop the guard → unmount
        rt.run_once();
        assert_eq!(rt.live_task_count(), before - 1, "dropping the guard reaps the leaf task");
    }

    /// A view-embedded child mounted through the macro renders at its anchor — the ordinary
    /// `render(live_view! { … Component … })` path, awaited to resolve the whole initial tree.
    #[test]
    fn a_macro_embedded_child_renders() {
        #[derive(Debug)]
        enum PMsg {}

        #[component]
        async fn Inner(ctx: Ctx<Setup, crate::Never>) -> Result {
            Ok(ctx.render(live_view! { span { ("inner") } }).await?)
        }

        async fn parent(ctx: Ctx<Setup, PMsg>) -> Result {
            let mut ctx = ctx.render(live_view! { div { Inner } }).await?;
            loop {
                let _ = ctx.recv().await?;
            }
        }

        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx = rt.ctx::<PMsg>();
        rt.spawn(spawn_live(parent, ctx, report_to_log));
        rt.run_to_quiescence();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        assert_eq!(driver.set_texts(), vec!["inner"]);
    }

    /// A child that fails **before** rendering resolves `mount_child`'s `Err`, which the parent's
    /// `render` propagates as its own render failure — up to the top-level report here.
    #[test]
    fn a_pre_render_fault_propagates_to_the_report() {
        #[derive(Debug)]
        enum PMsg {}

        #[component]
        async fn Failing(_ctx: Ctx<Setup, crate::Never>) -> Result {
            Err("boom".into())
        }

        async fn parent(ctx: Ctx<Setup, PMsg>) -> Result {
            let mut ctx = ctx.render(live_view! { div { Failing } }).await?;
            loop {
                let _ = ctx.recv().await?;
            }
        }

        let reported = Rc::new(RefCell::new(None));
        let sink = Rc::clone(&reported);
        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx = rt.ctx::<PMsg>();
        rt.spawn(spawn_live(parent, ctx, move |error| {
            *sink.borrow_mut() = Some(error.to_string());
        }));
        rt.run_to_quiescence();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        assert_eq!(reported.borrow().as_deref(), Some("boom"));
    }

    /// A component that fails during **setup** reports inside its spawner's first `run_once`, so
    /// the mount can still refuse through its typed `err` arm instead of returning a mount that
    /// silently isn't there.
    #[test]
    fn a_setup_failure_reports_within_the_spawners_first_run() {
        async fn fails_at_setup(_ctx: Ctx<Setup, ()>) -> Result {
            Err("setup-boom".into())
        }

        let reported: Rc<RefCell<Option<String>>> = Default::default();
        let sink = Rc::clone(&reported);
        let mut rt = Runtime::new();
        rt.spawn(spawn_live(fails_at_setup, rt.ctx(), move |error| {
            *sink.borrow_mut() = Some(error.to_string());
        }));
        rt.run_to_quiescence();
        assert_eq!(reported.borrow().as_deref(), Some("setup-boom"));
    }

    /// A render-once component **returns** its mount, but its fault line stays live: the
    /// parked `finish` drives its inbox, so a fault raised after the return (here, a store
    /// absorb the owner cannot apply) still reaches the report. `Ok(ctx.render(v).await?)`
    /// and `…finish().await` are equivalent in fault behavior, not just in shape.
    #[test]
    fn a_returned_mounts_fault_line_stays_live() {
        type SinkSlot = Rc<RefCell<Option<crate::SeedSink>>>;

        async fn render_once(ctx: Ctx<Setup, crate::Never>, slot: SinkSlot) -> Result {
            let sink = ctx.absorber(|_turn: &crate::Turn, _bytes: &[u8]| {
                Err::<(), String>("absorb-boom".to_string())
            });
            *slot.borrow_mut() = Some(sink);
            Ok(ctx.render(live_view! { span { ("static") } }).await?)
        }

        let slot: SinkSlot = Default::default();
        let reported: Rc<RefCell<Option<String>>> = Default::default();
        let (s, sink_report) = (Rc::clone(&slot), Rc::clone(&reported));
        let mut driver = MockDriver::new();
        let mut rt = Runtime::new();
        rt.spawn(spawn_live(move |ctx| render_once(ctx, s), rt.ctx(), move |error| {
            *sink_report.borrow_mut() = Some(error.to_string());
        }));
        rt.run_to_quiescence();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        assert_eq!(*reported.borrow(), None, "it rendered and returned its mount");

        let sink = slot.borrow().as_ref().unwrap().clone();
        (sink.0)(b"junk");
        rt.run_to_quiescence();
        assert_eq!(reported.borrow().as_deref(), Some("absorb-boom"));
    }

    /// A component that renders and *then* fails reports once the loop runs on — past the point
    /// a mount could refuse, but the report still arrives.
    #[test]
    fn a_post_render_failure_reports_after_the_mount_is_live() {
        #[derive(Debug)]
        enum Msg {
            Go,
        }
        async fn fails_after_render(ctx: Ctx<Setup, Msg>) -> Result {
            let mut ctx = ctx.render(live_view! { span { ("live") } }).await?;
            let _ = ctx.recv().await?;
            Err("late-boom".into())
        }

        let reported: Rc<RefCell<Option<String>>> = Default::default();
        let sink = Rc::clone(&reported);
        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
        let sender = ctx.inbox_sender();
        rt.spawn(spawn_live(fails_after_render, ctx, move |error| {
            *sink.borrow_mut() = Some(error.to_string());
        }));
        rt.run_to_quiescence();
        rt.process_pending_view(&mut driver);
        assert_eq!(*reported.borrow(), None, "it rendered: the mount is live");
        assert_eq!(driver.set_texts(), vec!["live"]);

        sender.send(Msg::Go);
        rt.run_to_quiescence();
        assert_eq!(reported.borrow().as_deref(), Some("late-boom"));
    }
}
