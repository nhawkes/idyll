//! The **schema-first client loop**, end to end and natively: `fragment!`/`query!`
//! validate against the crate-root fixture `schema.json` and generate projections; the
//! op executor interprets the persisted artifact against resolvers; the executor's
//! output deserializes as `Preloaded<…Roots>`, replays into the JSON cache, and the
//! projections read it back — masking and reactivity intact. No node type exists on the
//! "client" side of this test.

use idyll_data::{execute, fragment, query, validate, Preloaded, Resolvers, Schema};

/// A scope for a test: a `Ctx` owns one, and a test has no component to receive one
/// from — the same mint production uses.
fn test_scope() -> (idyll::Runtime, idyll::Owner) {
    let rt = idyll::Runtime::new();
    let owner = rt.ctx::<()>().owner();
    (rt, owner)
}

// ── The client side: declared data needs (schema names, not Rust types) ────────────

fragment! { CommentRow on Comment { text } }
fragment! { Wallet on Money { amount, currency } }
fragment! { Byline on User { name, wallet: Wallet } }
fragment! { PostCard on Post { title, author: Byline, comments: [CommentRow] } }

query! { PostFeed() { posts: [PostCard] } }
query! { OnePost($id: u64) { post(id: $id): PostCard } }

// ── The server side: schema + resolvers over a toy source ──────────────────────────

fn schema() -> Schema {
    Schema::from_json(include_str!("../schema.json")).expect("fixture parses")
}

#[derive(Clone)]
struct Db;

impl Db {
    fn post_json(&self, id: u64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "title": format!("post {id}"),
            "author": 7,
            "comments": [100, 101],
        })
    }
}

/// `#[root]` under test end to end: the one signature emits descriptor + typed glue.
#[idyll_data::root]
async fn posts(db: &Db) -> Result<Vec<RawPost>, std::convert::Infallible> {
    Ok(vec![RawPost(db.post_json(1)), RawPost(db.post_json(2))])
}

fn resolvers() -> Resolvers<Db> {
    Resolvers::new().root(posts().resolver).fetch::<RawUser>().fetch::<RawComment>()
}

impl idyll_data::Fetch<Db> for RawUser {
    async fn fetch(_db: Db, id: u64) -> Result<RawUser, idyll_data::BoxError> {
        Ok(RawUser(serde_json::json!({
            "id": id, "name": "Ada", "wallet": { "amount": 5, "currency": "GBP" }
        })))
    }
}

impl idyll_data::Fetch<Db> for RawComment {
    async fn fetch(_db: Db, id: u64) -> Result<RawComment, idyll_data::BoxError> {
        Ok(RawComment(serde_json::json!({ "id": id, "text": format!("comment {id}") })))
    }
}

// Thin Node wrappers so the resolver table can name the schema types without real
// server model structs (a real server uses #[node] structs; the executor only sees
// JSON either way).
macro_rules! raw_node {
    ($name:ident, $tag:literal) => {
        #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        struct $name(serde_json::Value);
        impl idyll_data::Record for $name {
            const TYPE_NAME: &'static str = $tag;
        }
        impl idyll_data::Node for $name {
            type Id = u64;
            fn id(&self) -> u64 {
                self.0["id"].as_u64().expect("raw node id")
            }
        }
        // Raw wrappers have no field descriptors to publish; this test hand-builds
        // its schema, so the root's reachability registration contributes nothing.
        impl idyll_data::SchemaType for $name {
            fn register(_schema: &mut idyll_data::Schema) {}
        }
    };
}
raw_node!(RawPost, "Post");
raw_node!(RawUser, "User");
raw_node!(RawComment, "Comment");

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut cx = Context::from_waker(Waker::noop());
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
    }
}

#[test]
fn a_missing_fetcher_is_a_boot_failure_not_a_render_failure() {
    // The op typechecks against the schema either way — the schema cannot see which
    // resolvers exist. Forgetting a `.fetch::<T>()` must fail at boot, on the path the
    // crate advertises as boot-validated, rather than 500 on the first render that
    // follows the edge.
    let schema = schema();
    let op = idyll_data::CanonOp::from_canonical_json(&PostFeed::query_file().contents).unwrap();
    validate(&schema, &op).expect("typechecks: the schema knows nothing of resolvers");

    let complete = Resolvers::new().root(posts().resolver).fetch::<RawUser>().fetch::<RawComment>();
    idyll_data::validate_registered(&schema, &op, &complete).expect("every edge is registered");

    let missing_author = Resolvers::new().root(posts().resolver).fetch::<RawComment>();
    let err = idyll_data::validate_registered(&schema, &op, &missing_author)
        .expect_err("the `author` edge has no fetcher");
    assert_eq!(
        err,
        idyll_data::ValidateError::MissingFetcher { edge: "author".into(), target: "User".into() }
    );

    let no_root: Resolvers<Db> = Resolvers::new().fetch::<RawUser>().fetch::<RawComment>();
    let err = idyll_data::validate_registered(&schema, &op, &no_root)
        .expect_err("the `posts` root has no resolver");
    assert_eq!(err, idyll_data::ValidateError::MissingResolver { root: "posts".into() });
}

#[test]
fn the_full_loop_executes_replays_and_projects() {
    let schema = schema();

    // The operation's persisted artifact typechecks against the schema…
    let op = idyll_data::CanonOp::from_canonical_json(&PostFeed::query_file().contents).unwrap();
    validate(&schema, &op).expect("operation typechecks");

    // …the server interprets it…
    let executed = block_on(execute(&schema, &op, &resolvers(), &Db, &serde_json::json!({})))
        .expect("executes");

    // …and the client replays the wire bytes and reads through projections only.
    let preloaded: Preloaded<PostFeedRoots> =
        serde_json::from_slice(&executed.to_preloaded_json()).expect("wire shape matches Roots");
    let (_rt, owner) = test_scope();
    let cache = preloaded.seed.to_cache(owner).expect("the executed seed replays");

    let posts = preloaded.roots.posts();
    assert_eq!(posts.len(), 2);
    let turn = idyll::Turn::for_test();
    let card = block_on(PostCard::read(&cache, posts[0].clone())).now(&turn);
    assert_eq!(card.title, "post 1");

    // Node edge: the author landed in the seed and projects through Byline.
    let byline = block_on(Byline::read(&cache, card.author)).now(&turn);
    assert_eq!(byline.name, "Ada");

    // Value edge: no identity, so the child fragment embeds — already data.
    assert_eq!(byline.wallet.amount, 5);
    assert_eq!(byline.wallet.currency, "GBP");

    // List edge: every comment fetched, seeded, and readable.
    assert_eq!(card.comments.len(), 2);
    let row = block_on(CommentRow::read(&cache, card.comments[1].clone())).now(&turn);
    assert_eq!(row.text, "comment 101");
}

#[test]
fn the_artifact_identity_is_stable_and_self_verifying() {
    use sha2::{Digest, Sha256};
    let file = PostFeed::query_file();
    let mut hasher = Sha256::new();
    hasher.update(file.contents.as_bytes());
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(file.filename, format!("{hex}.query"), "filename == sha256(contents)");
    // The runtime identity (OpHash, two u64 words) is the digest's 128-bit prefix.
    assert_eq!(PostFeed::hash().to_string(), hex[..32]);

    // The operation inlines its fragment tree transitively — the artifact is complete.
    for field in ["title", "name", "amount", "text"] {
        assert!(file.contents.contains(field), "artifact missing inlined `{field}`:\n{}", file.contents);
    }
}

#[test]
fn variables_are_generated_and_named_args_validate() {
    // `OnePost($id: u64) { post(id: $id): PostCard }` compiled: the schema has the root,
    // the arg name, and the output type. The Vars struct is plain typed data.
    let vars = OnePostVars { id: 42 };
    assert_eq!(vars.id, 42);
}
