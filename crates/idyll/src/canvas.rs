//! The canvas surface: what a `canvas painting=(layers) {}` binding carries.
//!
//! The app says what the picture is — curves, the stretches of them that are lit, their
//! colours and widths, in the element's own pixels — and the framework puts it on a
//! canvas. No element handle and no drawing context reaches app code; a picture is a
//! reactive list, and the binding follows it like any other.
//!
//! **A picture is a list of layers, and a layer is a list of shapes** — a
//! [`SignalVec`] of [`SignalVec`]s, composited in row order with row 0 underneath. That
//! nesting is what makes the surface cheap: a shape is a row, so moving a pulse writes
//! one cell and wakes one effect, and a layer nobody writes is never visited at all. A
//! crowd card's thousand standing wires cost nothing per frame because nothing touches
//! them; the runtime keeps that layer's strokes on a bitmap of its own and blits it
//! under the traffic.
//!
//! What crosses the membrane is batched back up: the per-shape effects mark the slots
//! that moved, and one command per canvas per flush carries every changed layer's
//! changed slots. Reactivity is per shape; the wire is per frame.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use indexmap::IndexSet;

use crate::owner::Owner;
use crate::runtime::RuntimeCore;
use crate::signal::vec::{Consumer, Row, SignalVec};
use crate::signal::{Cx, Signal};

/// A cubic bezier in the canvas element's own CSS pixels — the geometry every [`Shape`]
/// strokes a stretch of.
#[derive(Clone, Copy, PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Curve {
    pub from: (f64, f64),
    pub c1: (f64, f64),
    pub c2: (f64, f64),
    pub to: (f64, f64),
}

/// One stroked stretch of a curve — a layer's only entry, because a wire and the
/// traffic riding it are the same picture at two lengths: a wire is the whole curve at
/// one opacity; a pulse is the stretch between its two edges, its opacity ramping to
/// the hard cut at the head.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Shape {
    pub curve: Curve,
    /// The stretch to stroke, in the curve's own parameter — `0.0` at the curve's
    /// `from`, `1.0` at its `to`.
    pub span: (f64, f64),
    /// Any CSS colour, resolved in the canvas element's own cascade — so a `var(…)`
    /// naming a design token paints the picture and the elements around it alike. The
    /// same vocabulary a `style:prop=(…)` value is written in.
    pub ink: String,
    /// Stroke width in CSS pixels.
    pub width: f64,
    /// Opacity at the two ends of [`span`](Shape::span), in that order. Equal is a flat
    /// stroke; unequal fades along the stretch.
    pub alpha: (f64, f64),
}

/// The floats one [`Shape`] occupies in a layer's flat run, after the slot it belongs
/// to.
pub(crate) const STRIDE: usize = 14;

/// What one layer has to say this flush: the slots whose shapes changed, and how many
/// slots the layer now has (so the far side can drop a tail it still holds).
#[derive(Clone, PartialEq, Debug)]
pub struct LayerDelta {
    pub layer: u32,
    pub slots: Vec<(u32, Shape)>,
    pub len: u32,
}

// ── The binding's source ────────────────────────────────────────────────────────────
//
// `@for` reads a live list as *structure* (order snapshot, op-stream, tracking) plus a
// typed row read. A canvas reads two of those, nested — and since the item type at the
// bottom is fixed, both halves fit in plain traits with no type parameter to carry.

/// One layer, as the runtime consumes it: its shapes' structure and their cells.
pub trait Shapes: 'static {
    fn snapshot_order(&self) -> Vec<Row>;
    fn consume(&self) -> Consumer;
    fn observe(&self, cx: &Cx);
    fn read(&self, row: Row) -> Option<Signal<Shape>>;
}

/// A picture, as the runtime consumes it: the layers' structure, and each layer.
pub trait Layers: 'static {
    fn snapshot_order(&self) -> Vec<Row>;
    fn consume(&self) -> Consumer;
    fn observe(&self, cx: &Cx);
    fn layer(&self, row: Row) -> Option<Rc<dyn Shapes>>;
}

/// A [`SignalVec`] viewed as one of the two halves (the item type pinned so the impls
/// are coherent — the same shape `@for`'s `Structure` takes).
struct Source<V, T> {
    source: V,
    _item: std::marker::PhantomData<fn() -> T>,
}

impl<V: SignalVec<Shape>> Shapes for Source<V, Shape> {
    fn snapshot_order(&self) -> Vec<Row> {
        self.source.snapshot_order()
    }
    fn consume(&self) -> Consumer {
        self.source.consume()
    }
    fn observe(&self, cx: &Cx) {
        self.source.observe(cx);
    }
    fn read(&self, row: Row) -> Option<Signal<Shape>> {
        self.source.read(row)
    }
}

impl<V: SignalVec<L>, L: SignalVec<Shape>> Layers for Source<V, L> {
    fn snapshot_order(&self) -> Vec<Row> {
        self.source.snapshot_order()
    }
    fn consume(&self) -> Consumer {
        self.source.consume()
    }
    fn observe(&self, cx: &Cx) {
        self.source.observe(cx);
    }
    fn layer(&self, row: Row) -> Option<Rc<dyn Shapes>> {
        let cell = self.source.read(row)?;
        let list = cell.peek().clone();
        Some(Rc::new(Source::<L, Shape> { source: list, _item: std::marker::PhantomData }))
    }
}

/// What `painting=(…)` hands the runtime: a list of layers, each a list of shapes.
/// Written by the `live_view!` expansion, so the app never names it.
pub fn painting<V, L>(source: V) -> Rc<dyn Layers>
where
    V: SignalVec<L>,
    L: SignalVec<Shape>,
{
    Rc::new(Source::<V, L> { source, _item: std::marker::PhantomData })
}

// ── The flat wire form ──────────────────────────────────────────────────────────────

/// The frame's distinct inks, and each changed layer's slots flattened into one run of
/// [`STRIDE`] floats indexing them. The layout, and why it is flat rather than a list
/// of records, are stated once — on `paint-cmd` in `idyll-host/wit/ssr.wit`, the
/// contract `runtime.js` reads it back by.
pub(crate) fn flatten(deltas: &[LayerDelta]) -> (Vec<String>, Vec<(u32, Vec<f32>, u32)>) {
    let mut inks: Vec<String> = Vec::new();
    let mut runs = Vec::with_capacity(deltas.len());
    for delta in deltas {
        let mut run = Vec::with_capacity(delta.slots.len() * (STRIDE + 1));
        for (slot, shape) in &delta.slots {
            run.push(*slot as f32);
            let ink = match inks.iter().position(|known| *known == shape.ink) {
                Some(known) => known,
                None => {
                    inks.push(shape.ink.clone());
                    inks.len() - 1
                }
            };
            let Curve { from, c1, c2, to } = shape.curve;
            for value in [
                from.0,
                from.1,
                c1.0,
                c1.1,
                c2.0,
                c2.1,
                to.0,
                to.1,
                shape.span.0,
                shape.span.1,
                shape.width,
                shape.alpha.0,
                shape.alpha.1,
                ink as f64,
            ] {
                run.push(value as f32);
            }
        }
        runs.push((delta.layer, run, delta.len));
    }
    (inks, runs)
}

// ── The binding ─────────────────────────────────────────────────────────────────────

/// One layer as the binding follows it: where its shapes' cells are, what order they
/// stand in, which slots have moved since the last command, and one effect per shape.
struct Layer {
    shapes: Rc<dyn Shapes>,
    consumer: Consumer,
    /// The layer's shapes in composite order — the slot a shape occupies *is* its index
    /// here. A set rather than a `Vec` because it is read both ways: the command asks
    /// what stands at a slot, and a shape's effect asks which slot it stands at, every
    /// time it runs. A layer of a thousand wires re-laid by a reflow runs all thousand,
    /// so the second direction has to be a lookup rather than a scan.
    order: IndexSet<Row>,
    moved: BTreeSet<u32>,
    /// Whether `order` has been read off the list yet: a consumer subscribes before it
    /// snapshots, so a layer's rows arrive by snapshot the first time and by op-stream
    /// forever after.
    seeded: bool,
    /// The length the far side was last told. `None` until it has been told anything,
    /// which is what makes a layer's first command carry the whole of it.
    sent: Option<u32>,
    /// Each shape's effect, held by its row: dropping the guard is what stops the
    /// effect, so a shape leaving the list takes its effect with it.
    effects: HashMap<Row, Rc<dyn std::any::Any>>,
}

/// A canvas's picture as the binding follows it. Held by the view's owner, so unmounting
/// drops it — and with it every shape's effect and every cell it was reading.
struct Painting {
    node_id: crate::driver::NodeId,
    layers: Vec<Row>,
    of: HashMap<Row, Layer>,
    /// Whether the layer rows have been read off the list yet (see [`Layer::seeded`]).
    seeded: bool,
    /// How many layers the far side was last told about.
    sent: Option<u32>,
    /// Whether a command is already queued for this flush — the per-shape effects mark
    /// what moved, and exactly one command carries all of it.
    queued: bool,
}

impl Painting {
    /// The one command this flush owes: every layer with slots that moved, plus the
    /// layer count whenever it has changed. Nothing to say is no command at all.
    fn command(&mut self) -> Option<crate::driver::DomOp> {
        let mut deltas = Vec::new();
        for (index, row) in self.layers.iter().enumerate() {
            let Some(layer) = self.of.get_mut(row) else { continue };
            let len = layer.order.len() as u32;
            if layer.moved.is_empty() && layer.sent == Some(len) {
                continue;
            }
            let slots = layer
                .moved
                .iter()
                .filter_map(|&slot| {
                    let row = *layer.order.get_index(slot as usize)?;
                    Some((slot, layer.shapes.read(row)?.peek().clone()))
                })
                .collect();
            layer.moved.clear();
            layer.sent = Some(len);
            deltas.push(LayerDelta { layer: index as u32, slots, len });
        }
        let layers = self.layers.len() as u32;
        if deltas.is_empty() && self.sent == Some(layers) {
            return None;
        }
        self.sent = Some(layers);
        Some(crate::driver::DomOp::Paint { node_id: self.node_id, layers, deltas })
    }
}

/// Queue the one command this flush owes, if nothing has queued it yet. It runs in the
/// deferred pass — after the flush's effects, before its ops are applied — which is what
/// turns a frame of per-shape marks into a frame of one command.
fn queue(core: &Rc<RuntimeCore>, state: &Rc<RefCell<Painting>>) {
    if std::mem::replace(&mut state.borrow_mut().queued, true) {
        return;
    }
    let state = Rc::downgrade(state);
    core.defer(Box::new(move |_runtime, driver| {
        let Some(state) = state.upgrade() else { return Vec::new() };
        let command = {
            let mut painting = state.borrow_mut();
            painting.queued = false;
            painting.command()
        };
        if let Some(command) = command {
            driver.apply(vec![command]);
        }
        Vec::new()
    }));
}

/// Follow one shape: reading its cell is what subscribes, and a write marks the slot it
/// stands at. The slot is looked up rather than captured because a shape's position is
/// its layer's to say, and a splice can move it.
fn follow_shape(
    core: &Rc<RuntimeCore>,
    state: &Rc<RefCell<Painting>>,
    layer: Row,
    shape: Row,
) -> Option<Rc<dyn std::any::Any>> {
    let cell = state.borrow().of.get(&layer)?.shapes.read(shape)?;
    let marked = Rc::downgrade(state);
    let mark_core = Rc::downgrade(core);
    Some(crate::signal::reaction::Reaction::spawn_guarded(core, move |cx| {
        let _ = cell.get(cx);
        let (Some(state), Some(core)) = (marked.upgrade(), mark_core.upgrade()) else { return };
        {
            let mut painting = state.borrow_mut();
            let Some(entry) = painting.of.get_mut(&layer) else { return };
            let Some(slot) = entry.order.get_index_of(&shape) else { return };
            entry.moved.insert(slot as u32);
        }
        queue(&core, &state);
    }))
}

/// Bring one layer's rows level with its list: new shapes get an effect (which paints
/// them by running), departed shapes lose theirs, and any shape whose slot moved is
/// marked — the far side keeps its picture by slot, so a shape that changed position
/// changed what that slot holds.
fn resync_layer(core: &Rc<RuntimeCore>, state: &Rc<RefCell<Painting>>, layer: Row, cx: &Cx) {
    let (shapes, fresh, was) = {
        let painting = state.borrow();
        let Some(entry) = painting.of.get(&layer) else { return };
        entry.shapes.observe(cx);
        let spliced = !entry.consumer.drain().is_empty();
        if !spliced && entry.seeded {
            return;
        }
        let fresh: IndexSet<Row> = entry.shapes.snapshot_order().into_iter().collect();
        (Rc::clone(&entry.shapes), fresh, entry.order.clone())
    };
    let arrived: Vec<Row> = {
        let mut painting = state.borrow_mut();
        let Some(entry) = painting.of.get_mut(&layer) else { return };
        entry.shapes = shapes;
        entry.seeded = true;
        entry.effects.retain(|row, _| fresh.contains(row));
        // A shape standing where a different one stood is a slot the far side has
        // something else in. A shape that has just arrived marks its own slot by
        // running, so it is not marked here.
        for (slot, row) in fresh.iter().enumerate() {
            if was.get_index(slot) != Some(row) && entry.effects.contains_key(row) {
                entry.moved.insert(slot as u32);
            }
        }
        let arrived = fresh.iter().filter(|row| !entry.effects.contains_key(row)).copied().collect();
        entry.order = fresh;
        arrived
    };
    for row in arrived {
        if let Some(effect) = follow_shape(core, state, layer, row) {
            if let Some(entry) = state.borrow_mut().of.get_mut(&layer) {
                entry.effects.insert(row, effect);
            }
        }
    }
    queue(core, state);
}

/// Install a `painting=(…)` binding on `node_id`. One effect follows the picture's
/// structure — the layers and, through them, each layer's shapes — and spawns the
/// per-shape effects that follow content.
pub(crate) fn install(
    owner: &Owner,
    core: &Rc<RuntimeCore>,
    node_id: crate::driver::NodeId,
    layers: Rc<dyn Layers>,
) {
    let state = Rc::new(RefCell::new(Painting {
        node_id,
        layers: Vec::new(),
        of: HashMap::new(),
        seeded: false,
        sent: None,
        queued: false,
    }));
    owner.retain(Rc::clone(&state) as Rc<dyn std::any::Any>);

    let consumer = layers.consume();
    let effect_core = Rc::downgrade(core);
    let structure = Rc::clone(&state);
    crate::signal::reaction::Reaction::spawn_in(owner, move |cx| {
        let Some(core) = effect_core.upgrade() else { return };
        layers.observe(cx);
        let spliced = !consumer.drain().is_empty();
        // A layer's own splices reach this effect through the tracking `resync_layer`
        // does, so it runs on those too — the outer list holding still says nothing
        // about the lists inside it.
        if spliced || !structure.borrow().seeded {
            let fresh = layers.snapshot_order();
            let standing: std::collections::HashSet<Row> = fresh.iter().copied().collect();
            let mut painting = structure.borrow_mut();
            painting.of.retain(|row, _| standing.contains(row));
            painting.layers = fresh.clone();
            painting.seeded = true;
            for row in fresh {
                if painting.of.contains_key(&row) {
                    continue;
                }
                let Some(shapes) = layers.layer(row) else { continue };
                let consumer = shapes.consume();
                painting.of.insert(
                    row,
                    Layer {
                        shapes,
                        consumer,
                        order: IndexSet::new(),
                        moved: BTreeSet::new(),
                        seeded: false,
                        sent: None,
                        effects: HashMap::new(),
                    },
                );
            }
        }
        let rows = structure.borrow().layers.clone();
        for row in rows {
            resync_layer(&core, &structure, row, cx);
        }
        queue(&core, &structure);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(ink: &str) -> Shape {
        Shape {
            curve: Curve { from: (1.0, 2.0), c1: (3.0, 4.0), c2: (5.0, 6.0), to: (7.0, 8.0) },
            span: (0.25, 0.75),
            ink: ink.to_string(),
            width: 2.5,
            alpha: (0.0, 0.9),
        }
    }

    /// The run is the documented layout behind each slot, and a colour used twice is
    /// carried once — across the whole command, not per layer.
    #[test]
    fn a_layer_flattens_to_slot_then_stride() {
        let deltas = vec![
            LayerDelta {
                layer: 0,
                slots: vec![(0, shape("var(--teal)")), (1, shape("var(--amber)"))],
                len: 2,
            },
            LayerDelta { layer: 1, slots: vec![(3, shape("var(--teal)"))], len: 4 },
        ];
        let (inks, runs) = flatten(&deltas);

        assert_eq!(inks, ["var(--teal)", "var(--amber)"]);
        assert_eq!(runs[0].1.len(), 2 * (STRIDE + 1));
        assert_eq!(
            runs[0].1[..STRIDE + 1],
            [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.25, 0.75, 2.5, 0.0, 0.9, 0.0]
        );
        assert_eq!(runs[0].1[STRIDE + 1], 1.0, "the second shape names its own slot");
        assert_eq!(runs[1], (1, runs[1].1.clone(), 4));
        assert_eq!(
            runs[1].1[STRIDE],
            0.0,
            "the second layer's shape reuses the first layer's ink",
        );
    }
}
