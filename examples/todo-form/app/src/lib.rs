#![cfg_attr(target_arch = "wasm32", feature(type_alias_impl_trait))]

//! Minimal hydration: **the add form is the only live**. The list renders below
//! it from the page's `todos` edge — plain rows, no row-level JS anywhere; a
//! mutation's refresh seed re-syncs them in place.

use idyll::{view, live_view, Ctx, Never, Setup};
use idyll_data::{fragment, query, Preloaded, Store};

fragment! { TodoRow on Todo { id, text, done } }
fragment! { PageFrag on Page { id, title, todos: [TodoRow] } }

query! { RouteQuery($request: idyll_data::Request) { route(request: $request): PageFrag } }

idyll_data::mutation! { AddTodoOp($text: String) = "add-todo"(text: $text) { id } }

pub type PageSeed = Preloaded<RouteQueryRoots>;

#[derive(Debug)]
pub enum Msg {
    Draft(String),
    Add,
}

pub async fn add(ctx: Ctx<Setup, Msg>, seed: PageSeed) -> idyll::Result {
    let store = Store::provide(&ctx, &seed)?;
    let live = PageFrag::read(&store.cache, store.page).await;
    let cache = store.cache.clone();
    let items = ctx.synced(
        move |cx| {
            live.get(cx)
                .todos
                .into_iter()
                .map(|frag| TodoRow::resolve(&cache, frag).get(cx))
                .collect()
        },
        |item| item.id,
    );
    let draft = ctx.mutable_signal(String::new());
    let mut ctx = ctx.render(live_view! {
        div style=("margin: 0 0 1rem; display: flex; gap: .5rem") {
            input id=("draft") type=("text") placeholder=("What needs doing?")
                value=($draft) oninput=>(|e| Some(Msg::Draft(e.value())))
            button id=("add") onclick=>(|_| Some(Msg::Add)) { "Add" }
        }
        ul id=("list") style=("list-style: none; padding: 0; margin: 0") {
            @for (_id, item) in $items {
                li style=(if $item.done {
                    "padding: .35rem 0; text-decoration: line-through; color: #999"
                } else {
                    "padding: .35rem 0"
                }) {
                    ($item.text)
                }
            }
        }
    }).await?;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            Msg::Draft(text) => draft.set(&turn, text),
            Msg::Add => {
                let text = draft.now(&turn).trim().to_string();
                if !text.is_empty() {
                    turn.mutate::<AddTodoOp>(AddTodoOpVars { text });
                    draft.set(&turn, String::new());
                }
            }
        }
    }
}

/// The route view — the root component: the page chrome around the one live.
pub async fn page(ctx: Ctx<Setup, Never>, _seed: PageSeed) -> idyll::Result {
    Ok(ctx.render_content(view! {
        div id=("app") style=("font-family: system-ui, sans-serif; max-width: 32rem; margin: 3rem auto; color: #222") {
            h1 style=("font-size: 1.4rem") { "Todos, one form" }
            live::Add()
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
        add(seed),
    }
}
