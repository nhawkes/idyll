//! # idyll-host — the synchronous SSR render membrane
//!
//! The reactive core is `!Send` (`Rc`/`RefCell`) and stays that way. Rather than
//! poison the ergonomics with `Send` bounds *or* seal the core inside an async wasm
//! fiber, the converged design keeps **host-boundary awaiting outside the sandbox**:
//! data is resolved host-side in native async Rust, then pushed through a render that
//! is **synchronous at the host boundary** (`seeded-data → HTML`).
//!
//! The guest is still async *internally* — idyll is futures-first, and the component
//! drives `ctx.render(..).await`. That async is a stackless state machine in the
//! guest's own linear memory; with data pre-seeded the render awaits nothing across the
//! host boundary, so `mount` produces its paint synchronously and the host drains it in
//! one uninterrupted call sequence.
//!
//! ## The contract is WIT, not a hand-rolled ABI
//!
//! The membrane talks to the guest through a **component-model interface** ([`wit/ssr.wit`]),
//! not a bespoke `alloc`/`render`-over-`memory` handshake. The `world app` the guest
//! exports is its **mount table**: `mount` returns a `mount-result`, and `flush` drains
//! the rest of the paint as a **command stream** (the host calls `flush` until `done`):
//!
//! ```wit
//! record mount-result { flush: flush-result, static-paint: bool }
//! export mount: func(live: live, parent: option<contexts>, seed: list<u8>)
//!     -> result<mount-result, string>;
//! ```
//!
//! `wasmtime::component::bindgen!` generates the typed host binding; the guest uses
//! `wit_bindgen`. The canonical ABI moves the seed in and a stream of typed
//! [`idyll::DomCommand`]s out — no `unsafe`, no pointer packing, no manual `memory` reads.
//! Emitting **commands** rather than one HTML string is deliberate: the **host** folds
//! them ([`idyll::fold_html`]) and owns the envelope (doctype, `<html>`, the data seed it
//! already holds, the dev runtime), the guest owns the app content, and the same stream
//! re-folds into DOM on the client — neither side does string surgery. A mount **fault is
//! `result::err`** — a typed error that surfaces as an `Err` here, never as in-band
//! `<pre>error…</pre>` HTML smuggled through the success channel.
//!
//! What the sandbox still earns: **isolation** (a blown render can't corrupt the host),
//! **isomorphism** (the same artifact hydrates on the client), and — load-bearing —
//! **preemption**. Native Rust cannot be interrupted; a runaway native render pins its
//! worker forever. A wasm render is **clocked**: each render gets a per-store epoch
//! deadline (~100 ticks ≈ 100ms), and on overrun it **traps — caught as
//! [`MountOutcome::BlewBudget`]**. Store-per-render isolates the blow-up to the one
//! render; shell and sibling boundaries are separate calls on separate stores.
//!
//! The epoch trap returning a *catchable* `Err` (not a process abort) requires
//! **wasmtime ≥ 46** on Windows-MSVC; earlier versions aborted the process in the
//! older `wasmtime_longjmp` unwinder.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Result};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, Trap};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

// The typed host binding, generated from the same WIT the guest is built against. The
// world is the app's **live table** (the same artifact also runs in the browser via
// jco); the membrane calls `mount` for each live's SSR paint.
wasmtime::component::bindgen!({
    world: "app",
    path: "wit",
});

/// The outcome of a single live mount.
///
/// A genuine fault is the `Err` arm of [`MembraneEngine::mount`]'s `Result` — a typed
/// [`MembraneError`], so a failure can never be mistaken for a paint. `BlewBudget` is
/// not an error: it is the caller's cue to bound the damage (the one live, not the
/// page).
#[derive(Debug)]
pub enum MountOutcome {
    /// The live's self-contained mount stream, in idyll's own command vocabulary —
    /// ready for [`idyll::fold_html`]. `static_paint` marks an island the browser adopts
    /// without re-running: no client work, no dependency outside its seed (see the WIT
    /// `mount-result`). The server stamps `data-static` on
    /// its wrapper so the browser skips the guest mount.
    Commands {
        commands: Vec<idyll::DomCommand>,
        static_paint: bool,
    },
    /// The mount overran its epoch budget and was trapped. Never retried in place.
    BlewBudget,
    /// The app declined the mount (the WIT `result`'s `err` arm: unknown live name,
    /// undecodable seed, missing args). Unlike a trap, the instance stays healthy —
    /// the caller ships the one wrapper unpainted and says why.
    Failed(String),
}

/// A hard fault crossing the membrane, **classified once at the wasm boundary** so no
/// caller re-inspects an opaque `anyhow`: the raw wasmtime error is turned into one of
/// these typed variants the moment it leaves the guest, and code downstream matches on
/// the variant instead of downcasting. A blown epoch budget is deliberately *not* here
/// — it is [`MountOutcome::BlewBudget`], an outcome the caller bounds, not a fault.
#[derive(Debug, thiserror::Error)]
pub enum MembraneError {
    /// The guest component could not be instantiated — a link/setup failure, before any
    /// app code runs. Not attributable to a specific mount.
    #[error("instantiating guest component")]
    Instantiate(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// The guest **trapped** mid-render: a panic (guests are `panic=abort`), an
    /// `unreachable`, a stack overflow. Never an epoch overrun (that is
    /// [`MountOutcome::BlewBudget`]), and never smuggled back as HTML.
    #[error("guest trapped during render")]
    Trap(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// Store state: the WASI context a `wasm32-wasip2` guest links against (its std runtime
/// imports `wasi:cli`/`wasi:io` even when the render itself does no I/O).
struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// A background thread that advances the engine epoch on a fixed interval, so a
/// per-store deadline of `n` ticks bounds a render to roughly `n * interval`.
struct EpochTicker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: Engine, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                engine.increment_epoch();
                std::thread::sleep(interval);
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The membrane's wasmtime engine + the compiled guest component + the WASI linker + the
/// epoch ticker.
///
/// The guest `Component` is compiled **once** here, not per render; each render only
/// *instantiates* a fresh store, which is cheap and gives deterministic per-render
/// signal ids (the hydration-id story lives in `idyll` itself).
pub struct MembraneEngine {
    engine: Engine,
    component: Component,
    linker: Linker<HostState>,
    // Dropped (joined) when the engine is dropped.
    _ticker: EpochTicker,
}

impl MembraneEngine {
    /// The default epoch tick interval. A 100ms render budget is ~100 ticks.
    pub const TICK: Duration = Duration::from_millis(1);

    /// Headroom (in epoch ticks) for instantiate before the render is clocked. Setup is
    /// fast host-driven work; this exists only because an epoch-interrupt store must have
    /// *some* deadline set to run at all.
    pub const SETUP_HEADROOM_TICKS: u64 = 1_000;

    /// Build the engine and compile the guest `wasm` **component** once. `wasm` is a
    /// `wasm32-wasip2` component exporting the `idyll:ssr` `guest` world's `render`.
    pub fn new(wasm: &[u8]) -> Result<Self> {
        let mut config = Config::new();
        // Preempt a runaway render instead of letting it pin a worker forever.
        config.epoch_interruption(true);
        config.wasm_component_model(true);

        let engine =
            Engine::new(&config).map_err(|e| anyhow!("configuring membrane engine: {e}"))?;
        let component = Component::new(&engine, wasm)
            .map_err(|e| anyhow!("compiling guest component: {e}"))?;

        // The linker is per-engine and reused across renders; wire WASI once.
        let mut linker = Linker::<HostState>::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| anyhow!("linking WASI into the membrane: {e}"))?;

        let ticker = EpochTicker::start(engine.clone(), Self::TICK);
        Ok(MembraneEngine {
            engine,
            component,
            linker,
            _ticker: ticker,
        })
    }

    /// A fresh store with WASI wired — one per call: deterministic ids, and a clean
    /// world to blow up in isolation. `insecure_seed` sets `wasi:random/insecure-seed`
    /// (returned as `(seed as u64, (seed >> 64) as u64)`): the server's unguessable
    /// per-request seed, which the browser receives identically, so the SSR render and
    /// the client hydration draw from the same seed.
    fn store(&self, insecure_seed: u128) -> Store<HostState> {
        let state = HostState {
            ctx: WasiCtxBuilder::new().insecure_random_seed(insecure_seed).build(),
            table: ResourceTable::new(),
        };
        let mut store = Store::new(&self.engine, state);
        // A store with epoch interruption traps immediately without a deadline.
        store.set_epoch_deadline(Self::SETUP_HEADROOM_TICKS);
        store
    }

    /// The guest's **live table, as data** — validated against at engine build so an
    /// unknown live reference is a loud server error, never a dead wrapper. (Within
    /// one build the typed table makes references compile-checked; this call exists for
    /// what types can't see: artifact skew — a dev server holding stale wasm.)
    pub fn live(&self) -> Result<Vec<String>> {
        let mut store = self.store(0);
        let guest = App::instantiate(&mut store, &self.component, &self.linker)
            .map_err(|e| anyhow!("instantiating guest component: {e}"))?;
        guest
            .call_live(&mut store)
            .map_err(|e| anyhow!("querying guest live table: {e}"))
    }

    /// Mount one live for its SSR paint, synchronously, inside a fresh store, clocked
    /// at `budget_ticks` epoch ticks. `seed` is the executed route query (serialized
    /// `Preloaded`); the guest's live table builds the live's args from it.
    /// `insecure_seed` is the request's unguessable random seed — the browser receives
    /// it identically, so the SSR render and the client hydration draw the same values.
    /// The browser later runs the **identical call** against the same seeds to hydrate.
    ///
    /// Returns:
    /// - `Ok(Commands(_))` — the live's mount stream, converted to idyll's own
    ///   command vocabulary (ready to fold into the wrapper);
    /// - `Ok(BlewBudget)` — the mount overran its epoch budget (a caught trap);
    /// - `Ok(Failed(_))` — the app declined the mount (the typed `err` arm);
    /// - `Err(_)` — a genuine fault. Faults are **never** smuggled back as HTML.
    pub fn mount(
        &self,
        live: &str,
        instance: u32,
        key: Option<&str>,
        seed: &[u8],
        insecure_seed: u128,
        budget_ticks: u64,
    ) -> std::result::Result<MountOutcome, MembraneError> {
        let mut store = self.store(insecure_seed);
        let guest = App::instantiate(&mut store, &self.component, &self.linker)
            .map_err(|e| MembraneError::Instantiate(e.into()))?;

        // Clock the mount itself — instantiate is trivial host-driven setup and
        // shouldn't count against (or trip on) the budget.
        store.set_epoch_deadline(budget_ticks);
        let live_ref = LiveRef {
            name: live.to_string(),
            instance,
            key: key.map(str::to_string),
        };
        // No parent: server mounts are isolated per store (the deliberate SSR
        // asymmetry) — context inheritance is a browser-side, shared-instance affair.
        //
        // `mount` returns the first flush slice; SSR wants the whole paint, so we
        // drain `flush` to completion. Every slice runs under the one epoch deadline
        // set above, so a runaway mount still trips it — sliced or not.
        match guest.call_mount(&mut store, &live_ref, None, seed, false) {
            Ok(Ok(result)) => {
                let static_paint = result.static_paint;
                match drain_paint(&guest, &mut store, result.flush) {
                    Ok(commands) => Ok(MountOutcome::Commands { commands, static_paint }),
                    Err(err) => classify_trap(err),
                }
            }
            Ok(Err(message)) => Ok(MountOutcome::Failed(message)),
            Err(err) => classify_trap(err.into()),
        }
    }
}

/// Drive the guest's `flush` to completion from a first slice, accumulating the whole
/// paint. Each slice runs under the store's one epoch deadline, so a mount that runs
/// away across slices still traps. `u32::MAX` asks the guest to drain as far as it can
/// per call — SSR has no frame to yield to.
fn drain_paint(
    guest: &App,
    store: &mut Store<HostState>,
    first: FlushResult,
) -> Result<Vec<idyll::DomCommand>> {
    let mut commands: Vec<idyll::DomCommand> =
        first.commands.into_iter().map(dom_command).collect();
    let mut done = first.done;
    while !done {
        let next = guest.call_flush(&mut *store, u32::MAX)?;
        commands.extend(next.commands.into_iter().map(dom_command));
        done = next.done;
    }
    Ok(commands)
}

/// The one place a raw wasm error is inspected. An epoch-deadline overrun (the clock
/// tripping mid-render — including on the very first SSR paint) comes back as
/// `Trap::Interrupt`, a blown budget the caller bounds to this one live. Everything
/// else becomes a typed [`MembraneError::Trap`] carrying the original error as its
/// source, so its trap kind and chain survive — and callers match the variant instead
/// of downcasting an opaque error whose full set they cannot see.
fn classify_trap(err: anyhow::Error) -> std::result::Result<MountOutcome, MembraneError> {
    if matches!(err.downcast_ref::<Trap>(), Some(Trap::Interrupt)) {
        Ok(MountOutcome::BlewBudget)
    } else {
        Err(MembraneError::Trap(err.into()))
    }
}

/// Mirror the WIT `command` back onto idyll's own [`DomCommand`] — the host-side twin of
/// the guest's `From<DomCommand> for Command`. Exhaustive both ways, so a vocabulary
/// change is a compile error on whichever side lags.
fn dom_command(c: Command) -> idyll::DomCommand {
    use idyll::driver::{HandlerId, NodeId, RequestId, SlotId, TemplateId};
    match c {
        Command::ReplaceTemplate(cmd) => idyll::DomCommand::ReplaceTemplate {
            template_id: TemplateId(cmd.template_id),
            template: idyll::template::Template {
                svg: cmd.svg,
                ..idyll::template::Template::from(
                    cmd.nodes.into_iter().map(tpl_node).collect::<Vec<_>>(),
                )
            }
            .with_styles(
                cmd.styles
                    .into_iter()
                    .map(|rule| idyll::StyleRule {
                        name: rule.name.into(),
                        css: rule.css.map(Into::into),
                    })
                    .collect(),
            ),
        },
        Command::SetText(cmd) => {
            idyll::DomCommand::SetText { node_id: NodeId(cmd.node), text: cmd.text }
        }
        Command::SetAttr(cmd) => idyll::DomCommand::SetAttr {
            node_id: NodeId(cmd.node),
            name: cmd.name.into(),
            value: cmd.value,
        },
        Command::SetStyleProp(cmd) => idyll::DomCommand::SetStyleProp {
            node_id: NodeId(cmd.node),
            name: cmd.name.into(),
            value: cmd.value,
        },
        Command::RemoveAttr(cmd) => {
            idyll::DomCommand::RemoveAttr { node_id: NodeId(cmd.node), name: cmd.name.into() }
        }
        Command::SetBoolAttr(cmd) => idyll::DomCommand::SetBoolAttr {
            node_id: NodeId(cmd.node),
            name: cmd.name.into(),
            value: cmd.value,
        },
        Command::MountFragment(cmd) => idyll::DomCommand::MountFragment {
            anchor_id: NodeId(cmd.anchor),
            template: TemplateId(cmd.template),
        },
        Command::ReplaceFragment(cmd) => idyll::DomCommand::ReplaceFragment {
            anchor_id: NodeId(cmd.anchor),
            template: TemplateId(cmd.template),
        },
        Command::RemoveFragment(anchor) => {
            idyll::DomCommand::RemoveFragment { anchor_id: NodeId(anchor) }
        }
        Command::DetachFragment(anchor) => {
            idyll::DomCommand::DetachFragment { anchor_id: NodeId(anchor) }
        }
        Command::AttachFragment(anchor) => {
            idyll::DomCommand::AttachFragment { anchor_id: NodeId(anchor) }
        }
        Command::MoveFragment(cmd) => idyll::DomCommand::MoveFragment {
            anchor_id: NodeId(cmd.anchor),
            after_anchor: NodeId(cmd.after),
        },
        Command::AddEventListener(cmd) => idyll::DomCommand::AddEventListener {
            node_id: NodeId(cmd.node),
            event_type: cmd.event_type.into(),
            handler_id: HandlerId(cmd.handler),
        },
        Command::RemoveEventListener(cmd) => idyll::DomCommand::RemoveEventListener {
            node_id: NodeId(cmd.node),
            event_type: cmd.event_type.into(),
            handler_id: HandlerId(cmd.handler),
        },
        Command::WatchMeasure(cmd) => idyll::DomCommand::WatchMeasure {
            node_id: NodeId(cmd.node),
            handler_id: HandlerId(cmd.handler),
        },
        Command::UnwatchMeasure(cmd) => idyll::DomCommand::UnwatchMeasure {
            node_id: NodeId(cmd.node),
            handler_id: HandlerId(cmd.handler),
        },
        Command::Paint(cmd) => idyll::DomCommand::Paint {
            node_id: NodeId(cmd.node),
            layers: cmd.layers,
            inks: cmd.inks,
            deltas: cmd
                .deltas
                .into_iter()
                .map(|delta| (delta.layer, delta.changes, delta.len))
                .collect(),
        },
        Command::BindSlot(cmd) => {
            idyll::DomCommand::BindSlot { slot: SlotId(cmd.slot), node_id: NodeId(cmd.node) }
        }
        Command::ServerRequest(cmd) => idyll::DomCommand::ServerRequest {
            request_id: RequestId(cmd.request),
            op: idyll::OpHash::from_words(cmd.msb, cmd.lsb),
            args: cmd.args,
        },
        Command::Navigate(cmd) => idyll::DomCommand::Navigate {
            request_id: RequestId(cmd.request),
            op: idyll::OpHash::from_words(cmd.msb, cmd.lsb),
            path: cmd.path,
        },
        Command::WatchNavigation(handler) => {
            idyll::DomCommand::WatchNavigation { handler_id: HandlerId(handler) }
        }
        Command::WatchSize(handler) => {
            idyll::DomCommand::WatchSize { handler_id: HandlerId(handler) }
        }
        Command::StartTicks(cmd) => idyll::DomCommand::StartTicks {
            handler_id: HandlerId(cmd.handler),
            interval_ms: cmd.interval_ms,
        },
        Command::StopTicks(handler) => {
            idyll::DomCommand::StopTicks { handler_id: HandlerId(handler) }
        }
        Command::FreeNodes(ids) => idyll::DomCommand::FreeNodes {
            node_ids: ids.into_iter().map(NodeId).collect(),
        },
        Command::MountRoot(template_id) => idyll::DomCommand::MountRoot {
            template_id: TemplateId(template_id),
        },
    }
}

fn tpl_node(node: TplNode) -> idyll::template::TplNode {
    use idyll::driver::SlotId;
    match node {
        TplNode::Text(text) => idyll::template::TplNode::Text(text.into()),
        TplNode::TextSlot(slot) => idyll::template::TplNode::TextSlot(SlotId(slot)),
        TplNode::AnchorSlot(slot) => idyll::template::TplNode::AnchorSlot(SlotId(slot)),
        TplNode::Element(el) => idyll::template::TplNode::Element {
            tag: el.tag.into(),
            attrs: el
                .attrs
                .into_iter()
                .map(|attr| idyll::template::TplAttr {
                    name: attr.name.into(),
                    value: attr.value.into(),
                })
                .collect::<Vec<_>>()
                .into(),
            slot: el.slot.map(SlotId),
            children: el.children,
        },
        TplNode::Live(live) => idyll::template::TplNode::Live {
            name: live.name.into(),
            key: live.key.map(Into::into),
            fallback: live.fallback,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A prebuilt guest **component** (from `tests/fixtures/`, built from a tiny
    /// `wit_bindgen` guest — see `fixtures/README.md`). Its `mount` branches on the
    /// live name so one fixture exercises every host path: `boom` → panic (trap →
    /// fault), `spin` → infinite loop (`BlewBudget`), `decline` → the typed `err` arm
    /// (`Failed`), `paint` → a canned stream.
    const GUEST: &[u8] = include_bytes!("../tests/fixtures/guest.wasm");

    fn engine() -> MembraneEngine {
        MembraneEngine::new(GUEST).expect("membrane engine from guest component")
    }

    fn paint(outcome: MountOutcome) -> String {
        match outcome {
            MountOutcome::Commands { commands, .. } => idyll::fold_html(&commands).into_string(),
            MountOutcome::BlewBudget => panic!("mount unexpectedly blew its budget"),
            MountOutcome::Failed(message) => panic!("mount unexpectedly declined: {message}"),
        }
    }

    #[test]
    fn islands_returns_the_guest_table() {
        assert_eq!(
            engine().live().expect("live"),
            vec!["paint", "boom", "spin", "decline"]
        );
    }

    // The typed `err` arm: an app-level decline is data, not a trap — the instance
    // stays healthy and the message survives the membrane.
    #[test]
    fn a_typed_mount_error_surfaces_as_failed_not_a_fault() {
        let engine = engine();
        match engine.mount("decline", 0, None, b"", 0, 100_000).expect("mount call succeeds") {
            MountOutcome::Failed(message) => {
                assert_eq!(message, "this live politely declines")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn mount_streams_fold_to_the_island_paint() {
        let engine = engine();
        // Generous budget: the guest finishes in microseconds, far under deadline.
        let html = paint(engine.mount("paint", 2, None, b"hello", 0, 100_000).expect("mount"));
        assert_eq!(html, "<p>paint#2 seed 5 bytes</p>");
    }

    // The WIT `mount-result`'s static-paint verdict crosses the membrane intact: the
    // fixture's canned paint declares itself static, and the outcome carries it through.
    #[test]
    fn a_static_paint_verdict_crosses_the_membrane() {
        let engine = engine();
        match engine.mount("paint", 0, None, b"x", 0, 100_000).expect("mount") {
            MountOutcome::Commands { static_paint, .. } => assert!(static_paint),
            other => panic!("expected Commands, got {other:?}"),
        }
    }

    #[test]
    fn store_per_mount_isolates_repeated_mounts() {
        let engine = engine();
        // Each mount gets a fresh store; repeated mounts don't interfere.
        for _ in 0..16 {
            assert_eq!(
                paint(engine.mount("paint", 0, None, b"x", 0, 100_000).expect("mount")),
                "<p>paint#0 seed 1 bytes</p>"
            );
        }
    }

    // THE GATE: a synchronous mount that overruns its epoch budget traps and is caught
    // as `BlewBudget` — a catchable blow-up, not a process abort. Requires wasmtime >= 46
    // on Windows-MSVC.
    #[test]
    fn runaway_mount_blows_its_budget() {
        let engine = engine();
        // `b"spin"` loops forever in the guest; the tight budget trips the deadline.
        match engine.mount("spin", 0, None, b"", 0, 1).expect("mount call itself must not error") {
            MountOutcome::BlewBudget => {}
            other => panic!("runaway mount should blow its budget, got {other:?}"),
        }
    }

    // A guest panic (guests are panic=abort → a trap) is a real `Err` — never HTML.
    #[test]
    fn guest_fault_is_a_typed_error_not_html() {
        let engine = engine();
        let err = engine.mount("boom", 0, None, b"", 0, 100_000)
            .expect_err("a guest trap must surface as an Err, not Ok(commands)");
        assert!(
            err.to_string().contains("trapped"),
            "expected a typed trap, got: {err}"
        );
    }
}
