use std::rc::Rc;

use crate::inbox::InboxSender;

/// A child → parent communication channel. Cheap to clone (Rc).
///
/// Create via `ctx.callback(Msg::Variant)` or `ctx.callback_map(|v| Msg::Got(v))`.
/// Pass clones into child components; they call `cb.call(value)` to send a
/// message to the parent without knowing its message type.
pub struct Callback<T: 'static> {
    f: Rc<dyn Fn(T)>,
}

impl<T: 'static> Clone for Callback<T> {
    fn clone(&self) -> Self {
        Callback {
            f: Rc::clone(&self.f),
        }
    }
}

impl<T: 'static> Callback<T> {
    pub(crate) fn new(f: impl Fn(T) + 'static) -> Self {
        Callback { f: Rc::new(f) }
    }

    /// Invoke the callback.
    pub fn call(&self, value: T) {
        (self.f)(value)
    }

    /// Partially apply a fixed argument; returns a `Callback<()>` that ignores
    /// its input and always sends `value`. Used by `on_delete.bind(row)` etc.
    pub fn bind<A: Clone + 'static>(&self, arg: A) -> Callback<()>
    where
        T: From<A>,
    {
        let f = Rc::clone(&self.f);
        Callback::new(move |()| f(T::from(arg.clone())))
    }

    /// Map the callback's input type. `Callback<A>` and `A: From<B>`'s mapper make a
    /// `Callback<B>` — how an atom that means something narrower than an `Event`
    /// (a typed value, a bare press) meets the DOM at its edge.
    pub fn contra_map<U: 'static>(&self, mapper: impl Fn(U) -> T + 'static) -> Callback<U> {
        let f = Rc::clone(&self.f);
        Callback::new(move |u| f(mapper(u)))
    }

    /// [`contra_map`](Self::contra_map) where the mapping may decline: `None` sends
    /// nothing. A control whose input can fail to be its own value — a slider whose
    /// field is mid-edit — reads as a callback with the failure already handled,
    /// rather than as a mailbox that exists only to swallow.
    pub fn try_contra_map<U: 'static>(
        &self,
        mapper: impl Fn(U) -> Option<T> + 'static,
    ) -> Callback<U> {
        let f = Rc::clone(&self.f);
        Callback::new(move |u| {
            if let Some(t) = mapper(u) {
                f(t)
            }
        })
    }
}

/// Internal: build a `Callback<T>` that sends `mapper(v)` into the given inbox.
/// A lambda written in a child component's arguments, bound to the inbox of the
/// component whose view it appears in — the same binding an event mapper gets, at the
/// same moment. Emitted by `live_view!`; not called by hand.
pub fn callback_from_sender<M: 'static, T: 'static>(
    sender: InboxSender<M>,
    mapper: impl Fn(T) -> M + 'static,
) -> Callback<T> {
    Callback::new(move |v| sender.send(mapper(v)))
}
