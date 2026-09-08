//! `@for` over plain data — fixed rows, compiled eagerly in content. Pins nested
//! shapes (a row containing its own `@if` + `@for`) — a content index's exact
//! structure, built the one way content is built: `view!`, a pure function of
//! the data, folded by `view_html`.

use idyll::{view, view_html, View};

#[derive(Clone)]
struct Section {
    title: String,
    items: Vec<String>,
}

fn page(sections: Vec<Section>, tail: Vec<String>) -> View {
    view! {
        h1 { "each" }
        @for section in (sections) {
            @if (!section.title.is_empty()) {
                h2 { (section.title.clone()) }
            }
            ul {
                @for item in (section.items) {
                    li { (item.clone()) }
                }
            }
        }
        h2 { "Tail" }
        ul {
            @for item in (tail) {
                li { (item.clone()) }
            }
        }
    }
}

#[test]
fn each_renders_fixed_rows_including_nested_fragments() {
    let sections = vec![
        Section { title: String::new(), items: vec!["intro".into()] },
        Section { title: "One".into(), items: vec!["a".into(), "b".into()] },
    ];
    let tail = vec!["x".into(), "y".into()];
    let html = view_html(&page(sections, tail), []);

    // Plain rows after the nested region — the easy case.
    assert!(html.contains("<h2>Tail</h2>"), "tail heading lost: {html}");
    assert!(html.contains("<li>x</li>") && html.contains("<li>y</li>"), "tail rows lost: {html}");

    // Rows whose bodies carry their own fragments (@if + nested @for).
    assert!(html.contains("<h2>One</h2>"), "titled section lost: {html}");
    assert!(html.contains("<li>a</li>") && html.contains("<li>b</li>"), "nested rows lost: {html}");
    assert!(html.contains("<li>intro</li>"), "untitled section's rows lost: {html}");
    // The empty-title section renders no h2 (the @if's else is empty).
    assert_eq!(html.matches("<h2>").count(), 2, "unexpected heading count: {html}");
}

// ── Fixed rows, live bindings ─────────────────────────────────────────────────────

mod live {
    use idyll::driver::{DomCommand, DomDriver as _, HandlerId};
    use idyll::{live_view, Ctx, Setup, MutableSignal};

    #[derive(Debug)]
    enum Msg {
        Paint,
    }

    /// A fixed `@for` (plain iterator) whose rows carry **reactive** bindings: the row
    /// list never changes, but each row's style tracks a signal the loop writes.
    /// Regression: the rows' binding guards must outlive the mount even though the
    /// static source itself never fires a change.
    async fn bars(ctx: Ctx<Setup, Msg>) -> idyll::Result {
        let cols: Vec<MutableSignal<String>> = (0..3).map(|_| ctx.mutable_signal("h0".to_string())).collect();
        let cols_view: Vec<idyll::Signal<String>> = cols.iter().map(|c| c.read()).collect();
        let mut ctx = ctx.render(live_view! {
            button onclick=>(|_| Msg::Paint) { "paint" }
            @for c in (cols_view) {
                div style=($c) {}
            }
        }).await?;
        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                Msg::Paint => {
                    for (i, c) in cols.iter().enumerate() {
                        c.set(&turn, format!("h{}", i + 1));
                    }
                }
            }
        }
    }

    #[test]
    fn fixed_rows_keep_their_reactive_bindings_alive() {
        let mut rt = idyll::Runtime::new();
        let mut driver = idyll::CommandBufferDriver::new();
        let ctx = rt.ctx::<Msg>();
        rt.spawn(idyll::component::spawn_live(|ctx| bars(ctx), ctx, idyll::component::report_to_log));
        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.flush(&mut driver);
        driver.take_commands(); // initial paint

        driver.dispatch_event(HandlerId(0), idyll::Event::default());
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        let commands = driver.take_commands();

        let styles: Vec<&str> = commands
            .iter()
            .filter_map(|c| match c {
                DomCommand::SetAttr { name, value, .. } if name == "style" => {
                    Some(value.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            styles,
            ["h1", "h2", "h3"],
            "each fixed row's style binding must re-fire on its signal's write"
        );
    }
}
