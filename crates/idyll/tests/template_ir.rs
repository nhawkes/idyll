//! Pins the `live_view!` macro's **Template IR** emission: the template as typed const data —
//! slots as first-class node kinds, adjacent static text merged at compile time, tree
//! shipped flat (pre-order + child counts, the WIT-compatible serialization). This is the
//! parse result of the one parser in the system (the macro); everything downstream
//! consumes it as data.

use std::borrow::Cow;

/// A scope for a test: a `Ctx` owns one, and a test has no component to receive one
/// from — the same mint production uses.
fn test_scope() -> (idyll::Runtime, idyll::Owner) {
    let rt = idyll::Runtime::new();
    let owner = rt.ctx::<()>().owner();
    (rt, owner)
}

use idyll::driver::SlotId;
use idyll::template::TplNode;
use idyll::{live_view, LiveView};

#[derive(Debug)]
enum Msg {
    Hit,
}

/// These tests read the IR a `live_view!` compiles to, so they build one outside any running
/// component. `live_view!` is a render closure now, so drive it to its `LiveView` via the dev
/// harness (childless IR views resolve in one poll).
fn build<F, Fut>(view: F) -> LiveView<Msg>
where
    F: FnOnce(idyll::RenderScope<Msg>) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<LiveView<Msg>, idyll::Fault>>,
{
    idyll::dev::render_to_view(view)
}

#[test]
fn the_macro_emits_the_template_as_flat_typed_ir() {
    let name = "world".to_string();
    let v: LiveView<Msg> = build(live_view! {
        div class=("wrap") {
            "Hello " (name.clone()) " and " "more"
            span { "static" }
        }
        button onclick=>(|_| Msg::Hit) { "go" }
    });

    let ir = &v.template;
    let nodes: &[TplNode] = &ir.nodes;

    let expected = [
        // div: `class=(...)` is a dynamic attr binding → the element gets a slot.
        TplNode::Element {
            tag: Cow::Borrowed("div"),
            attrs: Cow::Borrowed(&[]),
            slot: Some(SlotId(0)),
            children: 4,
        },
        TplNode::Text(Cow::Borrowed("Hello ")),
        TplNode::TextSlot(SlotId(1)),
        // " and " + "more" — adjacent static text merges AT COMPILE TIME.
        TplNode::Text(Cow::Borrowed(" and more")),
        TplNode::Element {
            tag: Cow::Borrowed("span"),
            attrs: Cow::Borrowed(&[]),
            slot: None,
            children: 1,
        },
        TplNode::Text(Cow::Borrowed("static")),
        // button: dynamics (onclick) → slot; text child.
        TplNode::Element {
            tag: Cow::Borrowed("button"),
            attrs: Cow::Borrowed(&[]),
            slot: Some(SlotId(2)),
            children: 1,
        },
        TplNode::Text(Cow::Borrowed("go")),
    ];
    assert_eq!(nodes, &expected, "IR mismatch");
}

#[test]
fn control_flow_is_an_anchor_node_and_rows_carry_their_own_ir() {
    let (_rt, owner) = test_scope();
    let items = owner.mutable_vec::<String>();
    let v: LiveView<Msg> = build(live_view! {
        ul id=("l") {
            @for (_row, item) in (items) {
                li { ($item) }
            }
        }
    });

    let ir = &v.template;
    let nodes: &[TplNode] = &ir.nodes;
    assert_eq!(
        nodes,
        &[
            TplNode::Element {
                tag: Cow::Borrowed("ul"),
                attrs: Cow::Borrowed(&[]),
                slot: Some(SlotId(0)),
                children: 1,
            },
            TplNode::AnchorSlot(SlotId(1)),
        ]
    );

    // The row body is its own template with its own IR: a list fragment is data
    // plus a `Row` arm in the block's dispatch, and mounting a row registers the
    // row's template as its own unit.
    let block = v.blocks.iter().find(|block| !block.fragments.is_empty()).expect("a list block");
    assert!(matches!(
        block.fragments[0].kind,
        idyll::live_view::FragmentSource::Ready(idyll::live_view::FragmentKind::List { .. })
    ));
    items.push(&idyll::Turn::for_test(), "x".to_string());
    let mut rt = idyll::Runtime::new();
    let mut driver = idyll::MockDriver::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(async move {
        let _ = ctx.render(|_| async move { Ok::<_, idyll::Fault>(v) }).await;
    });
    rt.run_once();
    rt.process_pending_view(&mut driver);
    let row_ir = [
        TplNode::Element {
            tag: Cow::Borrowed("li"),
            attrs: Cow::Borrowed(&[]),
            slot: None,
            children: 1,
        },
        TplNode::TextSlot(SlotId(0)),
    ];
    assert!(
        driver.templates.iter().any(|template| template.nodes[..] == row_ir),
        "the row's template registers as its own IR unit"
    );
}

#[test]
fn append_rebases_the_second_views_slots() {
    let left_label = "left".to_string();
    let right_label = "right".to_string();
    let left: LiveView<Msg> = build(live_view! { span { (left_label.clone()) } });
    let right: LiveView<Msg> = build(live_view! { em { (right_label.clone()) } });

    let composed = left.append(right);
    let nodes: &[TplNode] = &composed.template.nodes;

    // Both sub-views numbered their text slot 0; append must rebase the second so the
    // bindings can't collide.
    assert_eq!(
        nodes,
        &[
            TplNode::Element {
                tag: Cow::Borrowed("span"),
                attrs: Cow::Borrowed(&[]),
                slot: None,
                children: 1,
            },
            TplNode::TextSlot(SlotId(0)),
            TplNode::Element {
                tag: Cow::Borrowed("em"),
                attrs: Cow::Borrowed(&[]),
                slot: None,
                children: 1,
            },
            TplNode::TextSlot(SlotId(1)),
        ]
    );
    assert_eq!(fold_of(composed), "<span>left</span><em>right</em>");
}

#[test]
fn wrap_nests_the_whole_view_in_a_container() {
    let label = "inner".to_string();
    let wrapped: LiveView<Msg> =
        build(live_view! { h1 { "T" } p { (label.clone()) } }).wrap("article");

    assert_eq!(fold_of(wrapped), "<article><h1>T</h1><p>inner</p></article>");
}

/// The one serialization path, driven end to end: mount the view through a command
/// buffer (what every render does) and fold the stream to HTML.
fn fold_of<M: 'static>(view: LiveView<M>) -> String {
    let mut rt = idyll::Runtime::new();
    let mut driver = idyll::CommandBufferDriver::new();
    let ctx = rt.ctx::<M>();
    rt.spawn(async move {
        let _ = ctx
            .render(|_| async move { Ok::<_, idyll::Fault>(view) })
            .await;
    });
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    idyll::fold_html(&driver.take_commands()).into_string()
}
