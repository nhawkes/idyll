//! The per-view **index-dispatch scope**: every reactive leaf a `live_view!` block emits —
//! text/attr/style/bool bindings — is a `u32` into one monomorphic `match` the macro
//! generates, not a boxed closure. The graph addresses a leaf as *data*
//! (`(scope, index)` on its observer edges — see [`graph`](super::graph)), a write
//! sets the leaf's dirty bit, and the flush drains the scope by calling the block's
//! dispatch with each dirty index. The dispatch lives in the component's own code, so
//! a binding body is reached by one call through the block — no per-binding closure,
//! no `Reaction` wrapper, no vtable per leaf.
//!
//! Dependency tracking stays dynamic and fine-grained: a run detaches the index's
//! previous source edges and re-registers exactly what it reads this turn — the same
//! discipline as the graph's `run_update`, with the edges held both ways as data
//! (`reads[idx]` here, `Obs::Binding` on the source).
//!
//! Bindings are *leaves* with output-equality gates in their arms, so they trade the
//! graph's `Check` verification for "maybe stale → re-run": a mark anywhere upstream
//! re-runs the arm, whose reads pull computeds current ([`Signal::get`] runs
//! `update_if_necessary` first) and whose own cache gates the DOM write. The DOM op
//! stream is identical to the `Check`-verified one; only recompute counts differ.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::graph::NodeCore;
use super::Cx;

/// A dense dynamic bitset — the dirty set and nothing more. Indices are dense by
/// construction (blocks install contiguous ranges), so a `Vec<u64>` is exact.
#[derive(Default)]
pub(crate) struct Bits(Vec<u64>);

impl Bits {
    fn grow_for(&mut self, bit: u32) {
        let word = (bit / 64) as usize;
        if self.0.len() <= word {
            self.0.resize(word + 1, 0);
        }
    }

    pub(crate) fn set(&mut self, bit: u32) {
        self.grow_for(bit);
        self.0[(bit / 64) as usize] |= 1 << (bit % 64);
    }

    /// Drain the set bits in ascending order.
    pub(crate) fn take(&mut self) -> impl Iterator<Item = u32> {
        let words = std::mem::take(&mut self.0);
        words.into_iter().enumerate().flat_map(|(w, mut word)| {
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let bit = word.trailing_zeros();
                word &= word - 1;
                Some(w as u32 * 64 + bit)
            })
        })
    }
}

/// One installed dispatch block: `count` leaves starting at `base`, drained through
/// the block's `run` — the one erased seam per `live_view!` block (the macro-emitted
/// `match` over local indices, with the view's resolved node ids bound in).
struct Block {
    base: u32,
    count: u32,
    run: Rc<dyn Fn(&Cx, u32)>,
}

/// A mounted view's reactive scope. Created at mount, retained by the view's owner;
/// dropping it dies every edge pointing at it (they are `Weak`) and stops the flush
/// from running it (the pending queue holds it weakly).
#[derive(Default)]
pub(crate) struct ViewScope {
    dirty: RefCell<Bits>,
    /// Already in the flush queue — a scope enqueues once per settle, however many
    /// bits get marked.
    queued: Cell<bool>,
    /// itself to.
    /// Per index: the graph sources it read last run (strong, like a derived node's
    /// `sources`) — walked to detach before the index re-runs.
    reads: RefCell<Vec<Vec<Rc<NodeCore>>>>,
    blocks: RefCell<Vec<Block>>,
}

impl ViewScope {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// Install the next dispatch block and return its base index. Mount-time only.
    pub(crate) fn install(&self, count: u32, run: Rc<dyn Fn(&Cx, u32)>) -> u32 {
        let mut blocks = self.blocks.borrow_mut();
        let base = blocks.last().map(|b| b.base + b.count).unwrap_or(0);
        self.reads.borrow_mut().resize_with((base + count) as usize, Vec::new);
        blocks.push(Block { base, count, run });
        base
    }

    /// The running index, if an arm is on the stack — the attribution
    /// [`graph::track`](super::graph::track) records for a binding observer.
    /// Record `source` into the running index's read set (the strong half of the
    /// edge; the caller adds the weak half on the source). Returns `false` when the
    /// edge already exists, so the caller can skip the source side too.
    pub(crate) fn note_read(&self, idx: u32, source: &Rc<NodeCore>) -> bool {
        let mut reads = self.reads.borrow_mut();
        let set = &mut reads[idx as usize];
        if set.iter().any(|s| Rc::ptr_eq(s, source)) {
            return false;
        }
        set.push(Rc::clone(source));
        true
    }

    /// A source this index read changed (or may have): set its bit. Returns whether
    /// the scope needs enqueuing (first mark since the last flush).
    pub(crate) fn mark(&self, idx: u32) -> bool {
        self.dirty.borrow_mut().set(idx);
        !self.queued.replace(true)
    }

    /// Initial paint: run every installed index once, establishing edges and the
    /// first frame's ops. Blocks are leaves, so order is immaterial; ascending keeps
    /// the op stream deterministic.
    pub(crate) fn paint(self: &Rc<Self>) {
        let total = {
            let blocks = self.blocks.borrow();
            blocks.last().map(|b| b.base + b.count).unwrap_or(0)
        };
        for idx in 0..total {
            self.run(idx);
        }
    }

    /// Drain the dirty set. Arms never write signals, so one pass settles; a mark
    /// arriving *during* the drain (a pulled computed marking another index) lands in
    /// the fresh set and re-enqueues the scope.
    pub(crate) fn flush(self: &Rc<Self>) {
        self.queued.set(false);
        let pending: Vec<u32> = self.dirty.borrow_mut().take().collect();
        for idx in pending {
            self.run(idx);
        }
    }

    /// Run one index: detach its previous edges, bind its index into the `Cx`, dispatch.
    fn run(self: &Rc<Self>, idx: u32) {
        let old_sources = std::mem::take(&mut self.reads.borrow_mut()[idx as usize]);
        for source in old_sources {
            source.retain_observers(|scope, i| !(Rc::ptr_eq(scope, self) && i == idx));
        }
        let (run, local) = {
            let blocks = self.blocks.borrow();
            let block = blocks
                .iter()
                .find(|b| idx >= b.base && idx < b.base + b.count)
                .expect("a marked index was installed by a block");
            (Rc::clone(&block.run), idx - block.base)
        };
        run(&Cx::binding(self, idx), local);
    }
}

#[cfg(test)]
mod tests {
    use super::super::graph;
    use super::*;

    fn core() -> Rc<crate::runtime::RuntimeCore> {
        Rc::clone(crate::Runtime::new().core())
    }

    fn source() -> Rc<NodeCore> {
        Rc::new(NodeCore::source())
    }

    fn drain(core: &crate::runtime::RuntimeCore) {
        while let crate::signal::FlushStep::Ran { .. } = graph::flush_step(core) {}
    }

    fn read(scope: &Rc<ViewScope>, cx: &Cx, node: &Rc<NodeCore>) {
        let _ = scope;
        graph::track(cx, node);
    }

    /// A write to a source re-runs exactly the indices that read it, through the
    /// installed block, with reads re-attributed per run.
    #[test]
    fn a_write_marks_exactly_the_indices_that_read_the_source() {
        let core = core();
        let scope = ViewScope::new();
        let a = source();
        let b = source();

        let runs: Rc<RefCell<Vec<u32>>> = Rc::default();
        let log = Rc::clone(&runs);
        let (ra, rb) = (Rc::clone(&a), Rc::clone(&b));
        let for_dispatch = Rc::downgrade(&scope);
        scope.install(
            3,
            Rc::new(move |cx, idx| {
                log.borrow_mut().push(idx);
                let scope = for_dispatch.upgrade().expect("scope alive");
                match idx {
                    0 => read(&scope, cx, &ra),
                    1 => read(&scope, cx, &rb),
                    _ => {}
                }
            }),
        );
        scope.paint();
        assert_eq!(*runs.borrow(), vec![0, 1, 2], "paint runs every index once");
        runs.borrow_mut().clear();

        graph::source_changed(&core, &a);
        drain(&core);
        assert_eq!(*runs.borrow(), vec![0], "only the index that read `a`");
        runs.borrow_mut().clear();

        graph::source_changed(&core, &b);
        drain(&core);
        assert_eq!(*runs.borrow(), vec![1], "only the index that read `b`");
    }

    /// Re-tracking: an index that stops reading a source is no longer woken by it.
    #[test]
    fn dependencies_rebuild_each_run() {
        let core = core();
        let scope = ViewScope::new();
        let toggle = source();
        let value = source();
        let on = Rc::new(Cell::new(true));

        let runs: Rc<RefCell<Vec<u32>>> = Rc::default();
        let log = Rc::clone(&runs);
        let (rt, rv, ron) = (Rc::clone(&toggle), Rc::clone(&value), Rc::clone(&on));
        let for_dispatch = Rc::downgrade(&scope);
        scope.install(
            1,
            Rc::new(move |cx, idx| {
                log.borrow_mut().push(idx);
                let scope = for_dispatch.upgrade().expect("scope alive");
                read(&scope, cx, &rt);
                if ron.get() {
                    read(&scope, cx, &rv);
                }
            }),
        );
        scope.paint();
        runs.borrow_mut().clear();

        on.set(false);
        graph::source_changed(&core, &toggle);
        drain(&core);
        assert_eq!(*runs.borrow(), vec![0], "the toggle write re-ran the index");
        runs.borrow_mut().clear();

        graph::source_changed(&core, &value);
        drain(&core);
        assert!(runs.borrow().is_empty(), "a dropped dependency no longer wakes the index");
    }

    /// A dropped scope's edges die: writes to sources it read run nothing and the
    /// pending queue skips it.
    #[test]
    fn a_dropped_scope_is_inert() {
        let core = core();
        let scope = ViewScope::new();
        let a = source();
        let ra = Rc::clone(&a);
        let for_dispatch = Rc::downgrade(&scope);
        scope.install(
            1,
            Rc::new(move |cx, _| {
                if let Some(scope) = for_dispatch.upgrade() {
                    read(&scope, cx, &ra);
                }
            }),
        );
        scope.paint();
        graph::source_changed(&core, &a);
        drop(scope);
        drain(&core); // must not panic on the dead weak
        graph::source_changed(&core, &a); // dead edges skipped
    }
}
