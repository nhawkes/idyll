//! The live store: the page's replayed cache plus the page key, built from the
//! executed route query and rooted in the live that owns it. App code never
//! touches owners or wire bytes — [`Store::provide`] and [`Store::of`] take the
//! live's `Ctx` and thread authority internally.


use idyll::{Ctx, Setup};
use serde::de::DeserializeOwned;

use crate::error::{AbsorbError, CommitError, StoreError};
use crate::route::RouteRoots;
use crate::{Cache, Frag, NodeFragment, Preloaded};

#[derive_where::derive_where(Clone)]
pub struct Store<F: NodeFragment + 'static> {
    pub cache: Cache,
    /// The page this store was **seeded** with — a mount-time identity. Right for
    /// every live a document mounts (their mounts never outlive their page). An
    /// SPA live, whose mount persists across navigations, follows
    /// [`current_page`](Self::current_page) instead.
    pub page: Frag<F>,
    /// The page the app is showing **now** — app state the absorb maintains: every
    /// envelope's roots name the page that answered, and a navigation's answer is a
    /// different page.
    current: std::rc::Rc<idyll::MutableSignal<Frag<F>>>,
}

impl<F: NodeFragment + 'static> Store<F> {
    fn build<M: 'static, R: RouteRoots<Page = F>>(
        ctx: &Ctx<Setup, M>,
        seed: &Preloaded<R>,
    ) -> Result<Self, CommitError> {
        Ok(Store {
            cache: seed.seed.to_cache(ctx.owner())?,
            page: seed.roots.page(),
            current: std::rc::Rc::new(ctx.mutable_signal(seed.roots.page())),
        })
    }

    /// The page the app is showing now, as a signal — the SPA live's route source:
    /// a navigation's response names a new page, the absorb moves this, and a
    /// projection over it re-resolves. Under a document-mounted live it simply
    /// never moves.
    pub fn current_page(&self) -> idyll::Signal<Frag<F>> {
        self.current.read()
    }

    /// The store-root's constructor: build the store rooted in this live, register
    /// the absorb (an envelope's re-executed route query — a mutation refresh or a
    /// navigation response — applied **in this live's turn** so it lands in this
    /// live's message log), and provide both to descendant live.
    pub fn provide<M: 'static, R>(
        ctx: &Ctx<Setup, M>,
        seed: &Preloaded<R>,
    ) -> Result<Self, StoreError>
    where
        R: RouteRoots<Page = F> + DeserializeOwned + 'static,
    {
        let store = Self::build(ctx, seed)?;
        let cache = store.cache.clone();
        let current = std::rc::Rc::clone(&store.current);
        let sink = ctx.absorber(move |turn: &idyll::Turn, bytes: &[u8]| -> Result<(), AbsorbError> {
            let refresh: Preloaded<R> =
                serde_json::from_slice(bytes).map_err(AbsorbError::Decode)?;
            // Records first, then the pointer: a projection following `current`
            // resolves against a cache that already holds the page.
            cache.replay(turn, &refresh.seed).map_err(AbsorbError::Replay)?;
            let page = refresh.roots.page();
            if current.now(turn) != page {
                current.update(turn, |p| *p = page);
            }
            Ok(())
        });
        ctx.provide(sink);
        ctx.provide(store.clone());
        Ok(store)
    }

    /// A descendant live's view of the store: the provided one when mounted inside
    /// a store-root. When no provider exists, building one is a **mint**, and mints
    /// need a warrant: **owner position** — this mount is the root of its context
    /// tree (the server's isolated per-mount paint, or the browser's page root),
    /// where root-scoped state legitimately originates. A *child* mount with no
    /// provider is a mis-nesting: the live would render one perfect frame and then
    /// never see another absorb — refused as a component error for the boundary,
    /// never a silently-stale live.
    pub fn of<M: 'static, R: RouteRoots<Page = F>>(
        ctx: &Ctx<Setup, M>,
        seed: &Preloaded<R>,
    ) -> Result<Self, StoreError> {
        match ctx.use_context::<Self>() {
            Some(store) => Ok((*store).clone()),
            None if ctx.is_root_mount() => Ok(Self::build(ctx, seed)?),
            None => Err(NoStoreRoot { page: std::any::type_name::<F>() }.into()),
        }
    }
}

/// A live read the store with no store-root above it and no warrant to own one.
#[derive(Debug)]
pub struct NoStoreRoot {
    page: &'static str,
}

impl std::fmt::Display for NoStoreRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no store-root above this live provides Store<{}> — mount it inside its \
             store-root, or make this component the owner with Store::provide",
            self.page
        )
    }
}

impl std::error::Error for NoStoreRoot {}
