//! [`Reaction`] — an effect: a computation run for its side effects, not its value.
//! Reactions are the **pull roots** of the graph. A view binding (patch a text node,
//! set an attribute) and a control-flow fragment (`@if`/`@match` swap) are Reactions;
//! so is any `ctx.effect`.
//!
//! On creation a Reaction runs once (establishing its dependencies and its initial
//! effect — the mount paint). Thereafter a write to any dependency marks it stale and
//! schedules a flush; the flush re-runs it, pulling the values it reads current on
//! demand. Because it re-tracks every run, its dependency set follows exactly what
//! the body reads this turn.
//!
//! A Reaction is retained by an [`Owner`](crate::Owner) and disposes when that scope
//! does. An effect whose lifetime is finer than its owner's (a `ctx.listen`
//! subscription) is instead held by a guard — dropping the guard drops the effect.

use std::rc::{Rc, Weak};

use super::graph::{self, Lane, NodeCore};
use super::{Cx, SignalCell};
use crate::owner::Owner;
use crate::runtime::RuntimeCore;

/// A live effect. `Clone`; a `Weak` handle onto its cell — the effect is kept alive
/// by its owner's retention (or a guard), not by this handle, which only names it.
pub struct Reaction {
    _cell: Weak<SignalCell<()>>,
}

impl Clone for Reaction {
    fn clone(&self) -> Self {
        Reaction {
            _cell: self._cell.clone(),
        }
    }
}

/// Build an effect cell over `f` and run it once to establish its edges + initial
/// effect.
fn build_effect<F>(rt: &Rc<RuntimeCore>, lane: Lane, f: F) -> Rc<SignalCell<()>>
where
    F: Fn(&Cx) + 'static,
{
    let recompute: Rc<dyn Fn(&Cx) -> bool> = Rc::new(move |cx: &Cx| {
        f(cx);
        // No observers depend on an effect, so "changed" is immaterial.
        false
    });
    let cell =
        Rc::new(SignalCell::build(rt, (), Rc::new(NodeCore::derived(recompute, Some(lane)))));
    graph::init_derived(&cell.node());
    cell
}

impl Reaction {
    /// Spawn an urgent ([`Lane::Input`]) effect retained by `owner`. Runs `f` once
    /// now, then re-runs it whenever a dependency it read changes.
    pub(crate) fn spawn_in<F>(owner: &Owner, f: F) -> Self
    where
        F: Fn(&Cx) + 'static,
    {
        Self::spawn_in_lane(owner, Lane::Input, f)
    }

    /// Spawn an effect in a chosen scheduling [`Lane`], retained by `owner`.
    pub(crate) fn spawn_in_lane<F>(owner: &Owner, lane: Lane, f: F) -> Self
    where
        F: Fn(&Cx) + 'static,
    {
        // A disposed scope declines retention (`Owner::retain`), so building first
        // would run the effect's initial paint once from a dead scope and then drop
        // it silently — check before the first run, not after.
        if owner.is_disposed() {
            return Reaction { _cell: std::rc::Weak::new() };
        }
        let cell = build_effect(&owner.runtime(), lane, f);
        let handle = Reaction {
            _cell: Rc::downgrade(&cell),
        };
        owner.retain(cell);
        handle
    }

    /// Spawn an effect whose lifetime is the returned guard's: the guard holds the
    /// only strong reference, so dropping it stops the effect. For subscriptions
    /// finer-grained than an owner scope (`ctx.listen`).
    pub(crate) fn spawn_guarded<F>(rt: &Rc<RuntimeCore>, f: F) -> Rc<dyn std::any::Any>
    where
        F: Fn(&Cx) + 'static,
    {
        build_effect(rt, Lane::Input, f) as Rc<dyn std::any::Any>
    }
}
