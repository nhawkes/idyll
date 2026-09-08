//! The two-phase component lifecycle.
//!
//! A component is a future in two phases. **Resolve** is concrete and owned by the parent: the
//! future is polled to its *render point* — it may suspend awaiting the shared store, then it
//! renders (posting its view at its anchor) and parks at `recv`. **Hand-off** moves that
//! still-live future to the executor, where it coerces to `dyn` and lives out its message loop.
//!
//! [`run_to_render`] is the seam. The parent `await`s it while resolving; a `Live` result is
//! handed to the executor (the parent keeps a cancel-guard), a `Rendered` result is a
//! render-once component that completed at its render point, and an `Unrendered` result never
//! rendered — a pre-render fault, the parent's own render failure. The concrete→`dyn`
//! transition happens only at the hand-off — so the resolve call graph the island splitter
//! reads stays fully concrete.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

/// The **render witness**: the child's render sink [`mark`](RenderWitness::mark)s it the instant
/// the child posts its first view — exactly once, since the `Setup`→`Live` typestate makes render
/// single-shot. [`run_to_render`] *takes* it (`take`-once), and the take is the phase transition:
/// a marked witness **is** the proof the child rendered. The view itself flows to the DOM through
/// the child's own sink (stashed until its anchor lands), so the witness carries no payload — it
/// only reports *that* render happened. Rendering is a side effect inside the opaque component
/// future, so this runtime cell is irreducible, but it is encapsulated here and paired with the
/// future in [`Rendering`]; the never-marked case is just the suspend the phase API hides.
#[derive(Clone, Default)]
pub struct RenderWitness(Rc<Cell<bool>>);

impl RenderWitness {
    pub fn new() -> Self {
        RenderWitness::default()
    }

    /// The render sink calls this when the child posts its view.
    pub fn mark(&self) {
        self.0.set(true);
    }

    /// Take the render proof: `true` once, then `false` — the transition is spent on the take.
    fn take(&self) -> bool {
        self.0.replace(false)
    }
}

/// A component's **concrete** future paired with the [`RenderWitness`] its render sink marks — the
/// resolve phase as one value. In production both come from one place: `Ctx::resolving` mints the
/// witness beside the sink that marks it and the future is built from that same ctx, so the pair
/// handed here cannot be mis-wired. (Tests construct a future and witness directly to drive
/// `run_to_render` in isolation.)
pub struct Rendering<F> {
    future: Pin<Box<F>>,
    witness: RenderWitness,
}

impl<F: Future> Rendering<F> {
    pub fn new(future: F, witness: RenderWitness) -> Self {
        Rendering { future: Box::pin(future), witness }
    }
}

/// The outcome of resolving a component to its render point — three honest states.
pub enum Resolved<F: Future> {
    /// It rendered and parked at `recv` — its still-live future, to hand to the executor. The
    /// parent keeps a cancel-guard; dropping it cancels this future (and cascades).
    Live(Pin<Box<F>>),
    /// It rendered, then completed — a render-once (`M = Never`) component, or one that failed
    /// *after* rendering (a post-render fault). Its output is carried so the caller keeps it.
    Rendered(F::Output),
    /// It completed **without** rendering — a pre-render fault, which is the parent's own render
    /// failure (there is no view, and nothing to hand off).
    Unrendered(F::Output),
}

/// Poll a [`Rendering`] to its render point — the resolve half of the lifecycle:
///
/// - `Live(fut)` — `Pending` after the witness was marked: it parked at `recv`.
/// - `Rendered(out)` — the future completed and the witness was marked: it rendered first.
/// - `Unrendered(out)` — the future completed with the witness unmarked: a pre-render fault.
/// - `Pending` — `Pending` with the witness unmarked: suspended awaiting data (the nearest
///   suspense boundary shows its fallback).
///
/// Polled with the parent's waker, so when a suspended child's data lands the parent re-polls
/// down to here and the child advances to render.
pub fn run_to_render<F: Future>(rendering: Rendering<F>) -> RunToRender<F> {
    RunToRender { fut: Some(rendering.future), witness: rendering.witness }
}

pub struct RunToRender<F> {
    fut: Option<Pin<Box<F>>>,
    witness: RenderWitness,
}

impl<F: Future> Future for RunToRender<F> {
    type Output = Resolved<F>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `RunToRender` owns the boxed future by value; it is `Unpin`, so a plain `&mut` is
        // sound (the boxed future is what's pinned, and it never moves out until handed off).
        let this = self.get_mut();
        let fut = this.fut.as_mut().expect("run_to_render polled after it resolved");
        match fut.as_mut().poll(cx) {
            Poll::Ready(output) => {
                this.fut = None; // the future completed — drop it, keep only its output
                if this.witness.take() {
                    Poll::Ready(Resolved::Rendered(output))
                } else {
                    Poll::Ready(Resolved::Unrendered(output))
                }
            }
            // Taking the marked witness is the transition to the live phase.
            Poll::Pending if this.witness.take() => {
                Poll::Ready(Resolved::Live(this.fut.take().expect("present until taken")))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<F> Unpin for RunToRender<F> {}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    use super::{run_to_render, RenderWitness, Rendering, Resolved};

    /// A toy component: it suspends for `suspend_polls` polls (awaiting "data", re-waking each
    /// time), then renders (marks the witness), then either parks forever (`live`) or returns.
    struct Toy {
        witness: RenderWitness,
        suspend_polls: u32,
        polled: u32,
        live: bool,
        log: Rc<RefCell<Vec<String>>>,
        name: &'static str,
    }

    impl Future for Toy {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            self.polled += 1;
            if self.polled <= self.suspend_polls {
                cx.waker().wake_by_ref(); // "data still resolving" — ask to be re-polled
                return Poll::Pending; // suspended: not yet rendered
            }
            self.witness.mark(); // render: post the view (the sink marks the witness)
            self.log.borrow_mut().push(format!("{}:render", self.name));
            if self.live {
                Poll::Pending // parked at recv
            } else {
                Poll::Ready(()) // render-once: completed at render
            }
        }
    }

    impl Drop for Toy {
        fn drop(&mut self) {
            self.log.borrow_mut().push(format!("{}:drop", self.name));
        }
    }

    struct NoopWake;
    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
        fn wake_by_ref(self: &Arc<Self>) {}
    }

    fn drive<F: Future>(mut fut: Pin<&mut F>, max: u32) -> Option<F::Output> {
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        for _ in 0..max {
            if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
                return Some(out);
            }
        }
        None
    }

    #[test]
    fn resolves_to_live_only_after_it_renders() {
        let witness = RenderWitness::new();
        let log = Rc::new(RefCell::new(Vec::new()));
        let toy = Toy {
            witness: witness.clone(),
            suspend_polls: 2,
            polled: 0,
            live: true,
            log: Rc::clone(&log),
            name: "child",
        };
        let mut rtr = run_to_render(Rendering::new(toy, witness));
        match drive(Pin::new(&mut rtr), 10).expect("resolves within budget") {
            Resolved::Live(fut) => {
                assert_eq!(*log.borrow(), ["child:render"], "rendered exactly once, still live");
                // The still-live future is what the executor would drive; dropping it here
                // stands in for cancel — it tears down.
                drop(fut);
                assert_eq!(*log.borrow(), ["child:render", "child:drop"]);
            }
            _ => panic!("a parked live component resolves to Live"),
        }
    }

    #[test]
    fn a_render_once_component_resolves_to_rendered() {
        let witness = RenderWitness::new();
        let log = Rc::new(RefCell::new(Vec::new()));
        let toy = Toy {
            witness: witness.clone(),
            suspend_polls: 0,
            polled: 0,
            live: false, // completes at render
            log: Rc::clone(&log),
            name: "leaf",
        };
        let mut rtr = run_to_render(Rendering::new(toy, witness));
        match drive(Pin::new(&mut rtr), 10).expect("resolves") {
            Resolved::Rendered(()) => assert_eq!(*log.borrow(), ["leaf:render", "leaf:drop"]),
            Resolved::Unrendered(()) => panic!("it rendered before completing"),
            Resolved::Live(_) => panic!("a render-once component resolves to Rendered"),
        }
    }

    #[test]
    fn a_pre_render_completion_resolves_to_unrendered() {
        // A component that completes without ever marking its witness (a pre-render fault) is
        // `Unrendered` — the parent's own render failure.
        struct Boom;
        impl Future for Boom {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
                Poll::Ready(())
            }
        }
        let mut rtr = run_to_render(Rendering::new(Boom, RenderWitness::new()));
        match drive(Pin::new(&mut rtr), 10).expect("resolves") {
            Resolved::Unrendered(()) => {}
            _ => panic!("completing before render is Unrendered"),
        }
    }

    #[test]
    fn stays_suspended_until_it_renders() {
        // A component that never reaches its render point stays Pending — never yields Live.
        let witness = RenderWitness::new();
        let toy = Toy {
            witness: witness.clone(),
            suspend_polls: 100,
            polled: 0,
            live: true,
            log: Rc::new(RefCell::new(Vec::new())),
            name: "slow",
        };
        let mut rtr = run_to_render(Rendering::new(toy, witness));
        assert!(drive(Pin::new(&mut rtr), 5).is_none(), "still suspended, not rendered");
    }

    /// The cancel-guard tree: a parent holds its children's guards, then its own cleanup.
    /// Dropping the parent's guard cancels the subtree **child-before-parent** — which falls
    /// out of Rust field-drop order *only if* the guard has no `Drop` body (a body runs before
    /// fields, inverting it) and holds children in a field declared before its cleanup. This
    /// pins that layout so a future `impl Drop` here can't silently flip the unmount order.
    #[test]
    fn dropping_a_guard_cascades_child_before_parent() {
        struct OnDrop {
            name: &'static str,
            log: Rc<RefCell<Vec<String>>>,
        }
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.log.borrow_mut().push(self.name.to_string());
            }
        }
        // No `Drop` impl on Guard: children drop first (field order), then `cleanup`. The fields
        // exist only for that drop order — never read — which is exactly what this test pins.
        #[allow(dead_code)]
        struct Guard {
            children: Vec<Guard>,
            cleanup: OnDrop,
        }
        let log = Rc::new(RefCell::new(Vec::new()));
        let guard = |name, children| Guard {
            children,
            cleanup: OnDrop { name, log: Rc::clone(&log) },
        };
        let root = guard("root", vec![guard("mid", vec![guard("leaf", vec![])])]);
        drop(root);
        assert_eq!(*log.borrow(), ["leaf", "mid", "root"], "unmount runs child before parent");
    }
}
