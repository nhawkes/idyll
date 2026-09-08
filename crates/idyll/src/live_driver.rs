//! A **concrete** cooperative executor for live root futures.
//!
//! The guest drives every live's reducer loop here instead of as a `Box<dyn Future>`
//! task, so a live's code is reached by a **direct** poll (`R::poll` is monomorphic)
//! rather than a vtable dispatch — the property the code-splitter needs. `R` is the
//! guest's own `enum IslandRoot`, one variant per live; its variants hold
//! `Pin<Box<ConcreteFut>>`, so the enum is `Unpin` (no unsafe projection) while each
//! poll still lands on a concrete future.
//!
//! This is the same tri-part model as the generic [`Runtime`](crate::runtime): a slot
//! per root, a shared ready queue, and a per-slot waker that re-enqueues the slot when
//! its inbox is woken. It holds no boxed `dyn Future` and never dispatches a poll
//! through a vtable.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Wake, Waker};

/// An opaque handle to a mounted root — what the guest stores to later unmount it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SlotId(u64);

struct Slot<R> {
    id: SlotId,
    /// The live's root future. `R: Unpin` (its variants are `Pin<Box<_>>`), so it
    /// lives directly in the `Vec` and polling is a safe `Pin::new(&mut root)`.
    root: R,
    /// Set false when the root completes (a message-less component that returned, or a
    /// failure). A done slot is dropped on the next drain.
    live: bool,
}

/// Wakes a slot: pushes its id into the shared ready queue. Mirrors the runtime's
/// `TaskWaker`; single-threaded, the `Mutex` never contends.
struct SlotWaker {
    id: SlotId,
    ready: Arc<Mutex<VecDeque<SlotId>>>,
}

impl Wake for SlotWaker {
    fn wake(self: Arc<Self>) {
        self.ready.lock().unwrap().push_back(self.id);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.ready.lock().unwrap().push_back(self.id);
    }
}

/// Drives a set of concrete live root futures. Generic over `R`, the guest's
/// `enum IslandRoot`.
pub struct LiveDriver<R> {
    slots: Vec<Slot<R>>,
    ready: Arc<Mutex<VecDeque<SlotId>>>,
    next_id: u64,
}

impl<R> Default for LiveDriver<R> {
    fn default() -> Self {
        LiveDriver { slots: Vec::new(), ready: Arc::new(Mutex::new(VecDeque::new())), next_id: 1 }
    }
}

impl<R: Future<Output = ()> + Unpin> LiveDriver<R> {
    pub fn new() -> Self {
        Self::default()
    }

    fn waker_for(&self, id: SlotId) -> Waker {
        Waker::from(Arc::new(SlotWaker { id, ready: Arc::clone(&self.ready) }))
    }

    /// Mount a root and poll it **once**, now — running its setup and initial render to
    /// the first `await` (its `ctx.render` posts the pending view for the caller to
    /// process). Returns the slot handle. A root that finishes on this first poll (a
    /// message-less component that returned, or a synchronous failure) is marked done.
    pub fn mount(&mut self, root: R) -> SlotId {
        let id = SlotId(self.next_id);
        self.next_id += 1;
        self.slots.push(Slot { id, root, live: true });
        self.poll_slot(id);
        id
    }

    /// Poll one slot once. No-op for an unknown or done slot.
    fn poll_slot(&mut self, id: SlotId) {
        let waker = self.waker_for(id);
        let Some(slot) = self.slots.iter_mut().find(|s| s.id == id) else { return };
        if !slot.live {
            return;
        }
        let mut cx = Context::from_waker(&waker);
        if Pin::new(&mut slot.root).poll(&mut cx).is_ready() {
            slot.live = false;
        }
    }

    /// Poll every woken slot until the ready queue drains — the live half of
    /// `run_to_quiescence`. Reactive effects the polls triggered are drained separately
    /// by the flush; this only advances the reducer futures.
    pub fn drain(&mut self) {
        loop {
            // Pop before polling — holding the lock across `poll_slot` (which borrows
            // `self` and can itself enqueue) would deadlock/alias.
            let next = self.ready.lock().unwrap().pop_front();
            match next {
                Some(id) => self.poll_slot(id),
                None => break,
            }
        }
        self.slots.retain(|s| s.live);
    }

    /// Drop a mounted root — disposal. Its `Ctx`, owner, and reactive cells go with it.
    /// Unknown id: no-op.
    pub fn remove(&mut self, id: SlotId) {
        self.slots.retain(|s| s.id != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::task::Poll;

    /// A root that yields once, then records each wake — enough to prove the slot waker
    /// re-enqueues it and `drain` re-polls to quiescence, with no boxed `dyn Future`.
    struct Recorder {
        polls: Rc<RefCell<u32>>,
        wake_once: bool,
    }

    impl Future for Recorder {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            *self.polls.borrow_mut() += 1;
            if self.wake_once {
                self.wake_once = false;
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Pending
            }
        }
    }

    #[test]
    fn mount_polls_once_and_a_wake_re_polls() {
        let polls = Rc::new(RefCell::new(0u32));
        let mut driver: LiveDriver<Recorder> = LiveDriver::new();
        let id = driver.mount(Recorder { polls: Rc::clone(&polls), wake_once: true });
        assert_eq!(*polls.borrow(), 1, "mount polls once");
        driver.drain();
        assert_eq!(*polls.borrow(), 2, "the self-wake re-polled it exactly once");
        driver.remove(id);
        driver.drain();
        assert_eq!(*polls.borrow(), 2, "a removed slot is never polled again");
    }

    #[test]
    fn a_finished_root_is_dropped() {
        struct Done;
        impl Future for Done {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
                Poll::Ready(())
            }
        }
        let mut driver: LiveDriver<Done> = LiveDriver::new();
        driver.mount(Done);
        driver.drain();
        assert!(driver.slots.is_empty(), "a root that completed is reclaimed");
    }
}
