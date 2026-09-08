//! Fragments: a component's typed declaration of the fields it reads, validated
//! against the published schema (`fragment! { TodoItem on Todo { text, done } }` —
//! `Todo` resolves against `schema.json`, not a Rust type).
//!
//! The macro generates a plain **value struct** carrying exactly the selected fields,
//! so masking is physical: an unselected field has nowhere to live. Scalars are typed
//! from the schema; a **node** edge is a [`Frag`] — a typed id, the record reference —
//! and a **value** edge embeds the child fragment's struct directly (an embedded value
//! has no identity; its data is already in the parent record). Reads go through
//! [`Live`] — one tracked dependency per record cell.

use std::hash::Hash;
use std::marker::PhantomData;

use derive_where::derive_where;
use idyll::{Cx, Signal};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::ir::FragmentDef;
use crate::Cache;

/// Implemented by each `fragment!`-generated value struct.
pub trait Fragment: Sized {
    /// The **schema name** of the record this fragment is a view on (`"Todo"`).
    const ON: &'static str;

    /// The normalized selection descriptor.
    const DEF: &'static FragmentDef;

    /// Build the value struct from the record's current JSON.
    fn from_record(record: &serde_json::Value) -> Self;
}

/// A fragment on a **node** record — one with identity. `Id` is the schema's id type,
/// so a [`Frag`] is a typed record reference end to end. Fragments on embedded values
/// don't implement this: a value has no identity to reference.
pub trait NodeFragment: Fragment {
    type Id: Clone + Eq + Hash + std::fmt::Debug + Serialize + DeserializeOwned + 'static;
}

/// A **typed record reference** — Relay's `$key`: fragment `F`'s view of the node with
/// this id, and never the fields themselves. Resolve it through `F`'s `read`/`resolve`;
/// identity (`Eq`/`Hash`, keyed rows, live keys) is the id's.
#[derive_where(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Frag<F: NodeFragment> {
    id: F::Id,
    _fragment: PhantomData<fn() -> F>,
}

impl<F: NodeFragment> Frag<F> {
    /// Reference the record with this id. Normally produced by a generated edge
    /// accessor, an operation's roots, or `F::key(id)` — not by hand.
    pub fn from_id(id: F::Id) -> Self {
        Self { id, _fragment: PhantomData }
    }

    /// Consume the reference (fragment `F`'s generated `read` resolves it).
    pub fn into_id(self) -> F::Id {
        self.id
    }
}

/// A `Frag` **is** a live key: it encodes to the record's canonical wire id — the
/// exact string the cache normalizes by — and decodes back to a typed reference. The
/// typed path for `@live(Def, key = …)` markers.
impl<F: NodeFragment> idyll::live::IslandKey for Frag<F> {
    fn to_wire(&self) -> String {
        serde_json::to_string(&self.id).expect("record ids serialize")
    }
}

impl<F: NodeFragment> idyll::live::FromLiveKey for Frag<F> {
    fn from_wire(wire: &str) -> Self {
        let id = serde_json::from_str(wire)
            .unwrap_or_else(|err| panic!("live key `{wire}` is not a canonical id: {err}"));
        Frag::from_id(id)
    }
}

/// A live handle on one record: [`get`](Self::get) is the tracked read (one dependency
/// — the record cell), [`now`](Self::now) the untracked one — witnessed by
/// `Ctx<Live>`, so it only exists after `render()` and can never feed the painted
/// frame.
#[derive_where(Clone)]
pub struct Live<F: Fragment> {
    cell: Signal<serde_json::Value>,
    _fragment: PhantomData<fn() -> F>,
}

impl<F: Fragment> Live<F> {
    pub fn new(cell: Signal<serde_json::Value>) -> Self {
        Self { cell, _fragment: PhantomData }
    }

    pub fn get(&self, cx: &Cx) -> F {
        F::from_record(&self.cell.get(cx))
    }

    /// The record's current value, in this component's live turn.
    pub fn now(&self, witness: impl idyll::InTurn) -> F {
        F::from_record(&self.cell.now(witness))
    }

    /// The record as of this mount — a value, not a subscription: `Setup` runs exactly
    /// once per mount, so what this feeds is painted once and only changes by
    /// re-mounting. A page reads its route with this (transitions re-mount, so the
    /// route cannot change under it); a live reads store data it renders as static
    /// content.
    pub fn at_mount<M>(&self, ctx: &idyll::Ctx<idyll::Setup, M>) -> F {
        F::from_record(&self.cell.at_mount(ctx))
    }
}

/// The record's canonical wire id — what the cache normalizes by.
fn wire_id<F: NodeFragment>(frag: Frag<F>) -> serde_json::Value {
    serde_json::to_value(frag.into_id()).expect("record ids serialize")
}

/// Resolve a reference against the seeded store, suspending until the record is present
/// (**never** starts a load — data must have been declared and preloaded; an unseeded
/// read surfaces as a deferred boundary, never a silent refetch).
pub async fn read_fragment<F: NodeFragment>(cache: &Cache, frag: Frag<F>) -> Live<F> {
    Live::new(cache.read_json(F::ON, &wire_id(frag)).await)
}

/// Resolve a reference against a store that must already hold it — the synchronous form
/// for reads inside a tracked scope (a `ctx.synced` source cannot await). A missing
/// record is an incomplete seed: a bug, surfaced loudly.
pub fn resolve_fragment<F: NodeFragment>(cache: &Cache, frag: Frag<F>) -> Live<F> {
    let id = wire_id(frag);
    match cache.peek_json(F::ON, &id) {
        Some(signal) => Live::new(signal),
        None => panic!("`{}` {id} is not in the store — the seed is incomplete", F::ON),
    }
}

/// Parse one selected field out of a record's JSON. The server built the seed from the
/// same published schema the field's type came from, so a missing field or type
/// mismatch is schema drift the boot check should have caught: fail loud.
pub fn field_value<T: DeserializeOwned>(record: &serde_json::Value, on: &str, field: &str) -> T {
    let Some(value) = record.get(field) else {
        panic!("schema violation: `{on}` record has no field `{field}`: {record}");
    };
    serde_json::from_value(value.clone()).unwrap_or_else(|err| {
        panic!("schema violation: `{on}.{field}` does not parse as its schema type: {err}")
    })
}

/// The raw JSON of one field — edge accessors take ids/embedded values with it.
pub fn field_json(record: &serde_json::Value, on: &str, field: &str) -> serde_json::Value {
    record
        .get(field)
        .cloned()
        .unwrap_or_else(|| panic!("schema violation: `{on}` record has no field `{field}`: {record}"))
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
    use crate::ir::{FragmentDef, Sel};

    // Hand-written stand-ins for what `fragment!` emits.
    #[derive(Clone, PartialEq, Debug)]
    struct Avatar {
        name: String,
        money: MoneyFrag,
    }
    static AVATAR_DEF: FragmentDef = FragmentDef {
        name: "Avatar",
        on: "User",
        selection: &[
            Sel::Leaf { field: "name" },
            Sel::Spread { edge: "money", frag: &MONEY_DEF },
        ],
    };
    impl Fragment for Avatar {
        const ON: &'static str = "User";
        const DEF: &'static FragmentDef = &AVATAR_DEF;
        fn from_record(record: &serde_json::Value) -> Self {
            Avatar {
                name: field_value(record, "User", "name"),
                money: MoneyFrag::from_record(&field_json(record, "User", "money")),
            }
        }
    }
    impl NodeFragment for Avatar {
        type Id = u64;
    }

    #[derive(Clone, PartialEq, Debug)]
    struct MoneyFrag {
        amount: u64,
    }
    static MONEY_DEF: FragmentDef = FragmentDef {
        name: "MoneyFrag",
        on: "Money",
        selection: &[Sel::Leaf { field: "amount" }],
    };
    impl Fragment for MoneyFrag {
        const ON: &'static str = "Money";
        const DEF: &'static FragmentDef = &MONEY_DEF;
        fn from_record(record: &serde_json::Value) -> Self {
            MoneyFrag { amount: field_value(record, "Money", "amount") }
        }
    }

    fn block_ready<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Waker};
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(out) => out,
            Poll::Pending => panic!("read pending — not seeded"),
        }
    }

    #[test]
    fn a_node_frag_reads_typed_values_from_the_cache() {
        let turn = idyll::Turn::for_test();
        let (_rt, owner) = test_scope();
        let cache = Cache::rooted(owner);
        cache.upsert_json(
            &turn,
            "User",
            serde_json::json!({ "id": 7, "name": "Ada", "money": { "amount": 5 } }),
        );

        let frag = Frag::<Avatar>::from_id(7);
        let live = block_ready(read_fragment(&cache, frag));
        assert_eq!(live.now(&turn).name, "Ada");
    }

    #[test]
    fn an_unseeded_node_read_suspends() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let turn = idyll::Turn::for_test();
        let (_rt, owner) = test_scope();
        let cache = Cache::rooted(owner);
        let frag = Frag::<Avatar>::from_id(1);
        let mut cx = Context::from_waker(Waker::noop());
        let mut future = std::pin::pin!(read_fragment(&cache, frag));
        assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));

        let mut seed = crate::Seed::new();
        seed.push_raw("User", serde_json::json!({ "id": 1, "name": "Grace", "money": { "amount": 0 } }));
        cache.replay(&turn, &seed).expect("seed replays");
        assert!(matches!(future.as_mut().poll(&mut cx), Poll::Ready(_)));
    }

    #[test]
    fn a_value_edge_embeds_and_stays_live_over_its_parent() {
        let turn = idyll::Turn::for_test();
        let (_rt, owner) = test_scope();
        let cache = Cache::rooted(owner);
        cache.upsert_json(
            &turn,
            "User",
            serde_json::json!({ "id": 1, "name": "Ada", "money": { "amount": 5 } }),
        );

        let live = block_ready(read_fragment(&cache, Frag::<Avatar>::from_id(1)));
        assert_eq!(live.now(&turn).money.amount, 5);

        cache.upsert_json(
            &turn,
            "User",
            serde_json::json!({ "id": 1, "name": "Ada", "money": { "amount": 9 } }),
        );
        assert_eq!(
            live.now(&turn).money.amount,
            9,
            "embedded values rebuild with the parent record"
        );
    }

    #[test]
    fn frag_identity_is_the_typed_id() {
        use std::collections::HashSet;
        let a = Frag::<Avatar>::from_id(1);
        let b = Frag::<Avatar>::from_id(1);
        let c = Frag::<Avatar>::from_id(2);
        assert_eq!(a, b);
        assert_ne!(a, c);
        let set: HashSet<Frag<Avatar>> = [a, b, c].into_iter().collect();
        assert_eq!(set.len(), 2);
    }
}
