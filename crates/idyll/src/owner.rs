//! Disposal scope for reactive cells.
//!
//! An `Owner` is a node in a tree of scopes. Every reactive node created in a scope
//! is **retained** by it — the scope holds a strong reference, which is what keeps an
//! effect (nothing else references it) alive and what makes a subtree reclaim as a
//! unit. Disposing a scope drops its retained references and its child scopes; a node
//! with no other holder is freed immediately, one still read elsewhere lives on until
//! that reader drops. There is no separate reclamation pass — dropping the scope *is*
//! the reclamation.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::runtime::RuntimeCore;
use crate::MutableSignal;

/// Opaque identity of an [`Owner`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct OwnerId(u64);

impl OwnerId {
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

static OWNER_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_owner_id() -> OwnerId {
    OwnerId(OWNER_COUNTER.fetch_add(1, Ordering::Relaxed))
}

struct Scope {
    id: OwnerId,
    /// Child scopes (an `@for` row, an `@if` branch, a nested component). Disposing
    /// this scope disposes them first, so a spliced-out subtree reclaims as a unit.
    children: RefCell<Vec<Owner>>,
    /// Strong references to every reactive cell created in this scope — the retention
    /// that keeps effects alive and reclaims the subtree on disposal. Held as
    /// `Rc<dyn Any>`: heterogeneous cells (`SignalCell<T>`, `VecCell<T>`) need erasure to
    /// share a `Vec`. Nothing is ever *called* through this pointer — the graph reaches a
    /// node's core through the concrete `Rc<NodeCore>` edges, not through here — so it
    /// costs no dispatch on the reactive hot path. Its one vtable touch is `drop_in_place`
    /// at disposal, once per cell, which `Rc<dyn NodeApi>` paid before too. What moved off
    /// the hot path is the per-flush `.core()` call the deleted `NodeApi` trait required.
    retained: RefCell<Vec<Rc<dyn std::any::Any>>>,
    /// Idempotence guard: a scope can be disposed by its own last handle dropping and
    /// by its parent disposing it; the second is a no-op.
    disposed: Cell<bool>,
    /// The enclosing scope, if any — weak, since the parent holds the strong edge in
    /// `children`. Disposal detaches from here, so a long-lived parent (an `@for`
    /// spawning rows for hours) doesn't accumulate dead children.
    parent: Weak<Scope>,
    /// The runtime this scope's cells mark into — weak, so an owner never keeps a
    /// runtime alive.
    rt: Weak<RuntimeCore>,
}

impl Scope {
    /// Reclaim this scope and everything below it: dispose child scopes, then release
    /// every node retained here. Idempotent.
    fn dispose(&self) {
        if self.disposed.replace(true) {
            return;
        }
        let children = std::mem::take(&mut *self.children.borrow_mut());
        for child in children {
            child.0.dispose();
        }
        self.retained.borrow_mut().clear();
        if let Some(parent) = self.parent.upgrade() {
            // A parent mid-dispose already marked itself; its own drain drops us.
            if !parent.disposed.get() {
                parent.children.borrow_mut().retain(|c| c.0.id != self.id);
            }
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.dispose();
    }
}

/// A lifetime/reclamation scope for a set of reactive cells. Cheap to clone (shares
/// one identity); hand consumers [`Signal`](crate::Signal)s rather than the owner.
///
/// An owner is **received, not minted**: a component's arrives with its
/// [`Ctx`](crate::Ctx), and a library that wants to own cells takes one (or the `Ctx`
/// that implies it) from whoever is spawning it. There is no public constructor —
/// minting a scope is [`Ctx`]'s alone, so `Ctx::new().owner()` is the only way to a
/// root, and it names who owns it.
#[derive(Clone)]
pub struct Owner(Rc<Scope>);

fn new_scope(parent: Weak<Scope>, rt: Weak<RuntimeCore>) -> Rc<Scope> {
    Rc::new(Scope {
        id: next_owner_id(),
        children: RefCell::new(Vec::new()),
        retained: RefCell::new(Vec::new()),
        disposed: Cell::new(false),
        parent,
        rt,
    })
}

impl Owner {
    /// The root scope mint. Crate-private on purpose: an owner is **received, not
    /// minted** — a component's arrives with its [`Ctx`](crate::Ctx), and a library
    /// that owns cells takes one from whoever spawns it. Two callers mint:
    /// `Ctx::assemble` (one scope per component, on that component's runtime) and
    /// `build_slot` (one per slot instance, reclaimed with the instance's guard).
    pub(crate) fn new(rt: &Rc<RuntimeCore>) -> Self {
        Owner(new_scope(Weak::new(), Rc::downgrade(rt)))
    }

    /// The runtime this scope's cells mark into — alive for as long as components
    /// still create cells in it.
    pub(crate) fn runtime(&self) -> Rc<RuntimeCore> {
        self.0.rt.upgrade().expect("an owner's runtime outlives the components rooted in it")
    }

    /// A child disposal scope, owned by this one. Nodes rooted in the child are
    /// reclaimed when the child is disposed — explicitly (an `@if` branch swaps, a
    /// `@for` row is removed) or transitively when this owner is.
    pub fn child(&self) -> Owner {
        let child = Owner(new_scope(Rc::downgrade(&self.0), self.0.rt.clone()));
        self.0.children.borrow_mut().push(child.clone());
        child
    }

    /// Dispose this scope and its subtree now (idempotent). Used by the runtime when a
    /// branch/row unmounts before its parent component does.
    pub fn dispose(&self) {
        self.0.dispose();
    }

    /// Whether this scope has been reclaimed — a mint gate for machinery that would
    /// otherwise do work a dead scope can neither retain nor ever reclaim.
    pub(crate) fn is_disposed(&self) -> bool {
        self.0.disposed.get()
    }

    pub fn id(&self) -> OwnerId {
        self.0.id
    }

    /// Retain a reactive cell in this scope — the strong reference that keeps it alive
    /// until the scope disposes. Called by the node constructors below; the cell is held
    /// only as a keepalive, never called through.
    pub(crate) fn retain(&self, node: Rc<dyn std::any::Any>) {
        if !self.0.disposed.get() {
            self.0.retained.borrow_mut().push(node);
        }
    }

    /// An opaque handle that keeps this owner (and therefore its cells) alive. A
    /// server-rendered [`LiveView`](crate::LiveView) stashes one so its cells survive until
    /// the view is serialized and dropped.
    pub fn keepalive(&self) -> Rc<dyn std::any::Any> {
        Rc::clone(&self.0) as Rc<dyn std::any::Any>
    }

    /// A non-owning handle to this scope — see [`WeakOwner`].
    pub(crate) fn downgrade(&self) -> WeakOwner {
        WeakOwner(Rc::downgrade(&self.0))
    }

    /// Create a cell retained by (and reclaimed with) this owner.
    pub fn mutable_signal<T: 'static>(&self, value: T) -> MutableSignal<T> {
        MutableSignal::new_in(self, value)
    }

    /// Create a reactive list whose backing cells are retained by this owner.
    pub fn mutable_vec<T: 'static>(&self) -> crate::MutableVec<T> {
        crate::MutableVec::new_in(self)
    }

    /// The same, holding `values` from the start.
    pub fn mutable_vec_of<T: 'static>(&self, values: Vec<T>) -> crate::MutableVec<T> {
        crate::MutableVec::seeded_in(self, values)
    }

    /// Derive a [`Computed`](crate::Computed) rooted in this owner.
    pub fn computed<T, F>(&self, f: F) -> crate::Computed<T>
    where
        T: Clone + PartialEq + 'static,
        F: Fn(&crate::Cx) -> T + 'static,
    {
        crate::Computed::new_in(self, f)
    }

    /// Spawn an effect rooted in this owner: `f` runs now and re-runs whenever a
    /// reactive cell it read changes. Reclaimed when this owner is disposed.
    pub fn effect<F>(&self, f: F) -> crate::signal::reaction::Reaction
    where
        F: Fn(&crate::Cx) + 'static,
    {
        crate::signal::reaction::Reaction::spawn_in(self, f)
    }

    /// Derive a [`deferred`](crate::signal::deferred) read-only view of `source`,
    /// rooted in this owner — a value that trails `source` on the idle lane.
    pub fn deferred<T, F>(&self, source: F) -> crate::signal::Signal<T>
    where
        T: Clone + PartialEq + 'static,
        F: Fn(&crate::Cx) -> T + 'static,
    {
        crate::signal::deferred(self, source)
    }
}

/// A non-owning handle to a scope, for the one direction that must not own: a view's
/// **fragment builders** are reached through the reactions the view's own scope retains,
/// so a strong handle back to that scope would close a cycle — and a scope inside a cycle
/// never drops, so it never disposes. Its DOM guards are held elsewhere (the mount's, the
/// task's) and do drop, which is what makes the cycle a correctness bug and not merely a
/// leak: the ids go, the bindings stay, and the next write lands on a released node.
///
/// [`upgrade`](Self::upgrade) cannot legitimately fail — every caller runs from a reaction
/// the scope retains — so `None` means the scope is gone and there is nothing to build.
pub(crate) struct WeakOwner(Weak<Scope>);

impl WeakOwner {
    pub(crate) fn upgrade(&self) -> Option<Owner> {
        self.0.upgrade().map(Owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Disposal detaches from the parent — the parent's strong edge is gone, so the
    /// scope frees as soon as the last handle drops. This is what makes a long-lived
    /// parent under `@for` row churn not accumulate dead children.
    #[test]
    fn a_disposed_child_is_released_by_its_parent() {
        let rt = crate::Runtime::new();
        let parent = Owner::new(rt.core());
        let child = parent.child();
        let keep = child.keepalive();
        let probe = Rc::downgrade(&keep);
        drop(keep);
        child.dispose();
        drop(child);
        assert!(probe.upgrade().is_none(), "the parent still retains the disposed child");
        // And the parent's own disposal (which drains children) still works after.
        parent.dispose();
    }
}
