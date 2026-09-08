//! [`Computed`] — a cached value derived from whatever it read last run.
//!
//! A `Computed` is a derived [`graph`](super::graph) node: an observer of the cells
//! it reads *and* a source for the nodes that read it. It recomputes lazily (only
//! when pulled, and only if a dependency actually changed), re-tracking its
//! dependencies on every run — so a `Computed` whose read set depends on a value is
//! always subscribed to exactly what it read. It bails out via [`PartialEq`]: if the
//! recomputed value equals the cached one, its observers are never marked, which is
//! what makes a diamond recompute its apex once and a canceling change stop early.

use std::rc::{Rc, Weak};

use super::graph::{self, Lane, NodeCore};
use super::{Cx, Signal, SignalCell, SignalId};
use crate::owner::Owner;

/// A value derived from an explicit closure over other reactive reads. `Clone` — a
/// reader-shaped handle into the same `Rc` cell as [`Signal`] — so it flows into
/// bindings and other derivations. Read it tracked with [`get`](Self::get) inside a
/// computation, or untracked with [`now`](Self::now).
///
/// Owned by the scope that creates it (via [`Ctx::computed`](crate::Ctx::computed) /
/// [`Owner::computed`](crate::Owner::computed)); that scope bounds its lifetime.
pub struct Computed<T: 'static> {
    cell: Rc<SignalCell<T>>,
}

impl<T: 'static> Clone for Computed<T> {
    fn clone(&self) -> Self {
        Computed {
            cell: self.cell.clone(),
        }
    }
}

/// Build a derived node's cell: seed its value with one untracked evaluation of `f`,
/// then run it once with the node installed as the current observer so its reads
/// become the initial dependency edges. Retained by `owner`, so it disposes with
/// that scope. Shared by [`Computed`] and [`deferred`](super::deferred) (which passes
/// the idle [`Lane`]).
pub(crate) fn build_derived<T, F>(owner: &Owner, f: F, lane: Option<Lane>) -> Rc<SignalCell<T>>
where
    T: Clone + PartialEq + 'static,
    F: Fn(&Cx) -> T + 'static,
{
    // The value must exist before the cell does (edges attach to a constructed node),
    // so seed it untracked; the first run below establishes the real edges.
    let seed = f(&Cx::untracked());
    let rt = owner.runtime();
    let cell = Rc::new_cyclic(|weak: &Weak<SignalCell<T>>| {
        let weak = weak.clone();
        let recompute: Rc<dyn Fn(&Cx) -> bool> = Rc::new(move |cx: &Cx| {
            let Some(cell) = weak.upgrade() else { return false };
            let next = f(cx);
            let mut slot = cell.value().borrow_mut();
            if *slot == next {
                false
            } else {
                *slot = next;
                true
            }
        });
        SignalCell::build(&rt, seed, Rc::new(NodeCore::derived(recompute, lane)))
    });
    owner.retain(cell.clone());
    graph::init_derived(&cell.node());
    cell
}

impl<T: 'static> Computed<T> {
    /// Derive a value from `f`, rooted in `owner`.
    pub(crate) fn new_in<F>(owner: &Owner, f: F) -> Self
    where
        T: Clone + PartialEq,
        F: Fn(&Cx) -> T + 'static,
    {
        Computed {
            cell: build_derived(owner, f, None),
        }
    }

    fn node(&self) -> Rc<NodeCore> {
        self.cell.node()
    }

    pub fn id(&self) -> SignalId {
        self.cell.id()
    }

    /// The reader view of this computed — for handing to a binding or another
    /// derivation as a plain [`Signal`].
    pub fn read(&self) -> Signal<T> {
        Signal::from_cell(self.cell.clone())
    }

    /// Tracked read — pulls this computed current, registers it as a dependency of
    /// `cx`'s observer, and returns a clone.
    pub fn get(&self, cx: &Cx) -> T
    where
        T: Clone,
    {
        let node = self.node();
        graph::update_if_necessary(&node);
        graph::track(cx, &node);
        self.cell.value().borrow().clone()
    }

    /// The current value, in this component's live turn. Pulls current first so an
    /// untracked read still reflects settled state.
    pub fn now(&self, _turn: impl super::InTurn) -> T
    where
        T: Clone,
    {
        graph::update_if_necessary(&self.node());
        self.cell.value().borrow().clone()
    }
}

// Like `MutableSignal`, `Computed` has no `Display`: read it through `.get(cx)`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::Turn;
    use std::cell::{Cell, RefCell};

    fn scoped() -> (crate::Runtime, Owner) {
        let rt = crate::Runtime::new();
        let owner = Owner::new(rt.core());
        (rt, owner)
    }

    #[test]
    fn computed_derives_from_signal() {
        let (_rt, owner) = scoped();
        let n = owner.mutable_signal(3i32);
        let nr = n.read();
        let doubled = owner.computed(move |cx| nr.get(cx) * 2);
        assert_eq!(doubled.now(&Turn::mint()), 6);
        n.set(&Turn::mint(), 10);
        assert_eq!(doubled.now(&Turn::mint()), 20);
    }

    #[test]
    fn computed_chains() {
        let (_rt, owner) = scoped();
        let x = owner.mutable_signal(1i32);
        let xr = x.read();
        let a = owner.computed(move |cx| xr.get(cx) + 1);
        let b = owner.computed(move |cx| a.get(cx) * 3);
        assert_eq!(b.now(&Turn::mint()), 6);
        x.set(&Turn::mint(), 2);
        assert_eq!(b.now(&Turn::mint()), 9);
    }

    #[test]
    fn diamond_recomputes_apex_once_and_is_glitch_free() {
        let (rt, owner) = scoped();
        let count = owner.mutable_signal(2i32);
        let c1 = count.read();
        let doubled = owner.computed(move |cx| c1.get(cx) * 2);
        let c2 = count.read();
        let parity = owner.computed(move |cx| if c2.get(cx) % 2 == 0 { "even" } else { "odd" });
        let c3 = count.read();
        let summary = owner.computed(move |cx| {
            format!("{} is {}, doubled is {}", c3.get(cx), parity.get(cx), doubled.get(cx))
        });

        let observed = Rc::new(RefCell::new(Vec::<String>::new()));
        let runs = Rc::new(Cell::new(0u32));
        {
            let observed = Rc::clone(&observed);
            let runs = Rc::clone(&runs);
            owner.effect(move |cx| {
                runs.set(runs.get() + 1);
                observed.borrow_mut().push(summary.get(cx));
            });
        }
        rt.run_pending_effects();
        assert_eq!(runs.get(), 1);

        count.set(&Turn::mint(), 3);
        rt.run_pending_effects();

        assert_eq!(runs.get(), 2, "apex effect runs once per settled change");
        assert_eq!(
            &*observed.borrow(),
            &["2 is even, doubled is 4", "3 is odd, doubled is 6"]
        );
    }

    #[test]
    fn a_computed_created_inside_a_computation_leaks_no_edges_onto_its_creator() {
        let (rt, owner) = scoped();
        let outer = owner.mutable_signal(0i32);
        let inner = owner.mutable_signal(100i32);
        let runs = Rc::new(Cell::new(0u32));

        let child = owner.clone();
        let outer_r = outer.read();
        let inner_r = inner.read();
        {
            let runs = Rc::clone(&runs);
            owner.effect(move |cx| {
                runs.set(runs.get() + 1);
                let _ = outer_r.get(cx);
                let ir = inner_r.clone();
                let doubled = child.computed(move |cx| ir.get(cx) * 2);
                assert_eq!(doubled.now(&Turn::mint()), inner_r.now(&Turn::mint()) * 2);
            });
        }
        rt.run_pending_effects();
        assert_eq!(runs.get(), 1);

        inner.set(&Turn::mint(), 1);
        rt.run_pending_effects();
        assert_eq!(runs.get(), 1, "nested computed's seed leaked an edge onto its creator");

        outer.set(&Turn::mint(), 5);
        rt.run_pending_effects();
        assert_eq!(runs.get(), 2);
    }

    #[test]
    fn canceling_change_stops_early() {
        let (rt, owner) = scoped();
        let n = owner.mutable_signal(4i32);
        let nr = n.read();
        let parity = owner.computed(move |cx| nr.get(cx) % 2 == 0);

        let runs = Rc::new(Cell::new(0u32));
        {
            let runs = Rc::clone(&runs);
            owner.effect(move |cx| {
                runs.set(runs.get() + 1);
                let _ = parity.get(cx);
            });
        }
        rt.run_pending_effects();
        assert_eq!(runs.get(), 1);

        n.set(&Turn::mint(), 6);
        rt.run_pending_effects();
        assert_eq!(runs.get(), 1, "unchanged derived value stops propagation");
    }
}
