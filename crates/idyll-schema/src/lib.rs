//! # idyll-schema — the published contract artifact
//!
//! This crate exists so the schema types have exactly two consumers with no cycle:
//! `idyll-data` (the server defines and emits the schema, the executor types against
//! it) and `idyll-macros` (the client macros *read* `schema.json` at expansion time —
//! and idyll-data depends on idyll-macros, so the macros cannot depend back on it).
//!
//! The **published schema** — the server's contract, emitted as `schema.json`.
//!
//! The server *defines* the schema (nodes, values, roots, mutations) and publishes it as
//! one deterministic JSON artifact; the client macros (`fragment!`/`query!`/`mutation!`)
//! read that artifact at expansion time to validate selections and generate projections.
//! Queries and mutations execute **on the server, never in wasm** — the wasm containers
//! hold UI and its declared data needs, nothing else. (See `schema.md` for the whole
//! architecture; the dev server orchestrates emission → app build → registry load.)
//!
//! Construction is an **explicit builder** — no global registry, nothing auto-collected:
//!
//! ```ignore
//! fn schema() -> Schema {
//!     Schema::new()
//!         .node::<Todo>()          // descriptor emitted by #[node]
//!         .root(todos_schema())    // descriptor emitted by #[root]
//!         .mutation(
//!             MutationDef::new("add-todo")
//!                 .arg("text", FieldType::scalar("String"))
//!                 .returns("Todo"),
//!         )
//! }
//! ```
//!
//! Like the operation registry (`ir.rs`), the artifact is self-verifying: no maps, the
//! top-level collections are name-sorted, and the pretty JSON is byte-stable — so the
//! content hash is reproducible and the checked-in file is human-reviewable.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// An operation's **persisted identity**: the first 128 bits of the sha256 over its
/// canonical artifact bytes, as two words — parse-don't-validate, every value of the
/// type is a well-formed identity (a hex *string* admits garbage; this admits nothing
/// but bits). `Copy`, `Eq`, `Hash`; crosses WIT as a two-`u64` record.
///
/// The registry **filename** stays the full sha256 hex (self-verifying with plain
/// `sha256sum`); this is the runtime/wire form. The hex forms relate by prefix:
/// `format!("{op_hash}")` is the filename's first 32 characters. Serde uses the hex
/// string (never numbers — a `u64` through JavaScript's `JSON.parse` silently loses
/// precision above 2^53).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OpHash {
    msb: u64,
    lsb: u64,
}

impl OpHash {
    /// The identity of an artifact's exact bytes.
    pub fn of_bytes(contents: &[u8]) -> Self {
        let digest = Sha256::digest(contents);
        OpHash {
            msb: u64::from_be_bytes(digest[0..8].try_into().expect("8 bytes")),
            lsb: u64::from_be_bytes(digest[8..16].try_into().expect("8 bytes")),
        }
    }

    /// Reassemble from the two wire words (the WIT record's fields).
    pub fn from_words(msb: u64, lsb: u64) -> Self {
        OpHash { msb, lsb }
    }

    pub fn msb(&self) -> u64 {
        self.msb
    }

    pub fn lsb(&self) -> u64 {
        self.lsb
    }
}

impl std::fmt::Display for OpHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}{:016x}", self.msb, self.lsb)
    }
}

impl std::str::FromStr for OpHash {
    type Err = String;

    /// The HTTP-boundary parse: exactly 32 lowercase hex characters.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 32 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(format!("`{s}` is not a 32-hex operation hash"));
        }
        Ok(OpHash {
            msb: u64::from_str_radix(&s[0..16], 16).expect("validated hex"),
            lsb: u64::from_str_radix(&s[16..32], 16).expect("validated hex"),
        })
    }
}

impl Serialize for OpHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for OpHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A field's type in the published schema. Scalars carry the Rust primitive name
/// (`"String"`, `"u64"`, `"bool"`, …) so client codegen maps back 1:1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldType {
    /// A scalar leaf, by Rust primitive name.
    Scalar { name: String },
    /// A reference to a Node (`Ref<Post>`): follows as an edge.
    Ref { node: String },
    /// An embedded value record (`#[value]` type): inline in the parent, no identity.
    Value { value: String },
    /// A list of the inner type (`Vec<…>`).
    List { of: Box<FieldType> },
    /// An optional inner type (`Option<…>`).
    Optional { of: Box<FieldType> },
    /// **Content**: the resolver supplies source text, the data layer's registered
    /// content mapping executes with the operation, and View IR rides the wire — the
    /// strict lattice's one live→view door. One mapping per app until a second kind of
    /// content exists.
    Content,
}

impl FieldType {
    pub fn scalar(name: impl Into<String>) -> Self {
        FieldType::Scalar { name: name.into() }
    }

    pub fn reference(node: impl Into<String>) -> Self {
        FieldType::Ref { node: node.into() }
    }

    pub fn value(value: impl Into<String>) -> Self {
        FieldType::Value { value: value.into() }
    }

    pub fn list(of: FieldType) -> Self {
        FieldType::List { of: Box::new(of) }
    }

    pub fn optional(of: FieldType) -> Self {
        FieldType::Optional { of: Box::new(of) }
    }

    pub fn content() -> Self {
        FieldType::Content
    }
}

/// A named, typed field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    pub ty: FieldType,
}

/// A record type: a Node (has identity, normalized in the cache) or a value (embedded).
/// Fields keep declaration order — reordering them is a real schema change, and the
/// declaration is the readable source of truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordDef {
    pub name: String,
    pub fields: Vec<FieldDef>,
}

/// A root entry point: `todos() -> [Todo]`, `post(id: u64) -> Post`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootDef {
    pub name: String,
    pub args: Vec<FieldDef>,
    /// The Node type the root yields.
    pub output: String,
    /// `true` for a list root (`-> Vec<Node>`).
    pub list: bool,
}

/// A mutation entry point: `add-todo(text: String) -> Todo`. The response is a Node the
/// client selects fields from (the operation's recorded selection masks the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationDef {
    pub name: String,
    pub args: Vec<FieldDef>,
    /// The Node type the mutation yields.
    pub output: String,
}

impl MutationDef {
    pub fn new(name: impl Into<String>) -> Self {
        MutationDef {
            name: name.into(),
            args: Vec::new(),
            output: String::new(),
        }
    }

    pub fn arg(mut self, name: impl Into<String>, ty: FieldType) -> Self {
        self.args.push(FieldDef { name: name.into(), ty });
        self
    }

    pub fn returns(mut self, output: impl Into<String>) -> Self {
        self.output = output.into();
        self
    }
}

/// Emitted per `#[node]`/`#[value]`: the record's own schema descriptor.
pub trait DescribeRecord {
    fn describe() -> RecordDef;
}

/// The whole published schema. Build with the explicit fluent methods; serialize with
/// [`to_json`](Self::to_json).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Schema {
    pub nodes: Vec<RecordDef>,
    pub values: Vec<RecordDef>,
    /// Closed keyword sets (`#[value] enum` — unit variants only): a **leaf** on the
    /// wire (the variant name as a string), typed on both sides. This is how a parsed
    /// decision crosses the schema — the route kind, a status — instead of a stringly
    /// field re-validated wherever it lands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enums: Vec<EnumDef>,
    pub roots: Vec<RootDef>,
    pub mutations: Vec<MutationDef>,
}

/// A closed **sum type**: named variants, each carrying its own fields (empty = a
/// unit variant). The domain's alternatives — a route's kinds, a status — modeled as
/// alternatives, so each variant seeds exactly its own data and the client matches
/// exhaustively. Wire form is serde's external tagging: a unit variant is its name as
/// a string, a data variant `{"Name": { …fields }}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<VariantDef>,
}

/// One variant of an [`EnumDef`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldDef>,
}

impl EnumDef {
    pub fn variant(&self, name: &str) -> Option<&VariantDef> {
        self.variants.iter().find(|variant| variant.name == name)
    }
}

/// Implemented by `#[value]` enums — the descriptor enum registration publishes.
pub trait DescribeEnum {
    fn describe() -> EnumDef;
}

/// One schema type's registration: insert your own def **into the right section**
/// (nodes / values / enums — the type knows which it is), skipping if already present
/// (the cycle guard), then recurse into every named field type. Implemented by
/// `#[node]` and `#[value]` expansion — never by hand — so the schema is the
/// **reachability closure of the entry points**: name the roots and mutations,
/// GraphQL-style, and everything they can reach publishes itself. A type you forgot
/// to list is not a representable mistake.
pub trait SchemaType {
    fn register(schema: &mut Schema);
}

/// A root entry point plus the registration of everything it reaches — what `#[root]`
/// generates (`<fn>_schema()`) and [`Schema::root`] consumes.
pub struct RootEntry {
    pub def: RootDef,
    pub register: fn(&mut Schema),
}

/// A mutation entry point plus its reachability registration — what
/// `#[mutation_handler]` generates (`<fn>_schema()`) and [`Schema::mutation`] consumes.
pub struct MutationEntry {
    pub def: MutationDef,
    pub register: fn(&mut Schema),
}

impl Schema {
    pub fn new() -> Self {
        Schema::default()
    }

    /// The enum def by name, if this schema publishes one.
    pub fn enumeration_def(&self, name: &str) -> Option<&EnumDef> {
        self.enums.iter().find(|def| def.name == name)
    }

    /// Add a root entry point and everything reachable from it. The descriptor comes
    /// from `#[root]` (`<fn>_schema()`).
    pub fn root(mut self, root: RootEntry) -> Self {
        self.roots.push(root.def);
        (root.register)(&mut self);
        self
    }

    /// Add a mutation entry point and everything reachable from it.
    pub fn mutation(mut self, mutation: MutationEntry) -> Self {
        self.mutations.push(mutation.def);
        (mutation.register)(&mut self);
        self
    }

    /// Look up a record (node or value) by schema name.
    pub fn record(&self, name: &str) -> Option<&RecordDef> {
        self.nodes
            .iter()
            .chain(self.values.iter())
            .find(|record| record.name == name)
    }

    pub fn root_def(&self, name: &str) -> Option<&RootDef> {
        self.roots.iter().find(|root| root.name == name)
    }

    pub fn mutation_def(&self, name: &str) -> Option<&MutationDef> {
        self.mutations.iter().find(|mutation| mutation.name == name)
    }

    /// The canonical JSON artifact — `schema.json`'s exact contents. Top-level
    /// collections sort by name (builder call order and reachability order are not
    /// meaningful); fields keep declaration order. Pretty, map-free, byte-stable →
    /// reviewable and hashable. An inconsistent schema (a node reached as an inlined
    /// value) is the `Err` — boot refuses it, never a trap.
    pub fn to_json(&self) -> Result<String, SchemaError> {
        self.check_edges()?;
        let mut canonical = self.clone();
        canonical.nodes.sort_by(|a, b| a.name.cmp(&b.name));
        canonical.values.sort_by(|a, b| a.name.cmp(&b.name));
        canonical.roots.sort_by(|a, b| a.name.cmp(&b.name));
        canonical.mutations.sort_by(|a, b| a.name.cmp(&b.name));
        canonical.enums.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(serde_json::to_string_pretty(&canonical).expect("schema serializes to JSON"))
    }

    /// Every edge agrees with what its target was declared to be.
    ///
    /// A `#[node]` derive sees one item, so it cannot know whether a named field type is a
    /// node or a value — it guesses `value`, and a guess that lands on a node produces a
    /// record that is addressable (`Frag<F>` exists, because the type is in `nodes`) but
    /// never normalized (the edge is inlined, so nothing commits it). The read then waits
    /// for a record that cannot arrive: no error, no trap, an unpainted region.
    ///
    /// Assembly is the only place that sees both lists, so it is where the two are made to
    /// agree. Rejected rather than rewritten: a `Ref` edge is *fetched*, so silently
    /// promoting one would demand a fetcher the app never wrote.
    fn check_edges(&self) -> Result<(), SchemaError> {
        // Binds the inlined node's name when the edge is one — the proof travels with
        // the match instead of being re-derived from a boolean.
        let inlined_node = |ty: &FieldType| match ty {
            FieldType::Value { value } if self.nodes.iter().any(|node| node.name == *value) => {
                Some(value.clone())
            }
            _ => None,
        };
        for record in self.nodes.iter().chain(self.values.iter()) {
            for field in &record.fields {
                let inner = match &field.ty {
                    FieldType::List { of } | FieldType::Optional { of } => of,
                    ty => ty,
                };
                if let Some(node) = inlined_node(inner) {
                    return Err(SchemaError::InlinedNode {
                        record: record.name.clone(),
                        field: field.name.clone(),
                        node,
                    });
                }
            }
        }
        Ok(())
    }

    /// Parse a published `schema.json` (the client macros' input).
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Lowercase-hex SHA-256 of [`to_json`](Self::to_json) — the schema's identity, used
    /// to detect drift between the checked-in artifact and the server's definition.
    pub fn content_hash(&self) -> Result<String, SchemaError> {
        let mut hasher = Sha256::new();
        hasher.update(self.to_json()?.as_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        Ok(hex)
    }
}

/// Why a schema refuses to serialize as the canonical artifact. Boot matches on the
/// kind; [`Display`](std::fmt::Display) renders the guidance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// `record.field` reaches a `#[node]` type as an inlined value: the node is
    /// addressable (it is in `nodes`) but the edge never normalizes it, so a read
    /// of it would wait forever.
    InlinedNode { record: String, field: String, node: String },
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaError::InlinedNode { record, field, node } => write!(
                f,
                "`{record}.{field}` reaches node `{node}` as an inlined value, so `{node}` is \
                 addressable but never normalized and a read of it would hang. Declare `{node}` \
                 as `#[value]` if it is part of `{record}`, or make the edge a reference if it \
                 has its own identity."
            ),
        }
    }
}

impl std::error::Error for SchemaError {}

/// The core-module export name marking live entry `i`'s poll wrapper — the point where
/// always-loaded code hands the live's root future to its own code.
///
/// Live identity between the app build and the splitter is these **declared exports**,
/// exact-matched in the export section: `guest!` exports entry `i`'s wrappers under these
/// names (guest-table order — page, head, then the live), and the code splitter seeds
/// live `i`'s reachability from them and bounds the always-loaded walk at them.
pub fn live_root_export(entry: usize) -> String {
    format!("__idyll_live_root_{entry}")
}

/// The export name marking live entry `i`'s future constructor — the mount path's
/// hand-off into the live. See [`live_root_export`] for the contract.
pub fn live_make_export(entry: usize) -> String {
    format!("__idyll_live_make_{entry}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bare entries (the macros' entries also register their reachable types; these
    // tests exercise the schema artifact itself, not reachability).
    fn root_entry(def: RootDef) -> RootEntry {
        RootEntry { def, register: |_| {} }
    }

    fn mutation_entry(def: MutationDef) -> MutationEntry {
        MutationEntry { def, register: |_| {} }
    }

    /// `CodeGroup` declared a node but reached through an inlined edge: `Frag<CodeGroupFrag>`
    /// exists, nothing ever commits a `CodeGroup`, and the island hangs with no error and an
    /// unpainted region.
    #[test]
    fn an_edge_that_inlines_a_node_is_rejected() {
        let inlined = |ty: FieldType| Schema {
            nodes: vec![
                RecordDef {
                    name: "Page".into(),
                    fields: vec![FieldDef { name: "groups".into(), ty }],
                },
                RecordDef { name: "CodeGroup".into(), fields: Vec::new() },
            ],
            ..Schema::new()
        };

        let direct = inlined(FieldType::value("CodeGroup"));
        let error = direct.check_edges().expect_err("a node reached inline is rejected");
        assert_eq!(
            error,
            SchemaError::InlinedNode {
                record: "Page".into(),
                field: "groups".into(),
                node: "CodeGroup".into(),
            }
        );

        // A list hides the node the same way, and so does `Optional`.
        assert!(inlined(FieldType::list(FieldType::value("CodeGroup"))).check_edges().is_err());
        assert!(inlined(FieldType::optional(FieldType::value("CodeGroup"))).check_edges().is_err());

        // A value reached inline is the ordinary case, and a node reached by reference is
        // what a node is for. Neither is the bug.
        assert!(inlined(FieldType::value("CodeTab")).check_edges().is_ok());
        assert!(inlined(FieldType::reference("CodeGroup")).check_edges().is_ok());
    }

    fn sample() -> Schema {
        Schema::new()
            .root(root_entry(RootDef {
                name: "todos".into(),
                args: vec![],
                output: "Todo".into(),
                list: true,
            }))
            .mutation(mutation_entry(
                MutationDef::new("add-todo")
                    .arg("text", FieldType::scalar("String"))
                    .returns("Todo"),
            ))
    }

    #[test]
    fn op_hash_is_the_digest_prefix_and_round_trips_all_its_forms() {
        use sha2::{Digest, Sha256};
        let contents = b"canonical artifact bytes";
        let hash = OpHash::of_bytes(contents);

        // Display is the full digest's first 32 hex chars — filename-prefix compatible.
        let full: String = Sha256::digest(contents).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hash.to_string(), full[..32]);

        // FromStr (the HTTP boundary), words (the WIT wire), serde (hex string, not
        // numbers) all round-trip.
        assert_eq!(hash.to_string().parse::<OpHash>().unwrap(), hash);
        assert_eq!(OpHash::from_words(hash.msb(), hash.lsb()), hash);
        let json = serde_json::to_string(&hash).unwrap();
        assert_eq!(json, format!("\"{hash}\""));
        assert_eq!(serde_json::from_str::<OpHash>(&json).unwrap(), hash);

        // Garbage is unrepresentable: the parse rejects it at the boundary.
        assert!("not-a-hash".parse::<OpHash>().is_err());
        assert!(full.parse::<OpHash>().is_err(), "full 64-hex is not the 32-hex wire form");
    }

    #[test]
    fn schema_json_is_byte_stable_and_order_insensitive() {
        let a = Schema::new()
            .mutation(mutation_entry(MutationDef::new("b").returns("T")))
            .mutation(mutation_entry(MutationDef::new("a").returns("T")));
        let b = Schema::new()
            .mutation(mutation_entry(MutationDef::new("a").returns("T")))
            .mutation(mutation_entry(MutationDef::new("b").returns("T")));
        assert_eq!(a.to_json(), b.to_json());
        assert_eq!(a.content_hash(), b.content_hash());

        // Enums register in reachability order, which is not meaningful either —
        // two registration orders, one artifact.
        let e1 = EnumDef { name: "A".into(), variants: vec![] };
        let e2 = EnumDef { name: "B".into(), variants: vec![] };
        let mut x = Schema::new();
        x.enums.push(e1.clone());
        x.enums.push(e2.clone());
        let mut y = Schema::new();
        y.enums.push(e2);
        y.enums.push(e1);
        assert_eq!(x.to_json(), y.to_json());
    }

    #[test]
    fn schema_round_trips_through_its_artifact() {
        let schema = sample();
        let parsed = Schema::from_json(&schema.to_json().unwrap()).unwrap();
        assert_eq!(parsed.root_def("todos").unwrap().output, "Todo");
        assert_eq!(parsed.mutation_def("add-todo").unwrap().args[0].name, "text");
        // Round-tripping preserves the hash — the artifact IS the schema.
        assert_eq!(parsed.content_hash(), schema.content_hash());
    }

    #[test]
    fn changing_the_schema_changes_the_hash() {
        let schema = sample();
        let widened =
            sample().mutation(mutation_entry(MutationDef::new("remove-todo").returns("Todo")));
        assert_ne!(schema.content_hash(), widened.content_hash());
    }
}
