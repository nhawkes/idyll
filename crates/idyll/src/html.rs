//! The **server HTML fold** of the render's DOM-command stream.
//!
//! An idyll app has one render output: **templates (typed IR — see [`crate::template`]) +
//! a [`DomCommand`] stream** (what [`CommandBufferDriver`](crate::CommandBufferDriver)
//! records). Everything that shows the app is a *fold* of that output: the browser's
//! `runtime.js` folds it into the live DOM; **this module folds the identical stream into
//! an HTML string** for SSR. One program, two folds — SSR and hydration cannot diverge
//! because the SSR HTML *is* the serialization of the exact stream the client will claim
//! against.
//!
//! Nothing here parses anything: templates arrive as data (the `live_view!` macro is the one
//! parser in the system), the fold materializes them into a tiny node arena, commands
//! fill slots and position fragments, and serialization entity-encodes exactly once at
//! the edge. HTML is an **output-only** format; the encoded domain has its own type
//! ([`Html`]) so the boundary is visible in signatures.

use std::collections::HashMap;

use crate::driver::{DomCommand, NodeId, SlotId};

/// Entity-encoded HTML markup — the **encoded domain**. Values inside the fold's arena
/// (and everywhere else in idyll) are plain decoded `String`s; `Html` is produced only at
/// the serialization boundary, where escaping happens exactly once. `Deref<str>` for
/// reading; `into_string` to cross an explicit wire boundary (e.g. the WIT document).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Html(String);

impl Html {
    /// No content — what a marker declaring no fallback contributes. The only `Html`
    /// constructible from outside this module: encoded HTML otherwise comes from a
    /// fold, so there is no raw-splice door here to walk through.
    pub const EMPTY: Html = Html(String::new());

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::ops::Deref for Html {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Html {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A node in the fold's arena.
enum Node {
    /// An element: `<tag attrs>children</tag>` (or void, no children).
    Element {
        tag: String,
        attrs: Vec<(String, String)>,
        children: Vec<usize>,
        void: bool,
    },
    /// A text node — static text, or the target of a `SetText` (a materialized text slot).
    Text(String),
    /// A control-flow insertion point (an anchor slot) or a fragment anchor. Serializes
    /// as its children only — the anchor itself is scaffolding. A detached anchor (a
    /// `keep` branch switched away) keeps its children but serializes nothing until
    /// re-attached — the fold's mirror of the browser's detach/attach cycle.
    Anchor { children: Vec<usize>, detached: bool },
    /// A live boundary ([`TplNode::Live`](crate::template::TplNode::Live)):
    /// live-ness stays typed through the arena (so folding back to IR preserves it);
    /// the HTML edge serializes it as its addressable wrapper element. `fallback` is
    /// what the wrapper carries when nothing painted into it.
    Live { name: String, key: Option<String>, fallback: Vec<usize> },
}

/// The stateful fold: feed it the app's templates (IR) and its command stream, read the
/// HTML. The browser's `runtime.js` implements this same fold against the live DOM (in
/// claim mode on first load); the command-stream contract both rely on is pinned by
/// `examples/todo/app/tests/command_stream.rs`.
pub struct HtmlFold {
    /// Registered templates by id. `None` is a hole left by out-of-order ids — naming
    /// one is an emitter bug, distinct from a registered-but-empty template.
    templates: Vec<Option<crate::template::Template>>,
    /// Union of every ingested template's style rules — templates arrive in-band, so
    /// after a mount this is the complete rule set the folded content references.
    styles: Vec<crate::template::StyleRule>,
    arena: Vec<Node>,
    roots: Vec<usize>,
    /// Whether an explicit `MountRoot` has materialized the document root — so an empty
    /// root view can't let a later row template usurp the document before it arrives.
    root_materialized: bool,
    /// Pre-folded live paint, injected inside each live wrapper at serialization
    /// (composition by construction — never string surgery on emitted HTML). Keyed by
    /// the live's **mount identity** — `(name, per-name occurrence index in document
    /// order)` — so the same live component twice on a page splices distinct paint.
    /// The route pipeline fills this from membrane `mount` streams.
    island_content: HashMap<(String, u32), Html>,
    /// `NodeId` → arena index. Several node ids can map to one element (one per binding).
    node_map: HashMap<NodeId, usize>,
    /// `parent[child] = parent` — for `MoveFragment` relocation.
    parent: HashMap<usize, usize>,
    /// Slots of the **most recently ingested** template instance. One transient map
    /// suffices: the stream binds an instance's slots immediately after mounting it,
    /// before the next (contract pinned in `command_stream.rs`).
    slot_scratch: HashMap<SlotId, usize>,
}

impl HtmlFold {
    /// Start the fold. Templates arrive **in-band** (`ReplaceTemplate` commands — the
    /// stream is fully self-contained) and are only stored; the document root is
    /// materialized when an explicit `MountRoot` names its template (never inferred from
    /// registration order or `id 0`), so the stream's `BindSlot`s resolve against it.
    pub fn new() -> Self {
        HtmlFold {
            templates: Vec::new(),
            styles: Vec::new(),
            arena: Vec::new(),
            roots: Vec::new(),
            root_materialized: false,
            island_content: HashMap::new(),
            node_map: HashMap::new(),
            parent: HashMap::new(),
            slot_scratch: HashMap::new(),
        }
    }

    /// Provide one live **instance**'s pre-folded paint (from a membrane `mount`
    /// stream); it is written inside that instance's wrapper at serialization. The
    /// instance index is the per-name occurrence in document order — the same identity
    /// every fold derives by walking the same tree in the same order.
    pub fn set_island_content(&mut self, name: impl Into<String>, instance: u32, content: Html) {
        self.island_content.insert((name.into(), instance), content);
    }

    /// Fold one command into the document.
    pub fn apply(&mut self, command: &DomCommand) {
        match command {
            DomCommand::ReplaceTemplate { template_id, template } => {
                crate::template::union_styles(&mut self.styles, &template.styles);
                let idx = template_id.0 as usize;
                if self.templates.len() <= idx {
                    self.templates.resize(idx + 1, None);
                }
                self.templates[idx] = Some(template.clone());
            }
            // The stream's explicit root announcement: materialize the named
            // template as the document. (Never inferred from registration order —
            // templates are content-addressed, so a remount's root registers
            // nothing; and ids are global in the shared runtime, so `id 0` means
            // nothing either.)
            DomCommand::MountRoot { template_id } => {
                if !self.root_materialized {
                    self.root_materialized = true;
                    let root = self.registered_template(*template_id, "MountRoot");
                    self.roots = self.ingest(&root);
                }
            }
            DomCommand::BindSlot { slot, node_id } => {
                let Some(&idx) = self.slot_scratch.get(slot) else {
                    panic!(
                        "BindSlot({}) does not resolve against the current template's slots — \
                         binds follow their template's instantiation (stream contract 2)",
                        slot.0
                    );
                };
                self.node_map.insert(*node_id, idx);
            }
            DomCommand::SetText { node_id, text } => {
                let idx = self.bound(*node_id, "SetText");
                self.arena[idx] = Node::Text(text.clone());
            }
            DomCommand::SetAttr { node_id, name, value } => {
                let attrs = self.bound_attrs(*node_id, "SetAttr");
                attrs.retain(|(n, _)| n != name);
                attrs.push((name.clone().into_owned(), value.clone()));
            }
            DomCommand::SetStyleProp { node_id, name, value } => {
                // SSR has no CSSOM: the declaration merges into the serialized
                // `style` attribute (last write per property wins, like setProperty).
                let attrs = self.bound_attrs(*node_id, "SetStyleProp");
                // Carry the position rather than pushing and then asking the
                // vec where the thing went.
                let at = match attrs.iter().position(|(n, _)| n == "style") {
                    Some(at) => at,
                    None => {
                        attrs.push(("style".to_string(), String::new()));
                        attrs.len() - 1
                    }
                };
                let style = &mut attrs[at].1;
                let mut decls: Vec<(String, String)> = style
                    .split(';')
                    .filter_map(|d| {
                        let (p, v) = d.split_once(':')?;
                        Some((p.trim().to_string(), v.trim().to_string()))
                    })
                    .collect();
                decls.retain(|(p, _)| p != name);
                if !value.is_empty() {
                    decls.push((name.clone().into_owned(), value.clone()));
                }
                *style = decls
                    .iter()
                    .map(|(p, v)| format!("{p}:{v}"))
                    .collect::<Vec<_>>()
                    .join(";");
            }
            DomCommand::RemoveAttr { node_id, name } => {
                let attrs = self.bound_attrs(*node_id, "RemoveAttr");
                attrs.retain(|(n, _)| n != name);
            }
            DomCommand::SetBoolAttr { node_id, name, value } => {
                let attrs = self.bound_attrs(*node_id, "SetBoolAttr");
                attrs.retain(|(n, _)| n != name);
                if *value {
                    attrs.push((name.clone().into_owned(), String::new()));
                }
            }
            DomCommand::MountFragment { anchor_id, template }
            | DomCommand::ReplaceFragment { anchor_id, template } => {
                let anchor = self.anchor_idx(*anchor_id);
                let tpl = self.registered_template(*template, "MountFragment");
                let kids = self.ingest(&tpl);
                for &k in &kids {
                    self.parent.insert(k, anchor);
                }
                if let Node::Anchor { children, .. } = &mut self.arena[anchor] {
                    *children = kids;
                }
            }
            DomCommand::RemoveFragment { anchor_id } => {
                let idx = self.bound(*anchor_id, "RemoveFragment");
                self.detach(idx);
            }
            DomCommand::MoveFragment { anchor_id, after_anchor } => {
                let idx = self.anchor_idx(*anchor_id);
                // The after-anchor is a *reference*, never an introduction: it names a
                // sibling that already exists (stream contract 3).
                let after_idx = self.bound(*after_anchor, "MoveFragment.after_anchor");
                if let Some(&parent) = self.parent.get(&after_idx) {
                    self.detach(idx);
                    self.insert_after(parent, after_idx, idx);
                } else {
                    // A **top-level** region: the after-anchor sits among the
                    // document/live roots rather than inside an element, so the
                    // row inserts into the roots list itself. (The browser fold
                    // needs no special case — there the live wrapper is a real
                    // parent node.)
                    let pos = self
                        .roots
                        .iter()
                        .position(|&r| r == after_idx)
                        .unwrap_or_else(|| {
                            panic!("MoveFragment after {after_anchor:?}, which is neither parented nor a root")
                        });
                    self.detach(idx);
                    self.roots.retain(|&r| r != idx);
                    self.roots.insert(pos + 1, idx);
                }
            }
            DomCommand::DetachFragment { anchor_id } => {
                let idx = self.bound(*anchor_id, "DetachFragment");
                match &mut self.arena[idx] {
                    Node::Anchor { detached, .. } => *detached = true,
                    _ => panic!("DetachFragment on {anchor_id:?}, which is not an anchor"),
                }
            }
            DomCommand::AttachFragment { anchor_id } => {
                let idx = self.bound(*anchor_id, "AttachFragment");
                match &mut self.arena[idx] {
                    Node::Anchor { detached, .. } => *detached = false,
                    _ => panic!("AttachFragment on {anchor_id:?}, which is not an anchor"),
                }
            }
            // A frame's id release: the ids stop resolving, so a later consuming op
            // on one refuses like any unknown id — the free-last invariant is
            // enforced here exactly as in the browser fold, which is what lets a
            // Rust test catch an emission that frees too early.
            DomCommand::FreeNodes { node_ids } => {
                for node_id in node_ids {
                    self.node_map.remove(node_id);
                }
            }
            // Event wiring, host round-trips, and the subscriptions are **client**
            // concerns: the server paint discards them — the same bindings re-fire on
            // the client mount (a server mount still paints tick-0 state via ordinary
            // bindings).
            DomCommand::AddEventListener { .. }
            | DomCommand::RemoveEventListener { .. }
            | DomCommand::WatchMeasure { .. }
            | DomCommand::UnwatchMeasure { .. }
            | DomCommand::ServerRequest { .. }
            | DomCommand::Navigate { .. }
            | DomCommand::WatchNavigation { .. }
            | DomCommand::WatchSize { .. }
            | DomCommand::StartTicks { .. }
            | DomCommand::StopTicks { .. }
            // A display list has no HTML at all: the served `<canvas>` is blank and the
            // client mount's own first paint fills it, which is why a view that paints
            // one is never a static paint (`WiredView::blocks_static_paint`).
            | DomCommand::Paint { .. } => {}
        }
    }

    /// Serialize the folded document back to **resolved Template IR** — the same walk as
    /// [`html`](Self::html), but the output stays typed data. Slots don't survive: text
    /// slots were painted into text, attribute bindings into plain attributes, and
    /// anchors are scaffolding — they inline their children into the parent's direct
    /// child count. The result is a template with no dynamic positions: a
    /// [`View`](crate::template::View) component's payload.
    pub fn to_template(&self) -> crate::template::Template {
        let mut nodes = Vec::new();
        for &root in &self.roots {
            self.emit_ir(root, &mut nodes);
        }
        // The union of every ingested template's rules rides out with the resolved IR,
        // so a `View` carries the styles of everything its render touched —
        // including `@if`/`@for` branch templates the initial state never materialized.
        crate::template::Template::from(nodes).with_styles(self.styles.clone())
    }

    /// The style rules of every template this fold has ingested (deduped by name) —
    /// what a route pipeline joins and emits ahead of the content that references them.
    pub fn styles(&self) -> &[crate::template::StyleRule] {
        &self.styles
    }

    /// Emit one arena node as IR; returns how many **top-level** nodes it contributed
    /// (an anchor contributes its children's count; empty text contributes none).
    fn emit_ir(&self, idx: usize, out: &mut Vec<crate::template::TplNode>) -> u32 {
        use crate::template::{TplAttr, TplNode};
        match &self.arena[idx] {
            Node::Text(text) => {
                if text.is_empty() {
                    return 0;
                }
                out.push(TplNode::Text(text.clone().into()));
                1
            }
            Node::Anchor { children, detached } => {
                if *detached {
                    return 0;
                }
                let mut count = 0;
                for &c in children {
                    count += self.emit_ir(c, out);
                }
                count
            }
            Node::Live { name, key, fallback } => {
                // The header's fallback count is patched after its subtree emits, the
                // same shape `Node::Element` uses below.
                let header = out.len();
                out.push(TplNode::Text("".into()));
                let mut count = 0;
                for &c in fallback {
                    count += self.emit_ir(c, out);
                }
                out[header] = TplNode::Live {
                    name: name.clone().into(),
                    key: key.clone().map(Into::into),
                    fallback: count,
                };
                1
            }
            Node::Element { tag, attrs, children, .. } => {
                // Children emit first (their count patches the header afterwards).
                let header = out.len();
                out.push(TplNode::Text("".into()));
                let mut count = 0;
                for &c in children {
                    count += self.emit_ir(c, out);
                }
                out[header] = TplNode::Element {
                    tag: tag.clone().into(),
                    attrs: attrs
                        .iter()
                        .map(|(name, value)| TplAttr {
                            name: name.clone().into(),
                            value: value.clone().into(),
                        })
                        .collect::<Vec<_>>()
                        .into(),
                    slot: None,
                    children: count,
                };
                1
            }
        }
    }

    /// Serialize the folded document to clean, entity-encoded HTML, splicing each
    /// live instance's pre-supplied paint ([`set_island_content`](Self::set_island_content))
    /// into its slot. One serialization walk exists — [`segments`](Self::segments);
    /// this is its everything-known-up-front fold.
    pub fn html(&self) -> Html {
        let mut out = String::new();
        for segment in self.segments() {
            match segment {
                BodySegment::Html(html) => out.push_str(html.as_str()),
                BodySegment::Live { name, instance, key, fallback } => {
                    // Everything-known-up-front: no mount happened here, so no island is
                    // claimed static — the browser mounts each wrapper as usual.
                    out.push_str(&live_wrapper_open(&name, key.as_deref(), false));
                    match self.island_content.get(&(name, instance)) {
                        // Already-encoded HTML from the live's own fold — composed,
                        // not re-escaped.
                        Some(content) => out.push_str(content.as_str()),
                        // Nothing painted into this wrapper, so the marker's fallback is
                        // what it carries.
                        None => out.push_str(fallback.as_str()),
                    }
                    out.push_str(LIVE_WRAPPER_CLOSE);
                }
            }
        }
        Html(out)
    }

    /// Serialize the folded document as **segments**: encoded HTML runs, cut at each
    /// live's paint slot (just inside its wrapper's closing tag). A streaming
    /// consumer writes each HTML run to the wire, then mounts `(name, instance)` and
    /// writes the paint — the browser parses the shell while the server is still
    /// mounting. The walk IS document order, so the per-name occurrence count here
    /// derives the same mount identity as every other fold.
    pub fn segments(&self) -> Vec<BodySegment> {
        let mut sink = SegmentSink::default();
        let mut instances: HashMap<&str, u32> = HashMap::new();
        for &root in &self.roots {
            self.write_node(root, &mut sink, &mut instances);
        }
        sink.finish()
    }

    /// Serialize one arena node. The single place decoded values cross into the encoded
    /// domain — spec entity encoding via `html-escape`, applied exactly once.
    fn write_node<'a>(
        &'a self,
        idx: usize,
        sink: &mut SegmentSink,
        instances: &mut HashMap<&'a str, u32>,
    ) {
        match &self.arena[idx] {
            Node::Text(text) => sink.buf.push_str(&html_escape::encode_text(text)),
            Node::Anchor { children, detached } => {
                if *detached {
                    return;
                }
                for &c in children {
                    self.write_node(c, sink, instances);
                }
            }
            // A live boundary is a **cut**, not markup: the wrapper element is written
            // by whoever composes the paint into it ([`live_wrapper_open`]/[`html`], the
            // streaming server), because whether the island is a static paint is knowable
            // only where the mount happened, not here in the parent's fold.
            Node::Live { name, key, fallback } => {
                let instance = {
                    let counter = instances.entry(name.as_str()).or_insert(0);
                    let instance = *counter;
                    *counter += 1;
                    instance
                };
                // The fallback serializes through this same fold, into its own sink, so
                // it is encoded exactly once and cannot be cut by a live of its own.
                let mut nested = SegmentSink::default();
                for &c in fallback {
                    self.write_node(c, &mut nested, instances);
                }
                sink.live(name.clone(), instance, key.clone(), Html(nested.buf));
            }
            Node::Element {
                tag,
                attrs,
                children,
                void,
            } => {
                sink.buf.push('<');
                sink.buf.push_str(tag);
                for (name, value) in attrs {
                    sink.buf.push(' ');
                    sink.buf.push_str(name);
                    if !value.is_empty() {
                        sink.buf.push_str("=\"");
                        sink.buf
                            .push_str(&html_escape::encode_double_quoted_attribute(value));
                        sink.buf.push('"');
                    }
                }
                sink.buf.push('>');
                if !void {
                    for &c in children {
                        self.write_node(c, sink, instances);
                    }
                    sink.buf.push_str("</");
                    sink.buf.push_str(tag);
                    sink.buf.push('>');
                }
            }
        }
    }

    /// Materialize a template's IR into the arena, recording its slots into
    /// `slot_scratch` (replacing the previous instance's). Returns the top-level indices.
    /// A pure walk of typed data — nothing is parsed.
    fn ingest(&mut self, template: &crate::template::Template) -> Vec<usize> {
        self.slot_scratch.clear();
        let nodes = &template.nodes;
        let mut roots = Vec::new();
        let mut cursor = 0;
        while cursor < nodes.len() {
            roots.push(self.insert_ir(nodes, &mut cursor));
        }
        roots
    }

    /// Insert one IR node (and its subtree, via the pre-order child counts).
    fn insert_ir(&mut self, nodes: &[crate::template::TplNode], cursor: &mut usize) -> usize {
        use crate::template::TplNode;
        let node = &nodes[*cursor];
        *cursor += 1;
        match node {
            TplNode::Text(text) => self.push(Node::Text(text.to_string())),
            TplNode::TextSlot(slot) => {
                let idx = self.push(Node::Text(String::new()));
                self.slot_scratch.insert(*slot, idx);
                idx
            }
            TplNode::AnchorSlot(slot) => {
                let idx = self.push(Node::Anchor { children: Vec::new(), detached: false });
                self.slot_scratch.insert(*slot, idx);
                idx
            }
            TplNode::Element { tag, attrs, slot, children } => {
                let idx = self.push(Node::Element {
                    tag: tag.to_string(),
                    attrs: attrs
                        .iter()
                        .map(|attr| (attr.name.to_string(), attr.value.to_string()))
                        .collect(),
                    children: Vec::new(),
                    void: crate::template::is_void(tag),
                });
                let child_idxs: Vec<usize> = (0..*children)
                    .map(|_| self.insert_ir(nodes, cursor))
                    .collect();
                for &c in &child_idxs {
                    self.parent.insert(c, idx);
                }
                if let Node::Element { children, .. } = &mut self.arena[idx] {
                    *children = child_idxs;
                }
                if let Some(slot) = slot {
                    self.slot_scratch.insert(*slot, idx);
                }
                idx
            }
            TplNode::Live { name, key, fallback } => {
                let idx = self.push(Node::Live {
                    name: name.to_string(),
                    key: key.as_ref().map(|k| k.to_string()),
                    fallback: Vec::new(),
                });
                let fallback_idxs: Vec<usize> =
                    (0..*fallback).map(|_| self.insert_ir(nodes, cursor)).collect();
                for &c in &fallback_idxs {
                    self.parent.insert(c, idx);
                }
                if let Node::Live { fallback, .. } = &mut self.arena[idx] {
                    *fallback = fallback_idxs;
                }
                idx
            }
        }
    }

    fn push(&mut self, node: Node) -> usize {
        self.arena.push(node);
        self.arena.len() - 1
    }

    /// The arena node a node id refers to, creating a floating `Anchor` on first
    /// reference (fragment anchors are freshly allocated ids; they materialize on first
    /// structural use and get positioned by `MoveFragment`).
    fn anchor_idx(&mut self, id: NodeId) -> usize {
        if let Some(&idx) = self.node_map.get(&id) {
            return idx;
        }
        let idx = self.push(Node::Anchor { children: Vec::new(), detached: false });
        self.node_map.insert(id, idx);
        idx
    }

    /// The arena node a consuming command targets. The stream contract
    /// (`examples/todo/app/tests/command_stream.rs`) introduces every consumed id
    /// before use; a miss is a bug in the **emitter**, and the fold refuses to paint
    /// a wrong document rather than skip the op — fail loud, never silently misclaim.
    /// (Anchor *introduction* sites go through [`anchor_idx`], which materializes —
    /// that is contract point 3, not leniency.)
    fn bound(&self, id: NodeId, command: &str) -> usize {
        match self.node_map.get(&id) {
            Some(&idx) => idx,
            None => panic!("{command} targets {id:?}, which the stream never introduced"),
        }
    }

    /// [`bound`], refined to an element's attributes — attribute commands bind to
    /// element slots by construction, so anything else in the arena is an emitter bug.
    fn bound_attrs(&mut self, id: NodeId, command: &str) -> &mut Vec<(String, String)> {
        let idx = self.bound(id, command);
        match &mut self.arena[idx] {
            Node::Element { attrs, .. } => attrs,
            _ => panic!("{command} targets {id:?}, which is not an element"),
        }
    }

    /// The registered template a structural command names — templates travel in-band
    /// (`ReplaceTemplate`) before anything instantiates them (stream contract 1).
    fn registered_template(
        &self,
        id: crate::driver::TemplateId,
        command: &str,
    ) -> crate::template::Template {
        self.templates
            .get(id.0 as usize)
            .and_then(Option::as_ref)
            .cloned()
            .unwrap_or_else(|| panic!("{command} names unregistered {id:?}"))
    }

    fn detach(&mut self, idx: usize) {
        match self.parent.get(&idx) {
            Some(&p) => match &mut self.arena[p] {
                Node::Element { children, .. } | Node::Anchor { children, .. } => {
                    children.retain(|&c| c != idx);
                }
                Node::Text(_) | Node::Live { .. } => {}
            },
            // A **top-level** anchor (a child spliced among the document/live roots)
            // detaches from the roots list itself — the same special case `MoveFragment`
            // needs, because here the root region has no parent node standing in for it.
            None => self.roots.retain(|&r| r != idx),
        }
        self.parent.remove(&idx);
    }

    fn insert_after(&mut self, parent: usize, after: usize, idx: usize) {
        match &mut self.arena[parent] {
            Node::Element { children, .. } | Node::Anchor { children, .. } => {
                // The common case appends after the last child (a run of fragment moves in
                // order) — an O(1) push, so N moves stay O(N) instead of scanning the
                // sibling list each time. Only a genuine mid-list move pays the search.
                if children.last() == Some(&after) {
                    children.push(idx);
                } else {
                    let pos = children.iter().position(|&c| c == after).map(|p| p + 1);
                    children.insert(pos.unwrap_or(children.len()), idx);
                }
            }
            Node::Text(_) | Node::Live { .. } => {}
        }
        self.parent.insert(idx, parent);
    }
}

impl Default for HtmlFold {
    fn default() -> Self {
        Self::new()
    }
}

/// The closing tag of a live boundary's wrapper. Paired with [`live_wrapper_open`].
pub const LIVE_WRAPPER_CLOSE: &str = "</idyll-live>";

/// Open a live boundary's HTML edge: an addressable, layout-neutral wrapper the browser
/// finds by tag and claims. The one place this markup is spelled, so the non-streaming
/// fold ([`HtmlFold::html`]) and the streaming server cannot drift and the browser claims
/// the identical element. `static_paint` stamps `data-static` — the island wired no client
/// work and depends on nothing outside its seed, so the browser adopts this paint as-is
/// and never re-runs it. It is knowable only where the mount ran, so the non-streaming
/// fold always passes `false`.
pub fn live_wrapper_open(name: &str, key: Option<&str>, static_paint: bool) -> String {
    let mut tag = String::from("<idyll-live data-i=\"");
    tag.push_str(&html_escape::encode_double_quoted_attribute(name));
    tag.push('"');
    if let Some(key) = key {
        tag.push_str(" data-k=\"");
        tag.push_str(&html_escape::encode_double_quoted_attribute(key));
        tag.push('"');
    }
    if static_paint {
        tag.push_str(" data-static");
    }
    tag.push_str(" style=\"display:contents\">");
    tag
}

/// One piece of a serialized document body, in document order.
#[derive(Debug, Clone, PartialEq)]
pub enum BodySegment {
    /// A run of encoded HTML, ready for the wire.
    Html(Html),
    /// A live instance's paint slot (just inside its wrapper's closing tag): the
    /// consumer supplies the pre-folded mount stream for this mount identity here.
    Live {
        name: String,
        instance: u32,
        key: Option<String>,
        /// What the wrapper carries when the consumer has no paint to put in it —
        /// already encoded, by this same fold, from the marker's fallback subtree.
        fallback: Html,
    },
}

/// Accumulates HTML runs and cuts them into [`BodySegment`]s at live paint slots.
#[derive(Default)]
struct SegmentSink {
    buf: String,
    segments: Vec<BodySegment>,
}

impl SegmentSink {
    fn live(&mut self, name: String, instance: u32, key: Option<String>, fallback: Html) {
        if !self.buf.is_empty() {
            self.segments.push(BodySegment::Html(Html(std::mem::take(&mut self.buf))));
        }
        self.segments.push(BodySegment::Live { name, instance, key, fallback });
    }

    fn finish(mut self) -> Vec<BodySegment> {
        if !self.buf.is_empty() {
            self.segments.push(BodySegment::Html(Html(self.buf)));
        }
        self.segments
    }
}

/// Fold a complete (self-contained) command stream to HTML in one call.
pub fn fold_html(commands: &[DomCommand]) -> Html {
    let mut fold = HtmlFold::new();
    for command in commands {
        fold.apply(command);
    }
    fold.html()
}

#[cfg(test)]
mod strictness {
    use super::*;
    use crate::driver::{NodeId, TemplateId};

    // The fold asserts the stream contract instead of tolerating violations: an op on
    // an id the stream never introduced would paint a wrong document, and a wrong
    // document silently is the one outcome worse than a stop.

    #[test]
    #[should_panic(expected = "never introduced")]
    fn a_consuming_op_on_an_unintroduced_id_is_refused() {
        let mut fold = HtmlFold::new();
        fold.apply(&DomCommand::SetText { node_id: NodeId(7), text: "orphan".into() });
    }

    #[test]
    #[should_panic(expected = "unregistered")]
    fn a_mount_naming_an_unregistered_template_is_refused() {
        let mut fold = HtmlFold::new();
        fold.apply(&DomCommand::MountFragment {
            anchor_id: NodeId(1),
            template: TemplateId(9),
        });
    }

    #[test]
    #[should_panic(expected = "never introduced")]
    fn a_move_after_an_unintroduced_anchor_is_refused() {
        let mut fold = HtmlFold::new();
        fold.apply(&DomCommand::MoveFragment { anchor_id: NodeId(1), after_anchor: NodeId(2) });
    }
}

/// Serialize a **resolved** template (a [`View`](crate::template::View)'s IR —
/// no live slots) straight to HTML, injecting each live **instance**'s pre-folded
/// paint inside its wrapper. The route pipeline's native fold: no wasm renders a page
/// body — only the live inside it were mounted (through the membrane), and their
/// streams arrive here already folded, keyed by mount identity.
pub fn view_html(
    rendered: &crate::template::View,
    island_content: impl IntoIterator<Item = ((String, u32), Html)>,
) -> Html {
    let mut fold = HtmlFold::new();
    for ((name, instance), content) in island_content {
        fold.set_island_content(name, instance, content);
    }
    fold.apply(&DomCommand::ReplaceTemplate {
        template_id: crate::driver::TemplateId(0),
        template: rendered.template().clone(),
    });
    fold.apply(&DomCommand::MountRoot { template_id: crate::driver::TemplateId(0) });
    fold.html()
}

/// Serialize a **resolved** template as [`BodySegment`]s — the streaming form of
/// [`view_html`]: HTML runs cut at each live's paint slot, so a server can put
/// the shell on the wire before mounting a single live and splice each paint as its
/// mount completes.
pub fn view_segments(rendered: &crate::template::View) -> Vec<BodySegment> {
    let mut fold = HtmlFold::new();
    fold.apply(&DomCommand::ReplaceTemplate {
        template_id: crate::driver::TemplateId(0),
        template: rendered.template().clone(),
    });
    fold.apply(&DomCommand::MountRoot { template_id: crate::driver::TemplateId(0) });
    fold.segments()
}

// Content ([`View`]) is built eagerly by `view!` and programmatic IR
// builders — it never renders through a mount, because there is nothing live in it
// to run. The folds here consume command streams from real mounts (the membrane's
// page/live paints); `view_html` serializes content the host already holds.
