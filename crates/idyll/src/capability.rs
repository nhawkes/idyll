//! Client-only capability tokens (design §E).
//!
//! [`Client`] is an unforgeable, zero-sized proof that code is running in a
//! **live, client-side context** (post-hydrate). It is mintable only from
//! [`Ctx<Live>`](crate::Ctx) — whose client effects fire only on client mounts (the
//! SSR host states `client: false` across the membrane), after
//! `render()` — and is handed to event handlers and the `client_effect` hook.
//! It is *never* available during the setup/render phase that produces SSR HTML,
//! so client-only effects (DOM, `localStorage`, history) cannot run during
//! render and desynchronise hydration.
//!
//! Concrete browser capabilities (storage, history, an `eval` escape hatch) will
//! hang off this token as WIT host imports when they're needed — the app is pure
//! WIT+WASI, so there is no web-sys surface here. This module owns the token
//! itself so it is available (and type-checkable) on every target.

/// Zero-sized proof of a live, client-side context.
///
/// Obtain one from [`Ctx::<Live>::client`](crate::Ctx::client) or as the
/// argument to a `client_effect`/streaming event handler. The unit field is
/// private, so a `Client` cannot be constructed outside this crate — authority
/// you cannot name, you cannot invoke.
#[derive(Clone, Copy)]
pub struct Client(());

impl Client {
    /// Mint a token. Crate-private on purpose: only contexts that are provably
    /// client-side and post-hydrate (`Ctx<Live>`) may call this.
    pub(crate) fn new() -> Self {
        Client(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_token_is_zero_sized_and_copy() {
        assert_eq!(std::mem::size_of::<Client>(), 0);
        let c = Client::new();
        let a = c; // Copy
        let b = c;
        let _ = (a, b);
    }
}
