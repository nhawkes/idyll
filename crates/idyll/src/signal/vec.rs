//! Reactive lists: a **producer/consumer** split over a splice-op stream.
//!
//! A scalar cell's delta is degenerate — the last write wins, so a consumer only
//! needs the *current* value. A list's delta is not: to keep row identity and do O(1)
//! DOM work you need *every* op, in order. So a list is shaped like `Vec` (the
//! producer/state) plus `Iterator` (the consumer/op-stream):
//!
//! - **[`MutableVec<T>`]** — the producer. Authoritative order + per-row cells. Each
//!   structural mutation records a precise op ([`SpliceOp`]) — O(1) — fans it out to
//!   every live consumer, and marks its graph observers so the reactive flush wakes
//!   them.
//! - **[`Consumer`]** — the op-stream, one per `@for`. [`drain`](Consumer::drain)
//!   empties it. Multiple consumers each get every op: draining is per-consumer.
//!
//! Structure rides the reactive [`graph`](super::graph) for *waking and lifecycle* (a
//! `MutableVec` is a graph source; a `@for` is an effect that tracks it); the
//! op-stream rides alongside for *transport*. Per-row content is ordinary value-graph
//! reactivity — each row is a [`MutableSignal<T>`], so editing a row is O(1) and never
//! touches structure. The producer holds each row's writer; removing a row drops it,
//! and the cell is reclaimed once the last consumer reading it drops too.
//!
//! [`sync_by_key`](MutableVec::sync_by_key) is the list's **delta function**: it diffs
//! a whole new array against the current one and emits the *minimum* op set
//! (keep-by-identity, LIS-minimal moves) into the same stream.

use std::cell::{Ref, RefCell, RefMut};
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::rc::{Rc, Weak};

use super::graph::{self, NodeCore};
use super::{Cx, MutableSignal, Signal};
use crate::owner::Owner;
use crate::runtime::RuntimeCore;

/// A reactive list as the view consumes it: a snapshot of ordered rows, the per-row
/// read cell, and an op-stream subscription. `@for` renders any implementor; only
/// [`MutableVec`] also carries write access.
pub trait SignalVec<T: 'static>: Clone + 'static {
    /// Untracked snapshot of the current row order — a new consumer's initial mount
    /// reads this, then follows its op-stream forward.
    fn snapshot_order(&self) -> Vec<Row>;

    /// The row's read cell.
    fn read(&self, row: Row) -> Option<Signal<T>>;

    /// Subscribe a fresh op-stream. Call once per consumer; each drains its own.
    fn consume(&self) -> Consumer;

    /// Register the current computation as an observer of this list's structure, so a
    /// structural change wakes it. Called by the `@for` effect each run.
    fn observe(&self, cx: &Cx);

    fn len(&self) -> usize {
        self.snapshot_order().len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ── Row ───────────────────────────────────────────────────────────────────────

/// A stable handle to a row in a reactive list. Survives moves (sort); becomes a
/// no-op when the row is removed. `Copy`, cheap to pass around.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Row(u64);

// Row ids are runtime identities — they key consumers' mounted-row maps and never
// cross the hydration boundary — so per-process uniqueness is all they need.
static ROW_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_row() -> Row {
    Row(ROW_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

impl Row {
    pub(crate) fn fresh() -> Self {
        next_row()
    }
}

// ── Splice ops — the list delta algebra ─────────────────────────────────────────

/// One structural mutation. A consumer applies these to the DOM in order for O(1)
/// work per op — the list analogue of a scalar write.
#[derive(Debug, Clone)]
pub enum SpliceOp {
    /// A new row was inserted after the anchor row (`None` = prepended).
    Insert { row: Row, after: Option<Row> },
    /// A row was removed.
    Remove { row: Row },
    /// A row moved to after the anchor (`None` = moved to front).
    Move { row: Row, after: Option<Row> },
    /// All rows were removed.
    Clear,
}

/// One `@for`'s op-stream over a [`MutableVec`]. Dropping it unregisters. Not
/// `Clone`: each consumer is a distinct cursor — call [`MutableVec::consume`] again
/// for an independent one.
pub struct Consumer {
    queue: Rc<RefCell<VecDeque<SpliceOp>>>,
}

impl Consumer {
    /// Take every op recorded since the last drain, emptying the stream.
    pub fn drain(&self) -> Vec<SpliceOp> {
        self.queue.borrow_mut().drain(..).collect()
    }

    /// A consumer with no producer — its stream is always empty. For a fixed
    /// (non-reactive) `@for` source, which never mutates.
    pub fn detached() -> Self {
        Consumer {
            queue: Rc::new(RefCell::new(VecDeque::new())),
        }
    }
}

/// Which target positions may stand still during a keyed sync: the longest increasing
/// subsequence of surviving rows' old positions, read in target order (patience
/// algorithm, O(n log n)). `None` entries (fresh rows) are never marked.
fn lis_mask(old_pos_of: &[Option<usize>]) -> Vec<bool> {
    // Each tail carries `(target index, old position)` — the survivor-ness proven at
    // push rides along instead of being re-asserted at every probe.
    let mut tails: Vec<(usize, usize)> = Vec::new();
    let mut pred: Vec<Option<usize>> = vec![None; old_pos_of.len()];
    for (i, entry) in old_pos_of.iter().enumerate() {
        let Some(v) = *entry else { continue };
        let pos = tails.partition_point(|&(_, old)| old < v);
        pred[i] = pos.checked_sub(1).map(|p| tails[p].0);
        if pos == tails.len() {
            tails.push((i, v));
        } else {
            tails[pos] = (i, v);
        }
    }
    let mut mask = vec![false; old_pos_of.len()];
    let mut cursor = tails.last().map(|&(i, _)| i);
    while let Some(i) = cursor {
        mask[i] = true;
        cursor = pred[i];
    }
    mask
}

// ── MutableVec ──────────────────────────────────────────────────────────────────

struct VecInner<T: 'static> {
    /// Ordered row ids (the "list" shape).
    order: Vec<Row>,
    /// Per-row signal cells — the producer holds each row's writer.
    cells: HashMap<Row, MutableSignal<T>>,
    /// Live op-stream queues, one per consumer. Fan-out on mutation; dead
    /// (dropped-consumer) weaks are pruned lazily.
    consumers: Vec<Weak<RefCell<VecDeque<SpliceOp>>>>,
}

/// The `Rc` cell behind a reactive list: its state plus its reactive-graph record.
/// The record is a graph **source** — a structural mutation marks its observers (the
/// `@for` effects).
pub(crate) struct VecCell<T: 'static> {
    inner: RefCell<VecInner<T>>,
    core: Rc<NodeCore>,
    rt: Weak<RuntimeCore>,
}

/// A reactive list. The producer half of the split: a handle onto the `Rc` cell that
/// owns the `VecInner` container *and* its element cells. The cell is a graph source.
pub struct MutableVec<T: 'static> {
    cell: Rc<VecCell<T>>,
}

impl<T: 'static> Clone for MutableVec<T> {
    fn clone(&self) -> Self {
        MutableVec {
            cell: self.cell.clone(),
        }
    }
}

impl<T: 'static> MutableVec<T> {
    /// Create a list retained by `owner` holding `values`. Filling a list nobody has
    /// consumed or observed yet is construction, not mutation — there is no op-stream
    /// to send to and nothing to wake — which is why this asks for no turn.
    pub(crate) fn seeded_in(owner: &Owner, values: Vec<T>) -> Self {
        let list = MutableVec::new_in(owner);
        let Some(rt) = list.rt() else { return list };
        let mut inner = list.inner_mut();
        for value in values {
            let row = next_row();
            inner.order.push(row);
            inner.cells.insert(row, MutableSignal::new(&rt, value));
        }
        drop(inner);
        list
    }

    /// Create a list retained by `owner`, so its container and element cells are
    /// reclaimed when the owning scope is disposed. Public construction is via
    /// [`Ctx::mutable_vec`](crate::Ctx::mutable_vec) / [`Owner::mutable_vec`](crate::Owner::mutable_vec).
    pub(crate) fn new_in(owner: &Owner) -> Self {
        let cell = Rc::new(VecCell {
            inner: RefCell::new(VecInner {
                order: Vec::new(),
                cells: HashMap::new(),
                consumers: Vec::new(),
            }),
            core: Rc::new(NodeCore::source()),
            rt: Rc::downgrade(&owner.runtime()),
        });
        owner.retain(cell.clone());
        MutableVec { cell }
    }

    /// This list's runtime, if it still exists. A list outliving its runtime
    /// (teardown) has no observers left to wake and no views left to splice — a
    /// mutation then is a no-op, not an error, exactly as `SignalCell::wake` treats
    /// the same boundary for scalars.
    fn rt(&self) -> Option<Rc<RuntimeCore>> {
        self.cell.rt.upgrade()
    }

    fn node(&self) -> Rc<NodeCore> {
        self.cell.core.clone()
    }

    fn inner(&self) -> Ref<'_, VecInner<T>> {
        self.cell.inner.borrow()
    }

    fn inner_mut(&self) -> RefMut<'_, VecInner<T>> {
        self.cell.inner.borrow_mut()
    }

    /// Fan a batch of ops out to every live consumer, then wake graph observers. The
    /// single place structure notifies: op-stream transport + reactive wake.
    fn emit(&self, ops: Vec<SpliceOp>) {
        if ops.is_empty() {
            return;
        }
        {
            let mut inner = self.inner_mut();
            inner.consumers.retain(|w| w.strong_count() > 0);
            for weak in &inner.consumers {
                if let Some(queue) = weak.upgrade() {
                    queue.borrow_mut().extend(ops.iter().cloned());
                }
            }
        }
        if let Some(rt) = self.rt() {
            graph::source_changed(&rt, &self.node());
        }
    }

    /// Subscribe a fresh op-stream. Snapshot [`snapshot_order`](Self::snapshot_order)
    /// immediately after to seed a new consumer's initial mount.
    pub fn consume(&self) -> Consumer {
        let queue = Rc::new(RefCell::new(VecDeque::new()));
        self.inner_mut().consumers.push(Rc::downgrade(&queue));
        Consumer { queue }
    }

    /// Untracked snapshot of the current order.
    pub fn snapshot_order(&self) -> Vec<Row> {
        self.inner().order.clone()
    }

    /// Tracked read of the order — for a `computed`/effect that derives from the
    /// list's structure. Registers a dependency.
    pub fn rows(&self, cx: &Cx) -> Vec<Row> {
        graph::track(cx, &self.node());
        self.inner().order.clone()
    }

    /// Append a value; returns the new row handle.
    pub fn push(&self, _turn: impl crate::InTurn, value: T) -> Row {
        let row = next_row();
        let Some(rt) = self.rt() else { return row };
        let after = {
            let mut inner = self.inner_mut();
            let after = inner.order.last().copied();
            inner.order.push(row);
            inner.cells.insert(row, MutableSignal::new(&rt, value));
            after
        };
        self.emit(vec![SpliceOp::Insert { row, after }]);
        row
    }

    /// Insert a new row after `after` (or at the front if `None`).
    pub fn insert_after(&self, _turn: impl crate::InTurn, after: Option<Row>, value: T) -> Row {
        let row = next_row();
        let Some(rt) = self.rt() else { return row };
        {
            let mut inner = self.inner_mut();
            let pos = match after {
                None => 0,
                Some(anchor) => inner
                    .order
                    .iter()
                    .position(|r| *r == anchor)
                    .map(|i| i + 1)
                    .unwrap_or(inner.order.len()),
            };
            inner.order.insert(pos, row);
            inner.cells.insert(row, MutableSignal::new(&rt, value));
        }
        self.emit(vec![SpliceOp::Insert { row, after }]);
        row
    }

    /// Remove the row. Its cell is reclaimed once the last consumer reading it drops —
    /// a consumer may render the removed row until it drains the `Remove` op. Returns
    /// whether a row was actually removed.
    pub fn remove(&self, _turn: impl crate::InTurn, row: Row) -> bool {
        let removed = {
            let mut inner = self.inner_mut();
            inner.order.retain(|r| *r != row);
            inner.cells.remove(&row).is_some()
        };
        if removed {
            self.emit(vec![SpliceOp::Remove { row }]);
        }
        removed
    }

    /// In-place cell write — updates the row's value without re-instantiating its
    /// fragment. A value-graph write (per-row), not a structural op.
    pub fn set(&self, _turn: impl crate::InTurn, row: Row, value: T) {
        if let Some(cell) = self.inner().cells.get(&row) {
            cell.write(value);
        }
    }

    /// A reader for a row's cell. The producer keeps the writer; consumers read.
    pub fn get(&self, row: Row) -> Option<Signal<T>> {
        self.inner().cells.get(&row).map(|cell| cell.read())
    }

    /// Write a row's cell in place, only if the value changed (its own graph wake).
    fn write_row(&self, row: Row, value: T)
    where
        T: PartialEq,
    {
        let inner = self.inner();
        if let Some(cell) = inner.cells.get(&row) {
            if *cell.peek() != value {
                cell.write(value);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner().order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner().order.is_empty()
    }

    /// `cmp` runs against values this call owns, with no borrow of the list or of any
    /// row outstanding — so a comparator may read or write this very list without
    /// tripping over a lock it cannot see. (The snapshot is one clone per row against
    /// the sort's own n·log n comparisons.)
    pub fn sort_by<F>(&self, _turn: impl crate::InTurn, mut cmp: F)
    where
        F: FnMut(&T, &T) -> std::cmp::Ordering,
        T: Clone,
    {
        let mut indexed: Vec<(Row, T)> = {
            let inner = self.inner();
            inner.order.iter().map(|&row| (row, inner.cells[&row].peek().clone())).collect()
        };
        indexed.sort_by(|(_, a), (_, b)| cmp(a, b));
        let new_order: Vec<Row> = indexed.into_iter().map(|(row, _)| row).collect();
        // Same move contract as `sync_by_key`: rows on the LIS of old positions stand
        // still, the rest move after their (already-placed) new predecessor. An
        // already-sorted list is one full LIS — no ops, no wake.
        let ops: Vec<SpliceOp> = {
            let mut inner = self.inner_mut();
            if new_order == inner.order {
                return;
            }
            let old_pos: HashMap<Row, usize> =
                inner.order.iter().enumerate().map(|(i, &row)| (row, i)).collect();
            let old_pos_of: Vec<Option<usize>> =
                new_order.iter().map(|row| old_pos.get(row).copied()).collect();
            let keep = lis_mask(&old_pos_of);
            let ops = new_order
                .iter()
                .enumerate()
                .filter(|&(i, _)| !keep[i])
                .map(|(i, &row)| SpliceOp::Move {
                    row,
                    after: (i > 0).then(|| new_order[i - 1]),
                })
                .collect();
            inner.order = new_order;
            ops
        };
        self.emit(ops);
    }

    pub fn sort(&self, turn: impl crate::InTurn)
    where
        T: Clone + Ord,
    {
        self.sort_by(turn, |a, b| a.cmp(b));
    }

    /// Make the list equal to `values` **by position** — the unkeyed sibling of
    /// [`sync_by_key`](Self::sync_by_key), for a list whose order is a function of its
    /// contents rather than of who its rows are.
    ///
    /// Position is the identity: row *n* holds value *n*, always. A row whose value is
    /// unchanged is not written, so nothing reading it wakes; structure only ever moves
    /// at the tail, so every surviving row keeps the position a consumer already has it
    /// at. That is what a display list wants — slot *n* on a canvas is whatever the
    /// list's *n*th row says, and a still picture handed over again does nothing at all.
    pub fn sync(&self, _turn: impl crate::InTurn, values: Vec<T>)
    where
        T: PartialEq,
    {
        let Some(rt) = self.rt() else { return };
        let wanted = values.len();
        let overlap: Vec<Row> = {
            let inner = self.inner();
            inner.order.iter().take(wanted).copied().collect()
        };
        let mut values = values.into_iter();
        for (row, value) in overlap.iter().zip(values.by_ref()) {
            self.write_row(*row, value);
        }
        let ops = {
            let mut inner = self.inner_mut();
            let mut ops: Vec<SpliceOp> = Vec::new();
            for value in values {
                let row = next_row();
                let after = inner.order.last().copied();
                inner.order.push(row);
                inner.cells.insert(row, MutableSignal::new(&rt, value));
                ops.push(SpliceOp::Insert { row, after });
            }
            while inner.order.len() > wanted {
                let Some(row) = inner.order.pop() else { break };
                inner.cells.remove(&row);
                ops.push(SpliceOp::Remove { row });
            }
            ops
        };
        self.emit(ops);
    }

    /// Diff `new_values` against current rows by key — the list's **delta function**.
    /// Kept-by-identity, unchanged rows silent, removals/insertions never move
    /// neighbours, reorders emit the provable-minimum moves (the LIS of surviving old
    /// positions).
    ///
    /// **The identity is `(key, occurrence)`** — the nth duplicate of a key is its
    /// own identity, claimed in order on both sides. Unique keys (the whole intended
    /// domain) behave as plain keys; duplicate keys — server data this list didn't
    /// write — degrade to positional identity *within the duplicate group only*,
    /// deterministically and length-preservingly, and are logged in every build:
    /// a boundary case is designed, never a debug-only assert.
    pub fn sync_by_key<K: Eq + Hash>(
        &self,
        _turn: impl crate::InTurn,
        new_values: Vec<T>,
        key_fn: impl Fn(&T) -> K,
    ) where
        T: Clone + PartialEq,
    {
        self.sync_keyed(new_values, key_fn)
    }

    /// The un-witnessed door for the framework's own writer: [`KeyedVec::derived`]'s
    /// sync effect runs inside the reactive flush, where no turn exists — the exact
    /// precedent of [`MutableSignal::write`]. Crate-private on purpose.
    pub(crate) fn sync_keyed<K: Eq + Hash>(&self, new_values: Vec<T>, key_fn: impl Fn(&T) -> K)
    where
        T: Clone + PartialEq,
    {
        let Some(rt) = self.rt() else { return };
        let n = new_values.len();
        // Each key's target positions, in order — occurrence k of a key on the old
        // side claims the kth position on the new side.
        let mut target_pos: HashMap<K, std::collections::VecDeque<usize>> =
            HashMap::with_capacity(n);
        for (i, value) in new_values.iter().enumerate() {
            let positions = target_pos.entry(key_fn(value)).or_default();
            if positions.len() == 1 {
                eprintln!(
                    "sync_by_key: duplicate key (positions {} and {i}) — reconciling by \
                     (key, occurrence); rows in the duplicate group keep positional identity",
                    positions[0]
                );
            }
            positions.push_back(i);
        }

        let old_rows: Vec<Row> = self.inner().order.clone();
        // Keyed off values this call owns: `key_fn` is app code, so it runs with no
        // borrow of the list or of a row held.
        let old_keys: Vec<K> = {
            let old_values: Vec<T> = {
                let inner = self.inner();
                old_rows.iter().map(|row| inner.cells[row].peek().clone()).collect()
            };
            old_values.iter().map(&key_fn).collect()
        };
        let mut matched: Vec<Option<Row>> = vec![None; n];
        let mut old_pos_of: Vec<Option<usize>> = vec![None; n];
        let mut removed: Vec<Row> = Vec::new();
        for ((old_pos, row), key) in old_rows.iter().enumerate().zip(old_keys) {
            match target_pos.get_mut(&key).and_then(|positions| positions.pop_front()) {
                Some(i) => {
                    matched[i] = Some(*row);
                    old_pos_of[i] = Some(old_pos);
                }
                None => removed.push(*row),
            }
        }

        let keep = lis_mask(&old_pos_of);

        let mut ops: Vec<SpliceOp> = Vec::new();
        {
            let mut inner = self.inner_mut();
            for row in &removed {
                inner.cells.remove(row);
                ops.push(SpliceOp::Remove { row: *row });
            }
        }

        let mut prev: Option<Row> = None;
        let mut final_order: Vec<Row> = Vec::with_capacity(n);
        for (i, value) in new_values.into_iter().enumerate() {
            let placed = match matched[i] {
                Some(row) => {
                    // Per-row value write (its own graph wake), only if it changed.
                    self.write_row(row, value);
                    if !keep[i] {
                        ops.push(SpliceOp::Move { row, after: prev });
                    }
                    row
                }
                None => {
                    let row = next_row();
                    self.inner_mut()
                        .cells
                        .insert(row, MutableSignal::new(&rt, value));
                    ops.push(SpliceOp::Insert { row, after: prev });
                    row
                }
            };
            final_order.push(placed);
            prev = Some(placed);
        }

        self.inner_mut().order = final_order;
        self.emit(ops);
    }

    pub fn clear(&self, _turn: impl crate::InTurn) {
        let cleared = {
            let mut inner = self.inner_mut();
            if inner.order.is_empty() {
                return;
            }
            inner.order.clear();
            inner.cells.clear();
            true
        };
        if cleared {
            self.emit(vec![SpliceOp::Clear]);
        }
    }
}

impl<T: 'static> SignalVec<T> for MutableVec<T> {
    fn snapshot_order(&self) -> Vec<Row> {
        MutableVec::snapshot_order(self)
    }

    fn read(&self, row: Row) -> Option<Signal<T>> {
        self.get(row)
    }

    fn consume(&self) -> Consumer {
        MutableVec::consume(self)
    }

    fn observe(&self, cx: &Cx) {
        let _ = self.rows(cx);
    }

    fn len(&self) -> usize {
        MutableVec::len(self)
    }
}

// ── KeyedVec ──────────────────────────────────────────────────────────────────

/// A keyed view of a derived array: an effect re-syncs the list by key whenever the
/// source changes ([`MutableVec::sync_by_key`] — the delta boundary). Read-only: the
/// source is the single writer. Self-scoped — the list, its cells, and its re-sync
/// effect are held by an owner the `KeyedVec` carries, so they all reclaim when the
/// last `KeyedVec` handle drops.
pub struct KeyedVec<T: 'static, K: 'static> {
    inner: MutableVec<T>,
    key_fn: Rc<dyn Fn(&T) -> K>,
}

impl<T: 'static, K: 'static> Clone for KeyedVec<T, K> {
    fn clone(&self) -> Self {
        KeyedVec {
            inner: self.inner.clone(),
            key_fn: self.key_fn.clone(),
        }
    }
}

impl<T: Clone + PartialEq + 'static, K: Eq + Hash + 'static> KeyedVec<T, K> {
    /// Derive a keyed list from a tracked `source` (a fragment read, a filter over
    /// another list). An effect re-runs `source` — re-tracking its exact dependency
    /// set — and diffs the result into the derived list's op-stream at the
    /// [`sync_by_key`](MutableVec::sync_by_key) boundary. Read-only: the source is the
    /// single writer.
    /// The sync effect is rooted in `owner`, so it stops when `owner` disposes — the
    /// list may outlive it (something cached a handle) without the work outliving it.
    pub fn derived(
        owner: &Owner,
        source: impl Fn(&Cx) -> Vec<T> + 'static,
        key: impl Fn(&T) -> K + 'static,
    ) -> Self {
        let inner = MutableVec::new_in(owner);
        let sync = inner.clone();
        let key_fn: Rc<dyn Fn(&T) -> K> = Rc::new(key);
        let sync_key = key_fn.clone();
        owner.effect(move |cx| {
            sync.sync_keyed(source(cx), &*sync_key);
        });
        KeyedVec { inner, key_fn }
    }
}

impl<T: 'static, K: 'static> KeyedVec<T, K> {
    /// A row's key is fixed at insertion ([`sync_by_key`](MutableVec::sync_by_key)
    /// matches rows by key), so re-applying the key fn to the current value always
    /// yields the row's identity — what a keyed `@for` binds beside the live cell.
    pub(crate) fn key_fn(&self) -> Rc<dyn Fn(&T) -> K> {
        self.key_fn.clone()
    }
}

impl<T: 'static, K: 'static> SignalVec<T> for KeyedVec<T, K> {
    fn snapshot_order(&self) -> Vec<Row> {
        self.inner.snapshot_order()
    }

    fn read(&self, row: Row) -> Option<Signal<T>> {
        self.inner.get(row)
    }

    fn consume(&self) -> Consumer {
        self.inner.consume()
    }

    fn observe(&self, cx: &Cx) {
        let _ = self.inner.rows(cx);
    }

    fn len(&self) -> usize {
        self.inner.len()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-call write witness — tests are their own turn.
    fn t() -> crate::Turn<'static> {
        crate::Turn::for_test()
    }

    fn scoped() -> (crate::Runtime, Owner) {
        let rt = crate::Runtime::new();
        let owner = Owner::new(rt.core());
        (rt, owner)
    }

    fn seeded(values: &[i32]) -> (crate::Runtime, MutableVec<i32>, Consumer) {
        let (rt, owner) = scoped();
        let v: MutableVec<i32> = owner.mutable_vec();
        for &x in values {
            v.push(&t(), x);
        }
        let consumer = v.consume();
        (rt, v, consumer)
    }

    fn vals(v: &MutableVec<i32>) -> Vec<i32> {
        v.snapshot_order().iter().map(|&r| *v.get(r).unwrap().peek()).collect()
    }

    fn op_counts(ops: &[SpliceOp]) -> (usize, usize, usize) {
        let inserts = ops.iter().filter(|o| matches!(o, SpliceOp::Insert { .. })).count();
        let removes = ops.iter().filter(|o| matches!(o, SpliceOp::Remove { .. })).count();
        let moves = ops.iter().filter(|o| matches!(o, SpliceOp::Move { .. })).count();
        (inserts, removes, moves)
    }

    #[test]
    fn push_and_get() {
        let (_rt, owner) = scoped();
        let v: MutableVec<i32> = owner.mutable_vec();
        let r = v.push(&t(), 10);
        assert_eq!(*v.get(r).unwrap().peek(), 10);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn removed_row_readable_until_its_reader_drops() {
        let (_rt, owner) = scoped();
        let v: MutableVec<i32> = owner.mutable_vec();
        let r = v.push(&t(), 5);
        let reader = v.read(r).unwrap();
        assert!(v.remove(&t(), r));
        assert!(!v.remove(&t(), r), "a second remove is a no-op");
        assert_eq!(v.len(), 0);
        // Still readable through a held reader — a consumer may render it until it
        // drains the Remove op and drops the reader.
        assert_eq!(*reader.peek(), 5);
    }

    #[test]
    fn set_updates_cell_in_place() {
        let (_rt, owner) = scoped();
        let v: MutableVec<i32> = owner.mutable_vec();
        let r = v.push(&t(), 1);
        v.set(&t(), r, 42);
        assert_eq!(*v.get(r).unwrap().peek(), 42);
    }

    /// Positional sync keeps every surviving row where a consumer already has it: an
    /// unchanged value is not written at all, a changed one keeps its row, and structure
    /// only ever moves at the tail — which is what lets a consumer index by position.
    #[test]
    fn sync_keeps_position_and_splices_only_the_tail() {
        let (_rt, v, consumer) = seeded(&[1, 2, 3]);
        let rows = v.snapshot_order();

        v.sync(&t(), vec![1, 2, 3]);
        assert!(consumer.drain().is_empty(), "the same array is no ops at all");
        assert_eq!(v.snapshot_order(), rows);

        v.sync(&t(), vec![1, 9, 3]);
        assert!(consumer.drain().is_empty(), "a changed value is a cell write, not a splice");
        assert_eq!(vals(&v), vec![1, 9, 3]);
        assert_eq!(v.snapshot_order(), rows, "and the rows stand where they stood");

        v.sync(&t(), vec![1, 9, 3, 4, 5]);
        assert_eq!(op_counts(&consumer.drain()), (2, 0, 0), "growing appends");
        assert_eq!(v.snapshot_order()[..3], rows[..], "the rows it had are untouched");

        v.sync(&t(), vec![1, 9]);
        assert_eq!(op_counts(&consumer.drain()), (0, 3, 0), "shrinking drops the tail");
        assert_eq!(vals(&v), vec![1, 9]);

        v.sync(&t(), Vec::new());
        assert!(v.is_empty());
    }

    #[test]
    fn sort_reorders() {
        let (_rt, owner) = scoped();
        let v: MutableVec<i32> = owner.mutable_vec();
        v.push(&t(), 3);
        v.push(&t(), 1);
        v.push(&t(), 2);
        v.sort_by(&t(), |a, b| a.cmp(b));
        let vals: Vec<i32> = v.snapshot_order().iter().map(|&r| *v.get(r).unwrap().peek()).collect();
        assert_eq!(vals, vec![1, 2, 3]);
    }

    #[test]
    fn sort_bump_to_front_moves_one_row_not_the_rest() {
        let (_rt, v, c) = seeded(&[2, 3, 4, 1]);
        let r1 = v.snapshot_order()[3];
        v.sort(&t());
        assert_eq!(vals(&v), vec![1, 2, 3, 4]);
        let ops = c.drain();
        assert_eq!(op_counts(&ops), (0, 0, 1));
        assert!(
            matches!(ops[0], SpliceOp::Move { row, after: None } if row == r1),
            "the one move is 1 to the front: {ops:?}"
        );
    }

    #[test]
    fn sort_of_a_sorted_list_emits_nothing() {
        let (_rt, v, c) = seeded(&[1, 2, 3]);
        v.sort(&t());
        assert_eq!(op_counts(&c.drain()), (0, 0, 0));
    }

    #[test]
    fn sort_reverse_moves_n_minus_one() {
        let (_rt, v, c) = seeded(&[4, 3, 2, 1]);
        v.sort(&t());
        assert_eq!(vals(&v), vec![1, 2, 3, 4]);
        assert_eq!(op_counts(&c.drain()), (0, 0, 3));
    }

    #[test]
    fn sync_removal_never_moves_the_survivors() {
        let (_rt, v, c) = seeded(&[1, 2, 3]);
        let survivors = &v.snapshot_order()[1..];
        let (r2, r3) = (survivors[0], survivors[1]);
        v.sync_by_key(&t(), vec![2, 3], |x| *x);
        assert_eq!(vals(&v), vec![2, 3]);
        assert_eq!(v.snapshot_order(), vec![r2, r3], "survivors keep their rows");
        assert_eq!(op_counts(&c.drain()), (0, 1, 0), "one removal, zero moves");
    }

    #[test]
    fn sync_insert_in_the_middle_never_moves_the_neighbours() {
        let (_rt, v, c) = seeded(&[1, 3]);
        v.sync_by_key(&t(), vec![1, 2, 3], |x| *x);
        assert_eq!(vals(&v), vec![1, 2, 3]);
        assert_eq!(op_counts(&c.drain()), (1, 0, 0), "one insert, zero moves");
    }

    #[test]
    fn sync_bump_to_front_moves_one_row_not_the_rest() {
        let (_rt, v, c) = seeded(&[1, 2, 3, 4]);
        let r4 = v.snapshot_order()[3];
        v.sync_by_key(&t(), vec![4, 1, 2, 3], |x| *x);
        assert_eq!(vals(&v), vec![4, 1, 2, 3]);
        let ops = c.drain();
        assert_eq!(op_counts(&ops), (0, 0, 1));
        assert!(
            matches!(ops[0], SpliceOp::Move { row, after: None } if row == r4),
            "the one move is 4 to the front: {ops:?}"
        );
    }

    #[test]
    fn sync_reverse_moves_n_minus_one() {
        let (_rt, v, c) = seeded(&[1, 2, 3, 4]);
        v.sync_by_key(&t(), vec![4, 3, 2, 1], |x| *x);
        assert_eq!(vals(&v), vec![4, 3, 2, 1]);
        assert_eq!(op_counts(&c.drain()), (0, 0, 3));
    }

    #[test]
    fn sync_mixed_splice_emits_the_minimum_op_set() {
        let (_rt, v, c) = seeded(&[1, 2, 3, 4, 5]);
        v.sync_by_key(&t(), vec![5, 2, 6, 4], |x| *x);
        assert_eq!(vals(&v), vec![5, 2, 6, 4]);
        assert_eq!(op_counts(&c.drain()), (1, 2, 1));
    }

    /// Duplicate keys are server data, not UB: identity is `(key, occurrence)` —
    /// length-preserving, deterministic, rows stable within the duplicate group.
    #[test]
    fn duplicate_keys_reconcile_by_occurrence() {
        let (_rt, owner) = scoped();
        let v: MutableVec<(i32, &'static str)> = owner.mutable_vec();
        let c = v.consume();
        v.sync_by_key(&t(), vec![(7, "first"), (8, "only"), (7, "second")], |item| item.0);
        assert_eq!(v.snapshot_order().len(), 3, "duplicates never collapse the list");
        let rows = v.snapshot_order();
        c.drain();

        // Re-sync with the same shape: occurrences match in order — zero structural ops.
        v.sync_by_key(&t(), vec![(7, "first"), (8, "only"), (7, "second")], |item| item.0);
        assert_eq!(v.snapshot_order(), rows, "occurrence identity is stable");
        assert!(c.drain().is_empty(), "an unchanged duplicate group is silent");

        // Dropping one duplicate removes exactly one row; the survivor keeps identity
        // as occurrence 0.
        v.sync_by_key(&t(), vec![(7, "first"), (8, "only")], |item| item.0);
        assert_eq!(v.snapshot_order(), &rows[..2]);
        assert_eq!(op_counts(&c.drain()), (0, 1, 0), "one removal, nothing else");
    }

    #[test]
    fn two_consumers_each_see_every_op() {
        let (_rt, owner) = scoped();
        let v: MutableVec<i32> = owner.mutable_vec();
        let a = v.consume();
        let b = v.consume();
        v.push(&t(), 1);
        v.push(&t(), 2);
        assert_eq!(op_counts(&a.drain()), (2, 0, 0));
        assert_eq!(op_counts(&b.drain()), (2, 0, 0), "second consumer is independent");
    }

    #[test]
    fn sync_identical_values_touch_nothing() {
        let (_rt, owner) = scoped();
        let v: MutableVec<(u32, &'static str)> = owner.mutable_vec();
        for id in 1..=3 {
            v.push(&t(), (id, "a"));
        }
        let consumer = v.consume();

        v.sync_by_key(&t(), vec![(1, "a"), (2, "a"), (3, "a")], |item| item.0);
        assert!(consumer.drain().is_empty(), "unchanged sync is fully silent");

        v.sync_by_key(&t(), vec![(1, "a"), (2, "b"), (3, "a")], |item| item.0);
        assert!(consumer.drain().is_empty(), "a value change is not a structural op");
        assert_eq!(*v.get(v.snapshot_order()[1]).unwrap().peek(), (2, "b"));
    }
}

#[cfg(test)]
mod callback_reentrancy {
    use super::*;
    use crate::Owner;

    fn t() -> crate::Turn<'static> {
        crate::Turn::for_test()
    }

    fn scoped() -> (crate::Runtime, Owner) {
        let rt = crate::Runtime::new();
        let owner = Owner::new(rt.core());
        (rt, owner)
    }

    /// `sort_by` and `sync_by_key` take app closures. Neither may run while the list
    /// holds a borrow of itself: a comparator or key function that touches the same
    /// list is odd, but it is app code, and app code must not be able to crash.
    #[test]
    fn list_callbacks_run_with_no_borrow_held() {
        let (_rt, owner) = scoped();
        let v: MutableVec<i32> = owner.mutable_vec();
        for x in [3, 1, 2] {
            v.push(&t(), x);
        }

        let probe = v.clone();
        v.sort_by(&t(), |a, b| {
            probe.len();
            probe.snapshot_order();
            a.cmp(b)
        });
        assert_eq!(
            v.snapshot_order().iter().map(|&r| *v.get(r).unwrap().peek()).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        let probe = v.clone();
        v.sync_by_key(&t(), vec![9, 1, 2], move |value| {
            probe.len();
            *value
        });
        assert_eq!(
            v.snapshot_order().iter().map(|&r| *v.get(r).unwrap().peek()).collect::<Vec<_>>(),
            vec![9, 1, 2]
        );
    }
}

