//! The client fold, pinned without a browser: real live mount streams (the todo
//! app's `todos` and `prose` — the claim-hardening gauntlet) are written as fixtures
//! in the **wire encoding `runtime.js` consumes**, together with the server fold of
//! the same stream; the node/jsdom harness (`runtime/harness/harness.mjs`) then
//! asserts build/claim convergence: build must equal the server fold node for node,
//! claim must adopt the server DOM with identity, and post-claim patches must land
//! on adopted nodes.

use std::process::Command;

use idyll::component::spawn_live;
use idyll::driver::DomCommand;
use idyll::template::{Template, TplNode};
use idyll::{fold_html, CommandBufferDriver, Ctx, Runtime, Setup};
use serde_json::json;
use todo_app::{plot, prose, todos, Msg, PageSeed, PlotMsg, ProseMsg};

fn seeded_data() -> PageSeed {
    let payload = json!({
        "seed": { "commits": [
            { "Commit": { "type_tag": "Page",
                "json": {
                    "id": "/", "title": "Todos",
                    "route": { "Todos": { "todos": [1, 2] } },
                } } },
            { "Commit": { "type_tag": "Todo",
                "json": { "id": 1, "text": "Learn idyll", "done": true } } },
            { "Commit": { "type_tag": "Todo",
                "json": { "id": 2, "text": "Render through the membrane", "done": false } } },
        ] },
        "roots": { "route": "/" }
    });
    serde_json::from_value(payload).unwrap()
}

fn mount_stream<M, Fut>(root: impl FnOnce(Ctx<Setup, M>) -> Fut + 'static) -> Vec<DomCommand>
where
    M: 'static,
    Fut: std::future::Future<Output = idyll::Result> + 'static,
{
    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    // A root mount, through the seam a real mount uses — this harness is standing in
    // for `guest!`'s glue, so it starts a context tree the same way.
    let ctx = Ctx::<Setup, M>::for_mount(&rt, None);
    rt.spawn(spawn_live(root, ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    driver.take_commands()
}

/// The jco variant encoding `runtime.js` folds: `{ tag, val }` with kebab-case tags
/// and camelCase fields. Exhaustive on both mirrors, so a new `DomCommand` or
/// `TplNode` variant fails here until the wire encoding is decided.
fn wire_template(template: &Template) -> serde_json::Value {
    let nodes: Vec<serde_json::Value> = template
        .nodes
        .iter()
        .map(|node| match node {
            TplNode::Text(text) => json!({ "tag": "text", "val": text }),
            TplNode::TextSlot(slot) => json!({ "tag": "text-slot", "val": slot.0 }),
            TplNode::AnchorSlot(slot) => json!({ "tag": "anchor-slot", "val": slot.0 }),
            TplNode::Element { tag, attrs, slot, children } => json!({
                "tag": "element",
                "val": {
                    "tag": tag,
                    "attrs": attrs
                        .iter()
                        .map(|attr| json!({ "name": attr.name, "value": attr.value }))
                        .collect::<Vec<_>>(),
                    "slot": slot.map(|s| s.0),
                    "children": children,
                },
            }),
            TplNode::Live { name, key, fallback } => json!({
                "tag": "live",
                "val": { "name": name, "key": key, "fallback": fallback },
            }),
        })
        .collect();
    serde_json::Value::Array(nodes)
}

fn wire_command(command: &DomCommand) -> serde_json::Value {
    match command {
        DomCommand::ReplaceTemplate { template_id, template } => json!({
            "tag": "replace-template",
            "val": {
                "templateId": template_id.0,
                "nodes": wire_template(template),
                "svg": template.svg,
                "styles": template
                    .styles
                    .iter()
                    .map(|rule| json!({ "name": rule.name, "css": rule.css }))
                    .collect::<Vec<_>>(),
            },
        }),
        DomCommand::MountRoot { template_id } => {
            json!({ "tag": "mount-root", "val": template_id.0 })
        }
        DomCommand::BindSlot { slot, node_id } => {
            json!({ "tag": "bind-slot", "val": { "slot": slot.0, "node": node_id.0 } })
        }
        DomCommand::SetText { node_id, text } => {
            json!({ "tag": "set-text", "val": { "node": node_id.0, "text": text } })
        }
        DomCommand::SetAttr { node_id, name, value } => json!({
            "tag": "set-attr",
            "val": { "node": node_id.0, "name": name, "value": value },
        }),
        DomCommand::SetStyleProp { node_id, name, value } => json!({
            "tag": "set-style-prop",
            "val": { "node": node_id.0, "name": name, "value": value },
        }),
        DomCommand::RemoveAttr { node_id, name } => {
            json!({ "tag": "remove-attr", "val": { "node": node_id.0, "name": name } })
        }
        DomCommand::SetBoolAttr { node_id, name, value } => json!({
            "tag": "set-bool-attr",
            "val": { "node": node_id.0, "name": name, "value": value },
        }),
        DomCommand::MountFragment { anchor_id, template } => json!({
            "tag": "mount-fragment",
            "val": { "anchor": anchor_id.0, "template": template.0 },
        }),
        DomCommand::ReplaceFragment { anchor_id, template } => json!({
            "tag": "replace-fragment",
            "val": { "anchor": anchor_id.0, "template": template.0 },
        }),
        DomCommand::RemoveFragment { anchor_id } => {
            json!({ "tag": "remove-fragment", "val": anchor_id.0 })
        }
        DomCommand::DetachFragment { anchor_id } => {
            json!({ "tag": "detach-fragment", "val": anchor_id.0 })
        }
        DomCommand::AttachFragment { anchor_id } => {
            json!({ "tag": "attach-fragment", "val": anchor_id.0 })
        }
        DomCommand::MoveFragment { anchor_id, after_anchor } => json!({
            "tag": "move-fragment",
            "val": { "anchor": anchor_id.0, "after": after_anchor.0 },
        }),
        DomCommand::AddEventListener { node_id, event_type, handler_id } => json!({
            "tag": "add-event-listener",
            "val": { "node": node_id.0, "eventType": event_type, "handler": handler_id.0 },
        }),
        DomCommand::RemoveEventListener { node_id, event_type, handler_id } => json!({
            "tag": "remove-event-listener",
            "val": { "node": node_id.0, "eventType": event_type, "handler": handler_id.0 },
        }),
        DomCommand::WatchMeasure { node_id, handler_id } => json!({
            "tag": "watch-measure",
            "val": { "node": node_id.0, "handler": handler_id.0 },
        }),
        DomCommand::UnwatchMeasure { node_id, handler_id } => json!({
            "tag": "unwatch-measure",
            "val": { "node": node_id.0, "handler": handler_id.0 },
        }),
        DomCommand::Paint { node_id, layers, inks, deltas } => json!({
            "tag": "paint",
            "val": {
                "node": node_id.0,
                "layers": layers,
                "inks": inks,
                "deltas": deltas
                    .iter()
                    .map(|(layer, changes, len)| {
                        json!({ "layer": layer, "changes": changes, "len": len })
                    })
                    .collect::<Vec<_>>(),
            },
        }),
        DomCommand::ServerRequest { request_id, op, args } => json!({
            "tag": "server-request",
            "val": { "request": request_id.0, "msb": op.msb(), "lsb": op.lsb(), "args": args },
        }),
        DomCommand::Navigate { request_id, op, path } => json!({
            "tag": "navigate",
            "val": { "request": request_id.0, "msb": op.msb(), "lsb": op.lsb(), "path": path },
        }),
        DomCommand::WatchNavigation { handler_id } => {
            json!({ "tag": "watch-navigation", "val": handler_id.0 })
        }
        DomCommand::WatchSize { handler_id } => {
            json!({ "tag": "watch-size", "val": handler_id.0 })
        }
        DomCommand::StartTicks { handler_id, interval_ms } => json!({
            "tag": "start-ticks",
            "val": { "handler": handler_id.0, "intervalMs": interval_ms },
        }),
        DomCommand::StopTicks { handler_id } => {
            json!({ "tag": "stop-ticks", "val": handler_id.0 })
        }
        DomCommand::FreeNodes { node_ids } => json!({
            "tag": "free-nodes",
            "val": node_ids.iter().map(|n| n.0).collect::<Vec<_>>(),
        }),
    }
}

fn npm() -> &'static str {
    if cfg!(windows) { "npm.cmd" } else { "npm" }
}

/// The prose live driven through its gauntlet after mounting: keep-branch detach,
/// keyed reverse while detached, re-attach — the full stream, teardown included.
fn prose_driven_stream() -> Vec<DomCommand> {
    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = Ctx::<Setup, ProseMsg>::for_mount(&rt, None);
    let sender = ctx.inbox_sender();
    rt.spawn(spawn_live(
        |ctx| prose(ctx, seeded_data()),
        ctx,
        idyll::component::report_to_log,
    ));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    for msg in [ProseMsg::Toggle, ProseMsg::Reverse, ProseMsg::Toggle] {
        sender.send(msg);
        rt.run_to_quiescence();
        rt.flush(&mut driver);
    }
    driver.take_commands()
}

#[test]
fn the_client_fold_converges_with_the_server_fold() {
    let fixtures = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("fold-fixtures");
    std::fs::create_dir_all(&fixtures).unwrap();

    for (name, commands, build_only) in [
        ("todos", mount_stream::<Msg, _>(|ctx| todos(ctx, seeded_data())), false),
        ("prose", mount_stream::<ProseMsg, _>(|ctx| prose(ctx, seeded_data())), false),
        ("plot", mount_stream::<PlotMsg, _>(|ctx| plot(ctx, seeded_data())), false),
        // The driven stream: mount plus the teardown ops (detach, moves while
        // detached, re-attach) both folds must agree on — the region where every
        // divergence found by review has lived. Build-only: a post-interaction fold
        // is not an SSR document, so there is nothing to claim.
        ("prose-driven", prose_driven_stream(), true),
    ] {
        let html = fold_html(&commands);
        let fixture = json!({
            "name": name,
            "commands": commands.iter().map(wire_command).collect::<Vec<_>>(),
            "html": html.as_str(),
            "buildOnly": build_only,
        });
        std::fs::write(
            fixtures.join(format!("{name}.json")),
            serde_json::to_vec_pretty(&fixture).unwrap(),
        )
        .unwrap();
    }

    let harness = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/harness");
    if !harness.join("node_modules").exists() {
        let install = Command::new(npm())
            .args(["ci", "--no-audit", "--no-fund"])
            .current_dir(&harness)
            .output()
            .expect("npm is available (the server build already requires node)");
        assert!(
            install.status.success(),
            "npm install failed:\n{}",
            String::from_utf8_lossy(&install.stderr)
        );
    }

    let run = Command::new("node")
        .arg(harness.join("harness.mjs"))
        .arg(&fixtures)
        .current_dir(&harness)
        .output()
        .expect("node runs");
    println!("{}", String::from_utf8_lossy(&run.stdout));
    assert!(
        run.status.success(),
        "the client fold diverged:\n{}\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}
