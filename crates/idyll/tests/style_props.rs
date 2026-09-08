//! `style:prop=(…)` — per-declaration style bindings. Moving parts write ONE CSS
//! property per update (`set-style-prop` on the wire → `style.setProperty` in the
//! browser fold), so the untouched declarations are never reparsed; the server fold
//! merges the same commands into the serialized `style` attribute.

use idyll::driver::{DomCommand, DomDriver as _, HandlerId};
use idyll::{fold_html, live_view, Ctx, Setup};

#[test]
fn ssr_merges_declarations_into_the_style_attribute() {
    // `style:prop` is a live binding, so the merge under test is the fold of a
    // mounted view's first paint — the exact stream a server paint produces.
    let mut rt = idyll::Runtime::new();
    let mut driver = idyll::CommandBufferDriver::new();
    let ctx = rt.ctx::<idyll::Never>();
    rt.spawn(idyll::component::spawn_live(
        |ctx: Ctx<Setup, idyll::Never>| async move {
            Ok(ctx
                .render(live_view! {
                    div class=("dot")
                        style=("width:10px;height:10px")
                        style:transform=("translate(3px,4px)")
                        style:--qv-p=("37.5%") {}
                })
                .await?)
        },
        ctx,
        idyll::component::report_to_log,
    ));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    let html = fold_html(&driver.take_commands());
    assert!(
        html.contains("width:10px") && html.contains("height:10px"),
        "static style survives: {html}"
    );
    assert!(html.contains("transform:translate(3px,4px)"), "prop merged: {html}");
    assert!(html.contains("--qv-p:37.5%"), "custom property merged: {html}");
}

#[derive(Debug)]
enum Msg {
    Nudge,
}

async fn dot(ctx: Ctx<Setup, Msg>) -> idyll::Result {
    let x = ctx.mutable_signal(0u32);
    let x_view = x.read();
    let mut ctx = ctx.render(live_view! {
        div onclick=>(|_| Msg::Nudge)
            style=("width:10px")
            style:transform=(format!("translate({}px,0px)", $x_view)) {}
    }).await?;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            Msg::Nudge => x.update(&turn, |v| *v += 1),
        }
    }
}

#[test]
fn live_updates_write_one_declaration_not_the_attribute() {
    let mut rt = idyll::Runtime::new();
    let mut driver = idyll::CommandBufferDriver::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(idyll::component::spawn_live(|ctx| dot(ctx), ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    let initial = driver.take_commands();
    assert!(
        initial.iter().any(|c| matches!(
            c,
            DomCommand::SetStyleProp { name, value, .. }
                if name == "transform" && value == "translate(0px,0px)"
        )),
        "initial paint carries the declaration: {initial:?}"
    );

    driver.dispatch_event(HandlerId(0), idyll::Event::default());
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    let update = driver.take_commands();
    assert!(
        update.iter().any(|c| matches!(
            c,
            DomCommand::SetStyleProp { name, value, .. }
                if name == "transform" && value == "translate(1px,0px)"
        )),
        "the update is a single declaration: {update:?}"
    );
    assert!(
        !update.iter().any(|c| matches!(c, DomCommand::SetAttr { name, .. } if name == "style")),
        "the style attribute itself must not be rewritten: {update:?}"
    );
}
