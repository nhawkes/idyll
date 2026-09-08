//! Component slots — a view-typed prop a component places 0-to-many.
//!
//! A slot is a **broadcast**: the *sender* lives in the recipe parent (via
//! [`Ctx::slot`](crate::Ctx::slot)), where the recipe's `=>` callbacks bind to that parent's
//! inbox; the prop the component receives is the *receiver* ([`Slot`]). Each placement is one
//! independent subscription that materialises a fresh instance — own mailbox, own DOM — so
//! 0-to-many falls out of where the component places it. An instance is spawned
//! fire-and-forget into the flat executor (post-render, like a fragment); the component holds
//! only the erased receiver (the `Callback` seam) and, per placement, a [`SlotGuard`] leaf
//! whose drop unmounts the instance. One `dyn`, at the callback boundary.
//!
//! The two parent relations split here: **messages follow recipe** — bound at authoring,
//! placement cannot reroute them — while **context follows tree** — the instance subscribes
//! under the frame of the view that places it, so a wrapper's provides reach the children
//! placed inside it, wherever they were authored.

use std::rc::Rc;

use crate::ctx::ContextMap;
use crate::runtime::ChildId;

/// The receiver end of a slot, handed to a component as a view-typed prop. Placing it
/// (`(children)` in a view, [`place`](Slot::place) by hand) subscribes: a fresh instance is
/// built under the placing view's frame, anchored at `child_id`, and the returned
/// [`SlotGuard`] unmounts it when it drops. `M`-erased — the one `dyn`, at the same
/// `Rc<dyn Fn>` seam a [`Callback`](crate::Callback) is.
#[derive(Clone)]
pub struct Slot {
    subscribe: Rc<dyn Fn(ChildId, &ContextMap) -> SlotGuard>,
}

impl Slot {
    pub(crate) fn new(subscribe: Rc<dyn Fn(ChildId, &ContextMap) -> SlotGuard>) -> Self {
        Slot { subscribe }
    }

    /// Subscribe a placement under `frame` — the **tree** parent's frame, supplied by
    /// `wire_view` when the placing view mounts. Returns the guard whose drop unmounts
    /// the instance.
    pub(crate) fn place(&self, child_id: ChildId, frame: &ContextMap) -> SlotGuard {
        (self.subscribe)(child_id, frame)
    }
}

/// A placement's lifetime handle. Its `Drop` runs the unmount thunk built by
/// [`build_slot`](crate::ctx::build_slot) — dropping the instance's DOM guards, which cascades
/// to its embedded children's cancel-guards. It rides the placing view's `placement_guards`
/// exactly where a [`mount_child`](crate::component::mount_child) guard would, so an `@if`/`@for`
/// teardown unmounts the instance with no bespoke lifetime code.
pub struct SlotGuard {
    on_remove: Option<Box<dyn FnOnce()>>,
}

impl SlotGuard {
    pub(crate) fn new(on_remove: impl FnOnce() + 'static) -> Self {
        SlotGuard { on_remove: Some(Box::new(on_remove)) }
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Some(remove) = self.on_remove.take() {
            remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{component, live_view, Ctx, MockDriver, Result, Runtime, Setup};
    use crate::component::{report_to_log, spawn_live};

    /// The primitive end to end: a slot whose recipe embeds a child component renders its
    /// instance at the placement's anchor, and that embedded child mounts — proving the instance
    /// is framed at its placement and spawned into the executor.
    #[test]
    fn a_placed_slot_renders_and_drives_its_instance() {
        #[derive(Debug)]
        enum PMsg {}

        #[component]
        async fn Inner(ctx: Ctx<Setup, crate::Never>) -> Result {
            Ok(ctx.render(live_view! { span { ("inner") } }).await?)
        }

        async fn parent(ctx: Ctx<Setup, PMsg>) -> Result {
            let slot = ctx.slot(|scope| (live_view! { div { Inner } })(scope));
            let mut ctx = ctx
                .render(move |_| async move {
                    let anchored = crate::LiveView::new(vec![
                        crate::template::TplNode::AnchorSlot(crate::driver::SlotId(0)),
                    ]);
                    Ok(anchored.place(0, slot))
                })
                .await?;
            loop {
                let _ = ctx.recv().await?;
            }
        }

        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx: Ctx<Setup, PMsg> = rt.ctx::<PMsg>();
        rt.spawn(spawn_live(parent, ctx, report_to_log));
        rt.run_to_quiescence();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);

        assert_eq!(driver.set_texts(), vec!["inner"]);
    }

    /// Context follows tree: a child authored by the outer component but placed inside a
    /// wrapper resolves `use_context` against the **wrapper's** frame, so the wrapper's
    /// provide reaches the children placed in it — wherever they were authored. (Messages
    /// still follow recipe: the child's callbacks bind to its author's inbox.)
    #[test]
    fn a_slotted_child_reads_the_placing_wrappers_context() {
        #[derive(Debug)]
        enum PMsg {}

        #[derive(Clone)]
        struct Theme(&'static str);

        #[component]
        async fn ShowTheme(ctx: Ctx<Setup, crate::Never>) -> Result {
            let theme = ctx.use_context::<Theme>().map(|t| t.0).unwrap_or("unthemed");
            Ok(ctx.render(live_view! { span { (theme) } }).await?)
        }

        #[component]
        async fn Provider(ctx: Ctx<Setup, crate::Never>, children: crate::Slot) -> Result {
            ctx.provide(Theme("provided"));
            Ok(ctx.render(live_view! { div { (children) } }).await?)
        }

        async fn parent(ctx: Ctx<Setup, PMsg>) -> Result {
            // ShowTheme is AUTHORED here — this frame has no Theme — but PLACED inside
            // Provider, whose frame does.
            let mut ctx = ctx
                .render(live_view! { section { Provider { ShowTheme } } })
                .await?;
            loop {
                let _ = ctx.recv().await?;
            }
        }

        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx: Ctx<Setup, PMsg> = rt.ctx::<PMsg>();
        rt.spawn(spawn_live(parent, ctx, report_to_log));
        rt.run_to_quiescence();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);

        assert_eq!(driver.set_texts(), vec!["provided"]);
    }

    /// The `{ … }` children sugar end to end: a wrapper component declares `children: Slot` and
    /// places `(children)`; the parent calls it with a block, which becomes the slot recipe.
    #[test]
    fn the_children_block_sugar_wraps_a_component() {
        #[derive(Debug)]
        enum PMsg {}

        #[component]
        async fn Inner(ctx: Ctx<Setup, crate::Never>) -> Result {
            Ok(ctx.render(live_view! { span { ("wrapped") } }).await?)
        }

        #[component]
        async fn Wrapper(ctx: Ctx<Setup, crate::Never>, children: crate::Slot) -> Result {
            Ok(ctx.render(live_view! { div { (children) } }).await?)
        }

        async fn parent(ctx: Ctx<Setup, PMsg>) -> Result {
            let mut ctx = ctx
                .render(live_view! {
                    section { Wrapper { Inner } }
                })
                .await?;
            loop {
                let _ = ctx.recv().await?;
            }
        }

        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx: Ctx<Setup, PMsg> = rt.ctx::<PMsg>();
        rt.spawn(spawn_live(parent, ctx, report_to_log));
        rt.run_to_quiescence();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);

        assert_eq!(driver.set_texts(), vec!["wrapped"]);
    }
}
