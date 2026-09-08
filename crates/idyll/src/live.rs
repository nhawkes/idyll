//! **Live** — the one client-code primitive.
//!
//! A live is a **named hole in server content where live code mounts**. In a view it
//! is a marker — the `live::Todos()` mount call in content — emitted as a first-class
//! [`TplNode::Live`](crate::template::TplNode::Live) in the const IR (never a marker
//! element or `data-` attribute recovered by string comparison). No component runs at
//! the marker: the name binds to the app's **wasm live table**, and whoever splices
//! content containing the marker owes it a `mount((name, instance), seed)` — the server
//! host through the membrane for the SSR paint, the browser runtime for hydration and
//! for live arriving in spliced `View` content.
//!
//! **Identity is two-part.** `name` is the component identity — the live-table key,
//! validated against the guest's `live()` so a typo'd marker is a loud boot/render
//! error. `instance` is the mount identity — the per-name occurrence index in document
//! order; every fold derives it by walking the same tree in the same order, so the same
//! live component can appear more than once on a page.
//!
//! **Postures fall out of where the markers are drawn.** Every page is SSR + SPA
//! (Next-style — the router runtime always ships); the markers decide what else does:
//! zero markers → no seed, no app wasm (the runtime is just the router); leaf markers
//! → independent widgets; a shell live providing a store via context with nested
//! markers → the hybrid app; one marker around everything → the full SPA. Same
//! primitive, same wire format, same folds.
//!
//! The live's live half lives in the app crate's guest: its component (an ordinary
//! `async fn(Ctx<Setup, M>, args…)` message loop) and the explicit live table mapping
//! each name to a mount that builds the component's args from the page seed.
//!
//! **Keys** make repeated live honest. A row-shaped live (`@for` over server
//! content, one live per row) has no stable identity in document order — inserting a
//! row shifts every index — and no way to know *which* record is its own. A keyed
//! marker carries the record's canonical wire id: identity becomes `(name, key)`, and
//! the live resolves its data from the store by that key instead of by position.
//! The key is typed end to end: the app's `guest!` table declares each keyed live's
//! key type (exported as an [`LiveDef`]), the marker's `key = expr` must be exactly
//! that type, and [`IslandKey`]/[`FromLiveKey`] carry it across the wire as the
//! same canonical id string the cache normalizes by.

use std::borrow::Cow;

/// A typed live component handle, as `guest!` exports it (`app::live::Check`): the
/// table name plus the key type its component takes. The mount call in content —
/// `live::Check(frag)` — resolves the name from here and type-checks the key
/// against `Key`.
pub trait LiveDef {
    const NAME: &'static str;

    /// The component's key parameter type ([`NoKey`] for singletons).
    type Key;
}

/// A value usable as a live key: encodes to the record's **canonical wire id** (the
/// same string the cache normalizes by). Implemented for `String`/`&str` (the untyped
/// content path) and, in `idyll-data`, for `Frag<F>` — the typed path.
pub trait IslandKey {
    fn to_wire(&self) -> String;
}

/// The decode half, in the guest: the mount's key string back to the component's key
/// parameter type.
pub trait FromLiveKey: Sized {
    fn from_wire(wire: &str) -> Self;
}

/// The key type of live that have none. Deliberately not `IslandKey`: a keyed
/// marker on a keyless live is a type error at the marker.
pub struct NoKey;

impl IslandKey for String {
    fn to_wire(&self) -> String {
        self.clone()
    }
}

impl IslandKey for &str {
    fn to_wire(&self) -> String {
        (*self).to_string()
    }
}

impl FromLiveKey for String {
    fn from_wire(wire: &str) -> Self {
        wire.to_string()
    }
}

/// The typed marker's key encoder: `expr` must be exactly the live's declared key
/// type — a mismatched key is an ordinary type error at the marker site.
pub fn wire_key<D: LiveDef>(key: D::Key) -> Cow<'static, str>
where
    D::Key: IslandKey,
{
    Cow::Owned(key.to_wire())
}

/// A **keyless** marker's name. Demanding `Key = NoKey` is the other half of the
/// arity check [`wire_key`] gives a keyed marker: omitting `key = …` on a keyed
/// live is a type error here, at the marker, rather than a mount that cannot build
/// the component's arguments.
pub const fn keyless_name<D: LiveDef<Key = NoKey>>() -> &'static str {
    D::NAME
}
