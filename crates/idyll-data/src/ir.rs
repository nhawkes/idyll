//! The normalized operation **IR** — the custom, GraphQL-*shaped* intermediate form that
//! is the waist of the hourglass: `fragment!` / `query!` (and, later, a DSL) all lower
//! *into* it, and the persisted registry + server dispatch live *below* it.
//!
//! Two representations:
//!
//! - [`FragmentDef`] / [`Sel`] are `'static`-const so a `fragment!` can emit its selection
//!   as [`Fragment::DEF`](crate::Fragment::DEF) with no allocation — sub-fragments are
//!   inlined by pointer (Relay-classic: fragments inline into the operation).
//! - [`CanonOp`] / [`CanonSel`] are the owned **canonical** form used for
//!   content-addressing: selections are name-sorted and the fragment *name* is dropped, so
//!   cosmetic edits (reordering fields, renaming a fragment) do not churn the hash. The
//!   canonical JSON is both the reviewable `<hash>.query` file contents *and* the bytes the
//!   hash is taken over, so `filename == sha256(contents)` — a self-verifying registry.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One entry in a selection set. `'static`-const so it can live in a [`Fragment::DEF`].
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Sel {
    /// A scalar leaf: read this field off the record.
    Leaf { field: &'static str },
    /// Follow an edge (`Ref<_>`) and spread `frag` on the target.
    Spread {
        edge: &'static str,
        frag: &'static FragmentDef,
    },
    /// Follow an edge *list* (`Vec<Ref<_>>`) and spread `frag` on each element.
    List {
        edge: &'static str,
        frag: &'static FragmentDef,
    },
    /// A **query root field**: `post(id: $id): PostCard`. `args` are the resolver's
    /// argument *parameter* names (not the variable names — those are cosmetic and stay
    /// out of the hash), so `post(id:…)` and `post(slug:…)` are distinct operations.
    Root {
        field: &'static str,
        args: &'static [&'static str],
        /// `true` for a list root (`posts: [PostCard]` → `Vec<Node>`), `false` for a
        /// single root (`post(id:…): PostCard` → `Node`). Part of the operation's
        /// identity — a list vs single root is a different execution contract.
        list: bool,
        frag: &'static FragmentDef,
    },
    /// A **sum-typed field** selected per variant, exhaustively:
    /// `route { Todos { todos: [TodoCheck] }, Prose {} }`. Each variant's selection
    /// reads that variant's own fields; the projection is a generated Rust enum the
    /// component matches.
    Enum {
        field: &'static str,
        variants: &'static [VariantSel],
    },
}

/// One variant's selection inside a [`Sel::Enum`].
#[derive(Debug, Clone, Copy, Serialize)]
pub struct VariantSel {
    pub name: &'static str,
    pub selection: &'static [Sel],
}

/// A fragment's normalized selection, with sub-fragments inlined by reference.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct FragmentDef {
    /// The fragment's Rust name — for diagnostics only; excluded from the canonical hash.
    pub name: &'static str,
    /// The schema record type this fragment reads (`"Post"`).
    pub on: &'static str,
    /// The selection set, in source order (canonicalized to name order for hashing).
    pub selection: &'static [Sel],
}

// ── Canonical (owned, sorted, name-free) form for content-addressing ────────────────

/// Owned canonical selection entry. `#[serde(tag = "kind")]` order and struct field order
/// are declaration-deterministic; the only non-determinism (source order of a selection
/// set) is removed by sorting in [`CanonOp::from_def`]. `Deserialize` because a persisted
/// registry artifact is also the **input** to the server's op executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CanonSel {
    Leaf { field: String },
    Spread { edge: String, frag: Box<CanonOp> },
    List { edge: String, frag: Box<CanonOp> },
    Root {
        field: String,
        args: Vec<String>,
        list: bool,
        frag: Box<CanonOp>,
    },
    Enum {
        field: String,
        variants: Vec<CanonVariant>,
    },
}

/// One variant's canonical selection inside a [`CanonSel::Enum`] — variants ride
/// name-sorted, selections canonicalized like any other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CanonVariant {
    pub(crate) variant: String,
    pub(crate) selection: Vec<CanonSel>,
}

impl CanonSel {
    /// The name this entry sorts by (field or edge). Kind breaks ties.
    fn sort_key(&self) -> (&str, u8) {
        match self {
            CanonSel::Leaf { field } => (field.as_str(), 0),
            CanonSel::Spread { edge, .. } => (edge.as_str(), 1),
            CanonSel::List { edge, .. } => (edge.as_str(), 2),
            CanonSel::Root { field, .. } => (field.as_str(), 3),
            CanonSel::Enum { field, .. } => (field.as_str(), 4),
        }
    }
}

/// The canonical, content-addressable form of a fragment/operation. The fragment *name*
/// is deliberately absent — identity is the *shape* (the record it is on + its sorted
/// selection), so renaming a fragment or reordering fields keeps the same hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonOp {
    pub(crate) on: String,
    pub(crate) selection: Vec<CanonSel>,
}

impl CanonOp {
    /// Parse a persisted registry artifact (the exact `<hash>.query` contents). The
    /// executor's input; [`Self::content_hash`] re-verifies the identity after parsing.
    pub fn from_canonical_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Lower a `'static` [`FragmentDef`] tree to its canonical owned form: recurse into
    /// spreads and **sort** each selection set by name so source order is irrelevant.
    pub fn from_def(def: &FragmentDef) -> Self {
        CanonOp {
            on: def.on.to_string(),
            selection: canon_selection(def.selection),
        }
    }

    /// The canonical JSON that is *both* the `<hash>.query` file contents and the bytes
    /// hashed. Pretty-printed (serde_json's pretty writer is deterministic) so the
    /// committed registry is human-reviewable, with no maps anywhere, so byte-stable.
    pub fn to_canonical_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("canonical operation serializes to JSON")
    }

    /// The operation's runtime/wire identity: the digest's first 128 bits as an
    /// [`OpHash`](idyll_schema::OpHash) (`Display` = the filename's first 32 hex chars).
    pub fn op_hash(&self) -> idyll_schema::OpHash {
        idyll_schema::OpHash::of_bytes(self.to_canonical_json().as_bytes())
    }

    /// The lowercase-hex SHA-256 of the canonical JSON — the operation's persisted
    /// identity. Because it is taken over the exact file bytes, `filename == hash`.
    pub fn content_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.to_canonical_json().as_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

/// Lower one selection set to canonical form: each entry converted, then **sorted**
/// by name so source order is irrelevant to identity.
fn canon_selection(selection: &[Sel]) -> Vec<CanonSel> {
    let mut selection: Vec<CanonSel> = selection
        .iter()
        .map(|sel| match *sel {
            Sel::Leaf { field } => CanonSel::Leaf { field: field.to_string() },
            Sel::Spread { edge, frag } => CanonSel::Spread {
                edge: edge.to_string(),
                frag: Box::new(CanonOp::from_def(frag)),
            },
            Sel::List { edge, frag } => CanonSel::List {
                edge: edge.to_string(),
                frag: Box::new(CanonOp::from_def(frag)),
            },
            Sel::Root { field, args, list, frag } => {
                let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
                args.sort(); // arg order is not semantically meaningful
                CanonSel::Root {
                    field: field.to_string(),
                    args,
                    list,
                    frag: Box::new(CanonOp::from_def(frag)),
                }
            }
            Sel::Enum { field, variants } => {
                let mut variants: Vec<CanonVariant> = variants
                    .iter()
                    .map(|variant| CanonVariant {
                        variant: variant.name.to_string(),
                        selection: canon_selection(variant.selection),
                    })
                    .collect();
                variants.sort_by(|a, b| a.variant.cmp(&b.variant));
                CanonSel::Enum { field: field.to_string(), variants }
            }
        })
        .collect();
    selection.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    selection
}

/// The canonical, content-addressable form of a persisted **mutation** operation: the
/// schema mutation it invokes, the argument names it passes (sorted — cosmetic order
/// stays out of the hash), and the response **selection** on the output record (what
/// the server masks the handler's node down to). Same discipline as [`CanonOp`]:
/// the canonical JSON is the artifact's bytes, so `filename == sha256(contents)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonMutation {
    pub(crate) mutation: String,
    pub(crate) args: Vec<String>,
    pub(crate) response: CanonOp,
}

impl CanonMutation {
    /// Canonicalize from the macro-emitted parts.
    pub fn new(mutation: &str, args: &[&str], response: &FragmentDef) -> Self {
        let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        args.sort();
        CanonMutation {
            mutation: mutation.to_string(),
            args,
            response: CanonOp::from_def(response),
        }
    }

    /// Parse a persisted `<hash>.mutation` artifact (the executor's input).
    pub fn from_canonical_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn mutation_name(&self) -> &str {
        &self.mutation
    }

    pub fn to_canonical_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("canonical mutation serializes to JSON")
    }

    /// The operation's runtime/wire identity.
    pub fn op_hash(&self) -> idyll_schema::OpHash {
        idyll_schema::OpHash::of_bytes(self.to_canonical_json().as_bytes())
    }

    pub fn content_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.to_canonical_json().as_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

/// A persisted mutation ready to write into the registry directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationFile {
    /// `<sha256>.mutation`.
    pub filename: String,
    /// Canonical JSON; `sha256(contents) == <sha256>` in [`Self::filename`].
    pub contents: String,
}

impl MutationFile {
    pub fn from_parts(mutation: &str, args: &[&str], response: &FragmentDef) -> Self {
        let canon = CanonMutation::new(mutation, args, response);
        let contents = canon.to_canonical_json();
        let filename = format!("{}.mutation", canon.content_hash());
        MutationFile { filename, contents }
    }
}

/// A persisted operation ready to write into the registry directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryFile {
    /// `<sha256>.query`.
    pub filename: String,
    /// Canonical JSON; `sha256(contents) == <sha256>` in [`Self::filename`].
    pub contents: String,
}

impl QueryFile {
    /// Build the `<hash>.query` file for a `'static` operation descriptor.
    pub fn from_def(def: &FragmentDef) -> Self {
        let canon = CanonOp::from_def(def);
        let contents = canon.to_canonical_json();
        let filename = format!("{}.query", canon.content_hash());
        QueryFile { filename, contents }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `Avatar on User { name, avatar_url }`
    static AVATAR: FragmentDef = FragmentDef {
        name: "Avatar",
        on: "User",
        selection: &[
            Sel::Leaf { field: "name" },
            Sel::Leaf {
                field: "avatar_url",
            },
        ],
    };

    // `PostCard on Post { title, author: Avatar }`
    static POST_CARD: FragmentDef = FragmentDef {
        name: "PostCard",
        on: "Post",
        selection: &[
            Sel::Leaf { field: "title" },
            Sel::Spread {
                edge: "author",
                frag: &AVATAR,
            },
        ],
    };

    #[test]
    fn canonical_json_is_byte_stable_and_self_verifying() {
        let file = QueryFile::from_def(&POST_CARD);
        // The filename is exactly `<sha256 of contents>.query` — the registry verifies
        // itself with plain `sha256sum`, no idyll knowledge required.
        let mut hasher = Sha256::new();
        hasher.update(file.contents.as_bytes());
        let digest = hasher.finalize();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(file.filename, format!("{hex}.query"));

        // Rebuilding the same descriptor yields byte-identical output (no maps, pretty
        // writer is deterministic) — two machines/builds agree on the hash.
        assert_eq!(QueryFile::from_def(&POST_CARD), file);
    }

    #[test]
    fn reordering_a_selection_does_not_change_the_hash() {
        // Same fields, opposite source order → canonicalization sorts → same hash.
        static AVATAR_REORDERED: FragmentDef = FragmentDef {
            name: "Avatar",
            on: "User",
            selection: &[
                Sel::Leaf {
                    field: "avatar_url",
                },
                Sel::Leaf { field: "name" },
            ],
        };
        assert_eq!(
            CanonOp::from_def(&AVATAR).content_hash(),
            CanonOp::from_def(&AVATAR_REORDERED).content_hash(),
        );
    }

    #[test]
    fn renaming_a_fragment_does_not_change_the_hash() {
        // Identity is the shape (type + sorted selection), not the Rust name.
        static AVATAR_RENAMED: FragmentDef = FragmentDef {
            name: "ProfilePic",
            on: "User",
            selection: &[
                Sel::Leaf { field: "name" },
                Sel::Leaf {
                    field: "avatar_url",
                },
            ],
        };
        assert_eq!(
            CanonOp::from_def(&AVATAR).content_hash(),
            CanonOp::from_def(&AVATAR_RENAMED).content_hash(),
        );
    }

    #[test]
    fn changing_the_selection_changes_the_hash() {
        static AVATAR_NARROWED: FragmentDef = FragmentDef {
            name: "Avatar",
            on: "User",
            selection: &[Sel::Leaf { field: "name" }],
        };
        assert_ne!(
            CanonOp::from_def(&AVATAR).content_hash(),
            CanonOp::from_def(&AVATAR_NARROWED).content_hash(),
        );
    }

    #[test]
    fn spread_fragments_inline_into_the_operation() {
        // The canonical form of PostCard contains Avatar's selection inlined under the
        // `author` edge — one operation, fragments baked in (Relay-classic).
        let json = CanonOp::from_def(&POST_CARD).to_canonical_json();
        assert!(json.contains("\"avatar_url\""), "inlined child field missing:\n{json}");
        assert!(json.contains("\"author\""), "edge missing:\n{json}");
    }
}
