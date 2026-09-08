//! Style-rule collection through the render: rules ride in templates
//! ([`Template::styles`]), the fold unions every ingested template's rules, and the
//! paint that comes out is route-complete — including rules referenced only by
//! `@if` branches the initial state never materialized.

use std::borrow::Cow;

/// A scope for a test: a `Ctx` owns one, and a test has no component to receive one
/// from — the same mint production uses.
fn test_scope() -> (idyll::Runtime, idyll::Owner) {
    let rt = idyll::Runtime::new();
    let owner = rt.ctx::<()>().owner();
    (rt, owner)
}

use idyll::template::{StyleRule, Template, TplNode};
use idyll::{dev, LiveView};

type TestView = LiveView<()>;

fn rule(name: &str, css: &str) -> StyleRule {
    StyleRule {
        name: Cow::Owned(name.to_string()),
        css: Some(Cow::Owned(css.to_string())),
    }
}

fn unresolved(name: &str) -> StyleRule {
    StyleRule { name: Cow::Owned(name.to_string()), css: None }
}

fn div(class: &'static str, children: u32) -> TplNode {
    TplNode::Element {
        tag: Cow::Borrowed("div"),
        attrs: Cow::Owned(vec![idyll::template::TplAttr {
            name: Cow::Borrowed("class"),
            value: Cow::Borrowed(class),
        }]),
        slot: None,
        children,
    }
}

#[test]
fn view_carries_the_union_of_all_ingested_templates() {
    let inner: TestView =
        LiveView::new(vec![div("x-b", 0)]).with_styles(vec![rule("x-b", ".x-b{color:#0af}")]);
    let outer: TestView =
        LiveView::new(vec![div("x-a", 0)]).with_styles(vec![rule("x-a", ".x-a{padding:1rem}")]);

    let painted = dev::paint(outer.append(inner));

    let names: Vec<&str> = painted.styles.iter().map(|r| &*r.name).collect();
    assert_eq!(names, ["x-a", "x-b"]);
}

#[test]
fn branch_rules_ride_with_the_branch_that_materialized() {
    let (_rt, owner) = test_scope();
    let show = owner.mutable_signal(true);

    let base: TestView = LiveView::new(vec![
        div("x-base", 1),
        TplNode::AnchorSlot(idyll::driver::SlotId(0)),
    ])
    .with_styles(vec![rule("x-base", ".x-base{margin:0}")])
    .if_fragment(
        0,
        move |cx| show.get(cx),
        |_cx| {
            LiveView::new(vec![div("x-then", 0)])
                .with_styles(vec![rule("x-then", ".x-then{color:#f00}")])
        },
        None::<fn(&idyll::Cx) -> TestView>,
        false,
    );

    let painted = dev::paint(base.own(&owner));

    let names: Vec<&str> = painted.styles.iter().map(|r| &*r.name).collect();
    assert_eq!(names, ["x-base", "x-then"]);

    // The complement is a real limit the emission design accounts for: branch views
    // are lazy closures, so a branch the render never materialized registered no
    // template and its rules are NOT here. Live code (live) can materialize such a
    // branch later, client-side, where no table exists — which is why the document
    // sheet is the route's collected rules UNIONED WITH the full app StyleTable, not
    // the collection alone.
    let (_rt, owner) = test_scope();
    let hide = owner.mutable_signal(false);
    let unmaterialized: TestView = LiveView::new(vec![
        div("x-base", 1),
        TplNode::AnchorSlot(idyll::driver::SlotId(0)),
    ])
    .if_fragment(
        0,
        move |cx| hide.get(cx),
        |_cx| {
            LiveView::new(vec![div("x-then", 0)])
                .with_styles(vec![rule("x-then", ".x-then{color:#f00}")])
        },
        None::<fn(&idyll::Cx) -> TestView>,
        false,
    );
    let painted = dev::paint(unmaterialized.own(&owner));
    assert!(painted.styles.is_empty());
}

#[test]
fn union_upgrades_unresolved_rules_and_never_duplicates() {
    let template = Template::from(vec![div("x-a", 0)])
        .with_styles(vec![unresolved("x-a"), rule("x-b", ".x-b{gap:1rem}")])
        .with_styles(vec![rule("x-a", ".x-a{display:flex}"), unresolved("x-b")]);

    assert_eq!(
        template.styles.as_ref(),
        &[
            rule("x-a", ".x-a{display:flex}"),
            rule("x-b", ".x-b{gap:1rem}"),
        ]
    );
}

#[test]
fn the_styles_field_is_absent_from_the_wire_when_empty() {
    let bare = serde_json::to_value(Template::from(vec![div("x", 0)])).unwrap();
    assert!(bare.get("styles").is_none(), "no empty styles on the wire: {bare}");

    // And the pre-styles wire shape still parses (transitions' hand-built payloads).
    let legacy: Template = serde_json::from_value(serde_json::json!({ "nodes": [] })).unwrap();
    assert!(legacy.styles.is_empty());
}
