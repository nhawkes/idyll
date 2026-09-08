//! Live **markers** through the one-IR server fold: content declares named holes —
//! the `live::Def()` mount call — where live code mounts. No component runs there:
//! the fold serializes the addressable `<idyll-live data-i>` wrapper, the
//! membrane's mount streams are spliced inside by **mount identity**
//! `(name, document-order instance)`, and live-free content reports no live so
//! the host ships zero client bytes.

use idyll::{fold_html, live_view, view, view_html, Ctx, Html, Setup};

/// The typed def `guest!` would generate for the live table's `counter` entry.
struct Counter;
impl idyll::live::LiveDef for Counter {
    const NAME: &'static str = "counter";
    type Key = idyll::live::NoKey;
}

#[derive(Debug)]
enum Msg {
    Inc,
}

/// The interactive part — a counter with its own message loop. In the new world this
/// lives in the app's wasm live table; here we drive it natively to produce exactly
/// the paint stream the membrane would return for a mount.
async fn counter(ctx: Ctx<Setup, Msg>, start: u32) -> idyll::Result {
    let n = ctx.mutable_signal(start);
    let mut ctx = ctx.render(live_view! {
        button onclick=>(|_| Msg::Inc) { $n }
    }).await?;
    loop {
        let (msg, _turn) = ctx.recv().await?;
        match msg {
            Msg::Inc => {}
        }
    }
}

/// What the membrane's `mount` returns, folded: the live's initial paint.
fn island_paint(start: u32) -> Html {
    let mut rt = idyll::Runtime::new();
    let mut driver = idyll::CommandBufferDriver::new();
    let ctx = rt.ctx::<Msg>();
    rt.spawn(idyll::component::spawn_live(move |ctx| counter(ctx, start), ctx, idyll::component::report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    fold_html(&driver.take_commands())
}

#[test]
fn a_static_shell_declares_islands_and_the_fold_splices_their_paint() {
    let body = view! {
        main {
            h1 { "Static shell" }
            Counter()
        }
    };

    // The marker is typed IR: the names surface without string comparison, in
    // document order — whoever splices this content owes each a mount.
    assert_eq!(body.live(), vec![("counter".to_string(), None)]);

    let html = view_html(&body, [(("counter".to_string(), 0), island_paint(3))]);
    assert!(html.contains("<h1>Static shell</h1>"), "static content lost: {}", html.as_str());
    assert!(
        html.contains("<idyll-live data-i=\"counter\" style=\"display:contents\">"),
        "live wrapper missing: {}",
        html.as_str()
    );
    assert!(
        html.contains("<button>3</button>"),
        "live's spliced paint missing or dirty: {}",
        html.as_str()
    );
    // Clean document: no claim scaffolding of any kind in the shipped HTML.
    for scaffolding in ["data-s", "idyll-t ", "<!--"] {
        assert!(!html.contains(scaffolding), "fold leaked `{scaffolding}`: {}", html.as_str());
    }
}

#[test]
fn the_same_island_twice_splices_by_mount_identity() {
    let body = view! {
        section { Counter() }
        section { Counter() }
    };
    assert_eq!(body.live(), vec![("counter".to_string(), None), ("counter".to_string(), None)]);

    // Distinct instances get distinct paint — keyed (name, document-order index),
    // the identity every fold derives by walking the same tree in the same order.
    let html = view_html(
        &body,
        [
            (("counter".to_string(), 0), island_paint(1)),
            (("counter".to_string(), 1), island_paint(2)),
        ],
    );
    let first = html.find("<button>1</button>").expect("instance 0 paint");
    let second = html.find("<button>2</button>").expect("instance 1 paint");
    assert!(first < second, "instance paints out of document order: {}", html.as_str());
}

#[test]
fn an_island_free_view_reports_no_islands() {
    let body = view! {
        main {
            h1 { "Just words" }
        }
    };
    assert!(body.live().is_empty(), "live-free content must need no client: {:?}", body.live());
    assert!(view_html(&body, []).contains("Just words"));
}

// ── Cross-mount context inheritance: the store-root spine ─────────────────────
//
// The guest mounts every live into ONE runtime; a child live's scope parents to
// the enclosing live's frame (`Ctx::new_under`), so the parent's provides — a
// shared signal here, a store cache in the real pattern — reach it. One flush then
// carries both live' updates: cross-live reactivity is ordinary same-thread
// reactivity.

#[derive(Debug)]
enum ParentMsg {
    Bump,
}

#[derive(Debug)]
enum ChildMsg {}

/// What a store-root provides: a cell it owns (only IT writes; descendants read).
#[derive(Clone)]
struct SharedCount(idyll::Signal<u32>);

async fn providing_parent(ctx: Ctx<Setup, ParentMsg>) -> idyll::Result {
    let n = ctx.mutable_signal(1u32);
    ctx.provide(SharedCount(n.read()));
    let mut ctx = ctx.render(live_view! { p { $n } }).await?;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            ParentMsg::Bump => n.update(&turn, |v| *v += 1),
        }
    }
}

async fn reading_child(ctx: Ctx<Setup, ChildMsg>) -> idyll::Result {
    let shared = ctx.use_context::<SharedCount>().expect("child inherits the parent's provide");
    let n = shared.0.clone();
    let mut ctx = ctx.render(live_view! { span { ($n) } }).await?;
    loop {
        let _ = ctx.recv().await?;
    }
}

fn set_texts(commands: &[idyll::DomCommand]) -> Vec<&str> {
    commands
        .iter()
        .filter_map(|c| match c {
            idyll::DomCommand::SetText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_child_mount_inherits_the_parent_mounts_context_and_updates_in_the_same_flush() {
    use idyll::component::{report_to_log, spawn_live};

    // The guest's shared-runtime shape: one runtime + driver, two live roots.
    let mut rt = idyll::Runtime::new();
    let mut driver = idyll::CommandBufferDriver::new();

    let parent_ctx = rt.ctx::<ParentMsg>();
    let bump = parent_ctx.inbox_sender();
    let parent_handle = parent_ctx.context_handle();
    let _parent_task = rt.spawn_scoped(spawn_live(|ctx| providing_parent(ctx), parent_ctx, report_to_log));
    rt.run_once();
    let _parent_mount = rt.mount_root(&mut driver);
    rt.flush(&mut driver);
    assert!(fold_html(&driver.take_commands()).contains("<p>1</p>"));

    // The child mounts UNDER the parent's frame — the provide (made during the
    // parent's setup, after the handle was taken) is visible through the live frame.
    let child_ctx = Ctx::<Setup, ChildMsg>::for_mount(&rt, Some(&parent_handle));
    let child_task = rt.spawn_scoped(spawn_live(|ctx| reading_child(ctx), child_ctx, report_to_log));
    rt.run_once();
    let child_mount = rt.mount_root(&mut driver);
    rt.flush(&mut driver);
    assert!(fold_html(&driver.take_commands()).contains("<span>1</span>"));

    // One write in the parent's turn; one flush; BOTH live' texts update.
    bump.send(ParentMsg::Bump);
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    let update = driver.take_commands();
    assert_eq!(set_texts(&update), vec!["2", "2"], "both mounts update in one flush: {update:?}");

    // Unmounting the child (dropping its guards — the guest's `unmount`) frees its
    // nodes and unhooks its subscription: the next write updates the parent alone.
    drop(child_mount);
    drop(child_task);
    rt.flush(&mut driver);
    let teardown = driver.take_commands();
    assert!(
        teardown.iter().any(|c| matches!(c, idyll::DomCommand::FreeNodes { .. })),
        "child unmount must free its minted nodes: {teardown:?}"
    );
    bump.send(ParentMsg::Bump);
    rt.run_to_quiescence();
    rt.flush(&mut driver);
    assert_eq!(set_texts(&driver.take_commands()), vec!["3"], "the child's binding must be gone");
}
