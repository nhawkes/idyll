//! # Idyll — a render-once, futures-first UI framework for Rust
//!
//! Every view is a component in the app crate. The **page** is the root component —
//! the server mounts it through the membrane per request, the browser claims that
//! mount and re-mounts it on transitions — and a `live::Name()` mount call
//! inside it is a named hole where another live component mounts (see [`live`]).
//! **Content** is plain functions of data returning [`View`] IR (`view!` —
//! no closures, so nothing inside it can read a signal or receive an event), which a
//! component places with `(expr)` — fixed as a `View`, tracked as a `Signal<View>`.
//!
//! A live component is an `async fn` that:
//! 1. **Sets up** reactive state as signals (`MutableSignal<T>`, `SignalVec<T>`)
//! 2. **Renders exactly once** — enforced by typestate (`Ctx<Setup>` → `Ctx<Live>`)
//! 3. **Lives** as a message loop that mutates signals, never rebuilding the view
//!
//! The view updates itself through fine-grained signal bindings. Structural
//! change happens through reactive control flow (`@if`, `@for`, `@match`).
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use idyll::{live_view, Ctx, Result, Setup};
//!
//! enum Msg { Inc, Dec }
//!
//! async fn counter(ctx: Ctx<Setup, Msg>, start: i64) -> Result {
//!     let n = ctx.mutable_signal(start);
//!     let mut ctx = ctx.render(live_view! {
//!         button onclick=>(|_| Some(Msg::Dec)) { "−" }
//!         span { $n }
//!         button onclick=>(|_| Some(Msg::Inc)) { "+" }
//!     }).await?;
//!     loop {
//!         let (msg, turn) = ctx.recv().await?;
//!         match msg {
//!             Msg::Inc => n.update(&turn, |v| *v += 1),
//!             Msg::Dec => n.update(&turn, |v| *v -= 1),
//!         }
//!     }
//! }
//! ```

// The macros emit `::idyll::…` paths so they work in downstream crates; this
// self-alias lets the same expansion resolve inside this crate.
extern crate self as idyll;

pub mod boundary;
pub mod callback;
pub mod canvas;
pub mod capability;
pub mod component;
pub mod ctx;
pub mod dev;
pub mod driver;
pub mod html;
pub mod inbox;
pub mod lifecycle;
pub mod live;
pub mod live_driver;
pub mod mutate;
pub mod owner;
pub mod runtime;
pub mod signal;
pub mod slot;
pub mod template;
pub mod live_view;

// ── Re-exports ────────────────────────────────────────────────────────────────

pub use boundary::{error_boundary, suspense_boundary};
pub use callback::{callback_from_sender, Callback};
pub use canvas::{Curve, Shape};
pub use capability::Client;
pub use component::{mount_child, spawn_child, Component};
pub use ctx::{ContextHandle, Ctx, Live, Reducer, RenderScope, SeedSink, Setup};
pub use dev::{
    ABSORB_TY,
    replay_component, MessageLog, MessageRecord, ReplayInputError,
    ReplayInputKind, ReplayInputRecord, ReplayInputs,
};
pub use driver::{
    CommandBufferDriver, DomCommand, DomDriver, MockDriver, RequestError, UnknownTemplateId,
};
pub use html::{
    fold_html, live_wrapper_open, view_html, view_segments, BodySegment, Html, HtmlFold,
    LIVE_WRAPPER_CLOSE,
};
pub use inbox::InboxSender;
pub use mutate::{Mutation, MutationError, NavigateError, OpHash, Query};
pub use owner::{Owner, OwnerId};
pub use runtime::{
    fresh_child_id, ChildId, FlushBudget, FlushStatus, MountGuard, Runtime, RuntimeCore,
};
pub use signal::computed::Computed;
pub use slot::{Slot, SlotGuard};
pub use signal::reaction::Reaction;
pub use signal::vec::{KeyedVec, MutableVec, Row, SignalVec, SpliceOp};
pub use signal::{
    deferred, Cx, FlushStep, Lane,
    InTurn, ListenGuard, Signal, MutableSignal, SignalId, Turn,
};
pub use template::{View, StyleRule};
pub use live_view::{
    key, render_kind_of, Event, LiveView, Rect, Rendered, RenderInto, RenderKind, SlotKind, ViewCapture,
};

// Re-export Idyll's procedural macros.
pub use idyll_macros::{component, guest, live_view, view};

/// The message type of a component that handles none. No message can be constructed,
/// so nothing can ever be sent — the component renders and returns its mount.
pub enum Never {}

/// A component's result: the **live mount**, or the error its boundary catches. A
/// message-less component (`M = `[`Never`]) ends with `Ok(ctx.render(live_view! { … }).await?)`;
/// a component with messages cannot construct a `Ctx<Live, Never>`, so its only
/// non-error exit is not exiting — the `recv` loop diverges, and `!` coerces here.
pub type Result = std::result::Result<Ctx<Live, Never>, Box<dyn std::error::Error + 'static>>;

/// A boxed error as it travels the mount tree. A component's failure is a **terminal,
/// exceptional** value. **Before it renders**, it is the future's `Err`: the parent's `render`
/// awaits the child, so the fault propagates up the render-await chain (`render().await?`) to the
/// nearest error boundary — exactly as a store read's fault does. **After it renders**, the
/// parent is no longer awaiting: the fault resolves the component's own `recv` as `Err`, ending
/// its loop, and the terminal fault is delivered to the nearest error boundary's mailbox (see
/// [`ctx::FaultRoute`]). It is not a reactive status.
pub type Fault = Box<dyn std::error::Error + 'static>;
