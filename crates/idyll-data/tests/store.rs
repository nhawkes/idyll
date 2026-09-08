//! The store's authority rules: a provided store is shared; building one without a
//! provider is a **mint**, warranted only in owner position (a root mount — the
//! server's isolated paint, the browser's page root). A child mount with no provider
//! above it is a mis-nesting, refused as a typed error — never a silently-stale
//! live.

use idyll::{Ctx, Runtime, Setup};
use idyll_data::{fragment, Frag, Preloaded, Seed, Store};

fragment! { PostHead on Post { title } }

#[derive(serde::Serialize, serde::Deserialize)]
struct Roots {
    post: u64,
}

impl idyll_data::RouteRoots for Roots {
    type Page = PostHead;
    fn page(&self) -> Frag<PostHead> {
        Frag::from_id(self.post)
    }
}

fn seed() -> Preloaded<Roots> {
    let mut seed = Seed::new();
    seed.push_raw("Post", serde_json::json!({ "id": 1, "title": "t" }));
    Preloaded { seed, roots: Roots { post: 1 } }
}

#[test]
fn a_root_mount_may_own_and_a_child_shares_the_provided_store() {
    // Owner position: the context tree's root builds its own store.
    let rt = Runtime::new();
    let root: Ctx<Setup, ()> = rt.ctx();
    let owned = Store::of(&root, &seed()).expect("a root mount is the owner position");

    // A child under a provider shares it — one cache: a write through the provider's
    // handle is visible through the child's.
    let provider: Ctx<Setup, ()> = rt.ctx();
    let provided = Store::provide(&provider, &seed()).expect("the seed replays");
    let child: Ctx<Setup, ()> = Ctx::for_mount(&rt, Some(&provider.context_handle()));
    let shared = Store::of(&child, &seed()).expect("the provided store is in context");
    let turn = idyll::Turn::for_test();
    provided.cache.upsert_json(&turn, "Post", serde_json::json!({ "id": 9, "title": "w" }));
    // Resolving through the child's handle sees the provider's write — one cache.
    let live = PostHead::resolve(&shared.cache, PostHead::key(9));
    assert_eq!(live.now(&turn).title, "w");
    drop(owned);
}

#[test]
fn a_child_mount_without_a_provider_is_refused() {
    let rt = Runtime::new();
    let parent: Ctx<Setup, ()> = rt.ctx();
    let child: Ctx<Setup, ()> = Ctx::for_mount(&rt, Some(&parent.context_handle()));

    // No provider above, and not owner position: minting here would produce an
    // live whose first frame is perfect and which never absorbs again.
    let Err(err) = Store::of(&child, &seed()) else {
        panic!("a child mount cannot mint a store");
    };
    let message = err.to_string();
    assert!(
        message.contains("store-root") && message.contains("Store::provide"),
        "the refusal names the fix: {message}"
    );
}
