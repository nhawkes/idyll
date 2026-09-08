//! The op **executor**: a persisted registry artifact (canonical JSON) + the published
//! schema + an explicit resolver table → the `Preloaded`-shaped payload. This is the
//! server executing a *reviewed artifact* — no per-operation generated code anywhere.

use idyll_data::{
    execute, node, root, validate, value, BoxError, CanonOp, Fetch, Resolvers, Schema,
};

// ── The app under test: posts with authors ─────────────────────────────────────────

#[node]
pub struct Author {
    pub id: u64,
    pub name: String,
}

#[node]
pub struct Post {
    pub id: u64,
    pub title: String,
    pub author: idyll_data::Ref<Author>,
    /// Deliberately never selected by the artifact under test — must not cross the wire.
    pub draft: bool,
}

#[derive(Clone)]
struct Db;

impl Db {
    fn post(&self, id: u64) -> Post {
        Post { id, title: format!("post {id}"), author: idyll_data::Ref::new(7), draft: true }
    }
    fn author(&self, id: u64) -> Author {
        Author { id, name: format!("author {id}") }
    }
}

impl Fetch<Db> for Author {
    async fn fetch(db: Db, id: u64) -> Result<Author, BoxError> {
        Ok(db.author(id))
    }
}

/// The generated pair under test: `#[root]` emits `posts().entry` + `posts().resolver`
/// from this one signature — descriptor and typed execution glue cannot drift.
#[root]
async fn posts(db: &Db) -> Result<Vec<Post>, std::convert::Infallible> {
    Ok(vec![db.post(1), db.post(2)])
}

// The root's reachability closure registers Post and (through its edge) Author.
fn schema() -> Schema {
    Schema::new().root(posts().entry)
}

fn resolvers() -> Resolvers<Db> {
    Resolvers::new().root(posts().resolver).fetch::<Author>()
}

/// The persisted artifact under test: `posts { title, author { name } }` — written the
/// way the registry stores it (canonical JSON), because that file IS the input.
fn op() -> CanonOp {
    let json = serde_json::json!({
        "on": "Query",
        "selection": [{
            "kind": "root",
            "field": "posts",
            "args": [],
            "list": true,
            "frag": {
                "on": "Post",
                "selection": [
                    { "kind": "leaf", "field": "title" },
                    { "kind": "spread", "edge": "author",
                      "frag": { "on": "Author", "selection": [ { "kind": "leaf", "field": "name" } ] } }
                ]
            }
        }]
    });
    CanonOp::from_canonical_json(&json.to_string()).expect("artifact parses")
}

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
fn a_persisted_artifact_validates_and_executes_to_a_preloaded_payload() {
    let schema = schema();
    let op = op();
    validate(&schema, &op).expect("the artifact typechecks against the schema");

    let executed = block_on(execute(&schema, &op, &resolvers(), &Db, &serde_json::json!({})))
        .expect("executes");

    // The roots object is shaped like the generated `…Roots` struct: field → [ids].
    assert_eq!(executed.roots, serde_json::json!({ "posts": [1, 2] }));

    // Every reached node is seeded: 2 posts + the author edge fetched per post.
    let payload: serde_json::Value =
        serde_json::from_slice(&executed.to_preloaded_json()).unwrap();
    let commits = payload["seed"]["commits"].as_array().unwrap();
    let tags: Vec<&str> = commits.iter().map(|c| c["Commit"]["type_tag"].as_str().unwrap()).collect();
    assert_eq!(tags, ["Post", "Author", "Post", "Author"], "walk order: node then its edges");
    assert_eq!(commits[1]["Commit"]["json"]["name"], "author 7");

    // Masking is enforced at the source: the unselected `draft` field never crossed.
    assert_eq!(commits[0]["Commit"]["json"].get("draft"), None, "unselected field leaked");
    assert!(commits[0]["Commit"]["json"].get("title").is_some());
}

#[test]
fn boot_validation_rejects_schema_drift_loudly() {
    let op = op();

    // A schema that dropped the `author` edge: the checked-in artifact must fail at
    // boot. Drift is hand-built — a live registration can't produce it.
    let mut drifted = schema();
    drifted
        .nodes
        .iter_mut()
        .find(|node| node.name == "Post")
        .expect("Post registered")
        .fields
        .retain(|field| field.name != "author");
    let err = validate(&drifted, &op).unwrap_err();
    assert_eq!(
        err,
        idyll_data::ValidateError::UnknownField { scope: "Post".into(), field: "author".into() }
    );

    // An artifact naming a root the schema lacks.
    let mut no_root = schema();
    no_root.roots.clear();
    let err = validate(&no_root, &op).unwrap_err();
    assert_eq!(err, idyll_data::ValidateError::UnknownRoot { root: "posts".into() });
}

/// Absence is a value: a single root may yield `Option<T>`, and `Ok(None)` surfaces as
/// the typed [`ExecError::Absent`] — the server's 404, never a fault.
#[root]
async fn maybe_post(db: &Db, id: u64) -> Result<Option<Post>, std::convert::Infallible> {
    Ok((id == 1).then(|| db.post(1)))
}

#[test]
fn an_absent_single_root_is_typed_not_a_fault() {
    let schema = Schema::new().root(maybe_post().entry);
    let resolvers: Resolvers<Db> =
        Resolvers::new().root(maybe_post().resolver).fetch::<Author>();
    let op = CanonOp::from_canonical_json(
        &serde_json::json!({
            "on": "Query",
            "selection": [{
                "kind": "root", "field": "maybe_post", "args": ["id"], "list": false,
                "frag": { "on": "Post", "selection": [ { "kind": "leaf", "field": "title" } ] }
            }]
        })
        .to_string(),
    )
    .unwrap();
    idyll_data::validate(&schema, &op).expect("Option roots publish their Node output");

    // Present: executes normally.
    block_on(execute(&schema, &op, &resolvers, &Db, &serde_json::json!({ "id": 1 })))
        .expect("present record executes");

    // Absent: the typed outcome, not a stringly fault.
    let err = block_on(execute(&schema, &op, &resolvers, &Db, &serde_json::json!({ "id": 9 })))
        .unwrap_err();
    assert!(
        matches!(err, idyll_data::ExecError::Absent { ref root } if root == "maybe_post"),
        "expected Absent, got: {err}"
    );
}

// ── Sum-typed fields: validated exhaustively, masked and fetched per variant ────────

/// A sum on a node: the data variant carries a fetching edge, a selected leaf, and a
/// deliberately unselected leaf; the unit variant carries nothing.
#[value]
pub enum Feed {
    Pinned {
        author: idyll_data::Ref<Author>,
        note: String,
        /// Never selected by the artifact under test — must not cross the wire.
        secret: String,
    },
    Empty,
}

#[node]
pub struct Home {
    pub id: u64,
    pub feed: Feed,
}

#[root]
async fn home(db: &Db, pinned: bool) -> Result<Home, std::convert::Infallible> {
    let _ = db;
    let feed = if pinned {
        Feed::Pinned {
            author: idyll_data::Ref::new(7),
            note: "note".to_string(),
            secret: "never crosses".to_string(),
        }
    } else {
        Feed::Empty
    };
    Ok(Home { id: 1, feed })
}

/// `home { feed { Pinned { note, author { name } }, Empty {} } }`, as persisted.
fn home_op() -> CanonOp {
    let json = serde_json::json!({
        "on": "Query",
        "selection": [{
            "kind": "root", "field": "home", "args": ["pinned"], "list": false,
            "frag": {
                "on": "Home",
                "selection": [{
                    "kind": "enum",
                    "field": "feed",
                    "variants": [
                        { "variant": "Empty", "selection": [] },
                        { "variant": "Pinned", "selection": [
                            { "kind": "leaf", "field": "note" },
                            { "kind": "spread", "edge": "author",
                              "frag": { "on": "Author",
                                        "selection": [ { "kind": "leaf", "field": "name" } ] } }
                        ] }
                    ]
                }]
            }
        }]
    });
    CanonOp::from_canonical_json(&json.to_string()).expect("artifact parses")
}

#[test]
fn a_sum_typed_field_masks_and_fetches_the_matched_variant_only() {
    let schema = Schema::new().root(home().entry);
    let resolvers: Resolvers<Db> = Resolvers::new().root(home().resolver).fetch::<Author>();
    let op = home_op();
    validate(&schema, &op).expect("the exhaustive artifact typechecks");

    // The data variant: externally tagged, masked to its selection (`secret` gone),
    // the ref edge riding as its id AND fetched into the seed.
    let executed =
        block_on(execute(&schema, &op, &resolvers, &Db, &serde_json::json!({ "pinned": true })))
            .expect("executes");
    let payload: serde_json::Value =
        serde_json::from_slice(&executed.to_preloaded_json()).unwrap();
    let commits = payload["seed"]["commits"].as_array().unwrap();
    let tags: Vec<&str> =
        commits.iter().map(|c| c["Commit"]["type_tag"].as_str().unwrap()).collect();
    assert_eq!(tags, ["Home", "Author"], "the variant's edge fetches: {commits:?}");
    let feed = &commits[0]["Commit"]["json"]["feed"];
    assert_eq!(feed["Pinned"]["note"], "note");
    assert_eq!(feed["Pinned"]["author"], 7);
    assert_eq!(feed["Pinned"].get("secret"), None, "unselected variant field leaked");

    // The unit variant: a bare tag on the wire, nothing fetched.
    let executed =
        block_on(execute(&schema, &op, &resolvers, &Db, &serde_json::json!({ "pinned": false })))
            .expect("executes");
    let payload: serde_json::Value =
        serde_json::from_slice(&executed.to_preloaded_json()).unwrap();
    let commits = payload["seed"]["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1, "the empty feed fetches nothing: {commits:?}");
    assert_eq!(commits[0]["Commit"]["json"]["feed"], "Empty");
}

#[test]
fn boot_validation_demands_the_whole_sum() {
    let schema = Schema::new().root(home().entry);

    // An artifact that dropped the `Empty` arm: refused at boot, naming the variant.
    let partial = CanonOp::from_canonical_json(
        &serde_json::json!({
            "on": "Query",
            "selection": [{
                "kind": "root", "field": "home", "args": ["pinned"], "list": false,
                "frag": { "on": "Home", "selection": [{
                    "kind": "enum", "field": "feed",
                    "variants": [ { "variant": "Pinned", "selection": [
                        { "kind": "leaf", "field": "note" }
                    ] } ]
                }] }
            }]
        })
        .to_string(),
    )
    .unwrap();
    let err = validate(&schema, &partial).unwrap_err();
    assert!(
        matches!(
            &err,
            idyll_data::ValidateError::MissingVariant { variant, .. } if variant == "Empty"
        ),
        "unhelpful exhaustiveness error: {err}"
    );
}

// ── Embedded values: ride inline, but their own `Ref` edges still fetch ─────────────

/// A value with a fetching edge — reachable only *through* the value, so the walk must
/// recurse into embedded JSON to find it.
#[value]
pub struct Byline {
    pub author: idyll_data::Ref<Author>,
    pub label: String,
}

#[node]
pub struct Story {
    pub id: u64,
    pub byline: Byline,
    pub credits: Vec<Byline>,
}

#[root]
async fn story(db: &Db) -> Result<Story, std::convert::Infallible> {
    let _ = db;
    Ok(Story {
        id: 1,
        byline: Byline { author: idyll_data::Ref::new(3), label: "lead".to_string() },
        credits: vec![
            Byline { author: idyll_data::Ref::new(4), label: "photo".to_string() },
            Byline { author: idyll_data::Ref::new(5), label: "copy".to_string() },
        ],
    })
}

/// `story { byline { label, author { name } }, credits { label, author { name } } }`.
fn story_op() -> CanonOp {
    let byline_frag = serde_json::json!({
        "on": "Byline",
        "selection": [
            { "kind": "leaf", "field": "label" },
            { "kind": "spread", "edge": "author",
              "frag": { "on": "Author", "selection": [ { "kind": "leaf", "field": "name" } ] } }
        ]
    });
    let json = serde_json::json!({
        "on": "Query",
        "selection": [{
            "kind": "root", "field": "story", "args": [], "list": false,
            "frag": {
                "on": "Story",
                "selection": [
                    { "kind": "spread", "edge": "byline", "frag": byline_frag },
                    { "kind": "list", "edge": "credits", "frag": byline_frag }
                ]
            }
        }]
    });
    CanonOp::from_canonical_json(&json.to_string()).expect("artifact parses")
}

/// A `Ref` behind a value edge is part of the operation: the value rides inline in the
/// parent's commit (its ref as a bare id) AND the referenced node is fetched and
/// seeded — through a single value and through a value list alike. Without this, a
/// boot-validated artifact under-seeds and the client's read never resolves.
#[test]
fn a_ref_inside_an_embedded_value_fetches_and_seeds() {
    let schema = Schema::new().root(story().entry);
    let resolvers: Resolvers<Db> = Resolvers::new().root(story().resolver).fetch::<Author>();
    let op = story_op();
    validate(&schema, &op).expect("the artifact typechecks");
    idyll_data::validate_registered(&schema, &op, &resolvers)
        .expect("the fetcher the value's edge needs is registered");

    let executed = block_on(execute(&schema, &op, &resolvers, &Db, &serde_json::json!({})))
        .expect("executes");
    let payload: serde_json::Value =
        serde_json::from_slice(&executed.to_preloaded_json()).unwrap();
    let commits = payload["seed"]["commits"].as_array().unwrap();
    let tags: Vec<&str> =
        commits.iter().map(|c| c["Commit"]["type_tag"].as_str().unwrap()).collect();
    assert_eq!(
        tags,
        ["Story", "Author", "Author", "Author"],
        "every author behind a value edge seeds: {commits:?}"
    );

    let story = &commits[0]["Commit"]["json"];
    assert_eq!(story["byline"]["label"], "lead");
    assert_eq!(story["byline"]["author"], 3, "the value's ref rides as its id");
    assert_eq!(story["credits"][1]["author"], 5);
    let names: Vec<&str> =
        commits[1..].iter().map(|c| c["Commit"]["json"]["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["author 3", "author 4", "author 5"]);
}

#[test]
fn execution_refuses_unregistered_roots_and_fetchers() {
    let schema = schema();
    let op = op();

    // No resolver for the root.
    let err = block_on(execute(&schema, &op, &Resolvers::<Db>::new(), &Db, &serde_json::json!({})))
        .unwrap_err();
    assert!(err.to_string().contains("posts"), "unhelpful error: {err}");

    // Root registered but the edge fetcher missing.
    let no_fetcher: Resolvers<Db> = Resolvers::new()
        .root(posts().resolver);
    let err =
        block_on(execute(&schema, &op, &no_fetcher, &Db, &serde_json::json!({}))).unwrap_err();
    assert!(err.to_string().contains("Author"), "unhelpful error: {err}");
}

// ── Content fields: the mapping executes with the operation ───────────────────────

#[node]
pub struct Doc {
    pub id: u64,
    pub body: idyll_data::Content,
}

#[root]
async fn doc(db: &Db) -> Result<Doc, std::convert::Infallible> {
    let _ = db;
    Ok(Doc { id: 1, body: "hello content".into() })
}

fn doc_op() -> CanonOp {
    let json = serde_json::json!({
        "on": "Query",
        "selection": [{
            "kind": "root", "field": "doc", "args": [], "list": false,
            "frag": { "on": "Doc", "selection": [ { "kind": "leaf", "field": "body" } ] }
        }]
    });
    CanonOp::from_canonical_json(&json.to_string()).expect("artifact parses")
}

/// The strict lattice's one live→view door: a Content field's source runs through
/// the registered mapping DURING execution, so the payload carries View IR — the
/// read side deserializes a view, never source text. A Content field executed
/// without a mapping is a loud error, not a pass-through.
#[test]
fn content_fields_map_to_view_ir_in_the_payload() {
    let schema = Schema::new().root(doc().entry);
    let op = doc_op();
    validate(&schema, &op).expect("content leaf typechecks");

    let resolvers: Resolvers<Db> = Resolvers::new()
        .root(doc().resolver)
        .content(|source| idyll::View::text(source.to_uppercase()));
    let executed = block_on(execute(&schema, &op, &resolvers, &Db, &serde_json::json!({})))
        .expect("executes");
    let payload: serde_json::Value =
        serde_json::from_slice(&executed.to_preloaded_json()).unwrap();
    let body = &payload["seed"]["commits"][0]["Commit"]["json"]["body"];
    let view: idyll::View = serde_json::from_value(body.clone()).expect("the field IS a view");
    assert_eq!(
        view,
        idyll::View::text("HELLO CONTENT"),
        "the mapping ran with the operation; IR rode the wire"
    );

    let bare: Resolvers<Db> = Resolvers::new().root(doc().resolver);
    let err = block_on(execute(&schema, &op, &bare, &Db, &serde_json::json!({})))
        .expect_err("no mapping registered");
    assert!(format!("{err}").contains("content mapping"), "unhelpful error: {err}");
}
