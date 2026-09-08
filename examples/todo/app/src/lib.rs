#![cfg_attr(target_arch = "wasm32", feature(type_alias_impl_trait))]

//! The todo app's UI crate — **all the UI**: the page (the root component the server
//! mounts per request and the browser claims), its head, and the live. One wasip2
//! component paints on the server and hydrates in the browser; the server crate is
//! data and infra.

use idyll::{view, live_view, Ctx, Never, Setup};

pub mod atoms;
use idyll_data::{fragment, query, Frag, Preloaded, Store};

fragment! { TodoItem on Todo { id, text, done } }
fragment! { PageFrag on Page { id, title, route { Todos { todos: [TodoItem] }, Prose {} } } }

query! { RouteQuery($request: idyll_data::Request) { route(request: $request): PageFrag } }

idyll_data::mutation! { AddTodoOp($text: String) = "add-todo"(text: $text) { id, text, done } }

/// The executed route query — every mount's one argument, the page included.
pub type PageSeed = Preloaded<RouteQueryRoots>;

/// The route view — the root component. The server parsed the path exactly once; the
/// decision arrives as the seed's `route` sum, matched exhaustively — no fallback arm.
/// The route is a **mount-time value** (transitions re-mount, so it cannot change
/// under this component): a plain match, and each arm's paint is static chrome.
pub async fn page(ctx: Ctx<Setup, Never>, seed: PageSeed) -> idyll::Result {
    let store = Store::of(&ctx, &seed)?;
    let live = PageFrag::read(&store.cache, store.page).await;
    match live.at_mount(&ctx).route {
        PageFragRoute::Todos { .. } => Ok(ctx.render_content(view! {
            div id=("app") css=[atoms::styles::PAGE] {
                h1 css=[atoms::styles::TITLE] { "Todos" }
                live::Board()
                p { a id=("to-prose") href=("/prose") { "Prose gauntlet" } }
            }
        }).await?),
        PageFragRoute::Prose {} => Ok(ctx.render_content(view! {
            main css=[atoms::styles::PAGE] {
                h1 { "Prose" }
                live::Prose()
                p { a id=("to-todos") href=("/") { "Back to todos" } }
            }
        }).await?),
    }
}

/// The document-head content the app contributes. The `<title>` is a contract field
/// on the page node — the host writes it into the envelope, not the view.
pub async fn head(ctx: Ctx<Setup, Never>, _seed: PageSeed) -> idyll::Result {
    Ok(ctx.render_content(view! {
        meta charset=("utf-8")
        meta name=("viewport") content=("width=device-width, initial-scale=1")
    }).await?)
}

/// The store-root live: owns the page's store, provides it through context, and
/// declares the live that share it.
pub async fn board(ctx: Ctx<Setup, Never>, seed: PageSeed) -> idyll::Result {
    Store::provide(&ctx, &seed)?;
    Ok(ctx.render(live_view! {
        @live(live::Badge)
        @live(live::Todos)
    }).await?)
}

/// The todo edge, wherever the route put it — a page without todos simply has none.
fn todos_of(route: &PageFragRoute) -> &[Frag<TodoItem>] {
    match route {
        PageFragRoute::Todos { todos } => todos,
        PageFragRoute::Prose {} => &[],
    }
}

/// The count badge — a second live over the same records as the list.
pub async fn badge(ctx: Ctx<Setup, Never>, seed: PageSeed) -> idyll::Result {
    let store = Store::of(&ctx, &seed)?;
    let live = PageFrag::read(&store.cache, store.page).await;
    Ok(ctx.render(live_view! {
        p id=("badge") css=[atoms::styles::BADGE] {
            (todos_of(&$live.route).len().to_string()) " todos"
        }
    }).await?)
}

#[derive(Debug)]
pub enum Msg {
    Draft(String),
    Add,
    Dismiss,
}

#[idyll_styles::styles]
mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::{Face, Palette};

    pub const ADD: Style = css! {{
        display: "flex",
        gap: "0.5rem",
        margin: "0 0 1rem",
    }};
    pub const LIST: Style = css! {{
        list_style: "none",
        padding: "0",
        margin: "0",
    }};
    pub const ITEM: Style = css! {{
        padding: "0.35rem 0",
    }};
    pub const FIELD: Style = css! {{
        display: "flex",
        flex_direction: "column",
        gap: "4px",
    }};
    pub const LABEL: Style = css! {{
        font_family: Face::sans,
        font_size: "0.8125rem",
        color: Palette::muted,
    }};
    pub const DONE: Style = css! {{
        text_decoration: "line-through",
        color: Palette::muted,
    }};
}

pub async fn todos(ctx: Ctx<Setup, Msg>, seed: PageSeed) -> idyll::Result {
    let Store { cache, page, .. } = Store::of(&ctx, &seed)?;
    let live = PageFrag::read(&cache, page).await;
    let items = ctx.synced(
        move |cx| {
            todos_of(&live.get(cx).route)
                .iter()
                .map(|frag| TodoItem::resolve(&cache, frag.clone()).get(cx))
                .collect()
        },
        |item| item.id,
    );
    let draft = ctx.mutable_signal(String::new());
    let notice = ctx.mutable_signal(None);
    use crate::atoms::button::{Button, ButtonKind};
    use crate::atoms::input::TextInput;
    use crate::atoms::toast::Toast;
    let mut ctx = ctx.render(live_view! {
        div css=[styles::ADD] {
            label css=[styles::FIELD] {
                span css=[styles::LABEL] { "New todo" }
                TextInput name=("todo") placeholder=("What needs doing?") value=(draft)
                    typed=>(|text| Msg::Draft(text))
            }
            Button kind=(ButtonKind::Primary) label=("Add todo") pressed=>(|_| Msg::Add)
        }
        Toast notice=(notice) dismissed=>(|_| Msg::Dismiss)
        ul id=("todos") css=[styles::LIST] {
            @for (_id, item) in $items {
                li css=[styles::ITEM, $item.done => styles::DONE] {
                    ($item.text)
                }
            }
        }
    }).await?;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            Msg::Draft(text) => {
                draft.set(&turn, text);
                notice.set(&turn, None);
            }
            Msg::Dismiss => notice.set(&turn, None),
            // Success arrives as data: the refresh seed replays into the store and
            // the projections re-sync. Failure faults this live to its boundary.
            Msg::Add => match draft.now(&turn).trim() {
                "" => notice.set(&turn, Some("Type something to add it.".into())),
                text => {
                    let text = text.to_string();
                    turn.mutate::<AddTodoOp>(AddTodoOpVars { text: text.clone() });
                    draft.set(&turn, String::new());
                    notice.set(&turn, Some(format!("Added \"{text}\".")));
                }
            },
        }
    }
}

// ── The prose live: the claim-hardening gauntlet ───────────────────────────────────

#[derive(Debug)]
pub enum ProseMsg {
    Bump,
    Reverse,
    Toggle,
}

/// Deliberately the shapes that break naive positional hydration: dynamic text inline
/// between static text, and a list region that is not the last child of its parent.
/// The prose route carries no data — every row here is the live's own state.
pub async fn prose(ctx: Ctx<Setup, ProseMsg>, _seed: PageSeed) -> idyll::Result {
    let clicks = ctx.mutable_signal(0u32);
    let order = ctx.mutable_signal(vec![1u32, 2, 3]);
    let show = ctx.mutable_signal(true);
    let words = ctx.mutable_signal(vec!["claim", "hydrate", "splice"]);
    let mut ctx = ctx.render(live_view! {
        p id=("counter") {
            "You have clicked " $clicks " times — keep " "going" "!"
        }
        button id=("bump") onclick=>(|_| Some(ProseMsg::Bump)) { "Bump" }
        div id=("report") {
            @for w in $words [key = *w] {
                span { $w }
            }
            p id=("after") { "That list has an afterword — the region is not the last child." }
        }
        button id=("reverse") onclick=>(|_| Some(ProseMsg::Reverse)) { "Reverse" }
        ol id=("keyed") {
            @for n in $order [key = *n] {
                li { $n }
            }
        }
        button id=("toggle") onclick=>(|_| Some(ProseMsg::Toggle)) { "Toggle" }
        @if[keep] ($show) {
            p id=("kept") { "kept content" }
        }
    }).await?;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            ProseMsg::Bump => clicks.update(&turn, |v| *v += 1),
            ProseMsg::Reverse => order.update(&turn, |v| v.reverse()),
            ProseMsg::Toggle => show.update(&turn, |v| *v = !*v),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum PlotMsg {
    Grow,
}

/// The namespace gauntlet: SVG children that arrive from a *fragment* rather than from
/// their template's own root. A fragment's IR starts at the row's tag, so nothing in it
/// says "this lands inside an `<svg>`" — the namespace has to come from the mount. An
/// element built in the wrong one carries the right tag, the right attributes and the
/// right style, reports itself visible, and paints nothing, so only `isEqualNode`
/// against the parsed server fold catches it.
///
/// `foreignObject` is here for the other direction: the subtree inside it is HTML again.
pub async fn plot(ctx: Ctx<Setup, PlotMsg>, _seed: PageSeed) -> idyll::Result {
    let bars = ctx.mutable_signal(vec![10u32, 20, 30]);
    let mut ctx = ctx.render(live_view! {
        svg id=("plot") viewBox=("0 0 100 100") {
            rect x=("0") y=("0") width=("100") height=("100") {}
            @for h in $bars [key = *h] {
                g { rect width=("8") height=($h.to_string()) {} }
            }
            foreignObject x=("0") y=("0") width=("40") height=("20") {
                @for h in $bars [key = *h] {
                    span { $h }
                }
            }
        }
        button id=("grow") onclick=>(|_| Some(PlotMsg::Grow)) { "Grow" }
    }).await?;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            PlotMsg::Grow => bars.update(&turn, |v| v.push(40)),
        }
    }
}

idyll::guest! {
    seed: PageSeed,
    page: page(seed),
    head: head(seed),
    live: {
        board(seed),
        badge(seed),
        todos(seed),
        prose(seed),
        plot(seed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built executed route payload, shaped exactly like the interpreter's
    /// output: the Page record plus the Todo records its `todos` edge reached.
    pub(crate) fn seeded_data() -> PageSeed {
        let payload = serde_json::json!({
            "seed": { "commits": [
                { "Commit": { "type_tag": "Page",
                    "json": {
                        "id": "/", "title": "Todos — idyll",
                        "route": { "Todos": { "todos": [1, 2, 3] } }
                    } } },
                { "Commit": { "type_tag": "Todo",
                    "json": { "id": 1, "text": "Learn idyll", "done": true } } },
                { "Commit": { "type_tag": "Todo",
                    "json": { "id": 2, "text": "Preload a query", "done": true } } },
                { "Commit": { "type_tag": "Todo",
                    "json": { "id": 3, "text": "Render through the membrane", "done": false } } }
            ] },
            "roots": { "route": "/" }
        });
        serde_json::from_value(payload).unwrap()
    }

    #[test]
    fn the_todos_island_renders_the_same_content_as_a_command_stream() {
                use idyll::{CommandBufferDriver, DomCommand, Runtime};

        // Drive the *same* live component through a command-buffer driver (what
        // runtime.js will apply in the browser). No wasm, no JS bindings — pure idyll.
        let mut rt = Runtime::new();
        let mut driver = CommandBufferDriver::new();
        let ctx = rt.ctx::<Msg>();
        rt.spawn(idyll::component::spawn_live(
            |ctx| todos(ctx, seeded_data()),
            ctx,
            idyll::component::report_to_log,
        ));
        rt.run_once(); // setup + render
        rt.process_pending_view(&mut driver); // mount → template + event registrations
        rt.flush(&mut driver); // paint reactive text
        let commands = driver.commands();

        // The client render emits the same todo content the server rendered, as SetText…
        let text_of = |needle: &str| {
            commands.iter().any(|c| matches!(c, DomCommand::SetText { text, .. } if text.contains(needle)))
        };
        assert!(text_of("Learn idyll"), "live stream missing a todo: {commands:?}");
        assert!(text_of("Render through the membrane"), "live stream missing a todo: {commands:?}");

        // …and wires the Add button's click handler.
        assert!(
            commands.iter().any(|c| matches!(
                c,
                DomCommand::AddEventListener { event_type, .. } if event_type == "click"
            )),
            "live stream missing the Add click handler: {commands:?}"
        );
    }
}
