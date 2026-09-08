use std::rc::Rc;
use std::{cell::RefCell, collections::VecDeque};

use crate::live_view::Event;

// ── Operation types ───────────────────────────────────────────────────────────

/// Opaque node identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct NodeId(pub u32);

/// Identifies a compiled template.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct TemplateId(pub u32);

/// A slot within a template's binding table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct SlotId(pub u32);

/// Identifies a registered event handler.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct HandlerId(pub u32);

/// A registered event target: a dispatch (shared by every handler of one `live_view!`
/// block) plus the arm index this handler fires — the handler table holds indices
/// into block dispatches, not one closure per listener.
#[derive(Clone)]
pub struct EventHandler {
    dispatch: Rc<dyn Fn(u32, Event)>,
    idx: u32,
}

impl EventHandler {
    /// Arm `idx` of a block's event dispatch.
    pub fn arm(dispatch: Rc<dyn Fn(u32, Event)>, idx: u32) -> Self {
        EventHandler { dispatch, idx }
    }

    /// A standalone handler (a tick or navigation subscription's own closure).
    pub fn single(f: Rc<dyn Fn(Event)>) -> Self {
        EventHandler {
            dispatch: Rc::new(move |_, event| f(event)),
            idx: 0,
        }
    }

    pub fn call(&self, event: Event) {
        (self.dispatch)(self.idx, event);
    }
}

/// Identifies an in-flight server request (see [`crate::Mutation`]). Allocated from one
/// thread-local counter, so ids are unique across every live on the thread.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct RequestId(pub u32);

/// A single DOM patch operation. The runtime batches these per flush. `PartialEq`
/// is load-bearing: a binding arm's returned op is compared against its last
/// emission, and only a differing op joins the stream.
#[derive(Debug, Clone, PartialEq)]
pub enum DomOp {
    /// Announce the stream's **root template**: build/claim it as this mount's
    /// document. Explicit, never inferred — templates are content-addressed (a
    /// remount's root may already be registered, so "first replace-template" is no
    /// root signal), and template ids are global in the shared runtime (so `id 0`
    /// isn't one either).
    MountRoot {
        template: TemplateId,
    },
    SetText {
        node_id: NodeId,
        text: String,
    },
    SetAttr {
        node_id: NodeId,
        name: &'static str,
        value: String,
    },
    /// Write ONE CSS declaration (`el.style.setProperty`) instead of replacing the
    /// whole `style` attribute — no CSSOM reparse of untouched declarations, and the
    /// wire payload is just the changed property. Empty value clears the property.
    SetStyleProp {
        node_id: NodeId,
        name: &'static str,
        value: String,
    },
    RemoveAttr {
        node_id: NodeId,
        name: &'static str,
    },
    SetBoolAttr {
        node_id: NodeId,
        name: &'static str,
        value: bool,
    },
    MountFragment {
        anchor_id: NodeId,
        template: TemplateId,
    },
    ReplaceFragment {
        anchor_id: NodeId,
        template: TemplateId,
    },
    RemoveFragment {
        anchor_id: NodeId,
    },
    DetachFragment {
        anchor_id: NodeId,
    },
    AttachFragment {
        anchor_id: NodeId,
    },
    MoveFragment {
        anchor_id: NodeId,
        after_anchor: NodeId,
    },
    AddEventListener {
        node_id: NodeId,
        event_type: &'static str,
        handler_id: HandlerId,
    },
    RemoveEventListener {
        node_id: NodeId,
        event_type: &'static str,
        handler_id: HandlerId,
    },
    /// Observe an element's post-layout rectangle (a `measure=>` binding): the
    /// browser wires a ResizeObserver and delivers the root-relative rect to
    /// `handler_id` on mount and on resize. Not a DOM event — its own command,
    /// the same way ticks are (`StartTicks`), never a magic event-type string.
    WatchMeasure {
        node_id: NodeId,
        handler_id: HandlerId,
    },
    /// Disconnect a `WatchMeasure` observer (the binding's element unmounted).
    UnwatchMeasure {
        node_id: NodeId,
        handler_id: HandlerId,
    },
    /// What a `painting=(…)` binding has to say this frame: how many layers the canvas
    /// has, and — for each layer whose shapes moved — which slots moved and how long
    /// that layer now is. The runtime owns the element, its device-pixel scaling, the
    /// per-layer bitmaps, the clear and the strokes.
    ///
    /// Reactivity is per shape and the wire is per frame: the binding's per-shape
    /// effects mark slots as they are written, and one command carries the turn's whole
    /// mark. The membrane charges per crossing, so a surface that took one call per
    /// shape would cost more than the elements it replaces.
    Paint {
        node_id: NodeId,
        layers: u32,
        deltas: Vec<crate::canvas::LayerDelta>,
    },
    /// Ask the host environment to run a persisted server mutation, by its [`OpHash`]
    /// (nothing of the client's choosing — the server executes only boot-validated
    /// artifacts). The browser runtime POSTs `args` to `/__idyll/m/<hash>` and hands the
    /// response back through the component's `deliver` export;
    /// [`crate::deliver_response`] then resolves it to a message in the requesting
    /// component's inbox (commands out, messages in).
    ServerRequest {
        request_id: RequestId,
        op: idyll_schema::OpHash,
        args: Vec<u8>,
    },
    /// Ask the host environment to re-execute the persisted **route query** for
    /// `path` (`GET /__idyll/q/<hash>`) — the SPA live's navigation, as data. The
    /// browser pushes the history entry, fetches, and hands the `Preloaded` payload
    /// back through `deliver`; the response replays into the context store, so the
    /// live's route projection swaps its arm.
    Navigate {
        request_id: RequestId,
        op: idyll_schema::OpHash,
        path: String,
    },
    /// Subscribe `handler` to **navigation intents** — same-origin link clicks
    /// (the browser pushes history first) and `popstate` (history already moved).
    /// Each intent dispatches as an ordinary event whose `target_value` is the path;
    /// navigation is a message like everything else.
    WatchNavigation {
        handler_id: HandlerId,
    },
    /// Observe the **mount root's width** for `handler` (`ctx.resizes`) — a
    /// `ResizeObserver`, delivering its content-box width as an ordinary event whose
    /// `target_value` is the number, once on observe and again on every change. No node
    /// id: a mount has exactly one root, and the root is what the component gets to lay
    /// out in.
    WatchSize {
        handler_id: HandlerId,
    },
    /// Start delivering **ticks** to `handler` — the wire half of the Elm-style
    /// signal-gated time subscription (`ctx.every` / `ctx.frames`). `interval_ms:
    /// None` means animation frames (rAF); `Some(ms)` a fixed interval. The browser
    /// dispatches each tick as an ordinary event whose `timestamp` is the delta since
    /// the previous tick — ticks are messages, so a simulation replays from its
    /// message log like everything else.
    StartTicks {
        handler_id: HandlerId,
        interval_ms: Option<f64>,
    },
    /// Stop delivering ticks to `handler` (the gate went false).
    StopTicks {
        handler_id: HandlerId,
    },
    /// Release the browser-side `node id → DOM node` entries for an unmounted
    /// view instance. Ids are minted guest-side, so the guest ends them — without
    /// this, every removed keyed row strands its slot nodes (and their listener
    /// registrations) in the fold's maps forever.
    FreeNodes {
        node_ids: Vec<NodeId>,
    },
}

// No `Eq`: tick intervals are f64 (PartialEq only), so command equality in tests is
// PartialEq.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum DomCommand {
    ReplaceTemplate {
        template_id: TemplateId,
        template: crate::template::Template,
    },
    /// The stream's root announcement (see [`DomOp::MountRoot`]): whichever fold
    /// receives this builds/claims the named template as the mount's document.
    MountRoot {
        template_id: TemplateId,
    },
    SetText {
        node_id: NodeId,
        text: String,
    },
    SetAttr {
        node_id: NodeId,
        name: std::borrow::Cow<'static, str>,
        value: String,
    },
    /// One CSS declaration on the element's inline style (empty value clears it).
    SetStyleProp {
        node_id: NodeId,
        name: std::borrow::Cow<'static, str>,
        value: String,
    },
    RemoveAttr {
        node_id: NodeId,
        name: std::borrow::Cow<'static, str>,
    },
    SetBoolAttr {
        node_id: NodeId,
        name: std::borrow::Cow<'static, str>,
        value: bool,
    },
    MountFragment {
        anchor_id: NodeId,
        template: TemplateId,
    },
    ReplaceFragment {
        anchor_id: NodeId,
        template: TemplateId,
    },
    RemoveFragment {
        anchor_id: NodeId,
    },
    DetachFragment {
        anchor_id: NodeId,
    },
    AttachFragment {
        anchor_id: NodeId,
    },
    MoveFragment {
        anchor_id: NodeId,
        after_anchor: NodeId,
    },
    AddEventListener {
        node_id: NodeId,
        event_type: std::borrow::Cow<'static, str>,
        handler_id: HandlerId,
    },
    RemoveEventListener {
        node_id: NodeId,
        event_type: std::borrow::Cow<'static, str>,
        handler_id: HandlerId,
    },
    /// A layout-measurement subscription (see [`DomOp::WatchMeasure`]).
    WatchMeasure {
        node_id: NodeId,
        handler_id: HandlerId,
    },
    /// A measurement unsubscribe (see [`DomOp::UnwatchMeasure`]).
    UnwatchMeasure {
        node_id: NodeId,
        handler_id: HandlerId,
    },
    /// One frame's paint for a canvas (see [`DomOp::Paint`]), in the flat wire form
    /// `crate::canvas` documents: the frame's distinct inks, shared across the layers,
    /// and each changed layer's shapes in one run.
    Paint {
        node_id: NodeId,
        /// How many layers the canvas has, so the far side drops any it still holds
        /// beyond them.
        layers: u32,
        inks: Vec<String>,
        /// One entry per changed layer: which layer, its changed slots (each its slot
        /// index then its numbers), and how long it now is — so the far side can drop
        /// a tail it still holds when a layer shrinks.
        deltas: Vec<(u32, Vec<f32>, u32)>,
    },
    /// A server-mutation request (see [`DomOp::ServerRequest`]).
    ServerRequest {
        request_id: RequestId,
        op: idyll_schema::OpHash,
        args: Vec<u8>,
    },
    /// A navigation's route re-execution (see [`DomOp::Navigate`]).
    Navigate {
        request_id: RequestId,
        op: idyll_schema::OpHash,
        path: String,
    },
    /// A navigation-intent subscription (see [`DomOp::WatchNavigation`]).
    WatchNavigation {
        handler_id: HandlerId,
    },
    /// A mount-root width subscription (see [`DomOp::WatchSize`]).
    WatchSize {
        handler_id: HandlerId,
    },
    /// Tick-subscription control (see [`DomOp::StartTicks`]).
    StartTicks {
        handler_id: HandlerId,
        interval_ms: Option<f64>,
    },
    StopTicks {
        handler_id: HandlerId,
    },
    /// Release the fold's entries for these guest-minted node ids (view unmounted).
    FreeNodes {
        node_ids: Vec<NodeId>,
    },
    /// A slot's resolved node id. Emitted by [`CommandBufferDriver`] so a pure replay (the
    /// browser `runtime.js`) can map `node_id → DOM node`: after a template's HTML lands, its
    /// `data-s="<slot>"` elements are matched to node ids by these bindings, in order. Has no
    /// `DomOp` counterpart — it's driver bookkeeping made explicit on the wire.
    BindSlot {
        slot: SlotId,
        node_id: NodeId,
    },
}

/// Why a host round-trip (a [`DomCommand::ServerRequest`] POST, a
/// [`DomCommand::Navigate`] refetch) failed — the error half of the `deliver`
/// wire. The set is closed by construction: the browser's fetch either never
/// produced an HTTP response, or produced one that refuses. The *detail* inside
/// each variant is open text from the far side (an engine's fetch message, a
/// server's body) — a string inside a typed variant, never a typed fact inside a
/// string.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RequestError {
    /// The transport never produced a response: network down, DNS, aborted.
    Transport(String),
    /// The server answered, refusing: the status it sent and the body it said it with.
    Http { status: u16, body: String },
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::Transport(detail) => write!(f, "transport failed: {detail}"),
            RequestError::Http { status, body } => write!(f, "HTTP {status}: {body}"),
        }
    }
}

impl std::error::Error for RequestError {}

impl From<DomOp> for DomCommand {
    fn from(value: DomOp) -> Self {
        match value {
            DomOp::MountRoot { template } => DomCommand::MountRoot { template_id: template },
            DomOp::SetText { node_id, text } => DomCommand::SetText { node_id, text },
            DomOp::SetAttr {
                node_id,
                name,
                value,
            } => DomCommand::SetAttr {
                node_id,
                name: name.into(),
                value,
            },
            DomOp::SetStyleProp {
                node_id,
                name,
                value,
            } => DomCommand::SetStyleProp {
                node_id,
                name: name.into(),
                value,
            },
            DomOp::RemoveAttr { node_id, name } => DomCommand::RemoveAttr {
                node_id,
                name: name.into(),
            },
            DomOp::SetBoolAttr {
                node_id,
                name,
                value,
            } => DomCommand::SetBoolAttr {
                node_id,
                name: name.into(),
                value,
            },
            DomOp::MountFragment { anchor_id, template } => {
                DomCommand::MountFragment { anchor_id, template }
            }
            DomOp::ReplaceFragment { anchor_id, template } => {
                DomCommand::ReplaceFragment { anchor_id, template }
            }
            DomOp::RemoveFragment { anchor_id } => DomCommand::RemoveFragment { anchor_id },
            DomOp::DetachFragment { anchor_id } => DomCommand::DetachFragment { anchor_id },
            DomOp::AttachFragment { anchor_id } => DomCommand::AttachFragment { anchor_id },
            DomOp::MoveFragment {
                anchor_id,
                after_anchor,
            } => DomCommand::MoveFragment {
                anchor_id,
                after_anchor,
            },
            DomOp::AddEventListener {
                node_id,
                event_type,
                handler_id,
            } => DomCommand::AddEventListener {
                node_id,
                event_type: event_type.into(),
                handler_id,
            },
            DomOp::RemoveEventListener {
                node_id,
                event_type,
                handler_id,
            } => DomCommand::RemoveEventListener {
                node_id,
                event_type: event_type.into(),
                handler_id,
            },
            DomOp::WatchMeasure { node_id, handler_id } => {
                DomCommand::WatchMeasure { node_id, handler_id }
            }
            DomOp::UnwatchMeasure { node_id, handler_id } => {
                DomCommand::UnwatchMeasure { node_id, handler_id }
            }
            DomOp::Paint { node_id, layers, deltas } => {
                let (inks, deltas) = crate::canvas::flatten(&deltas);
                DomCommand::Paint { node_id, layers, inks, deltas }
            }
            DomOp::ServerRequest { request_id, op, args } => {
                DomCommand::ServerRequest { request_id, op, args }
            }
            DomOp::Navigate { request_id, op, path } => {
                DomCommand::Navigate { request_id, op, path }
            }
            DomOp::WatchNavigation { handler_id } => DomCommand::WatchNavigation { handler_id },
            DomOp::WatchSize { handler_id } => DomCommand::WatchSize { handler_id },
            DomOp::StartTicks { handler_id, interval_ms } => {
                DomCommand::StartTicks { handler_id, interval_ms }
            }
            DomOp::StopTicks { handler_id } => DomCommand::StopTicks { handler_id },
            DomOp::FreeNodes { node_ids } => DomCommand::FreeNodes { node_ids },
        }
    }
}

// ── DomDriver ─────────────────────────────────────────────────────────────────

/// Abstraction over the render sink: [`CommandBufferDriver`] records the command stream
/// both folds consume (the wasm component's one output); [`MockDriver`] asserts raw ops in
/// tests.
pub trait DomDriver {
    /// Register a template (its typed IR — see [`crate::template`]); returns an ID that
    /// structural ops reference. Templates are data; no markup crosses this boundary.
    fn register_template(&mut self, template: crate::template::Template) -> TemplateId;

    /// Apply a batch of DOM patch operations.
    fn apply(&mut self, ops: Vec<DomOp>);

    /// Allocate a fresh node ID.
    fn alloc_node_id(&mut self) -> NodeId;

    /// Resolve a node ID for a slot in the currently mounted template scope.
    fn alloc_slot_node_id(&mut self, slot: SlotId) -> NodeId {
        let _ = slot;
        self.alloc_node_id()
    }

    /// Allocate a fresh handler ID.
    fn alloc_handler_id(&mut self) -> HandlerId;

    /// Register an event handler by ID so the driver can dispatch events.
    fn register_event_handler(&mut self, id: HandlerId, handler: EventHandler);

    /// Dispatch a synthetic event to the registered handler (used in tests).
    fn dispatch_event(&self, handler_id: HandlerId, event: Event);

    /// Schedule a microtask. In WASM: `queueMicrotask`. In tests: synchronous.
    fn schedule_microtask(&self, f: Box<dyn FnOnce()>);
}

// ── MockDriver ────────────────────────────────────────────────────────────────

/// Test driver: records operations and allows synthetic event dispatch.
pub struct MockDriver {
    next_node_id: u32,
    next_template_id: u32,
    next_handler_id: u32,
    pub templates: Vec<crate::template::Template>,
    pub log: Vec<DomOp>,
    handlers: std::collections::HashMap<HandlerId, EventHandler>,
    microtasks: RefCell<VecDeque<Box<dyn FnOnce()>>>,
}

impl MockDriver {
    pub fn new() -> Self {
        MockDriver {
            next_node_id: 0,
            next_template_id: 0,
            next_handler_id: 0,
            templates: Vec::new(),
            log: Vec::new(),
            handlers: std::collections::HashMap::new(),
            microtasks: RefCell::new(VecDeque::new()),
        }
    }

    /// Every `SetText` this driver saw, in order — what a **live** binding wrote.
    pub fn set_texts(&self) -> Vec<String> {
        self.log
            .iter()
            .filter_map(|op| match op {
                DomOp::SetText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every text this driver saw: [`set_texts`](Self::set_texts) plus the static text
    /// of every template it ingested. Content (`view!`) carries its text in the
    /// template rather than through a `SetText`, so a test spanning both needs this.
    pub fn all_texts(&self) -> Vec<String> {
        let mut texts = self.set_texts();
        for template in &self.templates {
            for node in template.nodes.iter() {
                if let crate::template::TplNode::Text(text) = node {
                    texts.push(text.to_string());
                }
            }
        }
        texts
    }

    /// Fire an event to the handler with the given id. Useful in tests.
    pub fn fire(&self, handler_id: HandlerId, event: Event) {
        if let Some(h) = self.handlers.get(&handler_id) {
            h.call(event);
        }
    }

    /// The handler with the highest id registered so far — the most recent, since ids
    /// only ever increase.
    pub fn latest_handler(&self) -> Option<HandlerId> {
        self.handlers.keys().copied().max_by_key(|h| h.0)
    }

    pub fn pending_microtasks(&self) -> usize {
        self.microtasks.borrow().len()
    }

    pub fn drain_microtasks(&self) {
        loop {
            let task = self.microtasks.borrow_mut().pop_front();
            let Some(task) = task else {
                break;
            };
            task();
        }
    }

    pub fn drain_microtasks_for(driver: &Rc<RefCell<Self>>) {
        loop {
            let task = {
                let driver = driver.borrow();
                let task = driver.microtasks.borrow_mut().pop_front();
                task
            };
            let Some(task) = task else {
                break;
            };
            task();
        }
    }
}

impl Default for MockDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl DomDriver for MockDriver {
    fn register_template(&mut self, template: crate::template::Template) -> TemplateId {
        let id = TemplateId(self.next_template_id);
        self.next_template_id += 1;
        self.templates.push(template);
        id
    }

    fn apply(&mut self, ops: Vec<DomOp>) {
        for op in &ops {
            if let DomOp::RemoveEventListener { handler_id, .. } = op {
                self.handlers.remove(handler_id);
            }
        }
        self.log.extend(ops);
    }

    fn alloc_node_id(&mut self) -> NodeId {
        let id = NodeId(self.next_node_id);
        self.next_node_id += 1;
        id
    }

    fn alloc_handler_id(&mut self) -> HandlerId {
        let id = HandlerId(self.next_handler_id);
        self.next_handler_id += 1;
        id
    }

    fn register_event_handler(&mut self, id: HandlerId, handler: EventHandler) {
        self.handlers.insert(id, handler);
    }

    fn dispatch_event(&self, handler_id: HandlerId, event: Event) {
        if let Some(h) = self.handlers.get(&handler_id) {
            h.call(event);
        }
    }

    fn schedule_microtask(&self, f: Box<dyn FnOnce()>) {
        self.microtasks.borrow_mut().push_back(f);
    }
}

pub struct CommandBufferDriver {
    next_node_id: u32,
    next_template_id: u32,
    next_handler_id: u32,
    pub templates: Vec<crate::template::Template>,
    commands: Vec<DomCommand>,
    handlers: std::collections::HashMap<HandlerId, EventHandler>,
    microtasks: RefCell<VecDeque<Box<dyn FnOnce()>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownTemplateId {
    pub template_id: TemplateId,
}

impl std::fmt::Display for UnknownTemplateId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown template id {}", self.template_id.0)
    }
}

impl std::error::Error for UnknownTemplateId {}

impl CommandBufferDriver {
    pub fn new() -> Self {
        Self {
            next_node_id: 0,
            next_template_id: 0,
            next_handler_id: 0,
            templates: Vec::new(),
            commands: Vec::new(),
            handlers: std::collections::HashMap::new(),
            microtasks: RefCell::new(VecDeque::new()),
        }
    }

    pub fn commands(&self) -> &[DomCommand] {
        &self.commands
    }

    pub fn take_commands(&mut self) -> Vec<DomCommand> {
        std::mem::take(&mut self.commands)
    }

    pub fn replace_template(
        &mut self,
        template_id: TemplateId,
        template: impl Into<crate::template::Template>,
    ) -> Result<(), UnknownTemplateId> {
        let template = template.into();
        let Some(existing) = self.templates.get_mut(template_id.0 as usize) else {
            return Err(UnknownTemplateId { template_id });
        };
        *existing = template.clone();
        self.commands
            .push(DomCommand::ReplaceTemplate { template_id, template });
        Ok(())
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "templates": self.templates,
            "commands": self.commands,
        })
    }

    pub fn latest_handler(&self) -> Option<HandlerId> {
        self.handlers
            .keys()
            .copied()
            .max_by_key(|handler| handler.0)
    }

    pub fn pending_microtasks(&self) -> usize {
        self.microtasks.borrow().len()
    }

    pub fn drain_microtasks(&self) {
        loop {
            let task = self.microtasks.borrow_mut().pop_front();
            let Some(task) = task else {
                break;
            };
            task();
        }
    }
}

impl Default for CommandBufferDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl DomDriver for CommandBufferDriver {
    fn register_template(&mut self, template: crate::template::Template) -> TemplateId {
        // Content-addressed: identical templates share an id (rows re-register per
        // instance), and every genuinely new template is announced **in-band** — so the
        // stream is self-contained across the mount and every later dispatch (a branch
        // first rendered client-side still carries its template to the browser).
        if let Some(existing) = self.templates.iter().position(|t| *t == template) {
            return TemplateId(existing as u32);
        }
        let id = TemplateId(self.next_template_id);
        self.next_template_id += 1;
        self.templates.push(template.clone());
        self.commands
            .push(DomCommand::ReplaceTemplate { template_id: id, template });
        id
    }

    fn apply(&mut self, ops: Vec<DomOp>) {
        for op in ops {
            if let DomOp::RemoveEventListener { handler_id, .. } = &op {
                self.handlers.remove(handler_id);
            }
            self.commands.push(op.into());
        }
    }

    fn alloc_node_id(&mut self) -> NodeId {
        let id = NodeId(self.next_node_id);
        self.next_node_id += 1;
        id
    }

    fn alloc_slot_node_id(&mut self, slot: SlotId) -> NodeId {
        // Make the slot→node resolution explicit on the wire so a pure JS replay can map
        // `node_id → DOM node` from the template's `data-s` markers.
        let node_id = self.alloc_node_id();
        self.commands.push(DomCommand::BindSlot { slot, node_id });
        node_id
    }

    fn alloc_handler_id(&mut self) -> HandlerId {
        let id = HandlerId(self.next_handler_id);
        self.next_handler_id += 1;
        id
    }

    fn register_event_handler(&mut self, id: HandlerId, handler: EventHandler) {
        self.handlers.insert(id, handler);
    }

    fn dispatch_event(&self, handler_id: HandlerId, event: Event) {
        if let Some(handler) = self.handlers.get(&handler_id) {
            handler.call(event);
        }
    }

    fn schedule_microtask(&self, f: Box<dyn FnOnce()>) {
        self.microtasks.borrow_mut().push_back(f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dom_op_converts_to_serializable_command() {
        let command = DomCommand::from(DomOp::SetAttr {
            node_id: NodeId(7),
            name: "aria-label",
            value: "Close".to_string(),
        });

        assert_eq!(
            serde_json::to_value(&command).unwrap(),
            serde_json::json!({
                "SetAttr": {
                    "node_id": 7,
                    "name": "aria-label",
                    "value": "Close"
                }
            })
        );
    }

    #[test]
    fn command_buffer_driver_records_commands_and_removes_handlers() {
        let mut driver = CommandBufferDriver::new();
        let node_id = driver.alloc_node_id();
        let handler_id = driver.alloc_handler_id();
        driver.register_event_handler(handler_id, EventHandler::single(Rc::new(|_| {})));

        driver.apply(vec![
            DomOp::AddEventListener {
                node_id,
                event_type: "click",
                handler_id,
            },
            DomOp::RemoveEventListener {
                node_id,
                event_type: "click",
                handler_id,
            },
        ]);

        assert_eq!(driver.latest_handler(), None);
        assert_eq!(
            driver.commands(),
            &[
                DomCommand::AddEventListener {
                    node_id,
                    event_type: "click".into(),
                    handler_id
                },
                DomCommand::RemoveEventListener {
                    node_id,
                    event_type: "click".into(),
                    handler_id
                }
            ]
        );
    }

    fn tpl(tag: &'static str) -> crate::template::Template {
        crate::template::Template::from(vec![crate::template::TplNode::Element {
            tag: std::borrow::Cow::Borrowed(tag),
            attrs: std::borrow::Cow::Borrowed(&[]),
            slot: Some(SlotId(0)),
            children: 0,
        }])
    }

    #[test]
    fn command_buffer_driver_replaces_registered_template() {
        let mut driver = CommandBufferDriver::new();
        let template_id = driver.register_template(tpl("p"));

        driver.replace_template(template_id, tpl("section")).unwrap();

        assert_eq!(driver.templates, vec![tpl("section")]);
        // Registration itself is in-band (self-contained stream), then the replacement.
        assert_eq!(
            driver.commands(),
            &[
                DomCommand::ReplaceTemplate { template_id, template: tpl("p") },
                DomCommand::ReplaceTemplate { template_id, template: tpl("section") },
            ]
        );
    }

    #[test]
    fn command_buffer_driver_rejects_unknown_template_replacement() {
        let mut driver = CommandBufferDriver::new();

        let err = driver.replace_template(TemplateId(9), tpl("p")).unwrap_err();

        assert_eq!(
            err,
            UnknownTemplateId {
                template_id: TemplateId(9)
            }
        );
    }
}
