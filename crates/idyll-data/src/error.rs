//! Typed boot-validation refusals. Every way the registry can refuse an artifact is
//! a variant here, carrying the schema objects involved — callers and tests match on
//! the kind; `Display` renders the guidance a human reads at boot. Runtime execution
//! failures are a different animal (an open set of app resolver errors) and stay
//! [`ExecError`](crate::ExecError)/`BoxError`.

use idyll_schema::{FieldDef, FieldType};

use crate::store::NoStoreRoot;

/// Why an operation artifact failed boot validation — against the schema
/// ([`validate`](crate::validate)), the mutation table
/// ([`validate_mutation`](crate::validate_mutation)), the resolver table
/// ([`validate_registered`](crate::validate_registered)), or the route contract
/// ([`validate_route_contract`](crate::validate_route_contract)).
#[derive(Debug, Clone, PartialEq)]
pub enum ValidateError {
    /// A top-level selection that isn't a root field.
    TopLevelNotRoot { op: String },
    /// The operation names a root the schema doesn't declare.
    UnknownRoot { root: String },
    /// The operation's list-ness disagrees with the root's declaration.
    RootArity { root: String, schema_list: bool },
    /// The operation selects on a different record than the root yields.
    RootOutput { root: String, yields: String, selects: String },
    /// A fragment selects on a record the schema doesn't declare.
    UnknownRecord { record: String },
    /// A selection reads a field its scope doesn't have.
    UnknownField { scope: String, field: String },
    /// A spread edge whose field isn't a reference to (or embedding of) the target.
    SpreadEdge { scope: String, edge: String, target: String, found: FieldType },
    /// A list edge whose field isn't a list of the target.
    ListEdge { scope: String, edge: String, target: String, found: FieldType },
    /// An enum selection on a field that isn't sum-typed.
    NotSum { scope: String, field: String },
    /// An enum selection whose field names a type that isn't a schema enum.
    UnknownEnum { scope: String, field: String, name: String },
    /// A variant selection the enum doesn't declare.
    UnknownVariant { enumeration: String, variant: String },
    /// A sum-typed field matched non-exhaustively.
    MissingVariant { scope: String, field: String, enumeration: String, variant: String },
    /// A root selection nested inside a fragment.
    NestedRoot { root: String, scope: String },
    /// The artifact names a mutation the schema doesn't declare.
    UnknownMutation { mutation: String },
    /// The artifact passes an argument the mutation doesn't take.
    UnknownArgument { mutation: String, argument: String },
    /// The artifact's response selects on a different record than the mutation yields.
    MutationOutput { mutation: String, yields: String, selects: String },
    /// A selected root has no resolver registered.
    MissingResolver { root: String },
    /// A followed `Ref` edge has no fetcher registered for its target.
    MissingFetcher { edge: String, target: String },
    /// The schema declares no `route` root.
    NoRouteRoot,
    /// The `route` root yields a list; a route resolves to one page.
    RouteRootIsList,
    /// The `route` root's arguments aren't exactly `request: Request`.
    RouteRootArgs { found: Vec<FieldDef> },
    /// The schema doesn't publish the framework `Request` value (or publishes a
    /// drifted copy).
    RequestNotPublished,
    /// The `route` root yields a record the schema doesn't define.
    UnknownPageRecord { yields: String },
    /// A page contract field (`id`, `title`) is missing.
    PageFieldMissing { page: String, field: String, expected: FieldType },
    /// A page contract field exists with the wrong type.
    PageFieldType { page: String, field: String, expected: FieldType, found: FieldType },
}

impl std::fmt::Display for ValidateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use ValidateError::*;
        match self {
            TopLevelNotRoot { op } => {
                write!(f, "operation `{op}`: top-level selections must be root fields")
            }
            UnknownRoot { root } => write!(f, "root `{root}` is not in the schema"),
            RootArity { root, schema_list } => {
                let (schema_side, op_side) =
                    if *schema_list { ("a list", "single") } else { ("single", "a list") };
                write!(
                    f,
                    "root `{root}` is {schema_side} in the schema but the operation treats \
                     it as {op_side}"
                )
            }
            RootOutput { root, yields, selects } => {
                write!(f, "root `{root}` yields `{yields}` but the operation selects on `{selects}`")
            }
            UnknownRecord { record } => write!(f, "record `{record}` is not in the schema"),
            UnknownField { scope, field } => write!(f, "`{scope}` has no field `{field}`"),
            SpreadEdge { scope, edge, target, found } => {
                write!(f, "edge `{scope}.{edge}` is {found:?}, not a `{target}` reference")
            }
            ListEdge { scope, edge, target, found } => {
                write!(f, "edge `{scope}.{edge}` is {found:?}, not a list of `{target}`")
            }
            NotSum { scope, field } => write!(f, "`{scope}.{field}` is not a sum-typed field"),
            UnknownEnum { scope, field, name } => {
                write!(f, "`{scope}.{field}` names `{name}`, which is not a schema enum")
            }
            UnknownVariant { enumeration, variant } => {
                write!(f, "`{enumeration}` has no variant `{variant}`")
            }
            MissingVariant { scope, field, enumeration, variant } => write!(
                f,
                "`{scope}.{field}` does not select `{enumeration}::{variant}` — a sum type \
                 is matched exhaustively"
            ),
            NestedRoot { root, scope } => write!(f, "root `{root}` nested inside `{scope}`"),
            UnknownMutation { mutation } => write!(f, "mutation `{mutation}` is not in the schema"),
            UnknownArgument { mutation, argument } => {
                write!(f, "mutation `{mutation}` has no argument `{argument}`")
            }
            MutationOutput { mutation, yields, selects } => write!(
                f,
                "mutation `{mutation}` yields `{yields}` but the artifact selects on `{selects}`"
            ),
            MissingResolver { root } => write!(f, "root `{root}` has no resolver registered"),
            MissingFetcher { edge, target } => write!(
                f,
                "edge `{edge}` fetches `{target}`, which has no fetcher registered \
                 (`Resolvers::fetch::<{target}>()`)"
            ),
            NoRouteRoot => write!(
                f,
                "the schema has no `route` root — routing is a root query: \
                 `#[root] async fn route(db, request: Request) -> Result<Option<Page>, _>`"
            ),
            RouteRootIsList => write!(f, "the `route` root must yield one page, not a list"),
            RouteRootArgs { found } => {
                write!(f, "the `route` root must take exactly `request: Request` (found {found:?})")
            }
            RequestNotPublished => write!(
                f,
                "the schema must publish the framework `Request` value \
                 (`.value::<idyll_data::Request>()`)"
            ),
            UnknownPageRecord { yields } => {
                write!(f, "`route` yields `{yields}`, which the schema does not define")
            }
            PageFieldMissing { page, field, expected } => write!(
                f,
                "the page node `{page}` is missing the contract field `{field}: {expected:?}`"
            ),
            PageFieldType { page, field, expected, found } => write!(
                f,
                "the page node `{page}` must have `{field}` of type {expected:?} (found {found:?})"
            ),
        }
    }
}

impl std::error::Error for ValidateError {}

/// Why the cache refuses a commit at ingestion. Rows are field-merged by key, so a
/// commit must be an object carrying its identity — refused here, at the boundary,
/// never discovered by a read. (Reads assert the *schema* contract — field types —
/// which the client can't check shape-first without shipping the schema; structural
/// well-formedness it can, so it does.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    /// The commit's JSON is not an object — field-level merge has nothing to merge.
    NotAnObject { type_tag: String },
    /// The commit carries no `id` — a normalized record has identity by contract.
    NoId { type_tag: String },
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitError::NotAnObject { type_tag } => {
                write!(f, "`{type_tag}` commit is not a JSON object")
            }
            CommitError::NoId { type_tag } => write!(f, "`{type_tag}` commit has no `id`"),
        }
    }
}

impl std::error::Error for CommitError {}

/// Why a store refresh (a mutation's seed envelope, a navigation response) failed to
/// absorb — faulting the store-root live to its boundary with the kind intact.
#[derive(Debug)]
pub enum AbsorbError {
    /// The wire bytes aren't the `Preloaded` envelope the contract promises.
    Decode(serde_json::Error),
    /// The seed decoded but a commit refused to replay into the cache.
    Replay(CommitError),
}

impl std::fmt::Display for AbsorbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AbsorbError::Decode(err) => write!(f, "refresh seed does not decode: {err}"),
            AbsorbError::Replay(err) => write!(f, "refresh seed refused to replay: {err}"),
        }
    }
}

impl std::error::Error for AbsorbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AbsorbError::Decode(err) => Some(err),
            AbsorbError::Replay(err) => Some(err),
        }
    }
}

/// Why a live could not stand its store up.
#[derive(Debug)]
pub enum StoreError {
    /// This live read the store with no store-root above it and no warrant to own one.
    NoRoot(NoStoreRoot),
    /// The seed decoded but a commit refused to enter the cache. The same refusal an
    /// absorb reports ([`AbsorbError::Replay`]) — a seed is a seed whether it arrives
    /// with the page or after it, so both doors answer alike.
    Seed(CommitError),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::NoRoot(err) => write!(f, "{err}"),
            StoreError::Seed(err) => write!(f, "page seed refused to replay: {err}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::NoRoot(err) => Some(err),
            StoreError::Seed(err) => Some(err),
        }
    }
}

impl From<NoStoreRoot> for StoreError {
    fn from(err: NoStoreRoot) -> StoreError {
        StoreError::NoRoot(err)
    }
}

impl From<CommitError> for StoreError {
    fn from(err: CommitError) -> StoreError {
        StoreError::Seed(err)
    }
}

/// Why an executed route query's seed couldn't yield the page `title` the host
/// consumes. Boot validated the schema, so any of these means the resolver broke
/// the contract at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageTitleError {
    /// No record of the page type has the request path as its id.
    MissingRecord { page: String, path: String },
    /// The page record has no `title` field.
    MissingTitle,
    /// The page record's `title` is not a string.
    TitleNotString,
}

impl std::fmt::Display for PageTitleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageTitleError::MissingRecord { page, path } => write!(
                f,
                "the route query executed but its seed has no `{page}` record with id \
                 `{path}` — the page's id must be the request path"
            ),
            PageTitleError::MissingTitle => write!(f, "page record is missing contract field `title`"),
            PageTitleError::TitleNotString => write!(f, "page `title` is not a string"),
        }
    }
}

impl std::error::Error for PageTitleError {}
