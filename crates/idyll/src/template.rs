//! The **Template IR** — a template as typed data, not markup.
//!
//! The `live_view!` DSL is idyll's source language and the macro is the *one parser in the
//! system*: it parses at compile time and emits this IR as `const` data. From there the
//! template is data all the way down — the server fold walks it into its arena, the WIT
//! boundary carries it as a flat node list (the component model has no recursive types,
//! so the tree ships **pre-order with child counts** — the standard flat serialization),
//! and the browser fold materializes DOM from it directly (`createElement`, never
//! `innerHTML`). HTML exists only as an *output* format at the server's edge; nothing in
//! the framework re-parses it.
//!
//! Slots are **first-class node kinds** ([`TplNode::TextSlot`], [`TplNode::AnchorSlot`],
//! and the `slot` field on elements) — not markers smuggled through HTML's own signal set
//! (`data-s` attributes, wrapper elements, comment payloads). Parse, don't validate: an
//! IR value cannot represent an unaddressed or ambiguous dynamic position.

use std::borrow::Cow;

use crate::driver::SlotId;

/// One static attribute. `Cow` so the macro can emit `const` borrows while runtime
/// constructors (e.g. the live wrapper, which interpolates its name) can own.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TplAttr {
    pub name: Cow<'static, str>,
    pub value: Cow<'static, str>,
}

impl TplAttr {
    pub fn new(name: impl Into<Cow<'static, str>>, value: impl Into<Cow<'static, str>>) -> TplAttr {
        TplAttr { name: name.into(), value: value.into() }
    }
}

/// One style rule a template's elements reference by class (`css=[…]` in `live_view!`).
///
/// `name` is the class — its identity is the *declaration site* (file + `css!` name +
/// property + condition), never the value, so editing a value changes no template. `css`
/// is the full rule text: `Some` when the rule was rendered where values exist (native
/// code), `None` from the guest — wasm carries no style values, and the server joins the
/// name against the app's `StyleTable` (idyll-styles) wherever the rule is emitted.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StyleRule {
    pub name: Cow<'static, str>,
    pub css: Option<Cow<'static, str>>,
}

/// Union `src` into `dst` by rule name. A resolved text (`Some`) upgrades an unresolved
/// entry — the two can meet when a native render and a guest template carry the same
/// declaration — and rule text for one name never conflicts within a build (the name
/// hashes the declaration site).
pub(crate) fn union_styles(dst: &mut Vec<StyleRule>, src: &[StyleRule]) {
    for rule in src {
        match dst.iter_mut().find(|have| have.name == rule.name) {
            Some(have) => {
                if have.css.is_none() {
                    have.css = rule.css.clone();
                }
            }
            None => dst.push(rule.clone()),
        }
    }
}

/// One node of the flat, pre-order template tree.
///
/// An element's `children` counts its **direct** children; the subtree extent follows
/// from walking (pre-order + counts fully determine the tree).
///
/// The serde form is deliberately the **same shape jco gives `runtime.js`** for the WIT
/// `tpl-node` variant — `{"tag": "text-slot", "val": …}` — so a template has ONE wire
/// format whether it crosses the component-model boundary or rides inside a
/// [`View`] value: the browser fold walks both without translation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "tag", content = "val", rename_all = "kebab-case")]
pub enum TplNode {
    Element {
        tag: Cow<'static, str>,
        attrs: Cow<'static, [TplAttr]>,
        /// Bind target for dynamic attrs/events on this element, if any.
        slot: Option<SlotId>,
        /// Number of direct children (the subtree follows in pre-order).
        children: u32,
    },
    /// A static text run. The macro merges adjacent static text at compile time, so IR
    /// node boundaries equal DOM node boundaries in built DOM.
    Text(Cow<'static, str>),
    /// A dynamic text position: materialized as a real `Text` node by the folds
    /// (created in build mode; claimed — splitting merged SSR text — in claim mode).
    TextSlot(SlotId),
    /// A control-flow / child-component insertion point: materialized as an anchor
    /// (a comment node in the live DOM; a positional anchor in the HTML fold's arena).
    AnchorSlot(SlotId),
    /// An **live boundary**: the named bridge from static content to interactivity —
    /// first-class in the IR like every other framework concept (never a marker element
    /// or `data-` attribute recovered by string comparison). The folds serialize it
    /// at their edges (`<idyll-live data-i="name" data-k="key"
    /// style="display:contents">` in HTML / the DOM) and whoever splices content
    /// containing one owes it a `mount`.
    Live {
        name: Cow<'static, str>,
        /// The row identity for repeated live (see [`crate::live::IslandKey`]):
        /// mount identity is `(name, key)` when present, `(name, document-order
        /// index)` when not. Carries a record's canonical wire id, so the live
        /// resolves its own data from the store by this key.
        key: Option<Cow<'static, str>>,
        /// Direct fallback children, their subtrees following in pre-order exactly
        /// as [`TplNode::Element`]'s `children` — what stands where the live would
        /// be until a mount paints over it. A mount that refuses leaves it: the
        /// status went out with the prelude, so a fallback is the only honest thing
        /// left to send. `0` is a marker that would rather show nothing.
        fallback: u32,
    },
}

/// A template: the flat pre-order node list, plus the style rules its elements
/// reference by class. Styles ride *in* the template so every seam a template crosses —
/// `ReplaceTemplate` commands, [`View`] values, the membrane, the route payload —
/// carries them without a parallel channel; the folds union them per render.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Template {
    pub nodes: Cow<'static, [TplNode]>,
    #[serde(default, skip_serializing_if = "styles_are_empty")]
    pub styles: Cow<'static, [StyleRule]>,
    /// Whether these nodes are created in the SVG namespace.
    ///
    /// A template's own root tells the runtime nothing: a fragment's IR starts at the
    /// `@for` row's tag, and by the time the row is built its anchor is still parked
    /// off-tree — the rows are positioned *after* they are built, so the DOM cannot be
    /// asked. Only the compiler knows, from the lexical nesting at the point the
    /// template was written, so it says. Nesting *within* the template is still
    /// derived during the walk (`svg` enters, `foreignObject` leaves).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub svg: bool,
}

fn styles_are_empty(styles: &Cow<'static, [StyleRule]>) -> bool {
    styles.is_empty()
}

impl Template {
    pub const EMPTY: Template = Template {
        nodes: Cow::Borrowed(&[]),
        styles: Cow::Borrowed(&[]),
        svg: false,
    };

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn with_styles(mut self, styles: Vec<StyleRule>) -> Template {
        let mut union = self.styles.into_owned();
        union_styles(&mut union, &styles);
        self.styles = Cow::Owned(union);
        self
    }
}

impl From<&'static [TplNode]> for Template {
    fn from(nodes: &'static [TplNode]) -> Template {
        Template {
            nodes: Cow::Borrowed(nodes),
            styles: Cow::Borrowed(&[]),
            svg: false,
        }
    }
}

impl From<Vec<TplNode>> for Template {
    fn from(nodes: Vec<TplNode>) -> Template {
        Template {
            nodes: Cow::Owned(nodes),
            styles: Cow::Borrowed(&[]),
            svg: false,
        }
    }
}

/// **Content**: resolved Template IR as a plain value — the content plane's one type.
///
/// A `View` is a pure function of data by construction: it carries no slots, no
/// handlers, no closures, so nothing inside it *can* read a signal or receive an
/// event. Reactivity belongs to the component that builds it (reads tracked where the
/// `cx` is visible) and to the splice that carries it (a placed `Signal<View>` replaces the
/// content when its tracked source changes). Staticness isn't a discipline; it's the
/// type.
///
/// **Live ride inside.** A live marker is ordinary IR, so content may declare
/// interactivity: the marker crosses as data, and [`live`](Self::live) recovers
/// the names *from the IR itself* — no side-channel to drift. Whoever splices content
/// containing markers owes each a mount (`mount(name, seed)` via the app's live
/// table — the same contract everywhere).
///
/// Construction is `view! { … }`, a content builder over [`from_ir`](Self::from_ir),
/// or deserialization (the wire). A consumer splices it as-is; it cannot
/// re-parameterize it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct View(Template);

impl View {
    pub const EMPTY: View = View(Template::EMPTY);

    /// Build content from IR — the entry `view!` and programmatic content builders
    /// (a markdown mapper) use. **The no-slots invariant is enforced here**, the one
    /// construction boundary hand-built IR crosses: content is resolved by definition,
    /// so a slot in it is a bug in the builder, refused loudly rather than discovered
    /// by a fold downstream.
    pub fn from_ir(nodes: Vec<TplNode>, styles: Vec<StyleRule>) -> View {
        for node in &nodes {
            match node {
                TplNode::TextSlot(slot) | TplNode::AnchorSlot(slot) => {
                    panic!("content IR cannot carry slots (found slot {slot:?}): a slot is a live binding, and content is a value")
                }
                TplNode::Element { slot: Some(slot), .. } => {
                    panic!("content IR cannot carry slots (found element slot {slot:?}): a slot is a live binding, and content is a value")
                }
                _ => {}
            }
        }
        View(Template { nodes: Cow::Owned(nodes), styles: Cow::Owned(styles), svg: false })
    }

    /// A static text run — one DOM text node. Builders that need merged runs
    /// (IR node boundaries = DOM boundaries after a claim walk) merge *before*
    /// construction; two appended texts stay two nodes.
    pub fn text(text: impl Into<String>) -> View {
        View(Template { nodes: Cow::Owned(vec![TplNode::Text(Cow::Owned(text.into()))]), styles: Cow::Borrowed(&[]), svg: false })
    }

    /// One element wrapping `children`, with static attributes — `view!`'s element
    /// node as a plain constructor, for programmatic builders (the markdown mapper).
    pub fn element(
        tag: impl Into<Cow<'static, str>>,
        attrs: impl IntoIterator<Item = TplAttr>,
        children: View,
    ) -> View {
        let child_nodes = children.0.nodes.into_owned();
        let mut nodes = Vec::with_capacity(child_nodes.len() + 1);
        nodes.push(TplNode::Element {
            tag: tag.into(),
            attrs: Cow::Owned(attrs.into_iter().collect()),
            slot: None,
            children: root_count(&child_nodes),
        });
        nodes.extend(child_nodes);
        View(Template { nodes: Cow::Owned(nodes), styles: children.0.styles, svg: false })
    }

    /// A live mount marker — the named hole where live code mounts, identity
    /// `(name, key)`. The one inert→live transition; whoever splices content
    /// containing one owes it a mount.
    ///
    /// `fallback` is what stands in the hole until the mount paints over it, and what
    /// remains if the mount refuses. It cannot itself contain a marker: a fallback
    /// renders precisely when a mount did not happen, so a live inside one would owe
    /// a mount nobody is in a position to make.
    pub fn live_mount(name: impl Into<String>, key: Option<String>, fallback: View) -> View {
        let fallback_nodes = fallback.0.nodes.into_owned();
        assert!(
            !fallback_nodes.iter().any(|node| matches!(node, TplNode::Live { .. })),
            "a live's fallback cannot contain a live marker: the fallback renders where no mount happened, so nothing would mount it"
        );
        let mut nodes = Vec::with_capacity(fallback_nodes.len() + 1);
        nodes.push(TplNode::Live {
            name: Cow::Owned(name.into()),
            key: key.map(Cow::Owned),
            fallback: root_count(&fallback_nodes),
        });
        nodes.extend(fallback_nodes);
        View(Template { nodes: Cow::Owned(nodes), styles: fallback.0.styles, svg: false })
    }

    /// Concatenate content — `self`'s roots followed by `other`'s.
    pub fn append(mut self, other: View) -> View {
        self.0.nodes.to_mut().extend(other.0.nodes.into_owned());
        self.0.styles.to_mut().extend(other.0.styles.into_owned());
        self
    }

    /// Wrap the whole content in one element.
    pub fn wrap(mut self, tag: &'static str) -> View {
        let nodes = self.0.nodes.to_mut();
        let roots = root_count(nodes);
        nodes.insert(
            0,
            TplNode::Element {
                tag: Cow::Borrowed(tag),
                attrs: Cow::Borrowed(&[]),
                slot: None,
                children: roots,
            },
        );
        self
    }

    pub fn template(&self) -> &Template {
        &self.0
    }

    pub fn into_template(self) -> Template {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The live this content contains — `(name, key)` pairs read straight off the
    /// typed IR ([`TplNode::Live`] — the one source of truth; the flat pre-order list
    /// makes depth irrelevant). Whoever splices this content owes each a mount.
    pub fn live(&self) -> Vec<(String, Option<String>)> {
        islands_of(&self.0.nodes)
    }
}

/// The template leaf a one-shot `(expr)` interpolation occupies, chosen by its
/// value's [`SlotKind`](crate::SlotKind): text fills a [`TextSlot`](TplNode::TextSlot),
/// a view mounts at an [`AnchorSlot`](TplNode::AnchorSlot). A `const fn` so the macro
/// can select the leaf per-monomorphization inside a `const` template.
pub const fn slot_node(slot: u32, kind: crate::SlotKind) -> TplNode {
    match kind {
        crate::SlotKind::Text => TplNode::TextSlot(crate::driver::SlotId(slot)),
        crate::SlotKind::View => TplNode::AnchorSlot(crate::driver::SlotId(slot)),
    }
}

/// The number of root nodes in a flat pre-order IR list — an element's `children`
/// count for a child list built separately (`view!` emits this).
pub fn root_count(nodes: &[TplNode]) -> u32 {
    let mut count = 0u32;
    let mut cursor = 0usize;
    while cursor < nodes.len() {
        match &nodes[cursor] {
            TplNode::Element { children, .. } | TplNode::Live { fallback: children, .. } => {
                let children = *children;
                cursor += 1;
                cursor += subtree_len(&nodes[cursor..], children);
            }
            _ => cursor += 1,
        }
        count += 1;
    }
    count
}

/// Every live an IR carries, as `(name, key)` — the one scan every consumer of
/// live lists shares.
pub(crate) fn islands_of(nodes: &[TplNode]) -> Vec<(String, Option<String>)> {
    nodes
        .iter()
        .filter_map(|node| match node {
            TplNode::Live { name, key, .. } => {
                Some((name.to_string(), key.as_ref().map(|k| k.to_string())))
            }
            _ => None,
        })
        .collect()
}

/// Nodes consumed by `children` direct children (and their subtrees) starting at
/// `nodes[0]` — the pre-order walk's step size. Two node kinds carry a subtree: an
/// element's children, and a live's fallback.
pub(crate) fn subtree_len(nodes: &[TplNode], children: u32) -> usize {
    let mut cursor = 0usize;
    for _ in 0..children {
        match nodes.get(cursor) {
            Some(
                TplNode::Element { children, .. } | TplNode::Live { fallback: children, .. },
            ) => {
                let inner = *children;
                cursor += 1;
                cursor += subtree_len(&nodes[cursor..], inner);
            }
            Some(_) => cursor += 1,
            None => break,
        }
    }
    cursor
}

/// Serialization knowledge shared by every HTML emitter: void elements self-close.
pub(crate) fn is_void(tag: &str) -> bool {
    matches!(
        tag,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta"
            | "param" | "source" | "track" | "wbr"
    )
}
