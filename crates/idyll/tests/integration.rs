//! Integration tests: full component lifecycle, signal reactivity, callbacks.

/// A scope for a test: a `Ctx` owns one, and a test has no component to receive one
/// from — the same mint production uses.
fn test_scope() -> (idyll::Runtime, idyll::Owner) {
    let rt = idyll::Runtime::new();
    let owner = rt.ctx::<()>().owner();
    (rt, owner)
}

use idyll::{
    component::{report_to_log, spawn_live},
    driver::DomOp,
    component, key, replay_component, live_view, CommandBufferDriver, Ctx, DomCommand, Event,
    MessageLog,
    MockDriver, Rect, ReplayInputs, Result, Runtime, Setup, MutableVec,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn all_set_texts(driver: &MockDriver) -> Vec<String> {
    driver
        .log
        .iter()
        .filter_map(|op| {
            if let DomOp::SetText { text, .. } = op {
                Some(text.clone())
            } else {
                None
            }
        })
        .collect()
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

/// Minimal IR → pseudo-html for structural assertions (slots render as nothing).
fn ir_html(template: &idyll::template::Template) -> String {
    use idyll::template::TplNode;
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

fn count_ops(driver: &MockDriver, f: impl Fn(&DomOp) -> bool) -> usize {
    driver.log.iter().filter(|op| f(op)).count()
}


#[test]
fn client_effect_runs_post_mount_and_emits_a_message() {
    #[derive(Debug)]
    enum Msg {
        Loaded(i32),
    }

    async fn component(ctx: Ctx<Setup, Msg>) -> Result {
        let n = ctx.mutable_signal(0);
    let n_v = n.read();
        let mut ctx = ctx
            // A real effect would read `localStorage` via the `Client` token;
            // here we just emit the result as a message (the loop stays pure).
            .client_effect(|_client, sender| async move {
                sender.send(Msg::Loaded(42));
            })
            .render(live_view! { div { span { ($n_v) } } }).await?;
        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                Msg::Loaded(v) => n.set(&turn, v),
            }
        }
    }

    let mut driver = MockDriver::new();
    let mut rt = Runtime::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(spawn_live(component, ctx, report_to_log));
    rt.run_once(); // setup + render -> PENDING_VIEW
    rt.process_pending_view(&mut driver); // mount -> spawns the client effect
    rt.run_to_quiescence(); // effect emits Loaded(42); loop applies it
    rt.flush(&mut driver); // DOM patch: span text -> "42"

    // Initial mount paints "0"; the post-mount effect drives it to "42".
    assert_eq!(all_set_texts(&driver).first().map(String::as_str), Some("0"));
    assert_eq!(all_set_texts(&driver).last().map(String::as_str), Some("42"));
}

#[test]
fn pure_handler_maps_event_to_a_message() {
    #[derive(Debug)]
    enum Msg {
        Bumped,
    }

    async fn component(ctx: Ctx<Setup, Msg>) -> Result {
        let n = ctx.mutable_signal(0);
    let n_v = n.read();
        // Handlers are pure `Event -> Option<Msg>` — no `Client`, no effect. The click
        // becomes a message, so every state change flows through the (logged) loop.
        let mut ctx = ctx.render(live_view! {
            div {
                button onclick=>(|_ev| Some(Msg::Bumped)) { "go" }
                span { ($n_v) }
            }
        }).await?;
        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                Msg::Bumped => n.update(&turn, |v| *v += 1),
            }
        }
    }

    let mut driver = MockDriver::new();
    let mut rt = Runtime::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(spawn_live(component, ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver); // mount: registers click handler, paints "0"

    let handler = driver.latest_handler().expect("click handler registered");
    driver.fire(handler, Event::default()); // pure handler → Msg::Bumped into the inbox
    rt.run_to_quiescence(); // loop applies it -> n = 1
    rt.flush(&mut driver);

    assert_eq!(all_set_texts(&driver).last().map(String::as_str), Some("1"));
}

#[test]
fn ctx_message_log_records_delivered_messages() {
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    enum Msg {
        Start,
        Move(u32),
        Done,
    }

    async fn component(ctx: Ctx<Setup, Msg>, log_slot: Rc<RefCell<Option<MessageLog>>>) -> Result {
        let log = ctx.record_messages();
        *log_slot.borrow_mut() = Some(log);
        let mut ctx = ctx.render(live_view! { p { "log" } }).await?;
        let (msg, turn) = ctx.recv().await?;
        match msg {
            Msg::Start => {
                turn.emit(Msg::Move(1));
                turn.emit(Msg::Done);
            }
            other => panic!("unexpected first message: {other:?}"),
        }
        let (msg, _turn) = ctx.recv().await?;
        match msg {
            Msg::Move(1) => {}
            other => panic!("unexpected second message: {other:?}"),
        }
        let (msg, _turn) = ctx.recv().await?;
        match msg {
            Msg::Done => {}
            other => panic!("unexpected final message: {other:?}"),
        }
        std::future::pending().await
    }

    let log_slot = Rc::new(RefCell::new(None));
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();
    let log_slot_for_component = Rc::clone(&log_slot);
    rt.spawn(spawn_live(
        move |ctx| component(ctx, log_slot_for_component),
        ctx,
        report_to_log,
    ));
    rt.run_once();
    rt.process_pending_view(&mut driver);

    sender.send(Msg::Start);
    rt.run_to_quiescence();

    let log = log_slot.borrow().as_ref().unwrap().clone();
    let records = log.records();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].sequence, 0);
    assert_eq!(records[0].json, serde_json::json!("Start"));
    assert_eq!(records[1].sequence, 1);
    assert_eq!(records[1].json, serde_json::json!({ "Move": 1 }));
    assert_eq!(records[2].sequence, 2);
    assert_eq!(records[2].json, serde_json::json!("Done"));
    assert_eq!(log.to_json(), serde_json::to_value(records).unwrap());
    assert_eq!(
        log.decode::<Msg>().unwrap(),
        vec![Msg::Start, Msg::Move(1), Msg::Done]
    );
}

#[test]
fn replay_reconstructs_the_same_dom_from_the_same_message_log() {
    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    enum Msg {
        Inc,
        Dec,
    }

    async fn counter(ctx: Ctx<Setup, Msg>) -> Result {
        let n = ctx.mutable_signal(0i32);
    let n_v = n.read();
        let mut ctx = ctx.render(live_view! { output { ($n_v) } }).await?;
        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                Msg::Inc => n.update(&turn, |v| *v += 1),
                Msg::Dec => n.update(&turn, |v| *v -= 1),
            }
        }
    }

    let messages = vec![Msg::Inc, Msg::Inc, Msg::Dec, Msg::Inc];

    // Record a run, capturing its message log and folding its DOM stream to HTML.
    let mut first_driver = CommandBufferDriver::new();
    let mut rt = Runtime::new();
    let log = replay_component(&mut rt, &mut first_driver, counter, messages.clone());
    let first_html = idyll::fold_html(&first_driver.take_commands());

    // The log decodes back to exactly the messages we fed — the only artifact
    // replay needs (no state snapshots).
    let decoded: Vec<Msg> = log.decode().unwrap();
    assert_eq!(decoded, messages);

    // Replay that log into a fresh runtime/driver: identical DOM.
    let mut second_driver = CommandBufferDriver::new();
    let mut replay_rt = Runtime::new();
    replay_component(&mut replay_rt, &mut second_driver, counter, decoded);
    let second_html = idyll::fold_html(&second_driver.take_commands());

    assert_eq!(first_html.as_str(), second_html.as_str());
    assert!(first_html.as_str().contains("<output>2</output>"));
}

#[test]
fn ctx_replay_inputs_record_and_replay_time_and_random() {
    #[derive(Debug)]
    enum Msg {}

    let rt = Runtime::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let recording = ctx.record_replay_inputs();
    let first_now = ctx.now_millis().unwrap();
    let first_random = ctx.random_u64().unwrap();
    let records = recording.records();
    assert_eq!(records.len(), 2);

    let replay_ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    replay_ctx.replay_inputs(ReplayInputs::replay(records));

    assert_eq!(replay_ctx.now_millis().unwrap(), first_now);
    assert_eq!(replay_ctx.random_u64().unwrap(), first_random);
}

#[test]
fn dev_replay_component_feeds_messages_and_records_dom_patches() {
    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    enum Msg {
        Inc,
        Dec,
    }


    async fn component(ctx: Ctx<Setup, Msg>, start: i64) -> Result {
        let count = ctx.mutable_signal(start);
        let mut ctx = ctx.render(live_view! { span { $count } }).await?;
        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                Msg::Inc => count.update(&turn, |value| *value += 1),
                Msg::Dec => count.update(&turn, |value| *value -= 1),
            }
        }
    }

    let mut runtime = Runtime::new();
    let mut driver = MockDriver::new();
    let log = replay_component(
        &mut runtime,
        &mut driver,
        |ctx| component(ctx, 10),
        [Msg::Inc, Msg::Inc, Msg::Dec],
    );

    assert_eq!(
        log.decode::<Msg>().unwrap(),
        vec![Msg::Inc, Msg::Inc, Msg::Dec]
    );
    assert_eq!(
        all_set_texts(&driver),
        vec![
            "10".to_string(),
            "11".to_string(),
            "12".to_string(),
            "11".to_string()
        ]
    );
}

#[test]
fn replaying_a_log_into_an_edited_component_reconstructs_state() {
    // The hot-reload guarantee: after a code edit, re-instantiate the *new*
    // component and replay the recorded message log — state lands where it was,
    // even though the new compile numbers its slots differently. Replay
    // subsumes template-hot-swap, so slot ids need not be stable across edits.
    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    enum Msg {
        Inc,
        Dec,
    }

    // v1: the original component.
    async fn v1(ctx: Ctx<Setup, Msg>, start: i64) -> Result {
        let count = ctx.mutable_signal(start);
    let count_v = count.read();
        let mut ctx = ctx.render(live_view! { span { ($count_v) } }).await?;
        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                Msg::Inc => count.update(&turn, |v| *v += 1),
                Msg::Dec => count.update(&turn, |v| *v -= 1),
            }
        }
    }

    // v2: an "edit" — different markup and slot numbering (extra wrapper +
    // label), identical message handling.
    async fn v2(ctx: Ctx<Setup, Msg>, start: i64) -> Result {
        let count = ctx.mutable_signal(start);
    let count_v = count.read();
        let mut ctx = ctx.render(live_view! { div { strong { "count: " } span { ($count_v) } } }).await?;
        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                Msg::Inc => count.update(&turn, |v| *v += 1),
                Msg::Dec => count.update(&turn, |v| *v -= 1),
            }
        }
    }

    // Original session: drive v1, capture the delivered message log.
    let mut rt1 = Runtime::new();
    let mut d1 = MockDriver::new();
    let log = replay_component(&mut rt1, &mut d1, |ctx| v1(ctx, 10), [Msg::Inc, Msg::Inc, Msg::Dec]);
    let messages = log.decode::<Msg>().unwrap();
    assert_eq!(all_set_texts(&d1).last().map(String::as_str), Some("11"));

    // Code edit: remount v2 and replay the recorded log.
    let mut rt2 = Runtime::new();
    let mut d2 = MockDriver::new();
    let _ = replay_component(&mut rt2, &mut d2, |ctx| v2(ctx, 10), messages);

    // State reconstructed under the edited component.
    assert_eq!(all_set_texts(&d2).last().map(String::as_str), Some("11"));
}

#[test]
fn command_buffer_driver_records_replayed_component_commands() {
    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    enum Msg {
        Set(String),
    }


    async fn component(ctx: Ctx<Setup, Msg>) -> Result {
        let value = ctx.mutable_signal("initial".to_string());
        let value_for_view = value.read();
        let mut ctx = ctx.render(live_view! { span { ($value_for_view) } }).await?;
        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                Msg::Set(next) => value.set(&turn, next),
            }
        }
    }

    let mut runtime = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let log = replay_component(
        &mut runtime,
        &mut driver,
        component,
        [Msg::Set("updated".to_string())],
    );

    assert_eq!(
        log.decode::<Msg>().unwrap(),
        vec![Msg::Set("updated".to_string())]
    );
    assert!(driver.commands().iter().any(|command| {
        matches!(
            command,
            DomCommand::SetText {
                text,
                ..
            } if text == "updated"
        )
    }));
    assert!(driver.to_json().get("commands").is_some());
}

// ── Counter component ─────────────────────────────────────────────────────────

#[derive(Debug)]
enum CounterMsg {
    Inc,
    Dec,
}

async fn counter_component(ctx: Ctx<Setup, CounterMsg>, start: i64) -> Result {
    let n = ctx.mutable_signal(start);
    let nv = n.read();
    let mut ctx = ctx.render(live_view! { span { ($nv) } }).await?;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            CounterMsg::Inc => n.update(&turn, |v| *v += 1),
            CounterMsg::Dec => n.update(&turn, |v| *v -= 1),
        }
    }
}

#[test]
fn counter_initial_render() {
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, CounterMsg> = rt.ctx::<CounterMsg>();
    rt.spawn(spawn_live(|ctx| counter_component(ctx, 5i64), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["5"]);
}

#[test]
fn component_trait_spawns_with_tuple_arguments() {
    #[derive(Debug)]
    enum Msg {}

    async fn label(ctx: Ctx<Setup, Msg>, prefix: &'static str, value: i32) -> Result {
        let mut _ctx = ctx.render(live_view! {
            span { (format!("{prefix}:{value}")) }
        }).await?;
        std::future::pending().await
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();

    rt.spawn(spawn_live(|ctx| label(ctx, "n", 7), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);

    assert_eq!(all_set_texts(&driver), vec!["n:7"]);
}

#[test]
fn a_live_spawns_with_its_plain_arguments() {
    #[derive(Debug)]
    enum Msg {}

    async fn label(ctx: Ctx<Setup, Msg>, prefix: &'static str, value: i32) -> Result {
        let mut _ctx = ctx.render(live_view! {
            span { (format!("{prefix}:{value}")) }
        }).await?;
        std::future::pending().await
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();

    rt.spawn(spawn_live(|ctx| label(ctx, "n", 8), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);

    assert_eq!(all_set_texts(&driver), vec!["n:8"]);
}

#[test]
fn context_values_survive_render_transition() {
    #[derive(Debug)]
    enum Msg {
        Check,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();
    let seen = std::rc::Rc::new(std::cell::Cell::new(0));
    let seen_component = std::rc::Rc::clone(&seen);

    rt.spawn(spawn_live(
        move |ctx: Ctx<Setup, Msg>| async move {
            ctx.provide::<u32>(42);
            let mut ctx = ctx.render(live_view! { span { "context" } }).await?;
            let (msg, _turn) = ctx.recv().await?;
            match msg {
                Msg::Check => {
                    let value = ctx.use_context::<u32>().unwrap();
                    seen_component.set(*value);
                }
            }
            std::future::pending().await
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    sender.send(Msg::Check);
    rt.run_to_quiescence();

    assert_eq!(seen.get(), 42);
}

#[test]
fn delegated_event_mapper_delivers_messages() {
    #[derive(Debug)]
    enum Msg {
        Submit(String),
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let submitted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let submitted_component = std::rc::Rc::clone(&submitted);

    rt.spawn(spawn_live(
        move |ctx: Ctx<Setup, Msg>| async move {
            let mut ctx = ctx.render(live_view! {
                input onkeydown=>(key::enter(|e: idyll::Event| Msg::Submit(e.value())))
            }).await?;
            let (msg, _turn) = ctx.recv().await?;
            match msg {
                Msg::Submit(value) => submitted_component.borrow_mut().push(value),
            }
            std::future::pending().await
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    let handler = driver.latest_handler().unwrap();

    driver.fire(
        handler,
        idyll::Event {
            target_value: Some("ship".to_string()),
            key: Some("Escape".to_string()),
            timestamp: None,
            ..Default::default()
        },
    );
    rt.run_once();
    assert!(submitted.borrow().is_empty());

    driver.fire(
        handler,
        idyll::Event {
            target_value: Some("ship".to_string()),
            key: Some("Enter".to_string()),
            timestamp: None,
            ..Default::default()
        },
    );
    rt.run_to_quiescence();

    assert_eq!(&*submitted.borrow(), &["ship".to_string()]);
}

#[test]
fn removing_branch_unregisters_event_listener() {
    #[derive(Debug)]
    enum Msg {
        Clicked,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    // The shared signal lives on the SAME runtime as the component — per-runtime
    // cores mean a write marks only its own runtime's queues.
    let owner = ctx.owner();
    let show = owner.mutable_signal(true);
    let show_for_component = show.read();
    let clicks = std::rc::Rc::new(std::cell::Cell::new(0));
    let clicks_for_component = std::rc::Rc::clone(&clicks);

    rt.spawn(spawn_live(
        move |ctx: Ctx<Setup, Msg>| {
            let show = show_for_component.clone();
            let clicks = std::rc::Rc::clone(&clicks_for_component);
            async move {
                let mut ctx = ctx.render(live_view! {
                    @if ($show) {
                        button onclick=>(|_| Msg::Clicked) { "click" }
                    }
                }).await?;
                loop {
                    let (msg, _turn) = ctx.recv().await?;
                    match msg {
                        Msg::Clicked => clicks.set(clicks.get() + 1),
                    }
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    let handler = driver.latest_handler().unwrap();

    driver.fire(handler, idyll::Event::default());
    rt.run_once();
    assert_eq!(clicks.get(), 1);

    show.set(&idyll::Turn::for_test(), false);
    rt.flush(&mut driver);

    assert!(driver.log.iter().any(|op| {
        matches!(
            op,
            DomOp::RemoveEventListener {
                handler_id,
                event_type: "click",
                ..
            } if *handler_id == handler
        )
    }));

    driver.fire(handler, idyll::Event::default());
    rt.run_once();
    assert_eq!(clicks.get(), 1);
}

#[test]
fn counter_increments_three_times() {
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, CounterMsg> = rt.ctx::<CounterMsg>();
    let sender = ctx.inbox_sender();
    rt.spawn(spawn_live(|ctx| counter_component(ctx, 0i64), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);

    for _ in 0..3 {
        sender.send(CounterMsg::Inc);
        rt.run_once();
        rt.flush(&mut driver);
    }

    assert_eq!(all_set_texts(&driver), vec!["0", "1", "2", "3"]);
}

#[test]
fn signal_writes_schedule_one_microtask_flush() {
    #[derive(Debug)]
    enum Msg {}

    let rt = std::rc::Rc::new(std::cell::RefCell::new(Runtime::new()));
    let driver = std::rc::Rc::new(std::cell::RefCell::new(MockDriver::new()));
    let ctx: Ctx<Setup, Msg> = rt.borrow().ctx::<Msg>();
    let owner = ctx.owner();
    let signal = owner.mutable_signal(0);
    let signal_for_component = signal.read();
    Runtime::bind_flush_scheduler(std::rc::Rc::clone(&rt), std::rc::Rc::clone(&driver));

    rt.borrow_mut().spawn(spawn_live(
        move |ctx: Ctx<Setup, Msg>| {
            let signal = signal_for_component.clone();
            async move {
                let mut _ctx = ctx.render(live_view! { span { ($signal) } }).await?;
                std::future::pending().await
            }
        },
        ctx,
        report_to_log,
    ));
    rt.borrow_mut().run_once();
    rt.borrow_mut()
        .process_pending_view(&mut *driver.borrow_mut());

    signal.set(&idyll::Turn::for_test(), 1);
    signal.set(&idyll::Turn::for_test(), 2);
    assert_eq!(all_set_texts(&driver.borrow()), vec!["0"]);
    assert_eq!(driver.borrow().pending_microtasks(), 1);

    MockDriver::drain_microtasks_for(&driver);

    // Mark-and-pull coalesces: both writes mark the one binding, which runs once at
    // the flush and paints only the latest value — the intermediate "1" never
    // reaches the DOM (glitch-free), so one microtask yields one patch.
    assert_eq!(all_set_texts(&driver.borrow()), vec!["0", "2"]);
}

#[test]
fn live_ctx_emit_queues_self_message() {
    #[derive(Debug)]
    enum Msg {
        Start,
        Finish,
    }

    let mut rt = Runtime::new();
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::<&'static str>::new()));
    let seen_for_component = std::rc::Rc::clone(&seen);
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        move |ctx: Ctx<Setup, Msg>| {
            let seen = std::rc::Rc::clone(&seen_for_component);
            async move {
                let mut ctx = ctx.render(live_view! { span { "self" } }).await?;
                let (msg, turn) = ctx.recv().await?;
                match msg {
                    Msg::Start => {
                        seen.borrow_mut().push("start");
                        turn.emit(Msg::Finish);
                    }
                    Msg::Finish => panic!("finish arrived before start"),
                }
                let (msg, _turn) = ctx.recv().await?;
                match msg {
                    Msg::Finish => seen.borrow_mut().push("finish"),
                    Msg::Start => panic!("start delivered twice"),
                }
                std::future::pending().await
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    sender.send(Msg::Start);
    rt.run_to_quiescence();

    assert_eq!(&*seen.borrow(), &["start", "finish"]);
}

#[test]
fn counter_decrements_below_zero() {
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, CounterMsg> = rt.ctx::<CounterMsg>();
    let sender = ctx.inbox_sender();
    rt.spawn(spawn_live(|ctx| counter_component(ctx, 0i64), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);

    for _ in 0..3 {
        sender.send(CounterMsg::Dec);
        rt.run_once();
        rt.flush(&mut driver);
    }

    let texts = all_set_texts(&driver);
    assert_eq!(texts.last().map(String::as_str), Some("-3"));
}

// ── Computed signal ───────────────────────────────────────────────────────────

#[test]
fn computed_derives_and_patches_dom() {
    #[derive(Debug)]
    enum Msg {
        Bump,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let n = ctx.mutable_signal(3i32);
            let n_r = n.read();
            let doubled = ctx.computed(move |cx| n_r.get(cx) * 2);
            let mut ctx = ctx.render(live_view! { span { ($doubled) } }).await?;
            loop {
                let (msg, turn) = ctx.recv().await?;
                match msg {
                    Msg::Bump => n.update(&turn, |v| *v += 1),
                }
            }
            #[allow(unreachable_code)]
            std::future::pending().await
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["6"]);

    sender.send(Msg::Bump); // n: 4, doubled: 8
    rt.run_once();
    rt.flush(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["6", "8"]);

    sender.send(Msg::Bump); // n: 5, doubled: 10
    rt.run_once();
    rt.flush(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["6", "8", "10"]);
}

#[test]
fn mutable_vec_push_and_remove() {
    let (_rt, owner) = test_scope();
    let v: MutableVec<String> = owner.mutable_vec();
    let r1 = v.push(&idyll::Turn::for_test(), "a".into());
    let r2 = v.push(&idyll::Turn::for_test(), "b".into());
    assert_eq!(v.len(), 2);
    v.remove(&idyll::Turn::for_test(), r1);
    assert_eq!(v.len(), 1);
    assert!(v.get(r1).is_none());
    assert_eq!(v.get(r2).unwrap().now(&idyll::Turn::for_test()).clone(), "b");
    // Removing a stale row is a no-op.
    assert!(!v.remove(&idyll::Turn::for_test(), r1));
}

#[test]
fn mutable_vec_set_updates_in_place() {
    let (_rt, owner) = test_scope();
    let v: MutableVec<i32> = owner.mutable_vec();
    let r = v.push(&idyll::Turn::for_test(), 1);
    v.set(&idyll::Turn::for_test(), r, 99);
    assert_eq!(v.get(r).unwrap().now(&idyll::Turn::for_test()).clone(), 99);
}

#[test]
fn mutable_vec_sort_by_ascending() {
    let (_rt, owner) = test_scope();
    let v: MutableVec<i32> = owner.mutable_vec();
    v.push(&idyll::Turn::for_test(), 30);
    v.push(&idyll::Turn::for_test(), 10);
    v.push(&idyll::Turn::for_test(), 20);
    v.sort_by(&idyll::Turn::for_test(), |a, b| a.cmp(b));
    let vals: Vec<i32> = v
        .snapshot_order()
        .iter()
        .map(|&r| v.get(r).unwrap().now(&idyll::Turn::for_test()).clone())
        .collect();
    assert_eq!(vals, vec![10, 20, 30]);
}

#[test]
fn mutable_vec_splice_log_records_insert() {
    use idyll::signal::vec::SpliceOp;
    let (_rt, owner) = test_scope();
    let v: MutableVec<i32> = owner.mutable_vec();
    let consumer = v.consume();
    let r = v.push(&idyll::Turn::for_test(), 42);
    let ops = consumer.drain();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], SpliceOp::Insert { row, after: None } if row == r));
    // The stream is drained.
    assert!(consumer.drain().is_empty());
}

#[test]
fn view_if_fragment_replaces_when_signal_changes() {
    #[derive(Debug)]
    enum Msg {
        Toggle,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let shown = ctx.mutable_signal(false);
            let shown_view = shown.read();
            let mut ctx = ctx.render(live_view! {
                div {
                    @if ($shown_view) {
                        span { "shown" }
                    } else {
                        span { "hidden" }
                    }
                }
            }).await?;
            loop {
                let (msg, turn) = ctx.recv().await?;
                match msg {
                    Msg::Toggle => shown.update(&turn, |value| *value = !*value),
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(all_fragment_html(&driver), vec!["<span>hidden</span>"]);

    sender.send(Msg::Toggle);
    rt.run_once();
    rt.flush(&mut driver);
    assert_eq!(
        all_fragment_html(&driver),
        vec!["<span>hidden</span>", "<span>shown</span>"]
    );
}

#[test]
fn view_match_fragment_replaces_when_signal_changes() {
    #[derive(Clone, PartialEq)]
    enum Mode {
        One,
        Two(String),
    }

    #[derive(Debug)]
    enum Msg {
        Switch,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let mode = ctx.mutable_signal(Mode::One);
            let mode_view = mode.read();
            let mut ctx = ctx.render(live_view! {
                div {
                    @match (mode_view) {
                        Mode::One => {
                            span { ("one") }
                        },
                        Mode::Two(label) => {
                            span { (label) }
                        },
                    }
                }
            }).await?;
            loop {
                let (msg, turn) = ctx.recv().await?;
                match msg {
                    Msg::Switch => mode.set(&turn, Mode::Two("two".to_string())),
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["one"]);

    sender.send(Msg::Switch);
    rt.run_once();
    rt.flush(&mut driver);

    assert_eq!(all_set_texts(&driver), vec!["one", "two"]);
    assert_eq!(
        all_fragment_html(&driver),
        vec![
            "<span></span>",
            "<span></span>"
        ]
    );
}

#[test]
fn if_branch_binding_can_capture_outer_signal_handle() {
    #[derive(Debug)]
    enum Msg {
        Rename,
        Toggle,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let shown = ctx.mutable_signal(true);
            let label = ctx.mutable_signal("alpha".to_string());
            let shown_view = shown.read();
            let label_view = label.read();
            let mut ctx = ctx.render(live_view! {
                div {
                    @if ($shown_view) {
                        span { ($label_view) }
                    } else {
                        span { "hidden" }
                    }
                }
            }).await?;
            loop {
                let (msg, turn) = ctx.recv().await?;
                match msg {
                    Msg::Rename => label.set(&turn, "beta".to_string()),
                    Msg::Toggle => shown.update(&turn, |value| *value = !*value),
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["alpha"]);

    sender.send(Msg::Rename);
    rt.run_once();
    rt.flush(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["alpha", "beta"]);

    sender.send(Msg::Toggle);
    rt.run_once();
    rt.flush(&mut driver);
    sender.send(Msg::Toggle);
    rt.run_once();
    rt.flush(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["alpha", "beta", "beta"]);
}

#[test]
fn match_arm_binding_can_capture_outer_signal_handle() {
    #[derive(Clone, PartialEq)]
    enum Mode {
        Label,
        Hidden,
    }

    #[derive(Debug)]
    enum Msg {
        Rename,
        Toggle,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let mode = ctx.mutable_signal(Mode::Label);
            let label = ctx.mutable_signal("alpha".to_string());
            let mode_view = mode.read();
            let label_view = label.read();
            let mut ctx = ctx.render(live_view! {
                div {
                    @match (mode_view) {
                        Mode::Label => {
                            span { ($label_view) }
                        },
                        Mode::Hidden => {
                            span { "hidden" }
                        },
                    }
                }
            }).await?;
            loop {
                let (msg, turn) = ctx.recv().await?;
                match msg {
                    Msg::Rename => label.set(&turn, "beta".to_string()),
                    Msg::Toggle => mode.update(&turn, |value| {
                        *value = match value {
                            Mode::Label => Mode::Hidden,
                            Mode::Hidden => Mode::Label,
                        };
                    }),
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["alpha"]);

    sender.send(Msg::Rename);
    rt.run_once();
    rt.flush(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["alpha", "beta"]);

    sender.send(Msg::Toggle);
    rt.run_once();
    rt.flush(&mut driver);
    sender.send(Msg::Toggle);
    rt.run_once();
    rt.flush(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["alpha", "beta", "beta"]);
}

#[test]
fn replacing_match_branch_cancels_inline_child_task() {
    #[derive(Clone, PartialEq)]
    enum Mode {
        Child,
        Other,
    }

    #[derive(Debug)]
    enum ParentMsg {
        Switch,
    }

    #[derive(Debug)]
    enum ChildMsg {
        Click,
    }

    #[component]
    async fn Child(ctx: Ctx<Setup, ChildMsg>, clicks: std::rc::Rc<std::cell::Cell<u32>>) -> Result {
        let mut ctx = ctx.render(live_view! {
            button onclick=>(|_| ChildMsg::Click) { "child" }
        }).await?;
        loop {
            let (msg, _turn) = ctx.recv().await?;
            match msg {
                ChildMsg::Click => clicks.set(clicks.get() + 1),
            }
        }
    }

    async fn parent(
        ctx: Ctx<Setup, ParentMsg>,
        clicks: std::rc::Rc<std::cell::Cell<u32>>,
    ) -> Result {
        let mode = ctx.mutable_signal(Mode::Child);
        let mode_view = mode.read();
        let clicks_view = std::rc::Rc::clone(&clicks);

        let mut ctx = ctx.render(live_view! {
            div {
                @match (mode_view) {
                    Mode::Child => {
                        Child clicks=(clicks_view.clone())
                    },
                    Mode::Other => {
                        span { "other" }
                    },
                }
            }
        }).await?;

        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                ParentMsg::Switch => mode.set(&turn, Mode::Other),
            }
        }
    }

    let clicks = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();
    let sender = ctx.inbox_sender();

    let clicks_for_parent = std::rc::Rc::clone(&clicks);
    rt.spawn(spawn_live(move |ctx| parent(ctx, clicks_for_parent), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    let stale_child_handler = driver.latest_handler().unwrap();

    driver.fire(stale_child_handler, idyll::Event::default());
    rt.run_to_quiescence();
    assert_eq!(clicks.get(), 1);

    sender.send(ParentMsg::Switch);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    driver.fire(stale_child_handler, idyll::Event::default());
    rt.run_to_quiescence();
    assert_eq!(clicks.get(), 1);
}

// `@match[keep]` detaches an arm's DOM on switch and reattaches it on switch-back,
// rather than remounting — so the subtree (its event bindings included) survives detach.
// The kept arm's button routes to the parent; firing it while the arm is detached still
// lands, proving the subtree is kept alive, not torn down. (Inline child *components*
// inside a kept arm are rejected at compile time — see the macro's `@match[keep]` guard —
// so this exercises the DOM-level keep path, the one the todo example relies on.)
#[test]
fn match_keep_detaches_without_destroying_subtree() {
    #[derive(Clone, PartialEq)]
    enum Mode {
        Kept,
        Other,
    }

    #[derive(Debug)]
    enum ParentMsg {
        Toggle,
        Bump,
    }

    async fn parent(
        ctx: Ctx<Setup, ParentMsg>,
        clicks: std::rc::Rc<std::cell::Cell<u32>>,
    ) -> Result {
        let mode = ctx.mutable_signal(Mode::Kept);
        let mode_view = mode.read();

        let mut ctx = ctx.render(live_view! {
            div {
                @match[keep] (mode_view) {
                    Mode::Kept => {
                        button onclick=>(|_| ParentMsg::Bump) { "kept" }
                    },
                    Mode::Other => {
                        span { "other" }
                    },
                }
            }
        }).await?;

        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                ParentMsg::Bump => clicks.set(clicks.get() + 1),
                ParentMsg::Toggle => {
                    mode.update(&turn, |value| {
                        *value = match value {
                            Mode::Kept => Mode::Other,
                            Mode::Other => Mode::Kept,
                        };
                    });
                }
            }
        }
    }

    let clicks = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();
    let sender = ctx.inbox_sender();

    let clicks_for_parent = std::rc::Rc::clone(&clicks);
    rt.spawn(spawn_live(move |ctx| parent(ctx, clicks_for_parent), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    let kept_handler = driver.latest_handler().unwrap();

    driver.fire(kept_handler, idyll::Event::default());
    rt.run_to_quiescence();
    assert_eq!(clicks.get(), 1);

    sender.send(ParentMsg::Toggle);
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::DetachFragment { .. })),
        1
    );

    // Detached, not destroyed: the kept arm's handler still routes to the parent.
    driver.fire(kept_handler, idyll::Event::default());
    rt.run_to_quiescence();
    assert_eq!(clicks.get(), 2);

    sender.send(ParentMsg::Toggle);
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::AttachFragment { .. })),
        1
    );
}

#[test]
fn view_for_mutable_vec_mounts_initial_and_inserted_rows() {
    #[derive(Debug)]
    enum Msg {
        Add(String),
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let items = ctx.mutable_vec();
            items.push(&idyll::Turn::for_test(), "a".to_string());
            items.push(&idyll::Turn::for_test(), "b".to_string());
            let items_view = items.clone();

            let mut ctx = ctx.render(live_view! {
                ul {
                    @for (_row, text) in (items_view) {
                        li { ($text) }
                    }
                }
            }).await?;

            loop {
                let (msg, _turn) = ctx.recv().await?;
                match msg {
                    Msg::Add(value) => {
                        items.push(&idyll::Turn::for_test(), value);
                    }
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["a", "b"]);

    sender.send(Msg::Add("c".to_string()));
    rt.run_once();
    rt.flush(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["a", "b", "c"]);
}

#[test]
fn removed_rows_free_their_minted_node_ids() {
    #[derive(Debug)]
    enum Msg {
        RemoveSecond,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let items = ctx.mutable_vec();
            items.push(&idyll::Turn::for_test(), "a".to_string());
            let second = items.push(&idyll::Turn::for_test(), "b".to_string());
            items.push(&idyll::Turn::for_test(), "c".to_string());
            let items_view = items.clone();

            let mut ctx = ctx.render(live_view! {
                ul {
                    @for (_row, text) in (items_view) {
                        li { ($text) }
                    }
                }
            }).await?;

            loop {
                let (msg, _turn) = ctx.recv().await?;
                match msg {
                    Msg::RemoveSecond => {
                        items.remove(&idyll::Turn::for_test(), second);
                    }
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::FreeNodes { .. })),
        0,
        "nothing freed while every row is live"
    );

    sender.send(Msg::RemoveSecond);
    rt.run_once();
    rt.flush(&mut driver);

    // The removed row's mount guards dropped: exactly one free-nodes op releases
    // the ids that row minted (its text slot), after the fragment removal.
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::RemoveFragment { .. })),
        1
    );
    assert_eq!(
        count_ops(
            &driver,
            |op| matches!(op, DomOp::FreeNodes { node_ids } if !node_ids.is_empty())
        ),
        1,
        "the unmounted row must free its minted node ids"
    );
}

#[test]
fn view_for_mutable_vec_removes_and_moves_rows_by_row_anchor() {
    #[derive(Debug)]
    enum Msg {
        RemoveSecond,
        Sort,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let items = ctx.mutable_vec();
            items.push(&idyll::Turn::for_test(), "c".to_string());
            let second = items.push(&idyll::Turn::for_test(), "a".to_string());
            items.push(&idyll::Turn::for_test(), "b".to_string());
            let items_view = items.clone();

            let mut ctx = ctx.render(live_view! {
                ul {
                    @for (_row, text) in (items_view) {
                        li { ($text) }
                    }
                }
            }).await?;

            loop {
                let (msg, _turn) = ctx.recv().await?;
                match msg {
                    Msg::RemoveSecond => {
                        items.remove(&idyll::Turn::for_test(), second);
                    }
                    Msg::Sort => {
                        items.sort_by(&idyll::Turn::for_test(), |a, b| a.cmp(b));
                    }
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["c", "a", "b"]);

    sender.send(Msg::RemoveSecond);
    rt.run_once();
    rt.flush(&mut driver);
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::RemoveFragment { .. })),
        1
    );

    sender.send(Msg::Sort);
    rt.run_once();
    rt.flush(&mut driver);
    assert!(count_ops(&driver, |op| matches!(op, DomOp::MoveFragment { .. })) >= 2);
}

#[test]
fn view_for_mutable_vec_clear_removes_all_row_fragments() {
    #[derive(Debug)]
    enum Msg {
        Clear,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let items = ctx.mutable_vec();
            items.push(&idyll::Turn::for_test(), "a".to_string());
            items.push(&idyll::Turn::for_test(), "b".to_string());
            items.push(&idyll::Turn::for_test(), "c".to_string());
            let items_view = items.clone();

            let mut ctx = ctx.render(live_view! {
                ul {
                    @for (_row, text) in (items_view) {
                        li { ($text) }
                    }
                }
            }).await?;

            loop {
                let (msg, _turn) = ctx.recv().await?;
                match msg {
                    Msg::Clear => items.clear(&idyll::Turn::for_test()),
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::MountFragment { .. })),
        3
    );

    sender.send(Msg::Clear);
    rt.run_once();
    rt.flush(&mut driver);

    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::RemoveFragment { .. })),
        3
    );
}

#[test]
fn a_fault_in_a_for_row_routes_to_the_error_boundary() {
    // The fault model for structural children under the flat executor: a component inside a
    // `@for` row is a fragment mount (fire-and-forget into the executor), so its fault does not
    // climb a drive tree — it routes to the nearest error boundary's mailbox, which overlays
    // its fallback. A failure in one row does not take down the whole live.
    use idyll::boundary::{error_boundary, Faulted};

    #[component]
    async fn Boom(_ctx: Ctx<Setup, idyll::Never>, _row: idyll::Signal<u32>) -> Result {
        Err("row-fault".into())
    }
    #[component]
    async fn Rows(ctx: Ctx<Setup, idyll::Never>) -> Result {
        let items = ctx.mutable_vec();
        items.push(&idyll::Turn::for_test(), 1u32);
        let items_view = items.clone();
        ctx.render(live_view! {
            ul {
                @for (_row, n) in (items_view) {
                    li { Boom _row=(n) }
                }
            }
        })
        .await?
        .finish()
        .await
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Faulted> = rt.ctx::<Faulted>();
    rt.spawn(spawn_live(
        |ctx| {
            error_boundary::<Rows>(ctx, RowsRequired {}, RowsOptional::default(), |error| {
                live_view! { span { (error) } }
            })
        },
        ctx,
        report_to_log,
    ));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    // The boundary caught the row's fault and rendered its fallback — the message appears as a
    // `SetText` (the `(error)` one-shot) or in a mounted content template.
    let mut texts = all_set_texts(&driver);
    for template in &driver.templates {
        for node in template.nodes.iter() {
            if let idyll::template::TplNode::Text(t) = node {
                texts.push(t.to_string());
            }
        }
    }
    assert!(
        texts.iter().any(|t| t.contains("row-fault")),
        "the error boundary must render the row's fault as its fallback: {texts:?}"
    );
}

#[test]
fn measure_binding_delivers_a_rect_to_the_inbox() {
    // The `measure=>` binding rides the event path: the runtime observes the element and
    // delivers its root-relative rect on `e.rect`. Here MockDriver stands in for the
    // ResizeObserver, firing a synthetic measurement — the same way it fires a click.
    #[derive(Debug)]
    enum Msg {
        Measured(Rect),
    }

    let measured = std::rc::Rc::new(std::cell::Cell::new(None));
    let measured_c = std::rc::Rc::clone(&measured);

    async fn comp(
        ctx: Ctx<Setup, Msg>,
        sink: std::rc::Rc<std::cell::Cell<Option<Rect>>>,
    ) -> Result {
        let mut ctx = ctx.render(live_view! {
            div measure=>(|e| e.rect().map(Msg::Measured)) { "box" }
        }).await?;
        loop {
            let (msg, _turn) = ctx.recv().await?;
            match msg {
                Msg::Measured(rect) => sink.set(Some(rect)),
            }
        }
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    rt.spawn(spawn_live(move |ctx| comp(ctx, measured_c), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);

    let handler = driver.latest_handler().unwrap();
    let rect = Rect { x: 10.0, y: 20.0, width: 200.0, height: 44.0 };
    driver.fire(handler, Event { rect: Some(rect), ..Default::default() });
    rt.run_to_quiescence();

    assert_eq!(measured.get(), Some(rect), "the measured rect must reach the inbox");
}

#[test]
fn a_deep_component_tree_is_flat_one_task_per_component() {
    // The flat-executor invariant: every component is its own executor task (a boxed future
    // handed off at render), and the tree is exactly that — no monomorphic child tree, no
    // per-component overhead beyond its one task. Top + Mid + three Leaves = five components =
    // five tasks, and the whole tree renders.
    #[component]
    async fn Leaf(ctx: Ctx<Setup, idyll::Never>, label: &'static str) -> Result {
        Ok(ctx.render(live_view! { span { (label) } }).await?)
    }
    #[component]
    async fn Mid(ctx: Ctx<Setup, idyll::Never>) -> Result {
        ctx.render(live_view! {
            div {
                Leaf label=("a")
                Leaf label=("b")
                Leaf label=("c")
            }
        }).await?
        .finish()
        .await
    }
    #[component]
    async fn Top(ctx: Ctx<Setup, idyll::Never>) -> Result {
        ctx.render(live_view! { div { Mid } }).await?.finish().await
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, idyll::Never> = rt.ctx::<idyll::Never>();
    rt.spawn(spawn_live(
        |ctx| <Top as idyll::component::Component>::run(ctx, TopRequired {}, TopOptional::default()),
        ctx,
        report_to_log,
    ));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    assert_eq!(
        rt.live_task_count(),
        5,
        "five components are five flat tasks (Top + Mid + three Leaves), no hidden extras"
    );
    assert_eq!(all_set_texts(&driver), vec!["a", "b", "c"]);
}

#[test]
fn profile_component_mount_cost_is_linear() {
    // Prong-1 guard: mounting N child components is one flat task per row (N + the root), and the
    // mount stays roughly **linear** — the executor's ready-queue polls only woken tasks, so
    // mounting N rows is N polls, not the 1+2+…+N a re-scanning drive would cost. The task-count
    // assertion pins the flat model; `--nocapture` prints the scaling table.
    #[component]
    async fn Leaf(ctx: Ctx<Setup, idyll::Never>, _row: idyll::Signal<u32>) -> Result {
        Ok(ctx.render(live_view! { span { "x" } }).await?)
    }

    fn mount_n(n: u32) -> (std::time::Duration, usize, usize) {
        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx: Ctx<Setup, idyll::Never> = rt.ctx::<idyll::Never>();
        rt.spawn(spawn_live(
            move |ctx: Ctx<Setup, idyll::Never>| async move {
                let items = ctx.mutable_vec();
                for i in 0..n {
                    items.push(&idyll::Turn::for_test(), i);
                }
                let items_view = items.clone();
                let mut ctx = ctx.render(live_view! {
                    div { @for (_row, r) in (items_view) { Leaf _row=(r) } }
                }).await?;
                loop {
                    let _ = ctx.recv().await?;
                }
            },
            ctx,
            report_to_log,
        ));
        let start = std::time::Instant::now();
        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        let elapsed = start.elapsed();
        let ops = count_ops(&driver, |_| true);
        (elapsed, rt.live_task_count(), ops)
    }

    println!("\n   N      tasks   dom_ops   mount        per-component");
    let mut prev: Option<(u32, std::time::Duration)> = None;
    for n in [16u32, 64, 256, 1024] {
        let (dt, tasks, ops) = mount_n(n);
        assert_eq!(tasks, n as usize + 1, "{n} rows are {n} flat tasks plus the root");
        let per = dt / n;
        let scale = prev.map(|(pn, pt)| {
            let ratio = dt.as_secs_f64() / pt.as_secs_f64();
            format!(" ({:.1}x for {:.0}x N)", ratio, n as f64 / pn as f64)
        });
        println!("   {n:<6} {tasks:<6}  {ops:<8}  {dt:>9.2?}   {per:.3?}{}", scale.unwrap_or_default());
        prev = Some((n, dt));
    }
}

#[test]
fn profile_component_update_cost_is_linear() {
    // Prong-1 guard, steady state. The mount profile above guards the *mount* half of "cheap
    // components"; this guards the *update* half — the one a running sim actually stresses at
    // 60fps. After mounting N child components each bound to its own row signal, refreshing
    // every row costs O(N), not O(N²): a per-row `set` wakes only that row's binding through
    // the reactive graph, and the parent `@for` never re-scans its children. `--nocapture`
    // prints the per-row-update cost holding flat as N grows.
    #[component]
    async fn Leaf(ctx: Ctx<Setup, idyll::Never>, row: idyll::Signal<u32>) -> Result {
        Ok(ctx.render(live_view! { span { ($row) } }).await?)
    }

    fn refresh_n(n: u32, rounds: u32) -> (std::time::Duration, usize) {
        let slot: std::rc::Rc<std::cell::RefCell<Option<MutableVec<u32>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot_c = slot.clone();
        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx: Ctx<Setup, idyll::Never> = rt.ctx::<idyll::Never>();
        rt.spawn(spawn_live(
            move |ctx: Ctx<Setup, idyll::Never>| async move {
                let items: MutableVec<u32> = ctx.mutable_vec();
                for i in 0..n {
                    items.push(&idyll::Turn::for_test(), i);
                }
                *slot_c.borrow_mut() = Some(items.clone());
                let items_view = items.clone();
                let mut ctx = ctx.render(live_view! {
                    div { @for (_row, r) in (items_view) { Leaf row=(r) } }
                }).await?;
                loop {
                    let _ = ctx.recv().await?;
                }
            },
            ctx,
            report_to_log,
        ));
        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.run_to_quiescence();
        rt.flush(&mut driver);

        let items = slot.borrow().clone().expect("the component published its list");
        let rows = items.snapshot_order();
        let start = std::time::Instant::now();
        for round in 1..=rounds {
            // A fresh value per row per round, so every binding actually fires (a computed
            // that reads an unchanged value bails on PartialEq and would measure nothing).
            for (i, &row) in rows.iter().enumerate() {
                items.set(&idyll::Turn::for_test(), row, round * n + i as u32);
            }
            rt.run_to_quiescence();
            rt.flush(&mut driver);
        }
        (start.elapsed(), rt.live_task_count())
    }

    println!("\n   N      updates    refresh_all    per-row-update");
    let rounds = 8u32;
    let mut prev: Option<(u32, std::time::Duration)> = None;
    for n in [16u32, 64, 256, 1024] {
        let (dt, tasks) = refresh_n(n, rounds);
        assert_eq!(tasks, n as usize + 1, "{n} rows stay {n} flat tasks plus the root under updates");
        let updates = n * rounds;
        let per = dt / updates;
        let scale = prev.map(|(pn, pt)| {
            let ratio = dt.as_secs_f64() / pt.as_secs_f64();
            format!(" ({:.1}x for {:.0}x N)", ratio, n as f64 / pn as f64)
        });
        println!("   {n:<6} {updates:<9}  {dt:>11.2?}   {per:.3?}{}", scale.unwrap_or_default());
        prev = Some((n, dt));
    }
}

#[test]
fn inline_component_mount_renders_child_view() {
    #[derive(Debug)]
    enum ParentMsg {}

    #[derive(Debug)]
    enum ChildMsg {}

    #[component]
    async fn Child(ctx: Ctx<Setup, ChildMsg>, label: &'static str) -> Result {
        let _ctx = ctx.render(live_view! {
            span { (label) }
        }).await?;
        std::future::pending().await
    }

    async fn parent(ctx: Ctx<Setup, ParentMsg>) -> Result {
        let mut ctx = ctx.render(live_view! {
            div {
                Child label=("child")
            }
        }).await?;
        loop {
            let _ = ctx.recv().await?; // ParentMsg is uninhabited: drives the child, never yields
        }
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();

    rt.spawn(spawn_live(parent, ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    assert_eq!(all_set_texts(&driver), vec!["child"]);
}

#[test]
fn inline_component_event_routes_to_child_inbox() {
    #[derive(Debug)]
    enum ParentMsg {}

    #[derive(Debug)]
    enum ChildMsg {
        Click,
    }

    #[component]
    async fn Child(ctx: Ctx<Setup, ChildMsg>, clicks: std::rc::Rc<std::cell::Cell<u32>>) -> Result {
        let mut ctx = ctx.render(live_view! {
            button onclick=>(|_| ChildMsg::Click) { "child" }
        }).await?;
        let (msg, _turn) = ctx.recv().await?;
        match msg {
            ChildMsg::Click => clicks.set(clicks.get() + 1),
        }
        std::future::pending().await
    }

    async fn parent(
        ctx: Ctx<Setup, ParentMsg>,
        clicks: std::rc::Rc<std::cell::Cell<u32>>,
    ) -> Result {
        let mut ctx = ctx.render(live_view! {
            div {
                Child clicks=(clicks)
            }
        }).await?;
        loop {
            let _ = ctx.recv().await?; // ParentMsg is uninhabited: drives the child, never yields
        }
    }

    let clicks = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();

    let clicks_for_parent = std::rc::Rc::clone(&clicks);
    rt.spawn(spawn_live(move |ctx| parent(ctx, clicks_for_parent), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    let handler = driver.latest_handler().unwrap();
    driver.fire(handler, idyll::Event::default());
    rt.run_to_quiescence();

    assert_eq!(clicks.get(), 1);
}

#[test]
fn removing_for_row_cancels_inline_child_task() {
    #[derive(Debug)]
    enum ParentMsg {
        Remove,
    }

    #[derive(Debug)]
    enum ChildMsg {
        Click,
    }

    #[component]
    async fn Child(
        ctx: Ctx<Setup, ChildMsg>,
        _text: idyll::Signal<String>,
        clicks: std::rc::Rc<std::cell::Cell<u32>>,
    ) -> Result {
        let mut ctx = ctx.render(live_view! {
            button onclick=>(|_| ChildMsg::Click) { "child" }
        }).await?;
        loop {
            let (msg, _turn) = ctx.recv().await?;
            match msg {
                ChildMsg::Click => clicks.set(clicks.get() + 1),
            }
        }
    }

    async fn parent(
        ctx: Ctx<Setup, ParentMsg>,
        clicks: std::rc::Rc<std::cell::Cell<u32>>,
    ) -> Result {
        let items = ctx.mutable_vec();
        let row = items.push(&idyll::Turn::for_test(), "row".to_string());
        let items_view = items.clone();
        let clicks_view = std::rc::Rc::clone(&clicks);

        let mut ctx = ctx.render(live_view! {
            ul {
                @for (_row, text) in (items_view) {
                    li {
                        Child _text=(text) clicks=(clicks_view.clone())
                    }
                }
            }
        }).await?;

        loop {
            let (msg, _turn) = ctx.recv().await?;
            match msg {
                ParentMsg::Remove => {
                    items.remove(&idyll::Turn::for_test(), row);
                }
            }
        }
    }

    let clicks = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();
    let sender = ctx.inbox_sender();

    let clicks_for_parent = std::rc::Rc::clone(&clicks);
    rt.spawn(spawn_live(move |ctx| parent(ctx, clicks_for_parent), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    let stale_child_handler = driver.latest_handler().unwrap();

    driver.fire(stale_child_handler, idyll::Event::default());
    rt.run_to_quiescence();
    assert_eq!(clicks.get(), 1);

    sender.send(ParentMsg::Remove);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    driver.fire(stale_child_handler, idyll::Event::default());
    rt.run_to_quiescence();
    assert_eq!(clicks.get(), 1);
}

#[test]
fn replacing_if_branch_cancels_inline_child_task() {
    #[derive(Debug)]
    enum ParentMsg {
        Hide,
    }

    #[derive(Debug)]
    enum ChildMsg {
        Click,
    }

    #[component]
    async fn Child(ctx: Ctx<Setup, ChildMsg>, clicks: std::rc::Rc<std::cell::Cell<u32>>) -> Result {
        let mut ctx = ctx.render(live_view! {
            button onclick=>(|_| ChildMsg::Click) { "child" }
        }).await?;
        loop {
            let (msg, _turn) = ctx.recv().await?;
            match msg {
                ChildMsg::Click => clicks.set(clicks.get() + 1),
            }
        }
    }

    async fn parent(
        ctx: Ctx<Setup, ParentMsg>,
        clicks: std::rc::Rc<std::cell::Cell<u32>>,
    ) -> Result {
        let shown = ctx.mutable_signal(true);
        let shown_view = shown.read();
        let clicks_view = std::rc::Rc::clone(&clicks);

        let mut ctx = ctx.render(live_view! {
            div {
                @if ($shown_view) {
                    Child clicks=(clicks_view.clone())
                } else {
                    span { "hidden" }
                }
            }
        }).await?;

        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                ParentMsg::Hide => shown.set(&turn, false),
            }
        }
    }

    let clicks = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();
    let sender = ctx.inbox_sender();

    let clicks_for_parent = std::rc::Rc::clone(&clicks);
    rt.spawn(spawn_live(move |ctx| parent(ctx, clicks_for_parent), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    let stale_child_handler = driver.latest_handler().unwrap();

    driver.fire(stale_child_handler, idyll::Event::default());
    rt.run_to_quiescence();
    assert_eq!(clicks.get(), 1);

    sender.send(ParentMsg::Hide);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    driver.fire(stale_child_handler, idyll::Event::default());
    rt.run_to_quiescence();
    assert_eq!(clicks.get(), 1);
}

#[test]
fn on_unmount_runs_cleanup_when_inline_child_scope_is_removed() {
    #[derive(Debug)]
    enum ParentMsg {
        Hide,
    }

    #[component]
    async fn Child(ctx: Ctx<Setup, ()>, cleaned: std::rc::Rc<std::cell::Cell<bool>>) -> Result {
        ctx.on_unmount(async move {
            cleaned.set(true);
        });
        let mut ctx = ctx.render(live_view! {
            span { "child" }
        }).await?;
        let _ = ctx.recv().await?;
        std::future::pending().await
    }

    async fn parent(
        ctx: Ctx<Setup, ParentMsg>,
        cleaned: std::rc::Rc<std::cell::Cell<bool>>,
    ) -> Result {
        let shown = ctx.mutable_signal(true);
        let shown_view = shown.read();
        let cleaned_view = std::rc::Rc::clone(&cleaned);

        let mut ctx = ctx.render(live_view! {
            div {
                @if ($shown_view) {
                    Child cleaned=(cleaned_view.clone())
                }
            }
        }).await?;

        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                ParentMsg::Hide => shown.set(&turn, false),
            }
        }
    }

    let cleaned = std::rc::Rc::new(std::cell::Cell::new(false));
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();
    let sender = ctx.inbox_sender();

    let cleaned_for_parent = std::rc::Rc::clone(&cleaned);
    rt.spawn(spawn_live(move |ctx| parent(ctx, cleaned_for_parent), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert!(!cleaned.get());

    sender.send(ParentMsg::Hide);
    rt.run_once();
    rt.flush(&mut driver);
    rt.run_to_quiescence();

    assert!(cleaned.get());
}

#[test]
fn on_unmount_waits_for_unmount_after_render_and_return() {
    #[derive(Debug)]
    enum ParentMsg {
        Hide,
    }

    #[component]
    async fn Child(ctx: Ctx<Setup, ()>, cleaned: std::rc::Rc<std::cell::Cell<bool>>) -> Result {
        ctx.on_unmount(async move {
            cleaned.set(true);
        });
        let _ctx = ctx.render(live_view! {
            span { "child" }
        }).await?;
        std::future::pending().await
    }

    async fn parent(
        ctx: Ctx<Setup, ParentMsg>,
        cleaned: std::rc::Rc<std::cell::Cell<bool>>,
    ) -> Result {
        let shown = ctx.mutable_signal(true);
        let shown_view = shown.read();
        let cleaned_view = std::rc::Rc::clone(&cleaned);

        let mut ctx = ctx.render(live_view! {
            div {
                @if ($shown_view) {
                    Child cleaned=(cleaned_view.clone())
                }
            }
        }).await?;

        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                ParentMsg::Hide => shown.set(&turn, false),
            }
        }
    }

    let cleaned = std::rc::Rc::new(std::cell::Cell::new(false));
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();
    let sender = ctx.inbox_sender();

    let cleaned_for_parent = std::rc::Rc::clone(&cleaned);
    rt.spawn(spawn_live(move |ctx| parent(ctx, cleaned_for_parent), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    assert!(!cleaned.get());

    sender.send(ParentMsg::Hide);
    rt.run_once();
    rt.flush(&mut driver);
    rt.run_to_quiescence();

    assert!(cleaned.get());
}

// `@if[keep]` detaches the then-arm's DOM on toggle-off and reattaches it on toggle-on,
// keeping the subtree (its event bindings included) alive across the hide. The then-arm's
// button routes to the parent; firing it while hidden still lands. (Inline child components
// in a kept arm are a compile error — see the macro guard — so this covers the DOM keep
// path, which is what the todo example uses.)
#[test]
fn if_keep_detaches_without_destroying_subtree() {
    #[derive(Debug)]
    enum ParentMsg {
        Toggle,
        Bump,
    }

    async fn parent(
        ctx: Ctx<Setup, ParentMsg>,
        clicks: std::rc::Rc<std::cell::Cell<u32>>,
    ) -> Result {
        let shown = ctx.mutable_signal(true);
        let shown_view = shown.read();

        let mut ctx = ctx.render(live_view! {
            div {
                @if[keep] ($shown_view) {
                    button onclick=>(|_| ParentMsg::Bump) { "kept" }
                } else {
                    span { "hidden" }
                }
            }
        }).await?;

        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                ParentMsg::Bump => clicks.set(clicks.get() + 1),
                ParentMsg::Toggle => shown.update(&turn, |value| *value = !*value),
            }
        }
    }

    let clicks = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();
    let sender = ctx.inbox_sender();

    let clicks_for_parent = std::rc::Rc::clone(&clicks);
    rt.spawn(spawn_live(move |ctx| parent(ctx, clicks_for_parent), ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    let kept_handler = driver.latest_handler().unwrap();

    driver.fire(kept_handler, idyll::Event::default());
    rt.run_to_quiescence();
    assert_eq!(clicks.get(), 1);

    sender.send(ParentMsg::Toggle);
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::DetachFragment { .. })),
        1
    );

    // Detached, not destroyed: the kept arm's handler still routes to the parent.
    driver.fire(kept_handler, idyll::Event::default());
    rt.run_to_quiescence();
    assert_eq!(clicks.get(), 2);

    sender.send(ParentMsg::Toggle);
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::AttachFragment { .. })),
        1
    );
    // Two structural mounts, neither a keep-remount: the then-arm and the else-arm.
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::MountFragment { .. })),
        2
    );
}

#[test]
fn view_for_keyed_signal_updates_matched_rows_in_place() {
    #[derive(Clone, PartialEq)]
    struct Hit {
        id: u32,
        title: String,
    }

    #[derive(Debug)]
    enum Msg {
        Replace,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let results = ctx.mutable_signal(vec![Hit {
                id: 1,
                title: "alpha".to_string(),
            }]);
            let results_view = results.read();

            let mut ctx = ctx.render(live_view! {
                ul {
                    @for hit in (results_view) [key = hit.id] {
                        li { ($hit.title.clone()) }
                    }
                }
            }).await?;

            loop {
                let (msg, turn) = ctx.recv().await?;
                match msg {
                    Msg::Replace => results.set(&turn, vec![Hit {
                        id: 1,
                        title: "beta".to_string(),
                    }]),
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["alpha"]);
    let mount_count = count_ops(&driver, |op| matches!(op, DomOp::MountFragment { .. }));

    sender.send(Msg::Replace);
    rt.run_once();
    rt.flush(&mut driver);

    assert_eq!(all_set_texts(&driver), vec!["alpha", "beta"]);
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::MountFragment { .. })),
        mount_count
    );
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::RemoveFragment { .. })),
        0
    );
}

#[test]
fn view_for_keyed_signal_inserts_removes_and_moves_by_key() {
    #[derive(Clone, PartialEq)]
    struct Hit {
        id: u32,
        title: String,
    }

    #[derive(Debug)]
    enum Msg {
        Replace,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let results = ctx.mutable_signal(vec![
                Hit {
                    id: 1,
                    title: "one".to_string(),
                },
                Hit {
                    id: 2,
                    title: "two".to_string(),
                },
            ]);
            let results_view = results.read();

            let mut ctx = ctx.render(live_view! {
                ul {
                    @for hit in (results_view) [key = hit.id] {
                        li { ($hit.title.clone()) }
                    }
                }
            }).await?;

            loop {
                let (msg, turn) = ctx.recv().await?;
                match msg {
                    Msg::Replace => results.set(&turn, vec![
                        Hit {
                            id: 2,
                            title: "two updated".to_string(),
                        },
                        Hit {
                            id: 3,
                            title: "three".to_string(),
                        },
                    ]),
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);
    assert_eq!(all_set_texts(&driver), vec!["one", "two"]);

    sender.send(Msg::Replace);
    rt.run_once();
    rt.flush(&mut driver);

    assert_eq!(
        all_set_texts(&driver),
        vec!["one", "two", "two updated", "three"]
    );
    assert!(count_ops(&driver, |op| matches!(op, DomOp::MoveFragment { .. })) >= 1);
    assert_eq!(
        count_ops(&driver, |op| matches!(op, DomOp::RemoveFragment { .. })),
        1
    );
}

#[test]
fn inline_component_inherits_parent_context() {
    #[derive(Debug)]
    enum ParentMsg {}

    #[component]
    async fn Child(ctx: Ctx<Setup, ()>) -> Result {
        let inherited = *ctx.use_context::<u32>().unwrap();
        let _ctx = ctx.render(live_view! {
            span { (inherited) }
        }).await?;
        std::future::pending().await
    }

    async fn parent(ctx: Ctx<Setup, ParentMsg>) -> Result {
        ctx.provide::<u32>(123);
        let mut ctx = ctx.render(live_view! {
            div {
                Child
            }
        }).await?;
        loop {
            let _ = ctx.recv().await?; // ParentMsg is uninhabited: drives the child, never yields
        }
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();

    rt.spawn(spawn_live(parent, ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    assert_eq!(all_set_texts(&driver), vec!["123"]);
}

#[test]
fn inline_component_in_for_scope_inherits_parent_context() {
    #[derive(Debug)]
    enum ParentMsg {}

    #[component]
    async fn Child(ctx: Ctx<Setup, ()>, text: idyll::Signal<String>) -> Result {
        let inherited = *ctx.use_context::<u32>().unwrap();
        let _ctx = ctx.render(live_view! {
            span { (format!("{}:{}", inherited, $text)) }
        }).await?;
        std::future::pending().await
    }

    async fn parent(ctx: Ctx<Setup, ParentMsg>) -> Result {
        ctx.provide::<u32>(7);
        let items = ctx.mutable_vec();
        items.push(&idyll::Turn::for_test(), "row".to_string());
        let items_view = items.clone();
        let mut ctx = ctx.render(live_view! {
            ul {
                @for (_row, text) in (items_view) {
                    li {
                        Child text=(text)
                    }
                }
            }
        }).await?;
        loop {
            let _ = ctx.recv().await?; // ParentMsg is uninhabited: drives the rows, never yields
        }
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();

    rt.spawn(spawn_live(parent, ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    assert_eq!(all_set_texts(&driver), vec!["7:row"]);
}

#[test]
fn inline_component_in_branch_scope_inherits_parent_context() {
    #[derive(Debug)]
    enum ParentMsg {}

    #[component]
    async fn Child(ctx: Ctx<Setup, ()>) -> Result {
        let inherited = *ctx.use_context::<u32>().unwrap();
        let _ctx = ctx.render(live_view! {
            span { (inherited) }
        }).await?;
        std::future::pending().await
    }

    async fn parent(ctx: Ctx<Setup, ParentMsg>) -> Result {
        ctx.provide::<u32>(55);
        let shown = ctx.mutable_signal(true);
        let shown_view = shown.read();
        let mut ctx = ctx.render(live_view! {
            div {
                @if ($shown_view) {
                    Child
                }
            }
        }).await?;
        loop {
            let _ = ctx.recv().await?; // ParentMsg is uninhabited: drives the arm, never yields
        }
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, ParentMsg> = rt.ctx::<ParentMsg>();

    rt.spawn(spawn_live(parent, ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    assert_eq!(all_set_texts(&driver), vec!["55"]);
}

// ── ctx.listen — cross-component signal observation ───────────────────────────

#[test]
fn listen_delivers_messages_from_external_signal() {
    use std::sync::{Arc, Mutex};

    #[derive(Debug, PartialEq)]
    enum Msg {
        Changed(i32),
    }

    // Share a signal between the test harness and the component — on the SAME
    // runtime: per-runtime cores mean a write marks only its own runtime's queues.
    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let external = ctx.owner().mutable_signal(0i32);
    let ext_for_component = external.read();

    let received: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));
    let received_component = Arc::clone(&received);

    rt.spawn(spawn_live(
        move |ctx: Ctx<Setup, Msg>| async move {
            // Subscribe: every change to ext_for_component sends Msg::Changed.
            let _guard = ctx.listen(&ext_for_component, |v| Some(Msg::Changed(*v)));

            let mut ctx = ctx.render(live_view! { span { "observer" } }).await?;

            // Receive exactly 3 messages.
            for _ in 0..3 {
                let (msg, _turn) = ctx.recv().await?;
                match msg {
                    Msg::Changed(v) => received_component.lock().unwrap().push(v),
                }
            }
            std::future::pending().await
        },
        ctx,
        report_to_log,
    ));

    // Setup: run until the component parks at recv().await.
    rt.run_once();
    rt.process_pending_view(&mut driver);

    // Drive the external signal; each write marks the listen effect, which fires on
    // the flush and emits `Changed`, waking the component.
    external.set(&idyll::Turn::for_test(), 1);
    rt.flush(&mut driver);
    rt.run_once(); // component wakes on Changed(1)

    external.set(&idyll::Turn::for_test(), 2);
    rt.flush(&mut driver);
    rt.run_once();

    external.set(&idyll::Turn::for_test(), 3);
    rt.flush(&mut driver);
    rt.run_to_quiescence(); // component exits after 3 messages

    assert_eq!(*received.lock().unwrap(), vec![1, 2, 3]);
}

// ── ctx.callback ─────────────────────────────────────────────────────────────

#[test]
fn callback_routes_child_events_to_parent_inbox() {
    use std::sync::{Arc, Mutex};

    #[derive(Debug, PartialEq)]
    enum Msg {
        Delete(u64),
    }

    let mut rt = Runtime::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    // Build a callback from ctx before moving ctx into the spawn.
    let cb = ctx.callback(Msg::Delete);

    let received: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let received2 = Arc::clone(&received);
    let mut driver = MockDriver::new();

    rt.spawn(spawn_live(
        move |ctx: Ctx<Setup, Msg>| async move {
            let mut ctx = ctx.render(live_view! { span { "parent" } }).await?;
            for _ in 0..2 {
                let (msg, _turn) = ctx.recv().await?;
                match msg {
                    Msg::Delete(id) => received2.lock().unwrap().push(id),
                }
            }
            std::future::pending().await
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);

    // "Child" fires the callback (in practice the child would call cb.call(row)).
    cb.call(10);
    cb.call(20);
    rt.run_to_quiescence();

    assert_eq!(*received.lock().unwrap(), vec![10, 20]);
}

// ── Replay: absorbs are messages; the log is totally ordered; the fold replays ─

#[test]
fn a_recorded_session_with_absorbs_replays_identically() {
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, PartialEq, Clone, serde::Serialize, serde::Deserialize)]
    enum Msg {
        Bump,
    }

    type Handles = Rc<RefCell<Option<(MessageLog, idyll::SeedSink)>>>;

    /// A store-owner-shaped component: absorbs fold into `store` (in this
    /// component's turn), messages bump a counter, the view renders both.
    async fn owner(ctx: Ctx<Setup, Msg>, handles: Handles) -> Result {
        let store = ctx.mutable_signal(String::new());
        let clicks = ctx.mutable_signal(0u32);
        let store_view = store.read();
        let clicks_view = clicks.read();
        // The store cell's sole writer moves into the absorber.
        let sink = ctx.absorber(
            move |turn: &idyll::Turn, bytes: &[u8]| -> std::result::Result<(), String> {
                let text = String::from_utf8_lossy(bytes).into_owned();
                store.update(turn, |value| value.push_str(&text));
                Ok(())
            },
        );
        *handles.borrow_mut() = Some((ctx.record_messages(), sink));
        let mut ctx = ctx.render(live_view! {
            span { ($store_view) ":" ($clicks_view) }
        }).await?;
        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                Msg::Bump => clicks.update(&turn, |v| *v += 1),
            }
        }
    }

    fn run_session(
        inject: impl Fn(&idyll::InboxSender<Msg>, &idyll::SeedSink, &mut Runtime),
    ) -> (Vec<String>, Vec<idyll::MessageRecord>) {
        let handles: Handles = Rc::new(RefCell::new(None));
        let mut rt = Runtime::new();
        let mut driver = MockDriver::new();
        let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
        let sender = ctx.inbox_sender();
        let handles_for_owner = Rc::clone(&handles);
        rt.spawn(spawn_live(move |ctx| owner(ctx, handles_for_owner), ctx, report_to_log));
        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.flush(&mut driver);
        let (log, sink) = handles.borrow().clone().unwrap();
        inject(&sender, &sink, &mut rt);
        rt.flush(&mut driver);
        (all_set_texts(&driver), log.records())
    }

    // Record: an interleaved session — absorb, message, absorb.
    let (live_texts, records) = run_session(|sender, sink, rt| {
        (sink.0)(b"a");
        rt.run_to_quiescence();
        sender.send(Msg::Bump);
        rt.run_to_quiescence();
        (sink.0)(b"b");
        rt.run_to_quiescence();
    });

    // The log carries the absorbs as entries with a total order.
    let tys: Vec<&str> = records.iter().map(|r| r.ty.as_str()).collect();
    assert_eq!(
        tys.iter().filter(|t| **t == idyll::ABSORB_TY).count(),
        2,
        "both absorbs recorded: {tys:?}"
    );
    assert!(
        records.windows(2).all(|w| w[0].sequence < w[1].sequence),
        "sequences are strictly ordered: {records:?}"
    );

    // Replay: re-inject the log in sequence order into a fresh mount.
    let (replayed_texts, _) = run_session(|sender, sink, rt| {
        for record in &records {
            if record.ty == idyll::ABSORB_TY {
                let bytes = match &record.json {
                    serde_json::Value::String(s) => s.clone().into_bytes(),
                    other => serde_json::to_vec(other).unwrap(),
                };
                (sink.0)(&bytes);
            } else {
                sender.send(serde_json::from_value(record.json.clone()).unwrap());
            }
            rt.run_to_quiescence();
        }
    });

    assert_eq!(replayed_texts, live_texts, "the fold reconstructs from the log");
}

// ── Static-paint detection ───────────────────────────
//
// An island is a *static paint* when its SSR paint is a pure function of its seed: it
// wires no client work and depends on nothing outside its own frame. The browser then
// adopts the served DOM as-is rather than re-running the component. The verdict is read
// off the mount's frame (`ContextHandle::paint_is_static`) after its initial mount.

#[test]
fn a_pure_paint_island_reports_static() {
    async fn component(ctx: Ctx<Setup, idyll::Never>) -> Result {
        ctx.render(live_view! { div { span { "hello" } } }).await?.finish().await
    }

    let mut driver = MockDriver::new();
    let mut rt = Runtime::new();
    let ctx = rt.ctx::<idyll::Never>();
    let frame = ctx.context_handle();
    rt.spawn(spawn_live(component, ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();

    assert!(frame.paint_is_static(), "no client work, no external read — a pure paint");
}

#[test]
fn a_listener_disqualifies_the_static_paint() {
    #[derive(Debug)]
    enum Msg {
        Bumped,
    }

    async fn component(ctx: Ctx<Setup, Msg>) -> Result {
        let mut ctx = ctx.render(live_view! {
            button onclick=>(|_ev| Some(Msg::Bumped)) { "go" }
        }).await?;
        loop {
            let (msg, _turn) = ctx.recv().await?;
            match msg {
                Msg::Bumped => {}
            }
        }
    }

    let mut driver = MockDriver::new();
    let mut rt = Runtime::new();
    let ctx = rt.ctx::<Msg>();
    let frame = ctx.context_handle();
    rt.spawn(spawn_live(component, ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();

    assert!(!frame.paint_is_static(), "a DOM listener needs the reducer live on the client");
}

/// A `painting=(…)` binding, against the whole of what it promises: reactivity is per
/// *shape* and the wire is per *frame*. One command carries the turn however many shapes
/// moved; a layer nothing wrote is not in it at all; a first frame is all of the layer
/// and a still one is nothing at all. And the view cannot be adopted from its SSR paint,
/// however inert everything else about it is, because a picture has no HTML to adopt.
#[test]
fn a_canvas_paints_the_layers_whose_shapes_moved_and_disqualifies_the_static_paint() {
    use idyll::{Curve, MutableVec, Shape};

    fn wire(ink: &str) -> Shape {
        Shape {
            curve: Curve { from: (0.0, 0.0), c1: (1.0, 0.0), c2: (2.0, 1.0), to: (3.0, 1.0) },
            span: (0.0, 1.0),
            ink: ink.to_string(),
            width: 1.5,
            alpha: (0.4, 0.4),
        }
    }

    enum Msg {
        MoveTraffic,
    }

    async fn component(ctx: Ctx<Setup, Msg>) -> Result {
        let scenery: MutableVec<Shape> = ctx.mutable_vec_of(vec![wire("var(--wall)")]);
        let traffic: MutableVec<Shape> = ctx.mutable_vec_of(vec![wire("var(--teal)")]);
        let picture = ctx.mutable_vec_of(vec![scenery.clone(), traffic.clone()]);
        let mut ctx = ctx
            .render(live_view! {
                div {
                    button onclick=>(|_| Some(Msg::MoveTraffic)) { "tick" }
                    canvas painting=(picture) {}
                }
            })
            .await?;
        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                Msg::MoveTraffic => {
                    let mut moved = wire("var(--teal)");
                    moved.span = (0.25, 0.75);
                    traffic.sync(&turn, vec![moved]);
                }
            }
        }
    }

    let mut driver = MockDriver::new();
    let mut rt = Runtime::new();
    let ctx = rt.ctx::<Msg>();
    let frame = ctx.context_handle();
    rt.spawn(spawn_live(component, ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();

    let painted = |driver: &MockDriver| -> Vec<(u32, Vec<idyll::canvas::LayerDelta>)> {
        driver
            .log
            .iter()
            .filter_map(|op| match op {
                DomOp::Paint { layers, deltas, .. } => Some((*layers, deltas.clone())),
                _ => None,
            })
            .collect()
    };

    let mount = painted(&driver);
    assert_eq!(mount.len(), 1, "the mount is one command, not one per shape");
    assert_eq!(mount[0].0, 2, "two layers, composited in row order");
    assert_eq!(
        mount[0].1,
        [
            idyll::canvas::LayerDelta {
                layer: 0,
                slots: vec![(0, wire("var(--wall)"))],
                len: 1,
            },
            idyll::canvas::LayerDelta {
                layer: 1,
                slots: vec![(0, wire("var(--teal)"))],
                len: 1,
            },
        ],
        "a first frame is every slot of every layer",
    );

    driver.log.clear();
    let handler = driver.latest_handler().expect("click handler registered");
    driver.fire(handler, Event::default());
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    let moved = painted(&driver);
    assert_eq!(moved.len(), 1, "one command for the turn");
    let mut lit = wire("var(--teal)");
    lit.span = (0.25, 0.75);
    assert_eq!(
        moved[0].1,
        [idyll::canvas::LayerDelta { layer: 1, slots: vec![(0, lit)], len: 1 }],
        "only the layer whose shape was written says anything",
    );

    driver.log.clear();
    rt.flush(&mut driver);
    rt.run_to_quiescence();
    assert!(painted(&driver).is_empty(), "a frame nobody wrote crosses as nothing");

    assert!(!frame.paint_is_static(), "a picture has no served HTML to adopt");
}



#[test]
fn reading_an_absent_context_disqualifies_the_static_paint() {
    async fn component(ctx: Ctx<Setup, idyll::Never>) -> Result {
        // Reaches for a context no one in this isolated frame provided: it resolves to
        // `None` here, but on the client an ancestor could satisfy it and then re-render
        // this view — so the paint is not one to adopt untouched.
        let _absent = ctx.use_context::<u32>();
        ctx.render(live_view! { div { "x" } }).await?.finish().await
    }

    let mut driver = MockDriver::new();
    let mut rt = Runtime::new();
    let ctx = rt.ctx::<idyll::Never>();
    let frame = ctx.context_handle();
    rt.spawn(spawn_live(component, ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();

    assert!(!frame.paint_is_static(), "an absent context read is an external dependency");
}

/// The teardown-ordering pin: unmounting a non-keep branch that embeds a component
/// crosses a task seam — the branch's guards drop in the swap, but the child's
/// `RemoveFragment` waits for its executor task's reap. The frame's `FreeNodes`
/// must come after every teardown op of that cascade; the strict fold refuses the
/// stream otherwise, so folding the whole thing IS the assertion.
#[test]
fn branch_teardown_with_an_embedded_component_folds_cleanly() {
    #[component]
    async fn Inner(ctx: Ctx<Setup, idyll::Never>) -> Result {
        ctx.render(live_view! { p { ("inner content") } }).await?.finish().await
    }

    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx: Ctx<Setup, idyll::Never> = rt.ctx::<idyll::Never>();
    let owner = ctx.owner();
    let show = owner.mutable_signal(true);
    let show_r = show.read();
    rt.spawn(spawn_live(
        move |ctx: Ctx<Setup, idyll::Never>| {
            let show = show_r.clone();
            async move {
                ctx.render(live_view! {
                    @if ($show) {
                        div { Inner }
                    }
                })
                .await?
                .finish()
                .await
            }
        },
        ctx,
        report_to_log,
    ));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    show.set(&idyll::Turn::for_test(), false);
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    rt.run_to_quiescence();
    rt.flush(&mut driver);

    let html = idyll::fold_html(&driver.take_commands());
    assert!(
        !html.contains("inner content"),
        "the embedded child's DOM must be gone after the branch swap: {html}"
    );
}

/// A keyed `@for` inside an `@if` arm, toggled: the rows are the arm's to reclaim.
///
/// A row's DOM is owned by its `MountedRow`, so dropping the row is what removes it — whether a
/// splice removed it or the region around it went away. Before that was true the rows were
/// half-RAII: their bindings and ids fell out of their lifetime, but their DOM was reclaimed by an
/// explicit op beside it, which the ancestor-teardown path never reached. The arm's own anchor
/// cannot cover them either — a row anchor is a *sibling* of the list anchor, not one of the arm's
/// nodes — so each visit left its rows behind and the next one appended a fresh set.
#[test]
fn for_rows_inside_a_branch_are_reclaimed_when_the_branch_swaps() {
    #[derive(Debug)]
    enum Msg {
        Toggle,
    }

    let mut rt = Runtime::new();
    let mut driver = MockDriver::new();
    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(
        |ctx: Ctx<Setup, Msg>| async move {
            let listed = ctx.mutable_signal(true);
            let shown = listed.read();
            let rows = ctx.constant(vec![1u32, 2, 3]);

            let mut ctx = ctx.render(live_view! {
                div {
                    @if ($shown) {
                        @for n in (rows) [key = *n] {
                            span { ($n.to_string()) }
                        }
                    } else {
                        em { "empty" }
                    }
                }
            }).await?;

            loop {
                let (msg, turn) = ctx.recv().await?;
                match msg {
                    Msg::Toggle => listed.update(&turn, |on| *on = !*on),
                }
            }
        },
        ctx,
        report_to_log,
    ));

    rt.run_once();
    rt.process_pending_view(&mut driver);

    // Each visit to the list arm mounts three rows; each departure must remove three. The two
    // counts moving together is the whole claim — a mount the teardown does not answer for is a
    // row left in the document, which is what this regressed on.
    let census = |driver: &MockDriver| {
        (
            count_ops(driver, |op| matches!(op, DomOp::MountFragment { .. })),
            count_ops(driver, |op| matches!(op, DomOp::RemoveFragment { .. })),
        )
    };
    let (mounts_first, removes_first) = census(&driver);
    assert!(mounts_first >= 3, "the arm mounted its rows: {mounts_first}");

    let mut history = vec![(mounts_first, removes_first)];
    for _ in 0..4 {
        sender.send(Msg::Toggle);
        rt.run_once();
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        history.push(census(&driver));
    }

    // Live fragments are what was mounted less what was removed. Entering the arm mounts its rows
    // and leaving removes them, so the count alternates between two values — and the claim is that
    // it *alternates* rather than climbing: a visit that left its rows behind would raise the
    // floor on every cycle, which is exactly what this regressed on.
    let live: Vec<usize> = history.iter().map(|(m, r)| m - r).collect();
    for (i, &n) in live.iter().enumerate() {
        assert!(n <= live[0], "no swap leaves more live than the first arm did: {live:?}");
        if i % 2 == 0 {
            assert_eq!(n, live[0], "back on the list arm, one arm's worth is live: {live:?}");
        }
    }
    assert!(
        live.contains(&(live[0] - 3)),
        "and leaving the arm really does drop its three rows: {live:?}",
    );
}
