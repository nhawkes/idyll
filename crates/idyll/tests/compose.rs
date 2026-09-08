//! View composition through **value interpolation** — a pre-built `LiveView<M>` placed at an
//! anchor with `(view)`, its own event block routing to the placing component's reducer. This
//! is the one composition door for a ready-made view (the eager `Name(args)` splice is gone;
//! reusable chrome is a `#[component]` with a `{ … }` children slot). Pins that a placed view
//! mounts and its events reach the parent.

use std::borrow::Cow;

use idyll::driver::{DomCommand, DomDriver as _, SlotId};
use idyll::{Ctx, Fault, LiveView, RenderScope, Setup};

#[derive(Debug)]
enum Msg {
    Ping,
}

/// Wrap a pre-built (childless) view as a render closure — the harness stand-in for what the
/// `live_view!` macro emits.
fn mount<M: 'static>(
    view: LiveView<M>,
) -> impl FnOnce(RenderScope<M>) -> std::future::Ready<Result<LiveView<M>, Fault>> {
    move |_| std::future::ready(Ok(view))
}

#[test]
fn a_slotted_view_mounts_and_its_events_reach_the_parent_reducer() {
    use idyll::template::{TplAttr, TplNode};
    // The hand-built form of a `(view)` slot: a pre-built sub-view placed at an anchor. Its own
    // event block came across at wire time, so its button's click routes to *this* component's
    // reducer — its own slot scope.
    async fn host(ctx: Ctx<Setup, Msg>) -> idyll::Result {
        let n = ctx.mutable_signal(0i64);
        let count = n.read();

        let body: LiveView<Msg> = LiveView::new(vec![TplNode::Element {
            tag: Cow::Borrowed("button"),
            attrs: Cow::Borrowed(&[]),
            slot: Some(SlotId(0)),
            children: 0,
        }])
        .event(0, "click", |_| Some(Msg::Ping));

        let parent = LiveView::new(vec![
            TplNode::Element {
                tag: Cow::Borrowed("div"),
                attrs: Cow::Owned(vec![TplAttr::new("id", "wrap")]),
                slot: None,
                children: 2,
            },
            TplNode::TextSlot(SlotId(0)),
            TplNode::AnchorSlot(SlotId(1)),
        ])
        .text(0, move |cx| count.get(cx).to_string())
        .slot(1, body);

        let mut ctx = ctx.render(mount(parent)).await?;
        loop {
            let (Msg::Ping, turn) = ctx.recv().await?;
            n.update(&turn, |v| *v += 1);
        }
    }

    let mut rt = idyll::Runtime::new();
    let mut driver = idyll::CommandBufferDriver::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(idyll::component::spawn_live(|ctx| host(ctx), ctx, idyll::component::report_to_log));
    rt.run_to_quiescence();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    driver.take_commands(); // initial paint

    let click = driver.latest_handler().expect("the slotted button registered a click handler");
    driver.dispatch_event(click, idyll::Event::default());
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    let texts: Vec<String> = driver
        .take_commands()
        .into_iter()
        .filter_map(|c| match c {
            DomCommand::SetText { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["1"], "the slotted view's click reached the parent's reducer");
}

#[test]
fn a_view_interpolation_places_a_passed_view_and_routes_its_events() {
    // `(body)` in a `live_view!` is a view interpolation: the passed `LiveView` mounts at an
    // anchor, and its events reach this component's reducer. A sibling text `(expr)` is a
    // one-shot fill — the two dispatch by type at construction. Built by hand here so `body`
    // is a ready-made value (the macro's async closure needs a render scope to run).
    use idyll::template::TplNode;

    async fn host(ctx: Ctx<Setup, Msg>) -> idyll::Result {
        let n = ctx.mutable_signal(0i64);
        let count = n.read();
        let body: LiveView<Msg> = LiveView::new(vec![
            TplNode::Element {
                tag: Cow::Borrowed("button"),
                attrs: Cow::Borrowed(&[]),
                slot: Some(SlotId(0)),
                children: 1,
            },
            TplNode::TextSlot(SlotId(1)),
        ])
        .event(0, "click", |_| Some(Msg::Ping))
        .text(1, move |cx| count.get(cx).to_string());

        // Card chrome: a one-shot title text and the interpolated body at an anchor.
        let card: LiveView<Msg> = LiveView::new(vec![
            TplNode::Element {
                tag: Cow::Borrowed("div"),
                attrs: Cow::Borrowed(&[]),
                slot: None,
                children: 2,
            },
            TplNode::TextSlot(SlotId(0)),
            TplNode::AnchorSlot(SlotId(1)),
        ])
        .oneshot_text(0, "Title".to_string())
        .slot(1, body);

        let mut ctx = ctx.render(mount(card)).await?;
        loop {
            let (Msg::Ping, turn) = ctx.recv().await?;
            n.update(&turn, |v| *v += 1);
        }
    }

    let mut rt = idyll::Runtime::new();
    let mut driver = idyll::CommandBufferDriver::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(idyll::component::spawn_live(|ctx| host(ctx), ctx, idyll::component::report_to_log));
    rt.run_to_quiescence();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    let mount_commands = driver.take_commands();
    assert!(
        mount_commands.iter().any(|c| matches!(c, DomCommand::SetText { text, .. } if text == "Title")),
        "the one-shot title filled its slot: {mount_commands:?}"
    );

    let click = driver.latest_handler().expect("the placed view's button registered a handler");
    driver.dispatch_event(click, idyll::Event::default());
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    let texts: Vec<String> = driver
        .take_commands()
        .into_iter()
        .filter_map(|c| match c {
            DomCommand::SetText { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["1"], "the placed view's click reached the parent's reducer");
}
