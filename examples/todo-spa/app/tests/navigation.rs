//! The **navigation round-trip** contract, end to end minus the network: mounting
//! the SPA live emits `WatchNavigation`; a navigation intent (the path, as an
//! event) produces a `Navigate` command carrying the route query's `OpHash`; the
//! response — the executed `Preloaded` payload, whole — replays into the store,
//! moves the current page, and the live's `@match` swaps its arm. Navigation is
//! messages out, data in; nothing else crosses.

use idyll::component::spawn_live;
use idyll::{CommandBufferDriver, DomCommand, DomDriver as _, Query as _, Runtime};
use todo_spa_app::{app, Msg, PageSeed, RouteQuery};

fn seed(path: &str, route: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "seed": { "commits": [
            { "Commit": { "type_tag": "Page",
                "json": { "id": path, "title": "Todos — SPA", "route": route } } },
            { "Commit": { "type_tag": "Todo",
                "json": { "id": 1, "text": "Learn idyll", "done": false } } },
        ] },
        "roots": { "route": path }
    })
}

#[test]
fn a_navigation_round_trips_as_intent_out_then_seed_driven_arm_swap() {
    let boot: PageSeed =
        serde_json::from_value(seed("/", serde_json::json!({ "Todos": { "todos": [1] } })))
            .unwrap();

    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(spawn_live(|ctx| app(ctx, boot), ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    let commands = driver.take_commands();

    // Mounting subscribes to navigation intents, exactly once.
    let watch = commands
        .iter()
        .filter_map(|c| match c {
            DomCommand::WatchNavigation { handler_id } => Some(*handler_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(watch.len(), 1, "one navigation subscription: {commands:?}");

    // The intent arrives as an event whose value is the path…
    driver.dispatch_event(
        watch[0],
        idyll::Event { target_value: Some("/about".into()), key: None, timestamp: None, rect: None },
    );
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    let commands = driver.take_commands();

    // …and the loop answers with a `Navigate` carrying the persisted artifact's hash.
    let request_id = commands
        .iter()
        .find_map(|c| match c {
            DomCommand::Navigate { request_id, op, path } => {
                assert_eq!(*op, RouteQuery::op_hash());
                assert_eq!(path, "/about");
                Some(*request_id)
            }
            _ => None,
        })
        .expect("the intent must emit a Navigate command: {commands:?}");

    // The response is the executed route query, whole: records replay, the current
    // page moves, the `@match` swaps to the About arm.
    let about = seed("/about", serde_json::json!("About"));
    assert!(rt.deliver_response(request_id, Ok(serde_json::to_vec(&about).unwrap())));
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    let commands = driver.take_commands();

    let swapped = commands.iter().any(|c| matches!(
        c,
        DomCommand::ReplaceTemplate { template, .. }
            if template.nodes.iter().any(|n| matches!(
                n,
                idyll::template::TplNode::Text(t) if t.contains("One live owns this page")
            ))
    ));
    assert!(swapped, "the About arm must mount from the replayed data: {commands:?}");
}
