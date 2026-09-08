//! # deferred — fine-grained `useDeferredValue`
//!
//! The idyll analogue of React's [`useDeferredValue`]: a search box whose input
//! stays responsive while an expensive results list trails behind.
//!
//! ```text
//!  query  ──(immediate)──▶  the <input>'s own binding
//!    │
//!  ctx.deferred(|cx| query.get(cx))  ──▶  deferred_query  ──▶  results list
//!                                          (re-commits on the idle lane, so a
//!                                           burst of typing re-renders the input
//!                                           now and the list only once it settles)
//! ```
//!
//! Like React, the results read `deferred_query`, not `query`. While the `Input`
//! lane churns (a burst of keystrokes) the results keep showing the previous query
//! and only catch up when the idle lane drains — the urgent path never waits on the
//! expensive one.
//!
//! [`useDeferredValue`]: https://react.dev/reference/react/useDeferredValue

use idyll::{
    component::{report_to_log, spawn_live}, live_view, Ctx, MockDriver, Result, Runtime, Setup,
};

const CATALOG: &[&str] = &["apple", "apricot", "banana", "grape", "orange"];

enum Msg {
    Query(String),
}

async fn search(ctx: Ctx<Setup, Msg>) -> Result {
    let query = ctx.mutable_signal(String::new());

    // `deferred_query` trails `query` on the idle lane. The results below read it,
    // so they lag a fast typist rather than re-filtering on every keystroke.
    let query_r = query.read();
    let deferred_query = ctx.deferred(move |cx| query_r.get(cx));

    // The "expensive" subtree: a keyed list re-derived from the deferred value.
    let dq = deferred_query.clone();
    let results = ctx.synced(
        move |cx| {
            let needle = dq.get(cx).to_lowercase();
            CATALOG
                .iter()
                .filter(|item| item.contains(&needle))
                .map(|item| item.to_string())
                .collect()
        },
        |item: &String| item.clone(),
    );

    let mut ctx = ctx.render(live_view! {
        input value=($query) oninput=>(|e| Some(Msg::Query(e.value())))
        p id=("echo") { ($deferred_query) }
        ul id=("results") {
            @for (_id, item) in $results {
                li { ($item) }
            }
        }
    }).await?;

    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            Msg::Query(q) => query.set(&turn, q),
        }
    }
}

fn main() {
    let mut driver = MockDriver::new();
    let mut rt = Runtime::new();

    let ctx: Ctx<Setup, Msg> = rt.ctx::<Msg>();
    let sender = ctx.inbox_sender();

    rt.spawn(spawn_live(search, ctx, report_to_log));
    rt.run_once();
    rt.process_pending_view(&mut driver);

    // Type "ap": the deferred echo settles on the query and the list filters to the
    // matching items once the flush (idle lane included) drains.
    sender.send(Msg::Query("ap".to_string()));
    rt.run_once();
    rt.flush(&mut driver);

    let texts: Vec<String> = driver
        .log
        .iter()
        .filter_map(|op| match op {
            idyll::driver::DomOp::SetText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    println!("SetText values (in order): {texts:?}");

    assert_eq!(
        texts.last().map(String::as_str),
        Some("ap"),
        "deferred echo catches up to the query after a full flush"
    );
    assert!(
        texts.iter().any(|t| t == "apple"),
        "results filtered to items containing “ap”: {texts:?}"
    );

    println!("\nAll assertions passed ✓");
}
