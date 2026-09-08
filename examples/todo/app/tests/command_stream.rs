//! The command-stream **shape contract** shared by both folds. The server's `fold_html`
//! and the browser's `runtime.js` are two implementations of one spec: a dumb fold over
//! `templates (IR) + commands`. The ordering invariants that fold relies on are pinned
//! here against the real app's mount stream:
//!
//! 1. The stream is **self-contained**: templates travel in-band as typed IR
//!    (`ReplaceTemplate` commands, content-deduplicated). The stream opens with the root
//!    (template id 0) — the opening `BindSlot`s resolve against *its* slots (and in the
//!    browser, the claim walk matches its IR against the live SSR DOM).
//! 2. Every `BindSlot` refers to a slot of the **most recently instantiated** template —
//!    the root until the first structural command, then the template referenced by the
//!    latest `MountFragment`/`ReplaceFragment` — so one transient slot scratch suffices.
//! 3. Every node id consumed by `SetText`/`SetAttr`/`AddEventListener`/… was introduced
//!    earlier: by a `BindSlot` or (for anchors) a prior structural command — fragment
//!    anchor ids materialize on first use.
//! 4. `fold_html(templates, commands)` — the server fold of this exact stream — produces
//!    the clean document: content present, zero client scaffolding (there are no markers
//!    anywhere to leak; this pins that they never come back).
//! 5. **Two node kinds carry a subtree** in the pre-order walk: an element's children and
//!    a live's fallback. Both folds must step over both — `template::subtree_len` and
//!    `runtime.js`'s `skipSubtree` — or every node after a fallback lands in the wrong
//!    place in one fold and not the other.

use idyll::component::spawn_live;
use idyll::driver::{DomCommand, NodeId};
use idyll::template::{Template, TplNode};
use idyll::{fold_html, CommandBufferDriver, DomDriver, Runtime};
use todo_app::{prose, todos, Msg, PageSeed, ProseMsg};

/// A hand-built executed payload — exactly the shape the interpreter serves and the
/// guest binds. The route-world wire shape: the Page record carries the contract
/// fields plus the todos edge; the roots object names the page by its id.
fn seeded_data(texts: &[(&str, bool)]) -> PageSeed {
    let mut commits: Vec<serde_json::Value> = texts
        .iter()
        .enumerate()
        .map(|(i, (text, done))| {
            serde_json::json!({ "Commit": {
                "type_tag": "Todo",
                "json": { "id": i as u64 + 1, "text": text, "done": done },
            }})
        })
        .collect();
    let ids: Vec<u64> = (1..=texts.len() as u64).collect();
    commits.push(serde_json::json!({ "Commit": {
        "type_tag": "Page",
        "json": {
            "id": "/", "title": "Todos",
            "route": { "Todos": { "todos": ids } },
        },
    }}));
    let payload = serde_json::json!({ "seed": { "commits": commits }, "roots": { "route": "/" } });
    serde_json::from_value(payload).unwrap()
}

fn mount_stream() -> Vec<DomCommand> {
    let seed = seeded_data(&[("Learn idyll", false), ("Render through the membrane", true)]);
    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(spawn_live(|ctx| todos(ctx, seed), ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    driver.take_commands()
}

// The page is the root component: its mount stream is subject to the same fold
// contract as any live's, and its paint declares the live as first-class
// IR nodes — the chrome and the markers travel in ONE stream now.
#[test]
fn the_page_mount_paints_chrome_and_declares_its_islands() {
    let seed = seeded_data(&[("Learn idyll", false)]);
    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = rt.ctx::<idyll::Never>();
    rt.spawn(spawn_live(|ctx| todo_app::page(ctx, seed), ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    let commands = driver.take_commands();

    let live: Vec<&str> = commands
        .iter()
        .filter_map(|c| match c {
            DomCommand::ReplaceTemplate { template, .. } => Some(template),
            _ => None,
        })
        .flat_map(|t| t.nodes.iter())
        .filter_map(|node| match node {
            TplNode::Live { name, .. } => Some(name.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(live, ["board"], "the todos page declares its store root: {commands:#?}");

    let text: Vec<String> = commands
        .iter()
        .filter_map(|c| match c {
            DomCommand::ReplaceTemplate { template, .. } => Some(template),
            _ => None,
        })
        .flat_map(|t| t.nodes.iter())
        .filter_map(|node| match node {
            TplNode::Text(text) => Some(text.to_string()),
            _ => None,
        })
        .collect();
    assert!(text.iter().any(|t| t == "Todos"), "the chrome heading paints: {text:?}");
    assert!(
        text.iter().any(|t| t == "Prose gauntlet"),
        "the cross-page link paints: {text:?}"
    );

    // The route is a mount-time value: the page's own paint is STATIC — chrome and
    // markers only. No slots (the route match is not a live binding), no handlers.
    let slots: Vec<&TplNode> = commands
        .iter()
        .filter_map(|c| match c {
            DomCommand::ReplaceTemplate { template, .. } => Some(template),
            _ => None,
        })
        .flat_map(|t| t.nodes.iter())
        .filter(|node| {
            matches!(
                node,
                TplNode::TextSlot(_)
                    | TplNode::AnchorSlot(_)
                    | TplNode::Element { slot: Some(_), .. }
            )
        })
        .collect();
    assert!(slots.is_empty(), "the page paint claims liveness: {slots:?}");
    assert!(
        !commands.iter().any(|c| matches!(
            c,
            DomCommand::BindSlot { .. } | DomCommand::AddEventListener { .. }
        )),
        "the page mount binds live machinery: {commands:?}"
    );
}

/// The slot ids a template declares: element slots, text slots, anchor slots.
fn slots_of(template: &Template) -> Vec<u32> {
    template
        .nodes
        .iter()
        .filter_map(|node| match node {
            TplNode::Element { slot, .. } => slot.map(|s| s.0),
            TplNode::TextSlot(slot) | TplNode::AnchorSlot(slot) => Some(slot.0),
            TplNode::Text(_) | TplNode::Live { .. } => None,
        })
        .collect()
}

#[test]
fn the_mount_stream_upholds_the_fold_contract() {
    let commands = mount_stream();
    for (i, c) in commands.iter().enumerate() {
        println!("{i:3}  {c:?}");
    }

    // (1) Self-contained: the stream opens by registering the root template (id 0).
    assert!(
        matches!(commands.first(), Some(DomCommand::ReplaceTemplate { template_id, .. }) if template_id.0 == 0),
        "stream must open with the root template registration"
    );

    // (2) + (3): replay the stream the way both folds do — a template registry built
    // in-band, one transient slot scratch refreshed at each instantiation, and every
    // consumer id introduced before use.
    let mut templates: Vec<Template> = Vec::new();
    let mut scratch: Vec<u32> = Vec::new();
    let mut known: Vec<NodeId> = Vec::new();
    for c in &commands {
        match c {
            DomCommand::ReplaceTemplate { template_id, template } => {
                let idx = template_id.0 as usize;
                if templates.len() <= idx {
                    templates.resize(idx + 1, Template::EMPTY);
                }
                templates[idx] = template.clone();
                if idx == 0 {
                    scratch = slots_of(template); // root registration = root claim
                }
            }
            DomCommand::MountFragment { anchor_id, template }
            | DomCommand::ReplaceFragment { anchor_id, template } => {
                known.push(*anchor_id); // anchors materialize on first structural use
                let template = templates
                    .get(template.0 as usize)
                    .unwrap_or_else(|| panic!("fragment references unregistered {template:?}"));
                scratch = slots_of(template);
            }
            DomCommand::BindSlot { slot, node_id } => {
                assert!(
                    scratch.contains(&slot.0),
                    "BindSlot({}) doesn't resolve against the current template's slots {scratch:?}",
                    slot.0
                );
                known.push(*node_id);
            }
            DomCommand::MoveFragment { anchor_id, after_anchor } => {
                known.push(*anchor_id);
                assert!(
                    known.contains(after_anchor),
                    "MoveFragment after unknown anchor {after_anchor:?}"
                );
            }
            // Free-last: released ids stop resolving, so any later consuming op on
            // one fails the known-id assertions below — the teardown-ordering
            // invariant, pinned in the same replay both folds implement.
            DomCommand::FreeNodes { node_ids } => {
                known.retain(|id| !node_ids.contains(id));
            }
            DomCommand::SetText { node_id, .. }
            | DomCommand::SetAttr { node_id, .. }
            | DomCommand::RemoveAttr { node_id, .. }
            | DomCommand::SetBoolAttr { node_id, .. }
            | DomCommand::AddEventListener { node_id, .. }
            | DomCommand::RemoveEventListener { node_id, .. } => {
                assert!(known.contains(node_id), "command targets an unknown node id: {c:?}");
            }
            _ => {}
        }
    }
    assert!(!known.is_empty(), "mount stream introduced no nodes");

    // Styles ride the stream **in-band**, on the templates whose elements reference
    // them: `css=[…]` classes appear in template attrs and their rules in
    // `Template::styles` (resolved text here — native; a wasm guest carries names
    // only). Content-dedupe holds with styles in the identity: three rows, ONE
    // row-template announcement.
    let styled: Vec<&Template> = commands
        .iter()
        .filter_map(|c| match c {
            DomCommand::ReplaceTemplate { template, .. } if !template.styles.is_empty() => {
                Some(template)
            }
            _ => None,
        })
        .collect();
    // A shorthand is authoring spelling; the row's atoms are the longhands it means.
    let item_rule = "padding-top:0.35rem";
    let row_templates: Vec<_> = styled
        .iter()
        .filter(|t| {
            t.styles
                .iter()
                .any(|r| r.css.as_deref().is_some_and(|css| css.contains(item_rule)))
        })
        .collect();
    assert_eq!(
        row_templates.len(),
        1,
        "one announcement for three identical styled rows: {styled:?}"
    );
    let row = row_templates[0];
    let rule = &row.styles[0];
    assert!(
        row.nodes.iter().any(|node| matches!(
            node,
            TplNode::Element { attrs, .. }
                if attrs.iter().any(|a| a.name == "class" && a.value.contains(&*rule.name))
        )),
        "the row's class attr names the rule that rides with it: {row:?}"
    );

    // (4) The server fold of this exact stream is the clean document.
    let html = fold_html(&commands);
    assert!(html.contains("Learn idyll"), "fold lost content: {}", html.as_str());
    assert!(
        html.contains("Render through the membrane"),
        "fold lost content: {}",
        html.as_str()
    );
    // Done styling is a reactive css entry (`css=[ITEM, $done => DONE]`): the done
    // row's class attr carries the variant classes, and the variant's rule text rode
    // in-band (rule_union ships last-wins losers too, so a client-side flip has its
    // rule present).
    assert!(
        html.contains("-done-text-decoration"),
        "the done row lost its variant classes: {}",
        html.as_str()
    );
    let done_rule_announced = commands.iter().any(|c| match c {
        DomCommand::ReplaceTemplate { template, .. } => template
            .styles
            .iter()
            .any(|r| r.name.contains("-done-text-decoration") && r.css.is_some()),
        _ => false,
    });
    assert!(done_rule_announced, "the DONE rule text must ride the stream in-band");
    for scaffolding in ["data-s", "idyll-t", "<!--"] {
        assert!(
            !html.contains(scaffolding),
            "fold leaked `{scaffolding}`: {}",
            html.as_str()
        );
    }
}

/// The prose live's server paint must include **every** fragment kind's initial
/// content: the SignalVec rows, the keyed rows, and the kept `@if` branch.
#[test]
fn prose_mount_paints_every_fragment_kind() {
    let seed = seeded_data(&[("Learn idyll", false)]);
    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = rt.ctx::<ProseMsg>();
    rt.spawn(spawn_live(|ctx| prose(ctx, seed), ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    let commands = driver.take_commands();
    for (i, c) in commands.iter().enumerate() {
        println!("{i:3}  {c:?}");
    }
    let mut fold = idyll::HtmlFold::new();
    for command in &commands {
        fold.apply(command);
    }
    let html = fold.html();

    assert!(html.contains("hydrate"), "report rows lost: {}", html.as_str());
    assert!(
        html.contains(r#"<ol id="keyed"><li>1</li><li>2</li><li>3</li></ol>"#),
        "keyed rows lost: {}",
        html.as_str()
    );
    assert!(html.contains("kept content"), "kept @if branch lost: {}", html.as_str());
}

/// The stream contract **past the initial mount** — the region every fold divergence
/// found by review lived in. One prose mount, then: a keep-branch toggle off and back
/// (`DetachFragment`/`AttachFragment`), a keyed reverse (`MoveFragment`s), and the
/// resulting frees. The whole accumulated stream folds to the same document a fresh
/// mount of the final state would paint — the totality claim, pinned.
#[test]
fn the_stream_stays_foldable_through_detach_attach_and_moves() {
    let seed = seeded_data(&[("Learn idyll", false)]);
    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = rt.ctx::<ProseMsg>();
    let sender = ctx.inbox_sender();
    rt.spawn(spawn_live(|ctx| prose(ctx, seed), ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);

    let drive = |rt: &mut Runtime, driver: &mut CommandBufferDriver, msg: ProseMsg| {
        sender.send(msg);
        rt.run_to_quiescence();
        rt.flush(driver);
    };
    drive(&mut rt, &mut driver, ProseMsg::Toggle); // keep-branch detaches
    drive(&mut rt, &mut driver, ProseMsg::Reverse); // keyed rows move
    drive(&mut rt, &mut driver, ProseMsg::Toggle); // keep-branch re-attaches

    let commands = driver.take_commands();
    let has = |pred: fn(&DomCommand) -> bool| commands.iter().any(pred);
    assert!(has(|c| matches!(c, DomCommand::DetachFragment { .. })), "toggle-off detaches");
    assert!(has(|c| matches!(c, DomCommand::AttachFragment { .. })), "toggle-on re-attaches");
    assert!(has(|c| matches!(c, DomCommand::MoveFragment { .. })), "reverse moves rows");

    let mut folded = idyll::HtmlFold::new();
    for command in &commands {
        folded.apply(command);
    }
    let html = folded.html();
    assert!(
        html.contains(r#"<ol id="keyed"><li>3</li><li>2</li><li>1</li></ol>"#),
        "the fold tracked the moves: {}",
        html.as_str()
    );
    assert!(
        html.contains("kept content"),
        "the re-attached keep branch serializes again: {}",
        html.as_str()
    );

    // The detached interval is honest too: fold only through the first toggle and the
    // kept branch must NOT serialize.
    let upto_reverse = commands
        .iter()
        .take_while(|c| !matches!(c, DomCommand::MoveFragment { .. }))
        .collect::<Vec<_>>();
    let mut partial = idyll::HtmlFold::new();
    for command in upto_reverse {
        partial.apply(command);
    }
    assert!(
        !partial.html().contains("kept content"),
        "a detached keep branch must serialize nothing: {}",
        partial.html().as_str()
    );

    // A tester firing the same messages against a fresh mount lands on the same
    // document — the accumulated stream and the replayed one agree.
    let seed = seeded_data(&[("Learn idyll", false)]);
    let mut rt2 = Runtime::new();
    let mut driver2 = CommandBufferDriver::new();
    let ctx2 = rt2.ctx::<ProseMsg>();
    let sender2 = ctx2.inbox_sender();
    rt2.spawn(spawn_live(|ctx| prose(ctx, seed), ctx2, idyll::component::report_to_log));
    rt2.run_once();
    rt2.process_pending_view(&mut driver2);
    rt2.flush(&mut driver2);
    for msg in [ProseMsg::Toggle, ProseMsg::Reverse, ProseMsg::Toggle] {
        sender2.send(msg);
        rt2.run_to_quiescence();
        rt2.flush(&mut driver2);
    }
    let mut replayed = idyll::HtmlFold::new();
    for command in &driver2.take_commands() {
        replayed.apply(command);
    }
    assert_eq!(html.as_str(), replayed.html().as_str(), "two mounts, one document");
}

/// The **mutation round-trip** contract, end to end minus the network: the Add click
/// produces a `ServerRequest` carrying the persisted operation's `OpHash` and **no
/// row**; the row mounts when the response envelope's **refresh seed** replays into
/// the store — record data never rides the outcome channel.
#[test]
fn a_mutation_round_trips_as_request_out_then_seed_driven_row_in() {
    use idyll::driver::HandlerId;
    use idyll::Mutation as _;
    use idyll_data::Store;
    use todo_app::AddTodoOp;

    let seed = seeded_data(&[("Learn idyll", false)]);
    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = rt.ctx::<Msg>();
    // Stand in for the board: the store and its absorb rooted on this component —
    // the refresh applies in its turn, before any queued message dequeues.
    Store::provide(&ctx, &seed).expect("a hand-built seed replays");
    rt.spawn(spawn_live(|ctx| todos(ctx, seed), ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    let mount = driver.take_commands(); // the mount stream — its handlers, not its shape
    let handler_for = |want: &str| {
        mount
            .iter()
            .find_map(|c| match c {
                DomCommand::AddEventListener { event_type, handler_id, .. } if event_type == want => {
                    Some(*handler_id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {want} handler in the mount: {mount:?}"))
    };

    // Type a todo, then click Add: the draft feeds the mutation's text (the field is
    // the live's own state, the row is the store's).
    let draft = handler_for("input");
    driver.dispatch_event(
        HandlerId(draft.0),
        idyll::Event { target_value: Some("Write a test".into()), key: None, timestamp: None, rect: None },
    );
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    driver.take_commands(); // the draft echo — the input reflects its own value

    let add = handler_for("click");
    driver.dispatch_event(HandlerId(add.0), idyll::Event { target_value: None, key: None, timestamp: None, rect: None });
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    let commands = driver.take_commands();

    // Commands out: the request carries ONLY the artifact's hash + typed vars, and
    // nothing mounted — the live renders the store, it doesn't invent rows.
    let request_id = commands
        .iter()
        .find_map(|c| match c {
            DomCommand::ServerRequest { request_id, op, args } => {
                assert_eq!(*op, AddTodoOp::op_hash());
                let args: serde_json::Value = serde_json::from_slice(args).unwrap();
                assert_eq!(args, serde_json::json!({ "text": "Write a test" }));
                Some(*request_id)
            }
            _ => None,
        })
        .expect("Add must emit a ServerRequest: {commands:?}");
    assert!(
        !commands.iter().any(|c| matches!(c, DomCommand::MountFragment { .. })),
        "no row may mount before the server answers: {commands:?}"
    );

    // The response envelope: the masked outcome plus the re-executed route query.
    // The seed replays into the store BEFORE the outcome message lands, so the
    // projection has already re-synced by the time the loop hears `Added(Ok)` —
    // whose payload the loop ignores entirely.
    let envelope = serde_json::json!({
        "outcome": { "id": 42, "text": "New todo", "done": false },
        "seed": {
            "seed": { "commits": [
                { "Commit": { "type_tag": "Page",
                    "json": {
                        "id": "/", "title": "Todos",
                        "route": { "Todos": { "todos": [1, 42] } },
                    } } },
                { "Commit": { "type_tag": "Todo",
                    "json": { "id": 42, "text": "New todo", "done": false } } },
            ] },
            "roots": { "route": "/" },
        },
    });
    assert!(rt.deliver_response(request_id, Ok(serde_json::to_vec(&envelope).unwrap())));
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    let commands = driver.take_commands();

    assert!(
        commands.iter().any(|c| matches!(c, DomCommand::MountFragment { .. })),
        "the seed-driven row must mount: {commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, DomCommand::SetText { text, .. } if text == "New todo")),
        "the row must carry the server's text: {commands:?}"
    );

    // A second delivery of the same id is a no-op (the continuation is single-shot).
    assert!(!rt.deliver_response(request_id, Err(idyll::RequestError::Transport("dup".into()))));
}

/// **The namespace rides with the template.** A fragment's IR begins at its row's tag,
/// and the row is built while its anchor is still parked off-tree — positioned only
/// afterwards — so neither the nodes nor the DOM can say which namespace to create it
/// in. The compiler knows from the lexical nesting, and says so here; `foreignObject`
/// is the door back out, so a region inside one is HTML again even though it sits
/// within the `<svg>`.
#[test]
fn a_fragment_carries_the_namespace_it_was_written_in() {
    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = rt.ctx::<todo_app::PlotMsg>();
    rt.spawn(spawn_live(
        |ctx| todo_app::plot(ctx, seeded_data(&[("unused", false)])),
        ctx,
        idyll::component::report_to_log,
    ));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    let commands = driver.take_commands();

    let templates: Vec<&Template> = commands
        .iter()
        .filter_map(|c| match c {
            DomCommand::ReplaceTemplate { template, .. } => Some(template),
            _ => None,
        })
        .collect();

    let first_tag = |t: &Template| match t.nodes.first() {
        Some(TplNode::Element { tag, .. }) => tag.to_string(),
        _ => String::new(),
    };
    let svg_of = |tag: &str| {
        templates.iter().find(|t| first_tag(t) == tag).map(|t| t.svg)
    };

    // The root owns the `<svg>` element itself, so its own nodes are HTML — entering
    // the namespace is that element's job, not the template's.
    assert_eq!(svg_of("svg"), Some(false), "the root template is HTML");
    assert_eq!(svg_of("g"), Some(true), "a row written inside <svg> is SVG");
    assert_eq!(
        svg_of("span"),
        Some(false),
        "a row written inside <foreignObject> is HTML again"
    );
}

/// Invariant 5. A fallback is a subtree hanging off a `Live`, so the pre-order walk has
/// to consume exactly it — no more, no less. Both failure modes are visible in the
/// serialized document: step too few nodes and the fallback's tail leaks out beside the
/// wrapper; step too many and the content after the marker is swallowed into it.
#[test]
fn a_live_fallback_is_a_subtree_the_pre_order_walk_steps_over() {
    use idyll::template::TplAttr;
    use idyll::View;

    let fallback = View::element(
        "div",
        [TplAttr { name: "class".into(), value: "fallback".into() }],
        View::text("stood in"),
    );
    // Two levels deep, with a sibling after each: the outer element's child count is
    // reached by *skipping* the inner one's subtree, and that skip is what has to know a
    // live carries a fallback. One level is not enough — the marker's own count is read
    // directly there, so a walker that forgot the fallback would still land right.
    let content = View::element(
        "div",
        [],
        View::element(
            "section",
            [],
            View::text("before")
                .append(View::live_mount("sim", Some("k".into()), fallback))
                .append(View::text("after")),
        )
        .append(View::text("tail")),
    );

    let mut fold = idyll::HtmlFold::new();
    fold.apply(&DomCommand::ReplaceTemplate {
        template_id: idyll::driver::TemplateId(0),
        template: content.into_template(),
    });
    fold.apply(&DomCommand::MountRoot { template_id: idyll::driver::TemplateId(0) });

    assert_eq!(
        fold.html().as_str(),
        "<div><section>before\
         <idyll-live data-i=\"sim\" data-k=\"k\" style=\"display:contents\">\
         <div class=\"fallback\">stood in</div>\
         </idyll-live>\
         after</section>tail</div>",
        "the fallback sits in its wrapper, `after` closes the section, `tail` follows it"
    );
}
