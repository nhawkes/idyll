#![cfg_attr(target_arch = "wasm32", feature(type_alias_impl_trait))]

//! The SPA posture: **one live at the top owns the page**. The server paints the
//! first frame through the same membrane; after that, navigation is data — the
//! live subscribes to navigation intents, answers with `turn.navigate` (the route
//! query re-executed, replayed into the store), and its `@match` on the route sum
//! swaps the arm. This is the one place route-as-a-signal is true: the mount
//! persists across navigations, so the route genuinely changes under it.

use idyll::{view, live_view, Ctx, Never, Setup};
use idyll_data::{fragment, query, Frag, Preloaded, Store};

fragment! { TodoItem on Todo { id, text, done } }
fragment! { PageFrag on Page { id, title, route { Todos { todos: [TodoItem] }, About {} } } }

query! { RouteQuery($request: idyll_data::Request) { route(request: $request): PageFrag } }

idyll_data::mutation! { AddTodoOp($text: String) = "add-todo"(text: $text) { id } }
idyll_data::mutation! { ToggleTodoOp($id: u64) = "toggle-todo"(id: $id) { id } }

pub type PageSeed = Preloaded<RouteQueryRoots>;

#[derive(Debug)]
pub enum Msg {
    Add,
    Toggle(u64),
    Navigate(String),
}

/// The todo edge, wherever the route put it — the about page simply has none.
fn todos_of(route: &PageFragRoute) -> &[Frag<TodoItem>] {
    match route {
        PageFragRoute::Todos { todos } => todos,
        PageFragRoute::About {} => &[],
    }
}

pub async fn app(ctx: Ctx<Setup, Msg>, seed: PageSeed) -> idyll::Result {
    let store = Store::provide(&ctx, &seed)?;
    // Await the seeded page once (data present by construction), then FOLLOW the
    // store's current page: a navigation's response names a new page record, and
    // this projection re-resolves to it.
    PageFrag::read(&store.cache, store.page.clone()).await;
    let current = store.current_page();
    let route = {
        let cache = store.cache.clone();
        let current = current.clone();
        ctx.computed(move |cx| PageFrag::resolve(&cache, current.get(cx)).get(cx).route)
    };
    let items = {
        let cache = store.cache.clone();
        ctx.synced(
            move |cx| {
                let page = PageFrag::resolve(&cache, current.get(cx)).get(cx);
                todos_of(&page.route)
                    .iter()
                    .map(|frag| TodoItem::resolve(&cache, frag.clone()).get(cx))
                    .collect()
            },
            |item| item.id,
        )
    };
    // Links are the navigation UI: the browser intercepts them (pushing history),
    // this subscription hears the path, and the loop answers with data.
    ctx.navigation(Msg::Navigate);
    let mut ctx = ctx.render(live_view! {
        @match $route {
            PageFragRoute::Todos { .. } => {
                button id=("add") style=("margin: 0 0 1rem") onclick=>(|_| Some(Msg::Add)) { "Add todo" }
                ul id=("list") style=("list-style: none; padding: 0; margin: 0") {
                    @for (id, item) in $items {
                        li style=("padding: .35rem 0; display: flex; gap: .5rem; align-items: center") {
                            input type=("checkbox") checked[$item.done]
                                onchange=>(move |_| Some(Msg::Toggle(id)))
                            span style=(if $item.done { "color: #999" } else { "" }) {
                                ($item.text)
                            }
                        }
                    }
                }
                p { a id=("to-about") href=("/about") { "About" } }
            }
            PageFragRoute::About {} => {
                p id=("about") { "One live owns this page; the route swaps under it as data." }
                p { a id=("to-todos") href=("/") { "Back to todos" } }
            }
        }
    }).await?;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            // Success arrives as data through the store; failure faults the live.
            Msg::Add => turn.mutate::<AddTodoOp>(AddTodoOpVars { text: "New todo".into() }),
            Msg::Toggle(id) => turn.mutate::<ToggleTodoOp>(ToggleTodoOpVars { id }),
            Msg::Navigate(path) => turn.navigate::<RouteQuery>(path),
        }
    }
}

/// The route view — the root component: the page chrome around the one live.
pub async fn page(ctx: Ctx<Setup, Never>, _seed: PageSeed) -> idyll::Result {
    Ok(ctx.render_content(view! {
        div id=("app") style=("font-family: system-ui, sans-serif; max-width: 32rem; margin: 3rem auto; color: #222") {
            h1 style=("font-size: 1.4rem") { "Todos, one live" }
            live::App()
        }
    }).await?)
}

/// Document-head content.
pub async fn head(ctx: Ctx<Setup, Never>, _seed: PageSeed) -> idyll::Result {
    Ok(ctx.render_content(view! {
        meta charset=("utf-8")
        meta name=("viewport") content=("width=device-width, initial-scale=1")
    }).await?)
}

idyll::guest! {
    seed: PageSeed,
    page: page(seed),
    head: head(seed),
    live: {
        app(seed),
    }
}
