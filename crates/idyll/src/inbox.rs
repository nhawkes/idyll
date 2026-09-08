use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::task::Waker;

struct InboxInner<M> {
    queue: VecDeque<M>,
    /// Store absorbs awaiting this component's turn (see [`Inbox::set_absorber`]).
    absorbs: VecDeque<Vec<u8>>,
    absorber: Option<Rc<dyn Fn(&crate::signal::Turn, &[u8])>>,
    waker: Option<Waker>,
    recorder: Option<Rc<dyn Fn(&M)>>,
    absorb_recorder: Option<Rc<dyn Fn(&[u8])>>,
}

/// The receive end of a component's message queue. Held inside `Ctx<Live, M>`.
pub(crate) struct Inbox<M>(Rc<RefCell<InboxInner<M>>>);

impl<M: 'static> Inbox<M> {
    pub(crate) fn new() -> Self {
        Inbox(Rc::new(RefCell::new(InboxInner {
            queue: VecDeque::new(),
            absorbs: VecDeque::new(),
            absorber: None,
            waker: None,
            recorder: None,
            absorb_recorder: None,
        })))
    }

    pub(crate) fn sender(&self) -> InboxSender<M> {
        InboxSender(Rc::clone(&self.0))
    }

    /// Poll for the next message. Pending absorbs apply first — **in this
    /// component's turn** — so a store write is always attributable to its owner
    /// and lands in the owner's log.
    pub(crate) fn poll_recv(&self, cx: &mut std::task::Context<'_>) -> std::task::Poll<M> {
        loop {
            let absorb = {
                let mut inner = self.0.borrow_mut();
                match inner.absorbs.pop_front() {
                    Some(bytes) => {
                        let apply = inner.absorber.clone();
                        let record = inner.absorb_recorder.clone();
                        Some((bytes, apply, record))
                    }
                    None => None,
                }
            };
            let Some((bytes, apply, record)) = absorb else { break };
            if let Some(record) = record {
                record(&bytes);
            }
            if let Some(apply) = apply {
                // The absorb *is* this store owner's turn: mint the turn so its
                // cache writes are attributable and land in the owner's log.
                apply(&crate::signal::Turn::mint(), &bytes);
            }
        }
        // Clone the recorder out and drop the borrow before invoking it: the
        // recorder runs the app's `Serialize` impl, and app code must never execute
        // under this inbox's exclusive borrow (the absorb path above already
        // follows this rule).
        let (msg, recorder) = {
            let mut inner = self.0.borrow_mut();
            match inner.queue.pop_front() {
                Some(msg) => (msg, inner.recorder.clone()),
                None => {
                    inner.waker = Some(cx.waker().clone());
                    return std::task::Poll::Pending;
                }
            }
        };
        if let Some(recorder) = recorder {
            recorder(&msg);
        }
        std::task::Poll::Ready(msg)
    }

    /// Register the absorb handler (the store's decode-and-replay). One per
    /// component — the store owner.
    pub(crate) fn set_absorber(&self, apply: Rc<dyn Fn(&crate::signal::Turn, &[u8])>) -> bool {
        let mut inner = self.0.borrow_mut();
        if inner.absorber.is_some() {
            return false;
        }
        inner.absorber = Some(apply);
        true
    }

    pub(crate) fn set_recorder(&self, recorder: Option<Rc<dyn Fn(&M)>>) {
        self.0.borrow_mut().recorder = recorder;
    }

    pub(crate) fn set_absorb_recorder(&self, recorder: Option<Rc<dyn Fn(&[u8])>>) {
        self.0.borrow_mut().absorb_recorder = recorder;
    }
}

/// The send half. Clone is cheap (shared Rc). Sharable with event handlers
/// and callbacks.
pub struct InboxSender<M>(Rc<RefCell<InboxInner<M>>>);

impl<M> Clone for InboxSender<M> {
    fn clone(&self) -> Self {
        InboxSender(Rc::clone(&self.0))
    }
}

impl<M: 'static> InboxSender<M> {
    pub fn send(&self, msg: M) {
        self.0.borrow_mut().queue.push_back(msg);
        self.wake();
    }

    /// Queue a store absorb for the owning component's next turn.
    pub(crate) fn send_absorb(&self, bytes: Vec<u8>) {
        self.0.borrow_mut().absorbs.push_back(bytes);
        self.wake();
    }

    fn wake(&self) {
        // Take the waker out before waking: edition-2021 temporaries would hold the
        // shared borrow across `wake()`, a landmine for any synchronous waker.
        let waker = self.0.borrow_mut().waker.take();
        if let Some(w) = waker {
            w.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll, Waker};

    use std::sync::Arc;
    use std::task::Wake;

    struct NoopWaker;
    impl Wake for NoopWaker {
        fn wake(self: Arc<Self>) {}
        fn wake_by_ref(self: &Arc<Self>) {}
    }

    fn noop_waker() -> Waker {
        Waker::from(Arc::new(NoopWaker))
    }

    #[derive(Debug, PartialEq, Clone, serde::Serialize)]
    enum Msg {
        Inc,
        Dec,
    }

    #[test]
    fn messages_queue_fifo() {
        let inbox: Inbox<Msg> = Inbox::new();
        let sender = inbox.sender();
        sender.send(Msg::Inc);
        sender.send(Msg::Dec);
        sender.send(Msg::Inc);
        let w = noop_waker();
        let mut cx = Context::from_waker(&w);
        assert_eq!(inbox.poll_recv(&mut cx), Poll::Ready(Msg::Inc));
        assert_eq!(inbox.poll_recv(&mut cx), Poll::Ready(Msg::Dec));
        assert_eq!(inbox.poll_recv(&mut cx), Poll::Ready(Msg::Inc));
        assert_eq!(inbox.poll_recv(&mut cx), Poll::Pending);
    }

    #[test]
    fn absorbs_apply_before_messages_in_the_owners_poll() {
        let inbox: Inbox<Msg> = Inbox::new();
        let seen = Rc::new(RefCell::new(Vec::<String>::new()));
        let absorbed = Rc::clone(&seen);
        inbox.set_absorber(Rc::new(move |_turn: &crate::signal::Turn, bytes: &[u8]| {
            absorbed.borrow_mut().push(format!("absorb:{}", bytes.len()));
        }));
        let sender = inbox.sender();
        sender.send(Msg::Inc);
        sender.send_absorb(vec![1, 2, 3]);

        let w = noop_waker();
        let mut cx = Context::from_waker(&w);
        assert_eq!(inbox.poll_recv(&mut cx), Poll::Ready(Msg::Inc));
        assert_eq!(
            *seen.borrow(),
            vec!["absorb:3".to_string()],
            "the absorb applied inside the poll, before the message was returned"
        );
    }

    #[test]
    fn recorder_observes_delivered_messages() {
        let inbox: Inbox<Msg> = Inbox::new();
        let records = Rc::new(RefCell::new(Vec::<serde_json::Value>::new()));
        let records_for_recorder = Rc::clone(&records);
        inbox.set_recorder(Some(Rc::new(move |msg: &Msg| {
            records_for_recorder
                .borrow_mut()
                .push(serde_json::to_value(msg).unwrap());
        })));
        let sender = inbox.sender();
        sender.send(Msg::Inc);
        sender.send(Msg::Dec);

        let w = noop_waker();
        let mut cx = Context::from_waker(&w);
        assert_eq!(inbox.poll_recv(&mut cx), Poll::Ready(Msg::Inc));
        assert_eq!(inbox.poll_recv(&mut cx), Poll::Ready(Msg::Dec));

        assert_eq!(
            *records.borrow(),
            vec![serde_json::json!("Inc"), serde_json::json!("Dec")]
        );
    }

    #[test]
    fn sender_clone_works() {
        let inbox: Inbox<Msg> = Inbox::new();
        let sender = inbox.sender();
        let sender2 = sender.clone();
        sender2.send(Msg::Inc);
        let w = noop_waker();
        let mut cx = Context::from_waker(&w);
        assert_eq!(inbox.poll_recv(&mut cx), Poll::Ready(Msg::Inc));
    }
}
