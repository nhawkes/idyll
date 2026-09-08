//! **Boundaries** — where a subtree's failure or suspension stops being the subtree's
//! problem. A boundary is an ordinary component that arranges the two-phase lifecycle of
//! its child:
//!
//! - **Suspense is future-pending.** A component that has not posted its first view yet is
//!   pending. A [`suspense_boundary`] renders its fallback immediately and drives its child to
//!   its render point off to the side; when the whole subtree has rendered, it swaps the
//!   fallback for the child. (A suspension *after* first render just delays the child's own
//!   updates — nothing rolls up, and the boundary has already swapped.)
//! - **A fault is a value.** Before its child renders, an error resolves the child's future
//!   `Err`; after it renders, the child's live loop fires a message. Either way an
//!   [`error_boundary`] catches it through its **mailbox** — it installs a
//!   [`fault_sink`](crate::Ctx::fault_sink) so a descendant fault routes to *this* inbox as a
//!   message, which the reducer turns into a rendered fallback.
//!
//! They compose by installing different context entries. A suspense boundary is transparent to
//! faults: they route past it to the nearest error boundary's sink. The other direction is
//! **directional by design**: an error boundary drives its child fire-and-forget, outside the
//! witnessed mount chain, so a pending descendant does NOT roll up through it to an enclosing
//! suspense boundary — place the suspense boundary at or *below* the error boundary (wrapping
//! the same child), where the pending subtree is its own witnessed chain.
//!
//! Fallbacks are **recipes** — `live_view!` products, passed straight in. Per the stratified
//! model live code never authors content; an inert fallback is a live view that happens to
//! bind nothing. Both boundaries mount the fallback as an ordinary view-embedded child
//! ([`Fallback`]), so its lifetime is a guard's — no reactive gate. Fallback recipes are
//! `Never`-typed: an interactive fallback (a retry button) is a messaged-boundary design,
//! built when something needs one.

use std::cell::RefCell;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use crate::component::Component;
use crate::{Ctx, LiveView, Result, Setup};

/// The one message an [`error_boundary`] handles: a descendant fault, delivered to its mailbox.
#[derive(Debug)]
pub enum Faulted {
    Fault(Rc<dyn Error>),
}

type FallbackFut =
    Pin<Box<dyn Future<Output = std::result::Result<LiveView<crate::Never>, crate::Fault>>>>;
/// A fallback's render closure, boxed to ride as a component prop — what a
/// `live_view! { … }` product erases to.
type FallbackRecipe = Box<dyn FnOnce(crate::ctx::RenderScope<crate::Never>) -> FallbackFut>;

/// What a [`suspense_boundary`] accepts as its fallback: a `live_view! { … }` product,
/// passed straight in. The blanket impl erases the concrete closure so the boundary
/// keeps its one generic (`C`).
pub trait IntoFallback {
    fn into_recipe(self) -> FallbackRecipe;
}

impl<F, Fut> IntoFallback for F
where
    F: FnOnce(crate::ctx::RenderScope<crate::Never>) -> Fut + 'static,
    Fut: Future<Output = std::result::Result<LiveView<crate::Never>, crate::Fault>> + 'static,
{
    fn into_recipe(self) -> FallbackRecipe {
        Box::new(move |scope| Box::pin(self(scope)) as FallbackFut)
    }
}

/// What an [`error_boundary`] accepts as its fallback: a closure from the caught fault
/// to a `live_view! { … }` product — `|error| live_view! { span { (error) } }`.
pub trait IntoErrorFallback {
    fn build(&self, error: Rc<dyn Error>) -> FallbackRecipe;
}

impl<F, R> IntoErrorFallback for F
where
    F: Fn(Rc<dyn Error>) -> R,
    R: IntoFallback,
{
    fn build(&self, error: Rc<dyn Error>) -> FallbackRecipe {
        self(error).into_recipe()
    }
}

/// A trivial recipe mount: renders the fallback and holds it. Both boundaries mount their
/// fallback through this, so the fallback is an ordinary view-embedded child — its
/// cancel-guard removes it when dropped, which is the whole swap: no reactive gate, just a
/// guard's lifetime.
#[idyll::component]
async fn Fallback(ctx: Ctx<Setup, crate::Never>, recipe: FallbackRecipe) -> Result {
    Ok(ctx.render(|scope| recipe(scope)).await?)
}

/// A React-style **error boundary**: mounts `child` fire-and-forget and, when a fault arrives
/// from anywhere below, **unmounts the failed subtree** and mounts `fallback` in its place —
/// never dead-but-clickable UI under an overlay. It installs its own inbox as the subtree's
/// [`fault_sink`](crate::Ctx::fault_sink), so a child's fault — pre-render (its future's
/// `Err`) or post-render (its live loop's `Err`, e.g. a mutation's broken promise) — routes
/// here as a message. The first fault wins; suspensions are *not* caught (a pending descendant
/// is simply not shown until it renders).
pub async fn error_boundary<C: Component>(
    ctx: Ctx<Setup, Faulted>,
    required: C::Required,
    optional: C::Optional,
    fallback: impl IntoErrorFallback + 'static,
) -> Result {
    let frame = ctx.context_handle();
    // The fallback's faults must route PAST this boundary (a fallback failing for
    // the child's reason must not vanish into the reducer's first-fault-won arm), so
    // its frame is captured from the enclosing route *before* the sink installs.
    let fallback_frame =
        crate::ctx::FaultRoute::from_frame(&frame.0).shadowing_frame(&frame.0);
    // Route the subtree's faults to this inbox. Every descendant mounted under this frame finds
    // this sink (until a nearer error boundary shadows it).
    ctx.fault_sink(Faulted::Fault);
    let child_id = crate::runtime::fresh_child_id();
    let fallback_id = crate::runtime::fresh_child_id();
    // The child's cancel-guard is held where the reducer can drop it: catching a fault
    // unmounts the failed subtree — its DOM, listeners, and cells reclaim through the guard
    // cascade. The boundary's own unmount still reaps the child: this future owns the slot.
    let child_guard: Rc<RefCell<Option<crate::MountGuard>>> = Rc::new(RefCell::new(None));
    let guard_slot = Rc::clone(&child_guard);
    let mut ctx = ctx
        .render(move |scope| async move {
            let guard = crate::spawn_child::<C>(scope.frame(), required, optional, child_id);
            *guard_slot.borrow_mut() = Some(guard);
            Ok(LiveView::child_slot(child_id).append(LiveView::child_slot(fallback_id)))
        })
        .await?;
    let mut fallback_guard = None;
    loop {
        let (Faulted::Fault(fault), _turn) = ctx.recv().await?;
        if fallback_guard.is_some() {
            continue; // the first fault won; the subtree is already down
        }
        child_guard.borrow_mut().take();
        fallback_guard = Some(crate::spawn_child::<Fallback>(
            &fallback_frame,
            FallbackRequired { recipe: fallback.build(fault) },
            FallbackOptional::default(),
            fallback_id,
        ));
    }
}

/// A React-style **suspense boundary**: renders `fallback` immediately, drives `child` to its
/// render point off to the side, and swaps the fallback for the child once the whole subtree has
/// rendered.
///
/// The swap is a one-shot continuation. The fallback is a recipe mounted through [`Fallback`],
/// held by its cancel-guard; the child is driven by `mount_child`, whose resolving means "the
/// whole subtree has rendered, its view already spliced at `child_id`." At that instant the
/// fallback guard is dropped and the fallback vanishes. There is no `ready` signal and no
/// reducer loop: the transition happens once, at a point the resolve machinery already
/// computes, so it is a `await`-then-`drop`, not a gate.
///
/// Faults are *not* caught here; a pre-render fault clears the fallback (there is no child to
/// reveal) and routes past to the nearest error boundary.
pub async fn suspense_boundary<C: Component>(
    ctx: Ctx<Setup, crate::Never>,
    required: C::Required,
    optional: C::Optional,
    fallback: impl IntoFallback + 'static,
) -> Result {
    let fallback_id = crate::runtime::fresh_child_id();
    let child_id = crate::runtime::fresh_child_id();
    let recipe = fallback.into_recipe();
    ctx.render(move |scope| {
        let frame = scope.frame().clone();
        async move {
            // Mount the fallback now, as an ordinary guarded child; hand its guard to the resolve
            // task. When `mount_child` resolves (child's whole subtree rendered, its view already
            // spliced at `child_id`), drop the fallback guard — the one-shot swap. A pre-render
            // fault has no child to reveal: clear the fallback and route past.
            let fallback_guard = crate::spawn_child::<Fallback>(
                &frame,
                FallbackRequired { recipe },
                FallbackOptional::default(),
                fallback_id,
            );
            let child_task = frame.runtime().clone().spawn_pending(async move {
                let resolved = crate::mount_child::<C>(&frame, required, optional, child_id).await;
                // The child has resolved — rendered, or faulted pre-render — so the fallback's job
                // is done either way; drop it. Then hold the mount (or route the fault) and park.
                drop(fallback_guard);
                match resolved {
                    Ok(child) => {
                        let _keep = child;
                    }
                    Err(fault) => crate::ctx::FaultRoute::from_frame(&frame).route(fault),
                }
                std::future::pending::<()>().await
            });
            Ok(LiveView::child_slot(fallback_id)
                .append(LiveView::child_slot(child_id))
                .placement_guard(child_task))
        }
    })
    .await?
    .finish()
    .await
}

#[cfg(test)]
mod tests {
    //! Nesting is the property worth pinning: the failure/suspension is *two* levels
    //! down, and the boundary at the top still catches it.
    use crate::{
        component, component::{report_to_log, spawn_live}, driver::DomOp, live_view, Ctx,
        MockDriver, Never, Runtime, Setup,
    };

    /// Every text the driver saw: `SetText` writes (live splices) plus template `Text`
    /// nodes (content — `view!` fallbacks carry their text in the template itself).
    fn all_texts(driver: &MockDriver) -> Vec<String> {
        let mut texts: Vec<String> = driver
            .log
            .iter()
            .filter_map(|op| match op {
                DomOp::SetText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        for template in &driver.templates {
            for node in template.nodes.iter() {
                if let crate::template::TplNode::Text(text) = node {
                    texts.push(text.to_string());
                }
            }
        }
        texts
    }

    /// A suspension the test controls: it stays `Pending` (parking its waker) until `open` is
    /// called, so a child can be held mid-resolve across `run_to_quiescence` and released on cue —
    /// unlike a self-waking yield, which resolves within one drive and can't hold a pending phase.
    #[derive(Clone, Default)]
    struct Gate(std::rc::Rc<std::cell::RefCell<(bool, Option<std::task::Waker>)>>);

    impl Gate {
        fn open(&self) {
            let mut inner = self.0.borrow_mut();
            inner.0 = true;
            if let Some(waker) = inner.1.take() {
                waker.wake();
            }
        }
    }

    impl std::future::Future for Gate {
        type Output = ();
        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            let mut inner = self.0.borrow_mut();
            if inner.0 {
                std::task::Poll::Ready(())
            } else {
                inner.1 = Some(cx.waker().clone());
                std::task::Poll::Pending
            }
        }
    }

    #[test]
    fn suspense_boundary_catches_a_suspension_through_a_view_embedded_child() {
        // The suspension is two levels down, reached through the *macro* (`div { Slow }` →
        // mount_child), not a hand-built spawn — the boundary drives the whole subtree to render.
        // The `Gate` holds the descendant mid-resolve so the pending phase is real, not a
        // one-drive blip.
        #[component]
        async fn Slow(ctx: Ctx<Setup, idyll::Never>, gate: Gate) -> Result {
            gate.await;
            ctx.render(live_view! { span { ("ready") } }).await?.finish().await
        }
        #[component]
        async fn Middle(ctx: Ctx<Setup, idyll::Never>, gate: Gate) -> Result {
            ctx.render(live_view! { div { Slow gate=(gate) } }).await?.finish().await
        }

        let gate = Gate::default();
        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx: Ctx<Setup, Never> = rt.ctx::<Never>();
        rt.spawn(spawn_live(
            {
                let gate = gate.clone();
                |ctx| {
                    super::suspense_boundary::<Middle>(
                        ctx,
                        MiddleRequired { gate },
                        MiddleOptional::default(),
                        live_view! { span { "loading" } },
                    )
                }
            },
            ctx,
            report_to_log,
        ));
        // The child is gated: even driven to quiescence it stays suspended, so this is a genuine
        // pending phase — fallback up, nothing torn down.
        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        assert!(
            all_texts(&driver).contains(&"loading".to_string()),
            "the boundary must show its fallback while a descendant is pending: {:?}",
            all_texts(&driver)
        );
        let removes_while_pending =
            driver.log.iter().filter(|op| matches!(op, DomOp::RemoveFragment { .. })).count();
        assert_eq!(removes_while_pending, 0, "the fallback stays mounted while the child is pending");

        // Release the descendant: its whole subtree renders, and the fallback is torn down.
        gate.open();
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        assert!(
            all_texts(&driver).contains(&"ready".to_string()),
            "the fallback must clear once the view-embedded descendant renders: {:?}",
            all_texts(&driver)
        );
        // The one-shot swap: when the child's subtree renders, the fallback's DOM is *torn down* —
        // not hidden behind a still-live overlay. That teardown is the design's whole claim.
        let removes_after_swap =
            driver.log.iter().filter(|op| matches!(op, DomOp::RemoveFragment { .. })).count();
        assert!(
            removes_after_swap > removes_while_pending,
            "the fallback's fragment must be removed once the child takes over: {:?}",
            driver.log
        );
    }

    #[test]
    fn error_boundary_catches_a_pre_render_failure_below_an_inline_child() {
        // Mounting a component in a view is how apps compose, so a boundary must catch a failure
        // that arrives through one. The failure is two levels down: the boundary's child mounts
        // `Failing` inline, which errors before it renders.
        #[component]
        async fn Failing(_ctx: Ctx<Setup, idyll::Never>) -> Result {
            Err("inline-kaboom".into())
        }
        #[component]
        async fn Middle(ctx: Ctx<Setup, idyll::Never>) -> Result {
            ctx.render(live_view! { div { Failing } }).await?.finish().await
        }

        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx: Ctx<Setup, super::Faulted> = rt.ctx::<super::Faulted>();
        rt.spawn(spawn_live(
            |ctx| {
                super::error_boundary::<Middle>(
                    ctx,
                    MiddleRequired {},
                    MiddleOptional::default(),
                    |error: std::rc::Rc<dyn std::error::Error>| live_view! { span { (error) } },
                )
            },
            ctx,
            report_to_log,
        ));
        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);

        assert!(
            all_texts(&driver).contains(&"inline-kaboom".to_string()),
            "a view-mounted child's failure must route to the boundary: {:?}",
            all_texts(&driver)
        );
    }

    /// A fallback that faults must route PAST its own boundary — the boundary is
    /// already showing it, so the fault belongs to the next boundary up (React's
    /// semantics), never to the first-fault-won arm of the boundary's own reducer,
    /// where it would vanish without a trace.
    #[test]
    fn a_faulting_fallback_reaches_the_outer_boundary() {
        #[component]
        async fn Failing(_ctx: Ctx<Setup, idyll::Never>) -> Result {
            Err("child-kaboom".into())
        }
        #[component]
        async fn FragileFallbackChild(_ctx: Ctx<Setup, idyll::Never>) -> Result {
            Err("fallback-kaboom".into())
        }

        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx: Ctx<Setup, super::Faulted> = rt.ctx::<super::Faulted>();
        rt.spawn(spawn_live(
            |ctx| {
                // Outer boundary around an inner one whose fallback itself faults:
                // the inner catches the child, mounts its fallback, the fallback
                // faults — and that second fault must land HERE.
                super::error_boundary::<InnerBoundary>(
                    ctx,
                    InnerBoundaryRequired {},
                    InnerBoundaryOptional::default(),
                    |error: std::rc::Rc<dyn std::error::Error>| live_view! { span { (error) } },
                )
            },
            ctx,
            report_to_log,
        ));

        #[component]
        async fn InnerBoundary(ctx: Ctx<Setup, super::Faulted>) -> Result {
            super::error_boundary::<Failing>(
                ctx,
                FailingRequired {},
                FailingOptional::default(),
                |_error| live_view! { div { FragileFallbackChild } },
            )
            .await
        }

        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);

        assert!(
            all_texts(&driver).iter().any(|text| text.contains("fallback-kaboom")),
            "the fallback's own fault must surface at the outer boundary: {:?}",
            all_texts(&driver)
        );
    }

    /// The whole claim, pinned at the **folded document**: a child that faults after
    /// rendering is unmounted — its DOM gone, not overlaid — and the fallback stands
    /// in its place. `all_texts`-style assertions can't see this (a registered
    /// template counts even if it never lands in the document), so this folds the
    /// real command stream.
    #[test]
    fn error_boundary_unmounts_the_failed_subtree_and_shows_the_fallback() {
        use std::cell::RefCell;
        use std::rc::Rc;

        #[derive(Debug)]
        enum Msg {
            Boom,
        }
        type SenderSlot = Rc<RefCell<Option<crate::InboxSender<Msg>>>>;

        #[component]
        async fn Fragile(ctx: Ctx<Setup, Msg>, sender_slot: SenderSlot) -> Result {
            let mut ctx = ctx.render(live_view! { p { ("fragile content") } }).await?;
            *sender_slot.borrow_mut() = Some(ctx.inbox_sender());
            let (Msg::Boom, _turn) = ctx.recv().await?;
            Err("late-kaboom".into())
        }

        let sender_slot: SenderSlot = Default::default();
        let slot = Rc::clone(&sender_slot);
        let mut rt = Runtime::new();
        let mut driver = crate::CommandBufferDriver::new();
        let ctx: Ctx<Setup, super::Faulted> = rt.ctx::<super::Faulted>();
        rt.spawn(spawn_live(
            move |ctx| {
                super::error_boundary::<Fragile>(
                    ctx,
                    FragileRequired { sender_slot: slot },
                    FragileOptional::default(),
                    |_error| live_view! { span { ("fallback shown") } },
                )
            },
            ctx,
            report_to_log,
        ));
        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);

        let mut fold = crate::HtmlFold::new();
        for command in driver.commands() {
            fold.apply(command);
        }
        let html = fold.html();
        assert!(html.contains("fragile content"), "the child rendered: {html}");
        assert!(!html.contains("fallback shown"), "no fault yet: {html}");

        sender_slot.borrow().as_ref().unwrap().send(Msg::Boom);
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);

        let mut fold = crate::HtmlFold::new();
        for command in driver.commands() {
            fold.apply(command);
        }
        let html = fold.html();
        assert!(
            !html.contains("fragile content"),
            "the failed subtree must unmount, not linger under an overlay: {html}"
        );
        assert!(html.contains("fallback shown"), "the fallback takes its place: {html}");
    }

    /// The division of labour between the two boundaries, which nothing else pins: a
    /// suspense boundary holds its fallback up for a child that is *pending*, and does
    /// not catch one that faults — the fault carries on to the error boundary above.
    /// That passing-through is what the mutation of this test hangs on; the second
    /// assertion is the weaker reader-facing property (no loading state beside an
    /// error), which the outer boundary's own teardown is enough to satisfy.
    ///
    /// Folded, because a fallback that never landed still registers a template and
    /// would satisfy a text-level assertion.
    #[test]
    fn a_fault_under_a_suspense_boundary_takes_its_fallback_down_and_routes_past() {
        // Gated, so the fallback is genuinely standing before the fault arrives. A child
        // that fails on its first poll never lets one up, and then "it comes down" is a
        // claim about nothing.
        #[component]
        async fn FailsLate(_ctx: Ctx<Setup, idyll::Never>, gate: Gate) -> Result {
            gate.await;
            Err("kaboom".into())
        }
        #[component]
        async fn Suspended(ctx: Ctx<Setup, idyll::Never>, gate: Gate) -> Result {
            super::suspense_boundary::<FailsLate>(
                ctx,
                FailsLateRequired { gate },
                FailsLateOptional::default(),
                live_view! { span { ("still loading") } },
            )
            .await
        }

        let gate = Gate::default();
        let mut rt = Runtime::new();
        let mut driver = crate::CommandBufferDriver::new();
        let ctx: Ctx<Setup, super::Faulted> = rt.ctx::<super::Faulted>();
        rt.spawn(spawn_live(
            {
                let gate = gate.clone();
                |ctx| {
                    super::error_boundary::<Suspended>(
                        ctx,
                        SuspendedRequired { gate },
                        SuspendedOptional::default(),
                        |error: std::rc::Rc<dyn std::error::Error>| live_view! { p { (error) } },
                    )
                }
            },
            ctx,
            report_to_log,
        ));
        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);

        let folded = |driver: &crate::CommandBufferDriver| {
            let mut fold = crate::HtmlFold::new();
            for command in driver.commands() {
                fold.apply(command);
            }
            fold.html()
        };
        let pending = folded(&driver);
        assert!(
            pending.contains("still loading"),
            "the fallback must be standing before the fault, or the rest proves nothing: {pending}"
        );

        gate.open();
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);

        let html = folded(&driver);
        assert!(
            html.contains("kaboom"),
            "the fault must pass the suspense boundary and land at the error boundary: {html}"
        );
        assert!(
            !html.contains("still loading"),
            "a faulted child is not a pending one — the suspense fallback must come down: {html}"
        );
    }
}
