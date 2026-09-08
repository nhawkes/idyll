//! The **op executor** — the server's execution path for persisted operations.
//!
//! A registry artifact ([`CanonOp`]) is *data*; the published [`Schema`] types it; an
//! explicit [`Resolvers`] table is the only app code involved. The executor interprets
//! the operation's selection against the schema — call the named root, push each fetched
//! Node into a [`Seed`], follow `Ref` edges through the registered fetchers, recurse into
//! the sub-selections — and returns the seed plus the roots object the client's
//! `Preloaded<…Roots>` deserializes from.
//!
//! Nothing here is generated per operation: the registry is the executable surface, the
//! schema is the type system, and adding an operation is adding an *artifact*, not code.
//! (Per-operation generated `preload` still exists for the typed client crates; this path
//! is what lets the server execute any reviewed artifact without depending on them.)
//!
//! [`validate`] is the boot-time check: every registry op must typecheck against the
//! current schema — a dangling field or root is a **startup failure**, not a request-time
//! 500. Execution assumes a validated op and stays lean.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::ValidateError;
use crate::ir::{CanonMutation, CanonOp, CanonSel};
use crate::schema::{FieldType, MutationEntry, RootEntry, Schema};
use crate::{BoxError, Node, Seed};

type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type RootThunk<Src> =
    Arc<dyn Fn(Src, serde_json::Value) -> BoxFut<Result<serde_json::Value, BoxError>> + Send + Sync>;
type FetchThunk<Src> =
    Arc<dyn Fn(Src, serde_json::Value) -> BoxFut<Result<serde_json::Value, BoxError>> + Send + Sync>;
type MutationThunk<Src> =
    Arc<dyn Fn(Src, serde_json::Value) -> BoxFut<Result<serde_json::Value, BoxError>> + Send + Sync>;

/// The app's **explicit resolver table** — the server's whole execution surface: root
/// name → resolver, Node type → fetcher, mutation name → handler. Registered once next
/// to the schema definition (no global registry); typed at the registration site,
/// JSON-erased inside so one table serves every operation.
pub struct Resolvers<Src> {
    roots: HashMap<String, RootThunk<Src>>,
    fetchers: HashMap<String, FetchThunk<Src>>,
    mutations: HashMap<String, MutationThunk<Src>>,
    content: Option<ContentFn>,
}

/// The app's registered content mapping: source text to View IR, executed with the
/// operation wherever the data layer runs. One per app until a second kind of
/// content exists (the second-user rule); registered once, invocable on both sides.
pub type ContentFn = Arc<dyn Fn(&str) -> idyll::View + Send + Sync>;

/// One root's typed execution glue, emitted by `#[root]` (`<fn>_resolver()`): the
/// schema name plus a thunk that decodes the operation's named variables into the fn's
/// typed parameters **before** the body runs — no JSON digging in app code, and the
/// descriptor (`<fn>_schema()`) comes from the same signature, so they cannot drift.
pub struct RootResolver<Src> {
    name: String,
    thunk: RootThunk<Src>,
}

impl<Src: Clone + Send + Sync + 'static> RootResolver<Src> {
    /// Wrap a typed resolve future. Normally produced by `#[root]`, not by hand; the
    /// generated closure does the per-parameter [`arg`] decode and calls the fn.
    pub fn new<T, F, Fut>(name: &str, resolve: F) -> Self
    where
        T: Serialize,
        F: Fn(Src, serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, BoxError>> + Send + 'static,
    {
        RootResolver {
            name: name.to_string(),
            thunk: Arc::new(move |src, vars| {
                let fut = resolve(src, vars);
                Box::pin(async move { Ok(serde_json::to_value(fut.await?)?) })
            }),
        }
    }
}

/// One mutation's typed execution glue, emitted by `#[mutation]` (`<fn>_resolver()`) —
/// the exact mutation twin of [`RootResolver`]. The fn returns the full output **Node**;
/// [`execute_mutation`] masks it to the artifact's recorded selection.
pub struct MutationResolver<Src> {
    name: String,
    thunk: MutationThunk<Src>,
}

impl<Src: Clone + Send + Sync + 'static> MutationResolver<Src> {
    pub fn new<T, F, Fut>(name: &str, handler: F) -> Self
    where
        T: Node + Serialize,
        F: Fn(Src, serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, BoxError>> + Send + 'static,
    {
        MutationResolver {
            name: name.to_string(),
            thunk: Arc::new(move |src, vars| {
                let fut = handler(src, vars);
                Box::pin(async move { Ok(serde_json::to_value(fut.await?)?) })
            }),
        }
    }
}

/// One query entry, whole: the schema half (def + reachability registration) and the
/// execution half — generated together by `#[root]` from the one fn signature, so
/// they cannot drift. The value a `Query` group's field holds; its name travels
/// inside (the fn's identifier), so no field naming can misname an entry.
pub struct RootHandle<Src> {
    pub entry: RootEntry,
    pub resolver: RootResolver<Src>,
}

/// One mutation entry, whole — the mutation twin of [`RootHandle`]. The wire name
/// (`#[mutation_handler("add-todo")]`) lives in the def.
pub struct MutationHandle<Src> {
    pub entry: MutationEntry,
    pub resolver: MutationResolver<Src>,
}

/// A group of query entries — implemented by `#[derive(Queries)]` on the app's own
/// `Query` struct, whose fields hold [`RootHandle`]s. More queries are more fields;
/// the field names are the app's bindings, never wire names.
pub trait Queries<Src> {
    fn entries(self) -> Vec<RootHandle<Src>>;
}

/// A group of mutation entries — `#[derive(Mutations)]`'s trait. An app with no
/// mutations derives it on a unit struct.
pub trait Mutations<Src> {
    fn entries(self) -> Vec<MutationHandle<Src>>;
}

/// Node-associated loading — how the executor follows a `Ref<T>` edge. The app
/// implements it per Node for its source type:
/// `impl Fetch<Db> for Todo { async fn fetch(db: Db, id: u64) -> … }`.
pub trait Fetch<Src>: Node {
    fn fetch(src: Src, id: Self::Id) -> impl Future<Output = Result<Self, BoxError>> + Send;
}

/// The app's entry surface — GraphQL's root-type model, structurally: ONE typed
/// object whose `query`/`mutation` groups yield **both** the published schema (the
/// reachability closure of the entries) and the resolver table. The parallel
/// schema/resolver listings cannot drift because there is one listing.
pub struct Root<Q, M> {
    pub query: Q,
    pub mutation: M,
}

/// [`Root`], assembled: the schema and resolver table one root object yields. What
/// `Server::builder().root(…)` actually stores (`Root` converts via `Into`).
pub struct AppRoot<Src> {
    pub schema: Schema,
    pub resolvers: Resolvers<Src>,
}

impl<Src: Clone + Send + Sync + 'static> AppRoot<Src> {
    /// Attach `T`'s [`Fetch`] impl (GraphQL's node resolver — keyed by output type,
    /// not a root field). Chain one per fetched Node type.
    pub fn fetch<T>(mut self) -> Self
    where
        T: Fetch<Src> + Serialize,
        T::Id: DeserializeOwned + Send,
    {
        self.resolvers = self.resolvers.fetch::<T>();
        self
    }
}

impl<Q, M, Src> From<Root<Q, M>> for AppRoot<Src>
where
    Q: Queries<Src>,
    M: Mutations<Src>,
    Src: Clone + Send + Sync + 'static,
{
    /// Assemble the one listing into both halves.
    fn from(root: Root<Q, M>) -> Self {
        let mut schema = Schema::new();
        let mut resolvers = Resolvers::new();
        for handle in root.query.entries() {
            schema = schema.root(handle.entry);
            resolvers = resolvers.root(handle.resolver);
        }
        for handle in root.mutation.entries() {
            schema = schema.mutation(handle.entry);
            resolvers = resolvers.mutation(handle.resolver);
        }
        AppRoot { schema, resolvers }
    }
}

impl<Src: Clone + Send + Sync + 'static> AppRoot<Src> {
    /// Attach the app's content mapping (see [`Resolvers::content`]).
    pub fn content(mut self, mapping: impl Fn(&str) -> idyll::View + Send + Sync + 'static) -> Self {
        self.resolvers = self.resolvers.content(mapping);
        self
    }
}

/// Decode one named operation variable into a typed fn parameter — the generated
/// resolvers' boundary parse. Loud on both failure modes; app code never sees JSON.
pub fn arg<T: DeserializeOwned>(vars: &serde_json::Value, name: &str) -> Result<T, BoxError> {
    let value = vars
        .get(name)
        .ok_or_else(|| format!("missing operation variable `{name}`"))?;
    serde_json::from_value(value.clone())
        .map_err(|err| format!("operation variable `{name}` does not decode: {err}").into())
}

impl<Src: Clone + Send + Sync + 'static> Resolvers<Src> {
    pub fn new() -> Self {
        Resolvers {
            roots: HashMap::new(),
            fetchers: HashMap::new(),
            mutations: HashMap::new(),
            content: None,
        }
    }

    /// Register the app's content mapping (markdown -> View IR). A `Content` field
    /// executed without one is a loud operation error, never a silent pass-through.
    pub fn content(mut self, mapping: impl Fn(&str) -> idyll::View + Send + Sync + 'static) -> Self {
        self.content = Some(Arc::new(mapping));
        self
    }

    /// Register a root's typed glue (`.root(article_resolver())`).
    pub fn root(mut self, resolver: RootResolver<Src>) -> Self {
        self.roots.insert(resolver.name, resolver.thunk);
        self
    }

    /// Register `T`'s [`Fetch`] impl — the fetcher the executor follows `Ref<T>`
    /// edges with.
    pub fn fetch<T>(mut self) -> Self
    where
        T: Fetch<Src> + Serialize,
        T::Id: DeserializeOwned + Send,
    {
        self.fetchers.insert(
            T::TYPE_NAME.to_string(),
            Arc::new(|src, id_json| {
                let id: Result<T::Id, _> = serde_json::from_value(id_json);
                match id {
                    Ok(id) => Box::pin(async move {
                        Ok(serde_json::to_value(T::fetch(src, id).await?)?)
                    }),
                    Err(err) => Box::pin(std::future::ready(Err(BoxError::from(err)))),
                }
            }),
        );
        self
    }
}

impl<Src: Clone + Send + Sync + 'static> Resolvers<Src> {
    /// Register a mutation's typed glue (`.mutation(add_todo_resolver())`). State
    /// changes go through `Src`'s own interior mutability (a shared handle, like a
    /// pool); [`execute_mutation`] masks the returned node to the artifact's selection.
    pub fn mutation(mut self, resolver: MutationResolver<Src>) -> Self {
        self.mutations.insert(resolver.name, resolver.thunk);
        self
    }

    /// Whether a handler is registered for the named schema mutation (the boot check:
    /// a persisted artifact whose handler is missing is a startup failure).
    pub fn has_mutation(&self, name: &str) -> bool {
        self.mutations.contains_key(name)
    }

    /// Whether a resolver is registered for the named schema root.
    pub fn has_root(&self, field: &str) -> bool {
        self.roots.contains_key(field)
    }

    /// Whether a fetcher is registered for the named schema record — how the executor
    /// follows a `Ref<T>` edge to it.
    pub fn has_fetcher(&self, type_name: &str) -> bool {
        self.fetchers.contains_key(type_name)
    }
}

/// Validate a persisted mutation artifact against the schema: the mutation exists,
/// every argument it passes is declared, and the response selection typechecks on the
/// output record. Run for the whole registry **at boot**.
pub fn validate_mutation(schema: &Schema, m: &CanonMutation) -> Result<(), ValidateError> {
    let def = schema
        .mutation_def(&m.mutation)
        .ok_or_else(|| ValidateError::UnknownMutation { mutation: m.mutation.clone() })?;
    for arg in &m.args {
        if !def.args.iter().any(|declared| declared.name == *arg) {
            return Err(ValidateError::UnknownArgument {
                mutation: m.mutation.clone(),
                argument: arg.clone(),
            });
        }
    }
    if m.response.on != def.output {
        return Err(ValidateError::MutationOutput {
            mutation: m.mutation.clone(),
            yields: def.output.clone(),
            selects: m.response.on.clone(),
        });
    }
    validate_fragment(schema, &m.response)
}

/// Execute a **validated** mutation artifact: run its handler with the client's
/// variables and mask the returned node to the artifact's recorded selection — what the
/// operation didn't select does not cross back.
pub async fn execute_mutation<Src: Clone + Send + Sync + 'static>(
    schema: &Schema,
    m: &CanonMutation,
    resolvers: &Resolvers<Src>,
    src: &Src,
    vars: &serde_json::Value,
) -> Result<serde_json::Value, BoxError> {
    let thunk = resolvers
        .mutations
        .get(&m.mutation)
        .ok_or_else(|| format!("no handler registered for mutation `{}`", m.mutation))?;
    let node = thunk(src.clone(), vars.clone()).await?;
    mask_selection(schema, &m.response, &node, resolvers.content.as_ref())
}

impl<Src: Clone + Send + Sync + 'static> Default for Resolvers<Src> {
    fn default() -> Self {
        Self::new()
    }
}

/// Why an execution didn't produce a result: the data is **absent** (a single root
/// resolved to nothing — the content equivalent of a 404, not a fault) or something
/// actually failed. Absence is a *value* on the resolver side (`Result<Option<T>, E>`);
/// this is its typed surface at the boundary, so the server can answer 404 vs 500
/// without string-matching errors.
#[derive(Debug)]
pub enum ExecError {
    /// A single root yielded no record for these variables.
    Absent { root: String },
    Fault(BoxError),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Absent { root } => write!(f, "root `{root}` has no record for these variables"),
            ExecError::Fault(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<BoxError> for ExecError {
    fn from(err: BoxError) -> Self {
        ExecError::Fault(err)
    }
}

impl From<String> for ExecError {
    fn from(err: String) -> Self {
        ExecError::Fault(err.into())
    }
}

/// An executed operation: the seed to replay plus the roots object, shaped exactly like
/// the operation's generated `…Roots` struct (`field → id | [ids]` — a `Ref` serializes
/// as its bare id), so `Preloaded` deserializes from [`to_preloaded_json`](Self::to_preloaded_json).
#[derive(Debug)]
pub struct Executed {
    pub seed: Seed,
    pub roots: serde_json::Value,
}

impl Executed {
    /// The `Preloaded { seed, roots }` wire value — the page's seed (the executed route
    /// query), or the operation endpoint's response body.
    pub fn to_preloaded_value(&self) -> serde_json::Value {
        serde_json::json!({ "seed": self.seed, "roots": self.roots })
    }

    /// [`to_preloaded_value`](Self::to_preloaded_value) as bytes.
    pub fn to_preloaded_json(&self) -> Vec<u8> {
        serde_json::to_vec(&self.to_preloaded_value()).expect("executed operation serializes")
    }
}

/// Check the operation against the **resolver table**: every root it selects has a
/// resolver, and every `Ref` edge it follows has a fetcher. Run for the whole registry
/// at boot, beside [`validate`] — which sees only the schema, and so cannot catch this.
///
/// Without it, a missing `.fetch::<T>()` boots clean and 500s on the first render that
/// follows the edge, on a path the whole crate advertises as boot-validated. The
/// mutation half of the check is [`Resolvers::has_mutation`].
pub fn validate_registered<Src: Clone + Send + Sync + 'static>(
    schema: &Schema,
    op: &CanonOp,
    resolvers: &Resolvers<Src>,
) -> Result<(), ValidateError> {
    for sel in &op.selection {
        let CanonSel::Root { field, frag, .. } = sel else {
            continue; // shape is `validate`'s job; it runs first
        };
        if !resolvers.has_root(field) {
            return Err(ValidateError::MissingResolver { root: field.clone() });
        }
        check_fragment_fetchers(schema, frag, resolvers)?;
    }
    Ok(())
}

/// Walk a fragment's fetching edges the way [`follow_edges`] will, and demand a fetcher
/// for each target. Shares [`fields_edge_is_ref`] with the executor, so the two agree on
/// which edges fetch.
fn check_fragment_fetchers<Src: Clone + Send + Sync + 'static>(
    schema: &Schema,
    frag: &CanonOp,
    resolvers: &Resolvers<Src>,
) -> Result<(), ValidateError> {
    let record = schema
        .record(&frag.on)
        .ok_or_else(|| ValidateError::UnknownRecord { record: frag.on.clone() })?;
    check_selection_fetchers(schema, &record.fields, &frag.selection, resolvers)
}

fn check_selection_fetchers<Src: Clone + Send + Sync + 'static>(
    schema: &Schema,
    fields: &[idyll_schema::FieldDef],
    selection: &[CanonSel],
    resolvers: &Resolvers<Src>,
) -> Result<(), ValidateError> {
    for sel in selection {
        match sel {
            CanonSel::Leaf { .. } | CanonSel::Root { .. } => {}
            CanonSel::Spread { edge, frag } | CanonSel::List { edge, frag } => {
                if fields_edge_is_ref(fields, edge) {
                    if !resolvers.has_fetcher(frag.on.as_str()) {
                        return Err(ValidateError::MissingFetcher {
                            edge: edge.clone(),
                            target: frag.on.clone(),
                        });
                    }
                    check_fragment_fetchers(schema, frag, resolvers)?;
                } else {
                    // A value edge rides inline; its own edges still fetch.
                    check_fragment_fetchers(schema, frag, resolvers)?;
                }
            }
            CanonSel::Enum { field, variants } => {
                let enum_name = match fields.iter().find(|f| &f.name == field).map(|f| &f.ty) {
                    Some(FieldType::Value { value }) => value.as_str(),
                    _ => continue, // shape is `validate`'s job
                };
                let Some(def) = schema.enumeration_def(enum_name) else {
                    continue;
                };
                for selected in variants {
                    if let Some(variant) = def.variant(&selected.variant) {
                        check_selection_fetchers(
                            schema,
                            &variant.fields,
                            &selected.selection,
                            resolvers,
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Validate an operation against the **schema**: every root exists (with matching
/// list-ness and output type), every selected field exists on its record, and every edge
/// spread lands on the type the schema declares. Run for the whole registry at boot,
/// before [`validate_registered`] adds the resolver-table check.
pub fn validate(schema: &Schema, op: &CanonOp) -> Result<(), ValidateError> {
    for sel in &op.selection {
        let CanonSel::Root { field, list, frag, .. } = sel else {
            return Err(ValidateError::TopLevelNotRoot { op: op.content_hash() });
        };
        let root = schema
            .root_def(field)
            .ok_or_else(|| ValidateError::UnknownRoot { root: field.clone() })?;
        if root.list != *list {
            return Err(ValidateError::RootArity { root: field.clone(), schema_list: root.list });
        }
        if root.output != frag.on {
            return Err(ValidateError::RootOutput {
                root: field.clone(),
                yields: root.output.clone(),
                selects: frag.on.clone(),
            });
        }
        validate_fragment(schema, frag)?;
    }
    Ok(())
}

fn validate_fragment(schema: &Schema, frag: &CanonOp) -> Result<(), ValidateError> {
    let record = schema
        .record(&frag.on)
        .ok_or_else(|| ValidateError::UnknownRecord { record: frag.on.clone() })?;
    validate_selection(schema, &frag.on, &record.fields, &frag.selection)
}

/// Validate one selection set against its **field scope** — a record's fields or one
/// enum variant's: the two places a selection reads from share one rule set.
fn validate_selection(
    schema: &Schema,
    scope: &str,
    fields: &[idyll_schema::FieldDef],
    selection: &[CanonSel],
) -> Result<(), ValidateError> {
    let field_type = |name: &str| -> Result<&FieldType, ValidateError> {
        fields.iter().find(|f| f.name == name).map(|f| &f.ty).ok_or_else(|| {
            ValidateError::UnknownField { scope: scope.to_string(), field: name.to_string() }
        })
    };
    for sel in selection {
        match sel {
            CanonSel::Leaf { field } => {
                field_type(field)?;
            }
            CanonSel::Spread { edge, frag: child } => {
                match field_type(edge)? {
                    FieldType::Ref { node } | FieldType::Value { value: node } if *node == child.on => {}
                    other => {
                        return Err(ValidateError::SpreadEdge {
                            scope: scope.to_string(),
                            edge: edge.clone(),
                            target: child.on.clone(),
                            found: other.clone(),
                        })
                    }
                }
                validate_fragment(schema, child)?;
            }
            CanonSel::List { edge, frag: child } => {
                match field_type(edge)? {
                    // Node-reference lists (fetched) and embedded value lists (inline)
                    // both spread as `[Child]`.
                    FieldType::List { of }
                        if matches!(&**of, FieldType::Ref { node } if *node == child.on)
                            || matches!(&**of, FieldType::Value { value } if *value == child.on) => {}
                    other => {
                        return Err(ValidateError::ListEdge {
                            scope: scope.to_string(),
                            edge: edge.clone(),
                            target: child.on.clone(),
                            found: other.clone(),
                        })
                    }
                }
                validate_fragment(schema, child)?;
            }
            CanonSel::Enum { field, variants } => {
                // A sum-typed field is matched **whole**: every schema variant selected
                // exactly once (the closed set is the point), each variant's selection
                // validated against that variant's own fields.
                let FieldType::Value { value } = field_type(field)? else {
                    return Err(ValidateError::NotSum {
                        scope: scope.to_string(),
                        field: field.clone(),
                    });
                };
                let def = schema.enumeration_def(value).ok_or_else(|| ValidateError::UnknownEnum {
                    scope: scope.to_string(),
                    field: field.clone(),
                    name: value.clone(),
                })?;
                for selected in variants {
                    let variant =
                        def.variant(&selected.variant).ok_or_else(|| ValidateError::UnknownVariant {
                            enumeration: value.clone(),
                            variant: selected.variant.clone(),
                        })?;
                    validate_selection(
                        schema,
                        &format!("{value}::{}", variant.name),
                        &variant.fields,
                        &selected.selection,
                    )?;
                }
                for variant in &def.variants {
                    if !variants.iter().any(|v| v.variant == variant.name) {
                        return Err(ValidateError::MissingVariant {
                            scope: scope.to_string(),
                            field: field.clone(),
                            enumeration: value.clone(),
                            variant: variant.name.clone(),
                        });
                    }
                }
            }
            CanonSel::Root { field, .. } => {
                return Err(ValidateError::NestedRoot {
                    root: field.clone(),
                    scope: scope.to_string(),
                })
            }
        }
    }
    Ok(())
}

/// Execute a **validated** operation: call each root, seed every reached Node, follow
/// edges through the fetchers, and shape the roots object. `vars` are the client's named
/// variables, passed through to the root resolvers.
///
/// A single root resolving to JSON `null` (a resolver's `Ok(None)`) is a typed
/// [`ExecError::Absent`] — content that isn't there, distinct from a fault.
pub async fn execute<Src: Clone + Send + Sync + 'static>(
    schema: &Schema,
    op: &CanonOp,
    resolvers: &Resolvers<Src>,
    src: &Src,
    vars: &serde_json::Value,
) -> Result<Executed, ExecError> {
    let mut seed = Seed::new();
    let mut roots = serde_json::Map::new();
    for sel in &op.selection {
        let CanonSel::Root { field, list, frag, .. } = sel else {
            return Err(format!("unvalidated operation reached execute: {sel:?}").into());
        };
        let thunk = resolvers
            .roots
            .get(field)
            .ok_or_else(|| format!("no resolver registered for root `{field}`"))?;
        let out = thunk(src.clone(), vars.clone()).await?;
        if *list {
            let nodes = match out {
                serde_json::Value::Array(nodes) => nodes,
                other => return Err(format!("list root `{field}` yielded non-array {other}").into()),
            };
            let mut ids = Vec::with_capacity(nodes.len());
            for node in nodes {
                ids.push(seed_node(schema, &resolvers.fetchers, src, &mut seed, frag, node, resolvers.content.as_ref()).await?);
            }
            roots.insert(field.clone(), serde_json::Value::Array(ids));
        } else {
            if out.is_null() {
                return Err(ExecError::Absent { root: field.clone() });
            }
            let id = seed_node(schema, &resolvers.fetchers, src, &mut seed, frag, out, resolvers.content.as_ref()).await?;
            roots.insert(field.clone(), id);
        }
    }
    Ok(Executed { seed, roots: serde_json::Value::Object(roots) })
}

/// Mask a fetched node's JSON down to the operation's **selection**: the id (identity
/// always rides), each selected leaf, each edge's raw id(s), and value edges masked
/// recursively by their child selection. What the operation didn't select does not
/// cross the wire — masking is enforced at the source, not just in the client types.
fn mask_selection(
    schema: &Schema,
    frag: &CanonOp,
    node: &serde_json::Value,
    content: Option<&ContentFn>,
) -> Result<serde_json::Value, BoxError> {
    let record = schema
        .record(&frag.on)
        .ok_or_else(|| format!("record `{}` is not in the schema", frag.on))?;
    let mut masked = serde_json::Map::new();
    if let Some(id) = node.get("id") {
        masked.insert("id".to_string(), id.clone());
    }
    mask_fields(schema, &frag.on, &record.fields, &frag.selection, node, content, &mut masked)?;
    Ok(serde_json::Value::Object(masked))
}

/// Mask one selection set within its field scope into `out` — shared by records and
/// enum variants.
fn mask_fields(
    schema: &Schema,
    scope: &str,
    fields: &[idyll_schema::FieldDef],
    selection: &[CanonSel],
    node: &serde_json::Value,
    content: Option<&ContentFn>,
    out: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), BoxError> {
    for sel in selection {
        let (field, value) = match sel {
            CanonSel::Leaf { field } => {
                let value = node
                    .get(field)
                    .cloned()
                    .ok_or_else(|| format!("`{scope}` JSON is missing `{field}`"))?;
                // A Content leaf transforms HERE: the resolver supplied source text,
                // the registered mapping executes with the operation, and View IR is
                // what rides the wire -- the strict lattice's one live->view door.
                let def = fields.iter().find(|f| &f.name == field);
                let value = if matches!(def.map(|d| &d.ty), Some(idyll_schema::FieldType::Content)) {
                    let source = value.as_str().ok_or_else(|| {
                        format!("`{scope}.{field}` content source is not a string")
                    })?;
                    let mapping = content.ok_or_else(|| {
                        format!(
                            "`{scope}.{field}` is Content but no content mapping is                              registered (Resolvers::content)"
                        )
                    })?;
                    serde_json::to_value(mapping(source))?
                } else {
                    value
                };
                (field, value)
            }
            CanonSel::List { edge, frag: child } => {
                let value = node
                    .get(edge)
                    .cloned()
                    .ok_or_else(|| format!("`{scope}` JSON is missing `{edge}`"))?;
                if fields_edge_is_ref(fields, edge) {
                    (edge, value) // Node-reference list: the ids ride
                } else {
                    // Embedded value list: mask each element by the child selection.
                    let serde_json::Value::Array(items) = value else {
                        return Err(
                            format!("`{scope}` edge `{edge}` is not an array: {value}").into()
                        );
                    };
                    let masked_items = items
                        .iter()
                        .map(|item| mask_selection(schema, child, item, content))
                        .collect::<Result<Vec<_>, _>>()?;
                    (edge, serde_json::Value::Array(masked_items))
                }
            }
            CanonSel::Spread { edge, frag: child } => {
                let target = node
                    .get(edge)
                    .cloned()
                    .ok_or_else(|| format!("`{scope}` JSON is missing edge `{edge}`"))?;
                if fields_edge_is_ref(fields, edge) {
                    (edge, target) // a Node edge rides as its id
                } else {
                    (edge, mask_selection(schema, child, &target, content)?) // inline value, masked
                }
            }
            CanonSel::Enum { field, variants } => {
                let value = node
                    .get(field)
                    .cloned()
                    .ok_or_else(|| format!("`{scope}` JSON is missing `{field}`"))?;
                (field, mask_enum(schema, scope, fields, field, variants, &value, content)?)
            }
            CanonSel::Root { field, .. } => {
                return Err(format!("root `{field}` nested in a fragment").into())
            }
        };
        out.insert(field.clone(), value);
    }
    Ok(())
}

/// Mask a sum-typed field: match the wire tag (a bare string for a unit variant,
/// `{"Tag": {…}}` for a data variant) and mask the payload by that variant's selection
/// against that variant's fields.
fn mask_enum(
    schema: &Schema,
    scope: &str,
    fields: &[idyll_schema::FieldDef],
    field: &str,
    variants: &[crate::ir::CanonVariant],
    value: &serde_json::Value,
    content: Option<&ContentFn>,
) -> Result<serde_json::Value, BoxError> {
    let enum_name = match fields.iter().find(|f| f.name == field).map(|f| &f.ty) {
        Some(FieldType::Value { value }) => value.as_str(),
        other => return Err(format!("`{scope}.{field}` is {other:?}, not a sum type").into()),
    };
    let def = schema
        .enumeration_def(enum_name)
        .ok_or_else(|| format!("`{enum_name}` is not a schema enum"))?;
    match value {
        serde_json::Value::String(tag) => {
            def.variant(tag)
                .ok_or_else(|| format!("`{enum_name}` has no variant `{tag}`"))?;
            Ok(value.clone())
        }
        serde_json::Value::Object(map) if map.len() == 1 => {
            let (tag, payload) = map.iter().next().expect("len checked");
            let variant = def
                .variant(tag)
                .ok_or_else(|| format!("`{enum_name}` has no variant `{tag}`"))?;
            let selected = variants
                .iter()
                .find(|v| v.variant == *tag)
                .ok_or_else(|| format!("selection has no arm for `{enum_name}::{tag}`"))?;
            let mut masked = serde_json::Map::new();
            mask_fields(
                schema,
                &format!("{enum_name}::{tag}"),
                &variant.fields,
                &selected.selection,
                payload,
                content,
                &mut masked,
            )?;
            Ok(serde_json::json!({ tag.clone(): masked }))
        }
        other => Err(format!(
            "`{scope}.{field}` is not externally tagged `{enum_name}` JSON: {other}"
        )
        .into()),
    }
}

/// Seed one fetched Node and recurse its selection's edges. Returns the node's id (the
/// `Ref` wire form). Boxed + lifetime-bound because fragment trees recurse.
fn seed_node<'a, Src: Clone + Send + Sync + 'static>(
    schema: &'a Schema,
    fetchers: &'a HashMap<String, FetchThunk<Src>>,
    src: &'a Src,
    seed: &'a mut Seed,
    frag: &'a CanonOp,
    node: serde_json::Value,
    content: Option<&'a ContentFn>,
) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, BoxError>> + Send + 'a>> {
    Box::pin(async move {
        let id = node
            .get("id")
            .cloned()
            .ok_or_else(|| format!("`{}` node JSON has no `id`: {node}", frag.on))?;
        seed.push_raw(&frag.on, mask_selection(schema, frag, &node, content)?);
        let record = schema
            .record(&frag.on)
            .ok_or_else(|| format!("record `{}` is not in the schema", frag.on))?;
        follow_edges(schema, fetchers, src, seed, &frag.on, &record.fields, &frag.selection, &node, content)
            .await?;
        Ok(id)
    })
}

/// Follow one selection set's **fetching** edges within its field scope — records and
/// enum variants share the walk. `Ref` edges fetch and seed; a value edge rides inline
/// (already masked into the parent's commit) but its own `Ref` edges still fetch, so
/// the walk recurses into it; a sum-typed field recurses into the variant the JSON's
/// tag matched, against that variant's own fields.
#[allow(clippy::too_many_arguments)]
fn follow_edges<'a, Src: Clone + Send + Sync + 'static>(
    schema: &'a Schema,
    fetchers: &'a HashMap<String, FetchThunk<Src>>,
    src: &'a Src,
    seed: &'a mut Seed,
    scope: &'a str,
    fields: &'a [idyll_schema::FieldDef],
    selection: &'a [CanonSel],
    node: &'a serde_json::Value,
    content: Option<&'a ContentFn>,
) -> Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send + 'a>> {
    Box::pin(async move {
        for sel in selection {
            match sel {
                CanonSel::Leaf { .. } => {}
                CanonSel::Spread { edge, frag: child } => {
                    let target = node
                        .get(edge)
                        .cloned()
                        .ok_or_else(|| format!("`{scope}` JSON is missing edge `{edge}`"))?;
                    if fields_edge_is_ref(fields, edge) {
                        let fetcher = fetchers
                            .get(child.on.as_str())
                            .ok_or_else(|| format!("no fetcher registered for `{}`", child.on))?;
                        let fetched = fetcher(src.clone(), target).await?;
                        seed_node(schema, fetchers, src, &mut *seed, child, fetched, content)
                            .await?;
                    } else {
                        let record = schema
                            .record(&child.on)
                            .ok_or_else(|| format!("record `{}` is not in the schema", child.on))?;
                        follow_edges(
                            schema,
                            fetchers,
                            src,
                            &mut *seed,
                            &child.on,
                            &record.fields,
                            &child.selection,
                            &target,
                            content,
                        )
                        .await?;
                    }
                }
                CanonSel::List { edge, frag: child } => {
                    let targets = match node.get(edge) {
                        Some(serde_json::Value::Array(items)) => items.clone(),
                        other => {
                            return Err(format!(
                                "`{scope}` edge `{edge}` is not an array: {other:?}"
                            )
                            .into())
                        }
                    };
                    if fields_edge_is_ref(fields, edge) {
                        let fetcher = fetchers
                            .get(child.on.as_str())
                            .ok_or_else(|| format!("no fetcher registered for `{}`", child.on))?;
                        for target in targets {
                            let fetched = fetcher(src.clone(), target).await?;
                            seed_node(schema, fetchers, src, &mut *seed, child, fetched, content)
                                .await?;
                        }
                    } else {
                        let record = schema
                            .record(&child.on)
                            .ok_or_else(|| format!("record `{}` is not in the schema", child.on))?;
                        for item in &targets {
                            follow_edges(
                                schema,
                                fetchers,
                                src,
                                &mut *seed,
                                &child.on,
                                &record.fields,
                                &child.selection,
                                item,
                                content,
                            )
                            .await?;
                        }
                    }
                }
                CanonSel::Enum { field, variants } => {
                    let value = node
                        .get(field)
                        .ok_or_else(|| format!("`{scope}` JSON is missing `{field}`"))?;
                    // A unit variant (bare tag) has no fields, so nothing fetches.
                    let serde_json::Value::Object(map) = value else { continue };
                    let Some((tag, payload)) = map.iter().next() else { continue };
                    let enum_name = match fields.iter().find(|f| &f.name == field).map(|f| &f.ty)
                    {
                        Some(FieldType::Value { value }) => value.as_str(),
                        other => {
                            return Err(
                                format!("`{scope}.{field}` is {other:?}, not a sum type").into()
                            )
                        }
                    };
                    let def = schema
                        .enumeration_def(enum_name)
                        .ok_or_else(|| format!("`{enum_name}` is not a schema enum"))?;
                    let variant = def
                        .variant(tag)
                        .ok_or_else(|| format!("`{enum_name}` has no variant `{tag}`"))?;
                    let selected = variants
                        .iter()
                        .find(|v| v.variant == *tag)
                        .ok_or_else(|| format!("selection has no arm for `{enum_name}::{tag}`"))?;
                    follow_edges(
                        schema,
                        fetchers,
                        src,
                        &mut *seed,
                        enum_name,
                        &variant.fields,
                        &selected.selection,
                        payload,
                        content,
                    )
                    .await?;
                }
                CanonSel::Root { field, .. } => {
                    return Err(format!("root `{field}` nested in a fragment").into())
                }
            }
        }
        Ok(())
    })
}

/// Whether an edge **fetches** (a `Ref`, or a list of `Ref`s — the executor follows it
/// through a registered fetcher) as opposed to riding inline (an embedded value, or a
/// list of them — already present in the parent's JSON). Scope-relative: records and
/// enum variants both carry field lists.
fn fields_edge_is_ref(fields: &[idyll_schema::FieldDef], edge: &str) -> bool {
    fields
        .iter()
        .find(|f| f.name == edge)
        .is_some_and(|f| match &f.ty {
            FieldType::Ref { .. } => true,
            FieldType::List { of } => matches!(&**of, FieldType::Ref { .. }),
            _ => false,
        })
}
