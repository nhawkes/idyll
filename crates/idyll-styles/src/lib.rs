//! Typed CSS for idyll — StyleX's model with **declaration-site class identity**.
//!
//! A [`Style`] is `const` data: atoms of (class, property, condition), built by the
//! `#[styles]` attribute from a fixed typed property table (the grammar lives in
//! `idyll-styles-parse`, shared with the source extractor). The class name hashes the
//! *declaration site* (workspace-relative file + const name + property + condition) —
//! never the value — and on wasm targets the rule text doesn't exist at all
//! ([`Atom`]'s `rule` field is native-only). Two consequences the whole framework
//! leans on:
//!
//! - editing a value changes zero markup, so a stylesheet swap restyles live content
//!   (stateful live included) without touching the DOM;
//! - a style-value edit produces byte-identical wasm, so the content-addressed asset
//!   manifest proves no client reload is needed.
//!
//! Composition is [`merge`]: last-wins per (property, condition), resolved at merge
//! time — atoms are single-class selectors of equal specificity, so stylesheet order
//! never matters. Rule *text* exists where values exist: natively rendered rules carry
//! it inline; guest-emitted rules carry only names, joined against the extracted
//! [`StyleTable`] server-side.

use std::borrow::Cow;
use std::marker::PhantomData;

use idyll::StyleRule;

#[cfg(not(target_arch = "wasm32"))]
pub mod extract;

// The typed style surface: `#[styles] mod styles { … }` holds the `css! {{ … }}`
// consts; `props!` declares constrained property sets.
pub use idyll_macros::{props, styles};

// ── Atoms ─────────────────────────────────────────────────────────────────────

/// The condition an atom's rule applies under. Part of the atom's identity: the same
/// logical property under different conditions is different atoms, so `padding` and
/// `mobile { padding }` coexist and merge independently.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Condition {
    None,
    /// `:hover` on the atom's own class.
    Hover,
    /// `:focus-visible` on the atom's own class.
    FocusVisible,
    /// `:active` on the atom's own class.
    Active,
    /// `:checked` on the atom's own class (form elements).
    Checked,
    /// `:disabled` on the atom's own class (form elements).
    Disabled,
    /// An `@media` query — `#[styles]` inlines the query text for `dark`/`mobile`/
    /// `desktop` blocks (the strings are named once, in `idyll-styles-parse`).
    Media(&'static str),
    /// A descendant element (`article` typography blocks: `.cls tag { … }`).
    /// Specificity 0-1-1, but the subject element differs from every bare atom's, so
    /// it collides with nothing merge wouldn't already resolve.
    Element(&'static str),
    /// A media query ∧ a descendant element (`mobile: { h1: { … } }`) — the one
    /// composed condition, for responsive typography on descendants.
    MediaElement(&'static str, &'static str),
    /// A range input's thumb, via the vendor pseudo-elements (webkit + moz twins).
    SliderThumb,
    /// A range input's thumb while the control is being dragged.
    ActiveSliderThumb,
    /// A range input's track, via the vendor pseudo-elements (webkit + moz twins).
    SliderTrack,
    /// A range input's thumb in WebKit only — the layout the two engines disagree on.
    WebkitSliderThumb,
    /// `> tag` — a direct child (not a descendant).
    Child(&'static str),
    /// `> tag:first-of-type` / `:last-of-type`.
    ChildEdge(&'static str, &'static str),
    /// `> a:<state> + b` — the sibling immediately after a state-carrying one.
    SiblingNext(&'static str, &'static str, &'static str),
    /// `> a:nth-of-type(i):checked ~ b:nth-of-type(i)` for `i in 1..=n`.
    CheckedNthPairs(&'static str, &'static str, u32),
    /// `:hover > tag` — the trigger reveals its own note.
    HoverChild(&'static str),
    /// `:focus-within > tag` — the keyboard's half.
    FocusWithinChild(&'static str),
    /// `@media (max-width: {n}px)`.
    MaxWidth(u32),
    /// `@media (pointer: coarse)`.
    PointerCoarse,
}

/// One declaration of a [`Style`]: a single-class selector for one property under one
/// condition. `rule` is the full rule text, pre-rendered by `#[styles]` at expansion —
/// the value never exists un-rendered — and exists only where values are allowed to
/// exist: native code. Wasm carries the identity, never the text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Atom {
    pub class: &'static str,
    /// The CSS property name (`margin-top`), as it appears in the rule.
    pub property: &'static str,
    pub condition: Condition,
    #[cfg(not(target_arch = "wasm32"))]
    pub rule: &'static str,
}

impl Atom {
    /// The atom as the wire form riding in templates: name always, text where it exists.
    pub fn style_rule(&self) -> StyleRule {
        StyleRule {
            name: Cow::Borrowed(self.class),
            #[cfg(not(target_arch = "wasm32"))]
            css: Some(Cow::Borrowed(self.rule)),
            #[cfg(target_arch = "wasm32")]
            css: None,
        }
    }
}

// ── Style ─────────────────────────────────────────────────────────────────────

/// Every property, allowed. Plain `Style` is `Style<All>`.
pub struct All;

/// The constraint a property-set marker satisfies per allowed property. `props!`
/// declares a marker with exactly the impls its set names; `css!` emits one
/// [`requires`](Style::requires) per property written, so the const's *declared type*
/// drives the check and a disallowed property is unrepresentable in the value.
pub trait Allows<Prop> {}

impl<Prop> Allows<Prop> for All {}

/// A `const` set of styled declarations, phantom-typed by its allowed property set.
/// Produced by `css!`; composed by [`merge`]; attached in `live_view!` via `css=[…]`.
pub struct Style<P = All> {
    atoms: &'static [Atom],
    props: PhantomData<fn() -> P>,
}

impl<P> Style<P> {
    pub const fn new(atoms: &'static [Atom]) -> Self {
        Style { atoms, props: PhantomData }
    }

    /// A compile-time witness that this style's property set allows `Prop` — `css!`
    /// chains one per property it emits; the bound is the entire body.
    pub const fn requires<Prop>(self) -> Self
    where
        P: Allows<Prop>,
    {
        self
    }

    pub const fn atoms(&self) -> &'static [Atom] {
        self.atoms
    }
}

impl<P> Clone for Style<P> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<P> Copy for Style<P> {}

impl<P> std::fmt::Debug for Style<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Style").field("atoms", &self.atoms).finish()
    }
}

/// The property-set markers `props!` grants and `css!` demands — one unit type per
/// entry in the typed property table (the table lives in `idyll-styles-parse`, the
/// style DSL's one parser; a missing marker here is a loud compile error there).
pub mod props {
    macro_rules! markers {
        ($($name:ident),* $(,)?) => {
            $(pub struct $name;)*
        };
    }

    markers! {
        Display, FlexDirection, AlignItems, AlignContent, JustifyItems, AlignSelf, JustifyContent, FlexWrap, Gap, ColumnGap,
        GridTemplateColumns, GridTemplateRows, GridAutoFlow, GridAutoRows, GridAutoColumns,
        GridArea, GridColumn, GridRow,
        BorderTop, BorderRight,
        FlexGrow, FlexShrink, FlexBasis, ColorScheme,
        Padding, PaddingTop, PaddingRight, PaddingBottom, PaddingLeft,
        Margin, MarginTop, MarginRight, MarginBottom, MarginLeft,
        Width, Height, MaxWidth, MinWidth, MinHeight, MaxHeight,
        FontFamily, FontSize, FontWeight, LineHeight,
        Color, Background, AccentColor,
        Border, BorderLeft, BorderBottom, BorderColor, BorderTopColor, BorderRadius, BoxShadow, Transition,
        BorderWidth, BorderStyle,
        BorderTopWidth, BorderTopStyle,
        BorderRightWidth, BorderRightStyle, BorderRightColor,
        BorderBottomWidth, BorderBottomStyle, BorderBottomColor,
        BorderLeftWidth, BorderLeftStyle, BorderLeftColor,
        Outline, OutlineWidth, OutlineStyle, OutlineColor, OutlineOffset,
        Position, Top, Right, Bottom, Left,
        ListStyle, Overflow, OverflowX, OverflowY, OverflowAnchor, TextDecoration, Cursor, WhiteSpace,
        Visibility, ZIndex, UserSelect, PointerEvents, BoxSizing, Inset, TextAlign, RowGap, Appearance,
        LetterSpacing, TextTransform, FontStyle, FontVariantNumeric, TextUnderlineOffset, TextWrap,
        TextDecorationThickness,
        Transform, Filter, TransformOrigin, WillChange, OffsetPath, OffsetDistance,
        OffsetRotate, Stroke, Fill, StrokeWidth, StrokeLinejoin, StrokeLinecap, VectorEffect, Opacity,
        Animation, Mask, AspectRatio,
    }
}

// ── Vars ──────────────────────────────────────────────────────────────────────

/// Value kinds a var can carry — one marker per [`VarKind` shape]
/// (`idyll-styles-parse`); a reference asserts the kind its property demands.
pub mod kind {
    pub struct Color;
    pub struct Length;
    pub struct Number;
    pub struct FontStack;
    /// A registered `<angle>` var — only declarable as `angle(…)`, since an angle
    /// is never a bare style value.
    pub struct Angle;
}

/// A typed CSS custom property, declared by `vars!` inside a `#[styles]` module and
/// referenced in `css!` values (`color: Palette::accent`). It carries the property's
/// *name* — its declaration-site identity — never a value: values live in the group's
/// `:root` rule in the stylesheet, so editing one restyles everything live, wasm
/// included, exactly like editing any other rule text.
pub struct Var<K> {
    pub name: &'static str,
    kind: PhantomData<fn() -> K>,
}

impl<K> Var<K> {
    pub const fn new(name: &'static str) -> Self {
        Var { name, kind: PhantomData }
    }

    /// This var **as a value** — the `var(--…)` reference. A `vars!` var is the one
    /// source of truth for a token that must be both a *class* (`css! { color: Palette::x }`)
    /// and an inline *value*: `style=(format!("border-color:{}", Palette::x.value()))`. So a
    /// per-item colour drawn from a token set (a heatmap, a load-tinted dot) needs no class
    /// per value, and no hex literal duplicated alongside the class. (`Var`'s own `Display`
    /// writes the bare name, for *declaring* the property; this writes the reference, for
    /// *reading* it.)
    pub const fn value(self) -> VarRef<K> {
        VarRef(self)
    }
}

/// Writes the custom property's name — so a component can compose an inline
/// declaration for it (`format!("{}:{}", Motion::a, angle)`) without ever spelling
/// the generated name by hand.
impl<K> std::fmt::Display for Var<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

/// A [`Var`] used as a **value**: `Display`s the `var(--…)` reference (see [`Var::value`]).
pub struct VarRef<K>(Var<K>);

impl<K> std::fmt::Display for VarRef<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "var({})", self.0.name)
    }
}

impl<K> Clone for VarRef<K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K> Copy for VarRef<K> {}

impl<K> std::fmt::Debug for VarRef<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VarRef(var({}))", self.0.name)
    }
}

/// Two references are equal when they name the same declaration — what a per-frame diff
/// asks, answerable without knowing any value.
impl<K> PartialEq for VarRef<K> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.0.name, other.0.name)
    }
}
impl<K> Eq for VarRef<K> {}

/// Two handles must be the same kind — what a `theme!` override asserts when its value
/// is another var (`page: Palette::panel`). Neither kind is named: unifying `K` is the
/// whole check, so remapping a colour onto a length fails to compile without anyone
/// having to say which is which.
pub const fn same_kind<K>(_target: Var<K>, _source: Var<K>) {}

impl<K> Clone for Var<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for Var<K> {}

// ── Keyframes ─────────────────────────────────────────────────────────────────

/// A named `@keyframes` animation, declared by `keyframes!` in a `#[styles]`
/// module. Carries its declaration-site *name* — the `@keyframes` rule text lives
/// in the style table like any rule. `Display` writes the name, so a component can
/// compose an `animation` shorthand at runtime (`style:animation=(…)`), the way the
/// sims re-trigger a walk each frame.
#[derive(Clone, Copy)]
pub struct Keyframes {
    pub name: &'static str,
}

impl Keyframes {
    pub const fn new(name: &'static str) -> Self {
        Keyframes { name }
    }
}

impl std::fmt::Display for Keyframes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

// ── Reactive entries ──────────────────────────────────────────────────────────

/// A `css=[…]` entry's value: a style, or nothing. `Style` lifts to `Some`, so
/// `css=[BASE, $on => SELECTED, $variant]` has one semantics — entries yield
/// style-or-nothing, the present ones merge in list order (`$sig => STYLE` is sugar
/// for `($sig).then_some(STYLE)`).
pub trait CssEntry {
    fn atoms_if(self) -> Option<&'static [Atom]>;
}

impl<P> CssEntry for Style<P> {
    fn atoms_if(self) -> Option<&'static [Atom]> {
        Some(self.atoms())
    }
}

impl<P> CssEntry for Option<Style<P>> {
    fn atoms_if(self) -> Option<&'static [Atom]> {
        self.map(|style| style.atoms())
    }
}

/// Every entry's rules, dedup'd by class — the **delivery** union for reactive
/// entries. [`merge`]'s last-wins picks the *attribute*; delivery must carry every
/// rule any state can activate, so the shadowed ones ship too. Open signal-carried
/// entries can't be named here at all — their delivery is the document's universe
/// sheet, which exists for exactly that.
pub fn rule_union(styles: &[&'static [Atom]]) -> Vec<StyleRule> {
    let mut out: Vec<StyleRule> = Vec::new();
    for atoms in styles {
        for atom in *atoms {
            if !out.iter().any(|rule| rule.name == atom.class) {
                out.push(atom.style_rule());
            }
        }
    }
    out
}

// ── Merge ─────────────────────────────────────────────────────────────────────

/// The result of composing styles: the class attribute for the element and the rules
/// those classes reference (to ride in the element's template).
#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    pub class_attr: String,
    pub rules: Vec<StyleRule>,
}

/// Compose styles: **last wins per (property, condition)**, resolved here rather than
/// by the cascade — the classes emitted never conflict in the stylesheet, so rule
/// order there is irrelevant. `css=[base, variant]` lowers to this.
pub fn merge(styles: &[&'static [Atom]]) -> Merged {
    let mut chosen: Vec<&Atom> = Vec::new();
    for atoms in styles {
        for atom in *atoms {
            let key = |have: &&Atom| {
                have.property == atom.property && have.condition == atom.condition
            };
            match chosen.iter_mut().find(|have| key(have)) {
                Some(slot) => *slot = atom,
                None => chosen.push(atom),
            }
        }
    }
    Merged {
        class_attr: chosen
            .iter()
            .map(|atom| atom.class)
            .collect::<Vec<_>>()
            .join(" "),
        rules: chosen.iter().map(|atom| atom.style_rule()).collect(),
    }
}

// ── StyleTable ────────────────────────────────────────────────────────────────

/// The workspace's rule texts, grouped by declaring package — the value source
/// guest-emitted rules join against, built by [`extract`] from the same `#[styles]`
/// modules the attribute compiled (source is the value source; no registry, no dump
/// binary). The package grouping is the scoping unit: a document sheet ships a rule
/// only when its declaring package is in the page's live universe (or the rule was
/// collected at render) — resolution, by contrast, always searches everything.
///
/// Serializes as the `package → name → css` map — the `rules` half of the
/// `styles.json` artifact `build` mode publishes beside the client bundle.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct StyleTable {
    rules: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl StyleTable {
    pub(crate) fn insert(&mut self, package: &str, name: String, css: String) {
        self.rules.entry(package.to_string()).or_default().insert(name, css);
    }

    /// The rule text for a class name, if any package declares it.
    pub fn resolve(&self, name: &str) -> Option<&str> {
        self.rules.values().find_map(|rules| rules.get(name)).map(String::as_str)
    }

    /// Every rule, `(name, css)`, package-major order — deterministic sheet output.
    pub fn rules(&self) -> impl Iterator<Item = (&str, &str)> {
        self.rules
            .values()
            .flatten()
            .map(|(name, css)| (name.as_str(), css.as_str()))
    }

    /// The rules of the given packages only — what a live page's sheet unions in.
    pub fn scoped<'a>(
        &'a self,
        packages: &'a std::collections::BTreeSet<String>,
    ) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.rules
            .iter()
            .filter(|(package, _)| packages.contains(*package))
            .flat_map(|(_, rules)| rules)
            .map(|(name, css)| (name.as_str(), css.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.rules.values().all(|rules| rules.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_declares_as_name_reads_as_reference() {
        let accent: Var<kind::Color> = Var::new("--v1-palette-accent");
        // Bare name for *declaring* the custom property inline;
        assert_eq!(accent.to_string(), "--v1-palette-accent");
        // `var(…)` reference for *reading* it as a value (an inline style, a computed pick).
        assert_eq!(accent.value().to_string(), "var(--v1-palette-accent)");
        // References compare by declaration identity, value unseen.
        assert_eq!(accent.value(), Var::<kind::Color>::new("--v1-palette-accent").value());
    }

    const fn atom(
        class: &'static str,
        property: &'static str,
        condition: Condition,
        rule: &'static str,
    ) -> Atom {
        Atom { class, property, condition, rule }
    }

    const CARD: &[Atom] = &[
        atom("x1-pad", "padding", Condition::None, ".x1-pad{padding:1.5rem}"),
        atom("x1-bg", "background", Condition::None, ".x1-bg{background:#ffffff}"),
        atom(
            "x1-bg-d",
            "background",
            Condition::Media("(prefers-color-scheme: dark)"),
            "@media (prefers-color-scheme: dark){.x1-bg-d{background:#101214}}",
        ),
    ];

    const ACTIVE: &[Atom] =
        &[atom("x2-bg", "background", Condition::None, ".x2-bg{background:#f4f6f8}")];

    #[test]
    fn merge_is_last_wins_per_property_and_condition() {
        let merged = merge(&[CARD, ACTIVE]);

        // `background` (bare) resolved to ACTIVE's atom; padding and the dark-mode
        // background are different keys and survive.
        assert_eq!(merged.class_attr, "x1-pad x2-bg x1-bg-d");
        assert_eq!(
            merged.rules.iter().map(|r| &*r.name).collect::<Vec<_>>(),
            ["x1-pad", "x2-bg", "x1-bg-d"]
        );
    }

    #[test]
    fn table_resolves_by_class_name_and_round_trips_json() {
        let mut table = StyleTable::default();
        for atom in CARD {
            table.insert("card-crate", atom.class.to_string(), atom.rule.to_string());
        }
        for atom in ACTIVE {
            table.insert("active-crate", atom.class.to_string(), atom.rule.to_string());
        }

        assert_eq!(table.resolve("x2-bg"), Some(".x2-bg{background:#f4f6f8}"));
        assert_eq!(table.resolve("missing"), None);

        // Scoping is by declaring package; the full iterator sees everything.
        let scope = std::collections::BTreeSet::from(["card-crate".to_string()]);
        let scoped: Vec<&str> = table.scoped(&scope).map(|(name, _)| name).collect();
        assert_eq!(scoped, ["x1-bg", "x1-bg-d", "x1-pad"]);
        assert_eq!(table.rules().count(), 4);

        let json = serde_json::to_string(&table).unwrap();
        assert_eq!(serde_json::from_str::<StyleTable>(&json).unwrap(), table);
    }

    #[test]
    fn constrained_styles_check_at_the_declared_type() {
        struct CardProps;
        impl Allows<props::Padding> for CardProps {}

        const PROMO: Style<CardProps> =
            Style::new(&[atom("x3-pad", "padding", Condition::None, ".x3-pad{padding:2rem}")])
                .requires::<props::Padding>();
        // `.requires::<props::Color>()` on PROMO would not compile — the marker has no
        // `Allows<Color>` impl. The unconstrained default accepts everything:
        const ANY: Style = Style::new(CARD).requires::<props::Color>();

        assert_eq!(PROMO.atoms().len(), 1);
        assert_eq!(ANY.atoms().len(), 3);
    }
}
