// The macros emit `::idyll_data::…` paths so they work in downstream crates; this
// self-alias lets the same expansion resolve inside this crate.
extern crate self as idyll_data;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::hash::Hash;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use idyll::{Owner, Signal, MutableSignal};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub mod error;
pub mod exec_op;
pub mod fragment;
pub mod ir;
#[cfg(feature = "registry")]
pub mod registry;
pub mod route;
pub mod store;
pub use idyll_schema as schema;

pub use error::{AbsorbError, CommitError, PageTitleError, StoreError, ValidateError};
pub use exec_op::{
    arg, execute, execute_mutation, validate, validate_mutation, validate_registered, AppRoot,
    ContentFn, ExecError, Executed,
    Fetch, MutationHandle, MutationResolver, Mutations, Queries, Resolvers, Root, RootHandle,
    RootResolver,
};

/// The server-side declaration of a **content field**: the resolver supplies source
/// text (markdown), and the registered content mapping ([`Resolvers::content`]) turns
/// it into View IR during operation execution — the app-side read yields
/// `idyll::View`. Serde-transparent: the node JSON carries the plain source string
/// until the mask transforms it, so resolvers stay stringly-simple.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Content(pub String);

impl From<String> for Content {
    fn from(source: String) -> Self {
        Content(source)
    }
}

impl From<&str> for Content {
    fn from(source: &str) -> Self {
        Content(source.to_string())
    }
}

/// Content is a schema leaf (`FieldType::Content`) — nothing further to register.
impl idyll_schema::SchemaType for Content {
    fn register(_schema: &mut idyll_schema::Schema) {}
}

/// A type-erased resolver/executor error. `Send + Sync` so execution futures stay
/// `Send` on the native host.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub use fragment::{
    field_json, field_value, read_fragment, resolve_fragment, Frag, Fragment, Live,
    NodeFragment,
};
pub use idyll_macros::{
    fragment, mutation, mutation_handler, node, query, root, value, Mutations, Queries,
};
pub use idyll_schema::{
    DescribeEnum, DescribeRecord, EnumDef, FieldDef, FieldType, MutationDef, MutationEntry,
    OpHash, RecordDef, RootDef, RootEntry, Schema, SchemaType, VariantDef,
};
pub use ir::{CanonMutation, CanonOp, FragmentDef, MutationFile, QueryFile, Sel, VariantSel};

pub use route::{page_title, validate_route_contract, Request, RouteRoots, ROUTE_ROOT};
pub use store::{NoStoreRoot, Store};

// Re-exported so generated projection code resolves `serde_json::Value` without the app
// depending on serde_json directly.
pub use serde_json;

/// The base every schema type shares: serializable, cloneable data. A **value type** —
/// no global identity, not independently fetchable — is *only* a `Record`: it rides
/// inline inside a containing [`Node`] and is reached by traversing an edge to it.
/// `#[value]` generates this. These types live **server-side** (the resolvers' world);
/// clients see only schema-generated projections.
pub trait Record: Clone + Serialize + DeserializeOwned + 'static {
    /// The type's **stable schema name** (`"User"`) — emitted by `#[node]`/`#[value]`.
    /// This is the one vocabulary shared by the published schema, [`FragmentDef::on`],
    /// and [`CacheMsg`] type tags. Unlike `std::any::type_name`, it is a deliberate,
    /// version-stable **wire identifier**.
    const TYPE_NAME: &'static str;
}

/// A **Node** is a globally-identified entity: it has an `id`, is independently
/// fetchable (the resolver table's fetchers load it by id), and is normalized in the
/// cache by `(schema type, id)`. `#[node]` generates this on top of [`Record`].
pub trait Node: Record {
    type Id: Clone + Eq + Hash + std::fmt::Debug + ToString + 'static;

    fn id(&self) -> Self::Id;
}

/// The cache's own message — the **only** way a record enters the store. Every write is
/// a recorded message, so dev replay and hydration are the same mechanism: feed the
/// `CacheMsg` log back and the store reconstructs exactly.
///
/// Ordering is positional, twice over: **within a seed, later commits supersede
/// earlier** (a diamond fetch resolves to the last write, per field); **across
/// replays, the later replay wins**. There is deliberately no per-commit version on
/// the wire — a number minted during one execution is meaningless in another's, and
/// true ordering between racing refreshes needs a server-stamped epoch, not a field
/// that looks like one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CacheMsg {
    /// Upsert a record's JSON under its schema type tag.
    Commit {
        type_tag: String,
        json: serde_json::Value,
    },
}

/// The serializable commit log an operation execution produces — the one data artifact
/// that crosses every membrane boundary: the SSR seed handed to the guest, the hydration
/// payload shipped to the browser, and the response to a client-navigation query. Plain
/// data (`Serialize`, `Send`), so the `!Send` reactive [`Cache`] never has to cross
/// anything — only its replayable log does.
///
/// Built host-side by the **op executor** ([`execute`]), which fetches against the
/// app's `Src` and records each reached Node; consumed by [`Cache::replay`] on the far
/// side (the guest for SSR, the browser for hydration/navigation).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Seed {
    commits: Vec<CacheMsg>,
}

impl Seed {
    pub fn new() -> Self {
        Self::default()
    }

    /// The committed records: `(schema type tag, record JSON)`, in commit order.
    pub fn records(&self) -> impl Iterator<Item = (&str, &serde_json::Value)> {
        self.commits.iter().map(|msg| match msg {
            CacheMsg::Commit { type_tag, json, .. } => (type_tag.as_str(), json),
        })
    }

    /// Record a fetched **Node** (server-side sugar over [`push_raw`](Self::push_raw)).
    pub fn push<T: Node>(&mut self, value: &T) {
        self.push_raw(
            T::TYPE_NAME,
            serde_json::to_value(value).expect("record serializes to JSON"),
        );
    }

    /// Record an already-serialized Node — the op executor's push (it works from the
    /// schema and resolver JSON, with no Rust node type in sight).
    pub fn push_raw(&mut self, type_tag: &str, json: serde_json::Value) {
        self.commits.push(CacheMsg::Commit {
            type_tag: type_tag.to_string(),
            json,
        });
    }

    pub fn commits(&self) -> &[CacheMsg] {
        &self.commits
    }

    pub fn len(&self) -> usize {
        self.commits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commits.is_empty()
    }

    /// The far-side entry point: replay this seed into a fresh cache rooted in
    /// `owner` — the component that owns the store presents its own authority.
    pub fn to_cache(&self, owner: Owner) -> Result<Cache, CommitError> {
        let cache = Cache::rooted(owner);
        cache.seed_into(self)?;
        Ok(cache)
    }

    /// Fold another seed's commits into this one — e.g. merging a deferred
    /// boundary's delta into the base seed. Position carries the ordering.
    pub fn absorb(&mut self, other: Seed) {
        self.commits.extend(other.commits);
    }
}

/// The result of executing an operation: the [`Seed`] to replay (or ship to the client
/// for hydration) plus the operation's **roots** — the top-level `Frag` keys the route
/// component reads from. `R` is the `query!`-generated `<Name>Roots` handle.
///
/// This is the route's proof-of-preload token: a component can't name a query without
/// evidence it was executed (no render-time fetch), and the roots hand out only child
/// `Frag` keys — never record data — so masking holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preloaded<R> {
    pub seed: Seed,
    pub roots: R,
}

impl<R: DeserializeOwned> Preloaded<R> {
    /// Decode the wire bytes a mount receives — the app's live table calls this,
    /// never `serde_json`.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|err| err.to_string())
    }
}

/// A typed reference to a [`Node`] — **server-side data modeling** (`author:
/// Ref<Author>` in a `#[node]` struct). On the wire it is the bare id; clients never
/// see this type, only the opaque ids inside their projections.
#[derive_where::derive_where(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ref<T: Node> {
    id: T::Id,
    _record: PhantomData<fn() -> T>,
}

impl<T> Serialize for Ref<T>
where
    T: Node,
    T::Id: Serialize,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.id.serialize(serializer)
    }
}

impl<'de, T> Deserialize<'de> for Ref<T>
where
    T: Node,
    T::Id: Deserialize<'de>,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::Id::deserialize(deserializer).map(Self::new)
    }
}

impl<T: Node> Ref<T> {
    pub fn new(id: T::Id) -> Self {
        Self {
            id,
            _record: PhantomData,
        }
    }

    pub fn id(&self) -> &T::Id {
        &self.id
    }
}

// ── The cache: schema-typed JSON records ─────────────────────────────────────────────

struct StoredRecord {
    signal: MutableSignal<serde_json::Value>,
}

/// `(schema type, canonical id)` — the normalization key.
type RecordKey = (String, String);

/// The canonical string form of a wire id (ids are small scalars; their JSON text is a
/// stable map key).
fn id_key(id: &serde_json::Value) -> String {
    serde_json::to_string(id).expect("record ids serialize")
}

/// The one shape rule every ingestion door applies: a commit is an object carrying
/// its `id`. Binds the proof — the id and the field map — so callers merge what was
/// matched instead of re-deriving it.
fn commit_shape<'a>(
    type_tag: &str,
    json: &'a serde_json::Value,
) -> Result<(&'a serde_json::Value, &'a serde_json::Map<String, serde_json::Value>), CommitError> {
    let serde_json::Value::Object(fields) = json else {
        return Err(CommitError::NotAnObject { type_tag: type_tag.to_string() });
    };
    let Some(id) = fields.get("id") else {
        return Err(CommitError::NoId { type_tag: type_tag.to_string() });
    };
    Ok((id, fields))
}

/// A handle to the normalized store, cheap to clone and shared through context.
///
/// The store is **schema-typed JSON**: records are keyed `(schema type name, id)` and
/// hold their wire JSON in a signal. No Rust node type exists client-side — fragments
/// read projections out of these cells ([`read_fragment`]), with field types drawn from
/// the published schema by the macros. Records only enter through [`CacheMsg::Commit`]
/// (replayed seeds), so the cache is the single writing authority for every cell.
#[derive(Clone)]
pub struct Cache {
    owner: Owner,
    records: Rc<RefCell<HashMap<RecordKey, StoredRecord>>>,
    /// Tasks suspended awaiting a record; woken after any commit applies.
    wakers: Rc<RefCell<Vec<Waker>>>,
}

impl Cache {
    /// The only constructor: a cache's cells belong to the presenting owner — a
    /// component node in an app, an explicit `Owner` in tests. There is deliberately
    /// no owner-less form.
    pub fn rooted(owner: Owner) -> Self {
        Self {
            owner,
            records: Rc::new(RefCell::new(HashMap::new())),
            wakers: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Insert a record built directly from JSON — the initial-seed path, which runs
    /// at setup with no turn. Every record is fresh (the cache starts empty and the
    /// seed is pre-merged by key), so this only ever *creates* cells. The seed was
    /// built by the server from the same schema, so a malformed record here is a
    /// broken contract: refused loudly, with the same shape rule every door applies.
    fn insert_json(&self, type_tag: &str, json: serde_json::Value) {
        let (id, _) = commit_shape(type_tag, &json).unwrap_or_else(|err| panic!("{err}: {json}"));
        let key = (type_tag.to_string(), id_key(id));
        self.records
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| StoredRecord { signal: self.owner.mutable_signal(json) });
    }

    /// Upsert a record's JSON in a turn (live data / a refresh commit). The id is the
    /// record's `id` field.
    pub fn upsert_json(
        &self,
        turn: &idyll::Turn,
        type_tag: &str,
        json: serde_json::Value,
    ) -> serde_json::Value {
        let (id, _) = commit_shape(type_tag, &json).unwrap_or_else(|err| panic!("{err}: {json}"));
        let id = id.clone();
        let key = (type_tag.to_string(), id_key(&id));
        let mut records = self.records.borrow_mut();
        match records.get(&key) {
            Some(record) => record.signal.set(turn, json),
            None => {
                records.insert(
                    key,
                    StoredRecord { signal: self.owner.mutable_signal(json) },
                );
            }
        }
        id
    }

    /// Apply one commit in a turn and notify its subscribers. See [`replay`](Self::replay)
    /// for the batch path.
    pub fn apply_commit(&self, turn: &idyll::Turn, msg: &CacheMsg) -> Result<(), CommitError> {
        if let Some(key) = self.apply_commit_silent(turn, msg)? {
            if let Some(record) = self.records.borrow().get(&key) {
                record.signal.notify(turn);
            }
        }
        self.wake_all();
        Ok(())
    }

    /// Apply one commit as a shallow field-merge — commits are masked to their
    /// operation's selection, and two operations may select different fields of the
    /// same record (a diamond), so incoming fields win and unmentioned fields
    /// persist. Returns the cell to notify, or `None` when nothing observable
    /// changed (an identical value, or a fresh record with no subscribers yet).
    fn apply_commit_silent(
        &self,
        turn: &idyll::Turn,
        msg: &CacheMsg,
    ) -> Result<Option<RecordKey>, CommitError> {
        let CacheMsg::Commit { type_tag, json } = msg;
        let (id, incoming) = commit_shape(type_tag, json)?;
        let key = (type_tag.clone(), id_key(id));
        let mut records = self.records.borrow_mut();
        match records.get_mut(&key) {
            Some(record) => {
                let mut merged = record.signal.now(turn);
                match &mut merged {
                    serde_json::Value::Object(existing) => {
                        for (field, value) in incoming {
                            existing.insert(field.clone(), value.clone());
                        }
                    }
                    // Every stored row entered through a door that refused
                    // non-objects; this is that invariant broken, not data.
                    other => panic!("stored `{type_tag}` row is not an object: {other}"),
                }
                if merged == record.signal.now(turn) {
                    return Ok(None);
                }
                record.signal.set_silent(turn, merged);
                Ok(Some(key))
            }
            None => {
                records.insert(
                    key,
                    StoredRecord {
                        signal: self.owner.mutable_signal(json.clone()),
                    },
                );
                Ok(None)
            }
        }
    }

    /// Replay a refresh [`Seed`] in a turn: apply every commit, then notify — writes
    /// land before any subscriber runs, so a projection re-reading the store
    /// mid-replay can never see a parent record pointing at children that haven't
    /// committed yet. Unchanged records notify nothing. (The initial seed goes
    /// through [`Seed::to_cache`], which needs no turn.)
    pub fn replay(&self, turn: &idyll::Turn, seed: &Seed) -> Result<(), CommitError> {
        let mut changed: HashSet<RecordKey> = HashSet::new();
        for commit in &seed.commits {
            if let Some(key) = self.apply_commit_silent(turn, commit)? {
                changed.insert(key);
            }
        }
        let records = self.records.borrow();
        for key in &changed {
            if let Some(record) = records.get(key) {
                record.signal.notify(turn);
            }
        }
        drop(records);
        self.wake_all();
        Ok(())
    }

    /// Build a cache from an initial seed at setup (no turn): merge the commits by
    /// record key at the JSON level, then create one cell per record. Merging
    /// happens before any cell exists, so no signal write — and thus no turn — is
    /// needed. Diamond selections in the seed compose here exactly as they would in
    /// a live merge.
    fn seed_into(&self, seed: &Seed) -> Result<(), CommitError> {
        // The merge holds the object `commit_shape` already narrowed, not a `Value`
        // that would have to be re-proved one on every merge.
        let mut merged: HashMap<RecordKey, serde_json::Map<String, serde_json::Value>> =
            HashMap::new();
        let mut order: Vec<RecordKey> = Vec::new();
        for CacheMsg::Commit { type_tag, json } in &seed.commits {
            let (id, fields) = commit_shape(type_tag, json)?;
            let key = (type_tag.clone(), id_key(id));
            match merged.get_mut(&key) {
                Some(existing) => {
                    for (field, value) in fields {
                        existing.insert(field.clone(), value.clone());
                    }
                }
                None => {
                    order.push(key.clone());
                    merged.insert(key, fields.clone());
                }
            }
        }
        for key in order {
            let fields = merged.remove(&key).expect("inserted with this key above");
            self.insert_json(&key.0, serde_json::Value::Object(fields));
        }
        Ok(())
    }

    /// Await a record's presence and return its live cell. **Never starts a load** —
    /// data must have been declared and executed up front; an unseeded read suspends
    /// (a deferred boundary or a bug surfaced), never a silent refetch.
    pub fn read_json(
        &self,
        type_tag: &str,
        id: &serde_json::Value,
    ) -> impl Future<Output = Signal<serde_json::Value>> {
        AwaitRecord {
            cache: self.clone(),
            key: (type_tag.to_string(), id_key(id)),
        }
    }

    /// A reader for a record's cell, if present — crate plumbing only.
    pub(crate) fn peek_json(&self, type_tag: &str, id: &serde_json::Value) -> Option<Signal<serde_json::Value>> {
        self.records
            .borrow()
            .get(&(type_tag.to_string(), id_key(id)))
            .map(|record| record.signal.read())
    }

    fn wake_all(&self) {
        let wakers = std::mem::take(&mut *self.wakers.borrow_mut());
        for waker in wakers {
            waker.wake();
        }
    }
}

/// The suspense future behind [`Cache::read_json`]: pending until the record is in the
/// store, at which point it yields the live cell. Re-polled when any commit applies.
struct AwaitRecord {
    cache: Cache,
    key: RecordKey,
}

impl Future for AwaitRecord {
    type Output = Signal<serde_json::Value>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let found = self
            .cache
            .records
            .borrow()
            .get(&self.key)
            .map(|record| record.signal.read());
        match found {
            Some(signal) => Poll::Ready(signal),
            None => {
                self.cache.wakers.borrow_mut().push(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// A scope for a test: a `Ctx` owns one, and a test has no component to receive
    /// one from — the same mint production uses.
    fn test_scope() -> (idyll::Runtime, idyll::Owner) {
        let rt = idyll::Runtime::new();
        let owner = rt.ctx::<()>().owner();
        (rt, owner)
    }

    use super::*;
    use std::task::Waker;

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct User {
        id: u64,
        name: String,
    }
    impl Record for User {
        const TYPE_NAME: &'static str = "User";
    }
    impl Node for User {
        type Id = u64;
        fn id(&self) -> u64 {
            self.id
        }
    }

    #[test]
    fn ref_serializes_as_record_id() {
        let reference = Ref::<User>::new(7);
        assert_eq!(serde_json::to_value(&reference).unwrap(), serde_json::json!(7));
        let back: Ref<User> = serde_json::from_value(serde_json::json!(7)).unwrap();
        assert_eq!(back.id(), &7);
    }

    #[test]
    fn replayed_seed_lands_records_readable_by_type_and_id() {
        let mut seed = Seed::new();
        seed.push(&User { id: 1, name: "Ada".into() });
        seed.push_raw("User", serde_json::json!({ "id": 2, "name": "Grace" }));

        let turn = idyll::Turn::for_test();
        let (_rt, owner) = test_scope();
        let cache = seed.to_cache(owner.clone()).expect("the seed replays");
        let one = cache.peek_json("User", &serde_json::json!(1)).unwrap();
        assert_eq!(one.now(&turn)["name"], "Ada");
        let two = cache.peek_json("User", &serde_json::json!(2)).unwrap();
        assert_eq!(two.now(&turn)["name"], "Grace");
    }

    /// Ingestion is where shape is decided: a commit that field-merge can't hold —
    /// a non-object, or one with no identity — refuses at the door with the kind
    /// typed, never a read discovering it later.
    #[test]
    fn a_malformed_commit_is_refused_at_ingestion() {
        let turn = idyll::Turn::for_test();
        let (_rt, owner) = test_scope();
        let cache = Cache::rooted(owner);
        let not_an_object = CacheMsg::Commit {
            type_tag: "User".into(),
            json: serde_json::json!("just a string"),
        };
        assert_eq!(
            cache.apply_commit(&turn, &not_an_object).unwrap_err(),
            CommitError::NotAnObject { type_tag: "User".into() }
        );
        let no_id = CacheMsg::Commit {
            type_tag: "User".into(),
            json: serde_json::json!({ "name": "unmoored" }),
        };
        assert_eq!(
            cache.apply_commit(&turn, &no_id).unwrap_err(),
            CommitError::NoId { type_tag: "User".into() }
        );
    }

    #[test]
    fn a_later_commit_supersedes_updating_the_same_cell() {
        let turn = idyll::Turn::for_test();
        let (_rt, owner) = test_scope();
        let cache = Cache::rooted(owner.clone());
        cache
            .apply_commit(&turn, &CacheMsg::Commit {
                type_tag: "User".into(),
                json: serde_json::json!({ "id": 1, "name": "current" }),
            })
            .expect("commit applies");
        let cell = cache.peek_json("User", &serde_json::json!(1)).unwrap();
        cache
            .apply_commit(&turn, &CacheMsg::Commit {
                type_tag: "User".into(),
                json: serde_json::json!({ "id": 1, "name": "newer" }),
            })
            .expect("commit applies");
        assert_eq!(
            cell.now(&turn)["name"],
            "newer",
            "signal identity holds across commits"
        );
    }

    #[test]
    fn an_id_less_commit_is_an_error_not_a_trap() {
        // Wire bytes the store cannot use are data, not a panic: guests are
        // panic=abort, so a trap here would take the whole page down instead of
        // faulting the store's owner to its boundary.
        let turn = idyll::Turn::for_test();
        let (_rt, owner) = test_scope();
        let cache = Cache::rooted(owner);
        let err = cache
            .apply_commit(&turn, &CacheMsg::Commit {
                type_tag: "User".into(),
                json: serde_json::json!({ "name": "no id here" }),
            })
            .expect_err("a commit with no `id` cannot be applied");
        assert!(err.to_string().contains("has no `id`"), "got: {err}");

        let seed = Seed {
            commits: vec![CacheMsg::Commit {
                type_tag: "User".into(),
                json: serde_json::json!({ "name": "no id here" }),
            }],
        };
        cache
            .replay(&turn, &seed)
            .expect_err("replay surfaces the same failure rather than trapping");
    }

    #[test]
    fn diamond_selections_merge_per_field() {
        // Two operations selected DIFFERENT fields of the same record; their masked
        // commits must compose, not clobber.
        let turn = idyll::Turn::for_test();
        let (_rt, owner) = test_scope();
        let cache = Cache::rooted(owner.clone());
        cache
            .apply_commit(&turn, &CacheMsg::Commit {
                type_tag: "User".into(),
                json: serde_json::json!({ "id": 1, "name": "Ada" }),
            })
            .expect("commit applies");
        cache
            .apply_commit(&turn, &CacheMsg::Commit {
                type_tag: "User".into(),
                json: serde_json::json!({ "id": 1, "role": "admin" }),
            })
            .expect("commit applies");
        let cell = cache.peek_json("User", &serde_json::json!(1)).unwrap();
        assert_eq!(cell.now(&turn)["name"], "Ada", "unmentioned field persists");
        assert_eq!(cell.now(&turn)["role"], "admin", "new field lands");
    }

    #[test]
    fn read_json_suspends_until_the_commit_lands_then_stays_live() {
        let turn = idyll::Turn::for_test();
        let (_rt, owner) = test_scope();
        let cache = Cache::rooted(owner);
        let mut cx = Context::from_waker(Waker::noop());
        let mut read = std::pin::pin!(cache.read_json("User", &serde_json::json!(9)));
        assert!(read.as_mut().poll(&mut cx).is_pending());

        cache
            .apply_commit(&turn, &CacheMsg::Commit {
                type_tag: "User".into(),
                json: serde_json::json!({ "id": 9, "name": "Ada" }),
            })
            .expect("commit applies");
        let Poll::Ready(signal) = read.as_mut().poll(&mut cx) else {
            panic!("read must resolve once the record lands");
        };
        assert_eq!(signal.now(&turn)["name"], "Ada");
    }

    #[test]
    fn the_commit_log_round_trips_through_json_into_a_fresh_cache() {
        let mut seed = Seed::new();
        seed.push(&User { id: 1, name: "Ada".into() });
        seed.push(&User { id: 1, name: "Lovelace".into() }); // diamond: later wins

        let wire = serde_json::to_string(&seed).unwrap();
        let replayed: Seed = serde_json::from_str(&wire).unwrap();
        let turn = idyll::Turn::for_test();
        let (_rt, owner) = test_scope();
        let cache = replayed.to_cache(owner.clone()).expect("the seed replays");
        let cell = cache.peek_json("User", &serde_json::json!(1)).unwrap();
        assert_eq!(cell.now(&turn)["name"], "Lovelace");
    }
}
