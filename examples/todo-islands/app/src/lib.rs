#![cfg_attr(target_arch = "wasm32", feature(type_alias_impl_trait))]

//! Data-driven todos with **per-row live**: the list renders from the page's
//! `todos` edge inside the `rows` store-root, each row carrying one keyed `check`
//! live — the marker's key IS the row's record reference, typed end to end. A
//! mutation's refresh seed re-syncs the keyed rows in place.

use idyll::{view, live_view, Ctx, Never, Setup};
use idyll_data::{fragment, query, Preloaded, Store};

fragment! { TodoCheck on Todo { id, text, done } }
fragment! { PageFrag on Page { id, title, todos: [TodoCheck] } }

query! { RouteQuery($request: idyll_data::Request) { route(request: $request): PageFrag } }

idyll_data::mutation! { AddTodoOp($text: String) = "add-todo"(text: $text) { id } }
idyll_data::mutation! { ToggleTodoOp($id: u64) = "toggle-todo"(id: $id) { id } }

pub type PageSeed = Preloaded<RouteQueryRoots>;

#[derive(Debug)]
pub enum RowsMsg {
    Add,
}

/// The store-root: an Add button above the list, which renders from the `todos`
/// edge — keyed rows, one `check` live each (the marker's key is the row's record
/// reference). A mutation's refresh seed re-syncs rows by identity.
pub async fn rows(ctx: Ctx<Setup, RowsMsg>, seed: PageSeed) -> idyll::Result {
    let store = Store::provide(&ctx, &seed)?;
    let live = PageFrag::read(&store.cache, store.page).await;
    let cache = store.cache.clone();
    let items = ctx.synced(
        move |cx| {
            live.get(cx)
                .todos
                .into_iter()
                .map(|frag| TodoCheck::resolve(&cache, frag).get(cx))
                .collect()
        },
        |item| TodoCheck::key(item.id),
    );
    let mut ctx = ctx.render(live_view! {
        button id=("add") style=("margin: 0 0 1rem") onclick=>(|_| Some(RowsMsg::Add)) { "Add todo" }
        ul id=("list") style=("list-style: none; padding: 0; margin: 0") {
            @for (frag, item) in $items {
                li style=("padding: .35rem 0; display: flex; gap: .5rem; align-items: center") {
                    @live(live::Check, key = frag)
                    span { ($item.text) }
                }
            }
        }
    }).await?;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            // Success arrives as data (the refresh seed re-syncs the keyed rows);
            // failure faults this live to its boundary.
            RowsMsg::Add => turn.mutate::<AddTodoOp>(AddTodoOpVars { text: "New todo".into() }),
        }
    }
}

#[derive(Debug)]
pub enum CheckMsg {
    Toggle,
}

/// One row's checkbox. The marker's key **is** this live's record reference: it
/// resolves its own todo from the live store — no positional convention anywhere.
pub async fn check(
    ctx: Ctx<Setup, CheckMsg>,
    seed: PageSeed,
    key: idyll_data::Frag<TodoCheck>,
) -> idyll::Result {
    let store = Store::of(&ctx, &seed)?;
    let live = TodoCheck::read(&store.cache, key).await;
    let mut ctx = ctx.render(live_view! {
        input type=("checkbox") checked[$live.done]
            onchange=>(|_| Some(CheckMsg::Toggle))
    }).await?;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            CheckMsg::Toggle => {
                let id = live.now(&turn).id;
                turn.mutate::<ToggleTodoOp>(ToggleTodoOpVars { id });
            }
        }
    }
}

/// The route view — the root component: the page chrome around the store-root hole.
pub async fn page(ctx: Ctx<Setup, Never>, _seed: PageSeed) -> idyll::Result {
    Ok(ctx.render_content(view! {
        div id=("app") style=("font-family: system-ui, sans-serif; max-width: 32rem; margin: 3rem auto; color: #222") {
            h1 style=("font-size: 1.4rem") { "Todos, data-driven" }
            live::Rows()
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
        rows(seed),
        check(seed, key: idyll_data::Frag<TodoCheck>),
    }
}
