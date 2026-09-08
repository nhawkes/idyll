//! **Content** (`View`): a plain value built by `view!` — a pure function of
//! the data in scope, resolved Template IR with no slots and no handlers.
//!
//! Pins the foundation: `view!` emits the same IR a mounted static view folds to
//! (the two emitters are held equal node-for-node), the value round-trips serde (the
//! wire), a placed `Signal<View>` splices it into a live view (reactive replace),
//! content composes as values (`append`/`wrap`), and live ride *inside* as
//! first-class IR — recovered typed, serialized at the HTML edge.

use idyll::component::{report_to_log, spawn_live};
use idyll::driver::DomOp;
use idyll::template::TplNode;
use idyll::{
    view, view_html, view_segments, live_view, BodySegment, Ctx,
    CommandBufferDriver, MockDriver, View, Runtime, Setup,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Minimal IR → pseudo-html for structural assertions.
fn ir_html(template: &idyll::template::Template) -> String {
    fn write(nodes: &[TplNode], cursor: &mut usize, count: usize, out: &mut String) {
        for _ in 0..count {
            let Some(node) = nodes.get(*cursor) else { return };
            *cursor += 1;
            match node {
                TplNode::Text(t) => out.push_str(t),
                TplNode::TextSlot(_) | TplNode::AnchorSlot(_) => {}
                TplNode::Element { tag, children, .. } => {
                    out.push('<');
                    out.push_str(tag);
                    out.push('>');
                    write(nodes, cursor, *children as usize, out);
                    out.push_str("</");
                    out.push_str(tag);
                    out.push('>');
                }
                TplNode::Live { name, .. } => {
                    out.push_str("<live:");
                    out.push_str(name);
                    out.push_str("/>");
                }
            }
        }
    }
    let mut out = String::new();
    let mut cursor = 0;
    write(&template.nodes, &mut cursor, template.nodes.len(), &mut out);
    out
}

fn all_fragment_html(driver: &MockDriver) -> Vec<String> {
    driver
        .log
        .iter()
        .filter_map(|op| match op {
            DomOp::MountFragment { template, .. } | DomOp::ReplaceFragment { template, .. } => {
                Some(ir_html(&driver.templates[template.0 as usize]))
            }
            _ => None,
        })
        .collect()
}

/// Test live defs — what `guest!` generates for each entry.
struct W;
impl idyll::live::LiveDef for W {
    const NAME: &'static str = "w";
    type Key = idyll::live::NoKey;
}

/// A dynamic-looking piece of content: splices, `@if`, `@for` — a pure function of
/// its arguments, evaluated eagerly.
fn article(title: &str, items: Vec<String>) -> View {
    view! {
        h1 { (title) }
        @if (!items.is_empty()) {
            ul {
                @for item in (items.iter()) {
                    li { (item) }
                }
            }
        }
    }
}

// ── The pin: the two emitters agree ──────────────────────────────────────────

/// `view!` builds IR eagerly; a component mounting the same markup with `live_view!`
/// folds its command stream to IR. **They must agree node-for-node** — this is the
/// contract that lets content and mounted views share every downstream consumer
/// (HTML fold, claim walk, wire format).
#[test]
fn view_ir_equals_the_mounted_static_view_fold() {
    let items = vec!["a".to_string(), "b".to_string()];

    let content = {
        let items = items.clone();
        view! {
            h1 id=("t") class=("big") { "Hello" (items.len()) }
            @if (!items.is_empty()) {
                ul {
                    @for item in (items.iter()) {
                        li draggable[true] hidden[false] { (item) " ok" }
                    }
                }
            } else {
                p { "none" }
            }
            W()
        }
    };

    let mounted = {
        let mut rt = Runtime::new();
        let mut driver = CommandBufferDriver::new();
        let ctx = rt.ctx::<()>();
        let n = items.len();
        rt.spawn(async move {
            let _ = ctx
                .render(live_view! {
                    h1 id=("t") class=("big") { "Hello" (n) }
                    @if (!items.is_empty()) {
                        ul {
                            @for item in (items.iter()) {
                                li draggable[true] hidden[false] { (item) " ok" }
                            }
                        }
                    } else {
                        p { "none" }
                    }
                    @live(W)
                })
                .await;
        });
        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.flush(&mut driver);
        let mut fold = idyll::HtmlFold::new();
        for command in &driver.take_commands() {
            fold.apply(command);
        }
        fold.to_template()
    };

    assert_eq!(
        content.template().nodes.as_ref(),
        mounted.nodes.as_ref(),
        "view! and the mounted fold diverged"
    );
}

// ── Composition: content is a value ───────────────────────────────────────────

/// HTML attributes are hyphenated and Rust identifiers cannot be, so an underscore
/// writes a hyphen — the same mapping `style:` properties use. Without it the whole
/// aria/data surface is unreachable from a view.
#[test]
fn underscores_in_attribute_names_write_hyphens() {
    let label = "Dismiss".to_string();
    let html = view_html(
        &view! {
            button aria_label=("Close") data_state=(label) type=("button") { "x" }
        },
        [],
    );
    assert!(html.contains(r#"aria-label="Close""#), "{html}");
    assert!(html.contains(r#"data-state="Dismiss""#), "{html}");
    // A name with no underscore is untouched.
    assert!(html.contains(r#"type="button""#), "{html}");
}

#[test]
fn content_appends_and_wraps_as_values() {
    let composed = view! { p { "first" } }
        .append(view! { p { "second" } })
        .wrap("main");
    assert_eq!(
        ir_html(composed.template()),
        "<main><p>first</p><p>second</p></main>"
    );
}

#[test]
fn empty_branches_resolve_to_nothing() {
    let empty = article("Bare", vec![]);
    assert_eq!(ir_html(empty.template()), "<h1>Bare</h1>");
    assert_eq!(
        ir_html(article("Hello", vec!["a".into(), "b".into()]).template()),
        "<h1>Hello</h1><ul><li>a</li><li>b</li></ul>"
    );
}

#[test]
fn view_round_trips_the_wire() {
    let content = article("Wire", vec!["x".into()]);
    let json = serde_json::to_string(&content).unwrap();
    let back: View = serde_json::from_str(&json).unwrap();
    assert_eq!(back, content);

    // The serde shape IS the jco shape (`{"tag": …, "val": …}`) — one template wire
    // format everywhere; the browser fold walks a View without translation.
    let value: serde_json::Value = serde_json::to_value(&content).unwrap();
    assert_eq!(value["nodes"][0]["tag"], "element");
    assert_eq!(value["nodes"][0]["val"]["tag"], "h1");
    assert_eq!(value["nodes"][1]["tag"], "text");
    assert_eq!(value["nodes"][1]["val"], "Wire");
}

// ── The splice: a placed `Signal<View>` in a live view ────────────────────────

#[test]
fn a_live_component_replaces_placed_content_when_its_signal_changes() {
    #[derive(Debug)]
    enum Msg {
        Swap,
    }

    let first = view! { p { "first" } };
    let second = view! { p { "second" } };

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        move |ctx: Ctx<Setup, Msg>| async move {
            let body = ctx.mutable_signal(first);
            let mut ctx = ctx.render(live_view! { div { (body.read()) } }).await?;
            loop {
                let (msg, turn) = ctx.recv().await?;
                match msg {
                    Msg::Swap => body.update(&turn, |v| *v = second.clone()),
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(all_fragment_html(&driver), vec!["<p>first</p>"]);

    sender.send(Msg::Swap);
    rt.run_once();
    rt.flush(&mut driver);
    assert_eq!(all_fragment_html(&driver), vec!["<p>first</p>", "<p>second</p>"]);
}

// ── Live ride inside ───────────────────────────────────────────────

#[test]
fn islands_cross_inside_rendered_content_and_surface_at_the_splice() {
    // Live are MARKERS in content: a named hole, no component here — the live
    // half lives in the app's wasm live table.
    let body = view! {
        p { "static prose" }
        W()
    };

    // The live is typed IR — recovered without string comparison — and it survives
    // the wire.
    assert_eq!(body.live(), vec![("w".to_string(), None)]);
    let back: View =
        serde_json::from_str(&serde_json::to_string(&body).unwrap()).unwrap();
    assert_eq!(back.live(), vec![("w".to_string(), None)]);

    // Wrapping the content keeps the marker; the fold serializes the wrapper, with
    // the mount's paint injected by mount identity.
    let page = body.wrap("main");
    assert_eq!(page.live(), vec![("w".to_string(), None)]);
    let paint = view! { button { "1" } };
    let html = view_html(&page, [(("w".to_string(), 0), view_html(&paint, []))]);
    assert_eq!(
        html.as_str(),
        "<main><p>static prose</p><idyll-live data-i=\"w\" style=\"display:contents\"><button>1</button></idyll-live></main>"
    );
}

// ── Segments: the streaming form of the same fold ─────────────────────────────

#[test]
fn segments_cut_at_island_paint_slots_and_recompose_to_the_same_html() {
    // Two instances of one live name: identity is (name, document-order index).
    let page = view! {
        main {
            p { "before" }
            W()
            p { "between" }
            W()
        }
    };

    let segments = view_segments(&page);
    let live: Vec<_> = segments
        .iter()
        .filter_map(|s| match s {
            BodySegment::Live { name, instance, .. } => Some((name.as_str(), *instance)),
            _ => None,
        })
        .collect();
    assert_eq!(live, vec![("w", 0), ("w", 1)]);

    // The cut is the whole live boundary: a streaming consumer owns the wrapper (so it can
    // stamp the mount's static-ness), wrapping each mount's paint exactly as the
    // everything-known fold does — and reproduces view_html byte for byte.
    let paint = |n: u32| view_html(&view! { button { (n) } }, []);
    let streamed: String = segments
        .iter()
        .map(|s| match s {
            BodySegment::Html(html) => html.as_str().to_owned(),
            BodySegment::Live { name, instance, key, .. } => format!(
                "{}{}{}",
                idyll::live_wrapper_open(name, key.as_deref(), false),
                paint(*instance).as_str(),
                idyll::LIVE_WRAPPER_CLOSE,
            ),
        })
        .collect();
    let folded = view_html(
        &page,
        [
            (("w".to_string(), 0), paint(0)),
            (("w".to_string(), 1), paint(1)),
        ],
    );
    assert_eq!(streamed, folded.as_str());
}
