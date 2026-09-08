//! **An unmounted subtree stops writing.** A view's ids are released by its mount guards;
//! its bindings are released by its scope. The two must go together, and for a
//! view-embedded child component they are reached by different routes — the guards through
//! the child's task, the scope only by the scope's own `Drop`. Anything that keeps that
//! scope alive past its unmount converts a leak into a use-after-free: `FreeNodes` releases
//! the ids, the surviving bindings keep painting, and the browser fold rejects the write on
//! a node it no longer knows.
//!
//! The shape that finds it is an `@if` arm holding a child component whose *own* view has a
//! fragment — the only configuration where a scope retains a reaction that can reach back
//! to it.

use idyll::driver::{DomCommand, NodeId};
use idyll::{live_view, CommandBufferDriver, Ctx, DomDriver, Never, Runtime, Setup, Signal};

#[derive(Debug)]
enum Msg {
    Switch,
    Paint,
}

/// A child component whose view carries both fragment kinds: a branch that re-selects as
/// the signal moves, and rows with reactive bindings of their own.
#[idyll::component]
async fn Panel(ctx: Ctx<Setup, Never>, label: Signal<String>, even: Signal<bool>) -> idyll::Result {
    let rows: Vec<Signal<String>> = (0..3).map(|_| label.clone()).collect();
    Ok(ctx
        .render(live_view! {
            div {
                @if ($even) { em { $label } } else { i { $label } }
                @for row in (rows) { b style=($row) {} }
            }
        })
        .await?)
}

async fn app(ctx: Ctx<Setup, Msg>) -> idyll::Result {
    let label = ctx.mutable_signal("v".to_string());
    let showing = ctx.mutable_signal(true);
    let show = showing.read();
    let even = {
        let label = label.read();
        ctx.computed(move |cx| label.get(cx).len() % 2 == 0).read()
    };
    let text = label.read();
    let mut ctx = ctx
        .render(live_view! {
            button onclick=>(|_| Msg::Switch) { "switch" }
            button onclick=>(|_| Msg::Paint) { "paint" }
            @if ($show) {
                Panel label=(text) even=(even)
            } else {
                div { "empty" }
            }
        })
        .await?;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            Msg::Switch => showing.update(&turn, |s| *s = !*s),
            Msg::Paint => label.update(&turn, |v| v.push('x')),
        }
    }
}

/// Ids a command may still name: introduced by `BindSlot` or a structural op, dropped by
/// `FreeNodes`/`RemoveFragment` — the browser fold's `nodes` map, replayed.
fn released_ids_written(commands: &[DomCommand]) -> Vec<(NodeId, DomCommand)> {
    let mut released: Vec<NodeId> = Vec::new();
    let mut offences = Vec::new();
    for command in commands {
        match command {
            DomCommand::BindSlot { node_id, .. } => released.retain(|id| id != node_id),
            DomCommand::MountFragment { anchor_id, .. }
            | DomCommand::ReplaceFragment { anchor_id, .. } => {
                released.retain(|id| id != anchor_id)
            }
            DomCommand::Paint { node_id, .. } => {
                if released.contains(node_id) {
                    offences.push((*node_id, command.clone()));
                }
            }
            DomCommand::RemoveFragment { anchor_id } => released.push(*anchor_id),
            DomCommand::FreeNodes { node_ids } => released.extend(node_ids.iter().copied()),
            DomCommand::SetText { node_id, .. }
            | DomCommand::SetAttr { node_id, .. }
            | DomCommand::SetStyleProp { node_id, .. }
            | DomCommand::RemoveAttr { node_id, .. }
            | DomCommand::SetBoolAttr { node_id, .. }
            | DomCommand::AddEventListener { node_id, .. }
            | DomCommand::WatchMeasure { node_id, .. } => {
                if released.contains(node_id) {
                    offences.push((*node_id, command.clone()));
                }
            }
            _ => {}
        }
    }
    offences
}

/// **An unmounted canvas stops painting, and lets go of its shapes.** A picture is not a
/// binding arm — it is one effect per shape, and the shape cells are the app's, not the
/// view's. So a canvas behind an `@if` is where the two could come apart: the effects are
/// held by the view's scope, the cells outlive it, and an effect that survived its scope
/// would go on writing to an id `free-nodes` has released.
#[test]
fn a_swapped_out_canvas_stops_painting_and_releases_its_shapes() {
    use idyll::driver::HandlerId;
    use idyll::{Curve, MutableVec, Shape};

    fn wire(at: f64) -> Shape {
        Shape {
            curve: Curve { from: (at, 0.0), c1: (1.0, 0.0), c2: (2.0, 1.0), to: (3.0, 1.0) },
            span: (0.0, 1.0),
            ink: "var(--wall)".to_string(),
            width: 1.0,
            alpha: (0.5, 0.5),
        }
    }

    async fn card(ctx: Ctx<Setup, Msg>) -> idyll::Result {
        let shapes: MutableVec<Shape> = ctx.mutable_vec_of(vec![wire(0.0), wire(1.0)]);
        let picture = ctx.mutable_vec_of(vec![shapes.clone()]);
        let showing = ctx.mutable_signal(true);
        let show = showing.read();
        let mut ctx = ctx
            .render(live_view! {
                button onclick=>(|_| Msg::Switch) { "switch" }
                button onclick=>(|_| Msg::Paint) { "paint" }
                @if ($show) { canvas painting=(picture) {} } else { div { "empty" } }
            })
            .await?;
        let mut at = 2.0;
        loop {
            let (msg, turn) = ctx.recv().await?;
            match msg {
                Msg::Switch => showing.update(&turn, |s| *s = !*s),
                Msg::Paint => {
                    at += 1.0;
                    shapes.sync(&turn, vec![wire(at), wire(at + 1.0)]);
                }
            }
        }
    }

    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(idyll::component::spawn_live(card, ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);

    let step = |rt: &mut Runtime, driver: &mut CommandBufferDriver, handler: u32| {
        driver.dispatch_event(HandlerId(handler), idyll::Event::default());
        rt.run_to_quiescence();
        rt.flush(driver);
    };
    let paints = |commands: &[DomCommand]| {
        commands.iter().filter(|c| matches!(c, DomCommand::Paint { .. })).count()
    };

    assert!(paints(&driver.take_commands()) > 0, "the mounted canvas paints");
    step(&mut rt, &mut driver, 1);
    assert_eq!(paints(&driver.take_commands()), 1, "a written shape is one command");

    step(&mut rt, &mut driver, 0); // swap the canvas away
    driver.take_commands();
    for _ in 0..3 {
        step(&mut rt, &mut driver, 1);
    }
    let after = driver.take_commands();
    assert_eq!(paints(&after), 0, "the swapped-out canvas is still painting: {after:#?}");

    // Swapping back mounts a fresh canvas, which paints the whole picture again — and over
    // the whole run nothing writes to an id the stream has released.
    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(idyll::component::spawn_live(card, ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    for _ in 0..4 {
        step(&mut rt, &mut driver, 1);
        step(&mut rt, &mut driver, 0);
        step(&mut rt, &mut driver, 1);
        step(&mut rt, &mut driver, 0);
        step(&mut rt, &mut driver, 1);
    }
    let commands = driver.take_commands();
    assert!(paints(&commands) > 0, "the remounted canvas paints again");
    let offences = released_ids_written(&commands);
    assert!(offences.is_empty(), "commands target released ids: {offences:#?}");
}

#[test]
fn a_swapped_out_child_component_stops_painting() {
    use idyll::driver::HandlerId;
    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(idyll::component::spawn_live(app, ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);

    let step = |rt: &mut Runtime, driver: &mut CommandBufferDriver, handler: u32| {
        driver.dispatch_event(HandlerId(handler), idyll::Event::default());
        rt.run_to_quiescence();
        rt.flush(driver);
    };
    step(&mut rt, &mut driver, 1); // paint, with the panel mounted
    step(&mut rt, &mut driver, 0); // swap the panel away
    driver.take_commands();

    // The panel is gone. Every write it would have made is a write to a released id.
    for _ in 0..3 {
        step(&mut rt, &mut driver, 1);
    }
    let after = driver.take_commands();
    assert!(
        after.is_empty(),
        "the swapped-out panel is still painting: {after:#?}"
    );

    // And across the whole run — mount, paint, swap, swap back, paint — no command ever
    // names an id the stream has released.
    let mut rt = Runtime::new();
    let mut driver = CommandBufferDriver::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(idyll::component::spawn_live(app, ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    for _ in 0..4 {
        step(&mut rt, &mut driver, 1);
        step(&mut rt, &mut driver, 0);
        step(&mut rt, &mut driver, 1);
        step(&mut rt, &mut driver, 1);
    }
    let commands = driver.take_commands();
    let offences = released_ids_written(&commands);
    assert!(offences.is_empty(), "commands target released ids: {offences:#?}");
}
