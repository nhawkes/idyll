//! The ONE parser for idyll-styles.
//!
//! A `#[styles] mod styles` body declares `Style` consts whose `css! {{ … }}`
//! initializers are JS-object literals: bare or quoted keys, string values validated
//! against a fixed typed property table, bare numbers for the unitless properties,
//! nested objects for conditions. Both consumers parse through this crate — the
//! `#[styles]` attribute at expansion and the source extractor building the style
//! table — so the grammar, the class names, and the rule text cannot drift apart.
//!
//! Class identity is the declaration site, never the value:
//! `i{hash8(path)}-{const}-{property}{condition}` where `path` is the
//! workspace-relative declaring file. Both consumers derive the path through
//! [`path_identity`], and a pinned test in idyll-styles asserts they agree.

use proc_macro2::{Span, TokenStream};
use std::path::Path;
use syn::ext::IdentExt;
use syn::parse::{ParseStream, Parser};
use syn::{braced, parenthesized, token, Error, Ident, Lit, LitInt, LitStr, Result, Token};

// ── The property table ────────────────────────────────────────────────────────

/// How a property's value parses. The table below is the working subset; a property
/// outside it is a compile error, never a string pass-through.
enum Kind {
    /// One of a fixed keyword set, written as CSS (`"inline-block"`).
    Keyword(&'static [&'static str]),
    /// One CSS length (`"2rem"`, `"860px"`, `"100%"`, `"0"`).
    Length,
    /// One to four lengths (`border-radius`, `padding`).
    Lengths,
    /// One to four of length-or-`auto` (`margin: "0 auto"`).
    LengthsOrAuto,
    /// Length, `auto`, or `none` (the sizing properties).
    Sizing,
    /// Length or `auto` (the inset properties).
    LengthOrAuto,
    /// `#hex`, `transparent`, `inherit`, or `currentcolor`.
    Color,
    /// [`Kind::Color`] plus `none` (`background`).
    Background,
    /// Bare unitless number or a length string (`line-height`).
    LineHeight,
    /// A bare unitless number (the flex factors).
    Number,
    /// The schemes a document supports — `"light dark"` is the point of the property,
    /// so it is a set, not one keyword.
    ColorScheme,
    /// Bare 100–900 or `"normal"`/`"bold"`.
    FontWeight,
    /// Freeform — font stacks are freeform by nature.
    FontFamily,
    /// `"none"` or `"<length> <solid|dashed|dotted> <color>"`.
    Border,
    /// `"none"` or 2–4 lengths then a color.
    Shadow,
    /// `"none"` or `"<property> <duration> [<easing>]"`. Also custom-property
    /// transitions (`--name <duration> [easing]`) for compositor interpolation.
    Transition,
    /// A CSS angle (`45deg`, `0.5turn`) or a `calc(…)` producing one.
    Angle,
    /// `transform` functions — `scale(…)`, `rotate(…)`, `translate(…)` etc. Written
    /// either as a string (the interior is CSS the author owns) or as the typed form
    /// `translate_y(calc(Group::field * 100%))`, whose interior is calc structure — so a
    /// transform can be driven by a var handle the way a gradient's angle can.
    Transform,
    /// `filter` functions — `brightness(…)`, `saturate(…)`, `blur(…)`, `contrast(…)`,
    /// `grayscale(…)`, `sepia(…)`, `invert(…)`, `opacity(…)`, `hue-rotate(…)`,
    /// `drop-shadow(…)` — a space-separated list, or `none`.
    Filter,
    /// `will-change` — a comma list of property names.
    Idents,
    /// `offset-path` — `none` or a `path("…")`.
    OffsetPath,
    /// `transform-origin` — one or two of the position keywords / lengths.
    Origin,
    /// `animation` — a `keyframes!` handle followed by the shorthand tail.
    Animation,
    /// `mask` — a gradient (the ring's feathered edge).
    Mask,
    /// One or two lengths — the `gap` shorthand (`"12px"` or `"1px 12px"` = row then column).
    Gap,
    /// A grid track list: `none`, `subgrid`, or a run of track sizes — a length, `fr`,
    /// `auto`, an intrinsic size, or a `minmax(…)`/`repeat(…)`/`fit-content(…)` function —
    /// with optional `[line-name]` brackets between them. Also the `grid-auto-rows`/
    /// `grid-auto-columns` implicit track sizes.
    GridTemplate,
    /// `grid-auto-flow`: `row`/`column`, optionally with `dense`.
    GridAutoFlow,
    /// A grid line placement (`grid-area`/`grid-column`/`grid-row`): `/`-separated ends,
    /// each an integer line, `span <n>`, a named line, or `auto`.
    GridLine,
}

/// One row of the typed surface: the key written in a style body, the CSS name it
/// lowers to, and the `idyll_styles::props` marker `props!` grants for it.
/// What an atom declares. Almost always a row of the typed table; a `theme!` block
/// declares custom properties instead, which the table cannot hold because their names
/// are derived from a `vars!` group rather than fixed.
#[derive(Clone)]
pub enum Prop {
    Table(&'static Property),
    /// A `vars!` handle's custom property: the name it lowers to, and the readable
    /// `group-field` that names the atom's class.
    Var { css: String, ident: String },
}

impl Prop {
    /// The property name as it appears in the rule.
    pub fn css(&self) -> &str {
        match self {
            Prop::Table(property) => property.css,
            Prop::Var { css, .. } => css,
        }
    }

    /// A vendor twin, if this property emits two declarations. Custom properties
    /// never do.
    fn css_twin(&self) -> Option<&'static str> {
        match self {
            Prop::Table(property) => property.css_twin,
            Prop::Var { .. } => None,
        }
    }

    /// The `props!` marker this property needs, if it is one the table constrains.
    pub fn marker(&self) -> Option<&'static str> {
        match self {
            Prop::Table(property) => Some(property.marker),
            Prop::Var { .. } => None,
        }
    }

    /// What "the same property" means for last-wins composition, and the fragment that
    /// names the class.
    pub fn identity(&self) -> &str {
        match self {
            Prop::Table(property) => property.rust,
            Prop::Var { ident, .. } => ident,
        }
    }

    /// The class-name fragment — readable, and never the value.
    fn class_fragment(&self) -> &str {
        match self {
            Prop::Table(property) => property.css,
            Prop::Var { ident, .. } => ident,
        }
    }
}

pub struct Property {
    pub rust: &'static str,
    pub css: &'static str,
    pub marker: &'static str,
    kind: Kind,
    /// A vendor-prefixed twin the declaration also emits (`appearance` also writes
    /// `-webkit-appearance`) — the one place a single property is two declarations.
    css_twin: Option<&'static str>,
}

const fn prop(
    rust: &'static str,
    css: &'static str,
    marker: &'static str,
    kind: Kind,
) -> Property {
    Property { rust, css, marker, kind, css_twin: None }
}

const fn prop_vendor(
    rust: &'static str,
    css: &'static str,
    marker: &'static str,
    css_twin: &'static str,
    kind: Kind,
) -> Property {
    Property { rust, css, marker, kind, css_twin: Some(css_twin) }
}

pub const PROPERTIES: &[Property] = &[
    prop("display", "display", "Display", Kind::Keyword(&["flex", "grid", "block", "inline", "inline-block", "inline-flex", "inline-grid", "none", "contents"])),
    prop("flex_direction", "flex-direction", "FlexDirection", Kind::Keyword(&["row", "column", "row-reverse", "column-reverse"])),
    prop("align_items", "align-items", "AlignItems", Kind::Keyword(&["center", "flex-start", "flex-end", "stretch", "baseline"])),
    prop("align_content", "align-content", "AlignContent", Kind::Keyword(&["center", "flex-start", "flex-end", "stretch", "space-between", "space-around", "space-evenly"])),
    prop("justify_items", "justify-items", "JustifyItems", Kind::Keyword(&["center", "start", "end", "stretch"])),
    prop("align_self", "align-self", "AlignSelf", Kind::Keyword(&["auto", "center", "flex-start", "flex-end", "stretch", "baseline"])),
    prop("justify_content", "justify-content", "JustifyContent", Kind::Keyword(&["center", "flex-start", "flex-end", "space-between", "space-around", "space-evenly"])),
    prop("flex_wrap", "flex-wrap", "FlexWrap", Kind::Keyword(&["wrap", "nowrap", "wrap-reverse"])),
    prop("flex_grow", "flex-grow", "FlexGrow", Kind::Number),
    prop("flex_shrink", "flex-shrink", "FlexShrink", Kind::Number),
    prop("flex_basis", "flex-basis", "FlexBasis", Kind::Sizing),
    prop("color_scheme", "color-scheme", "ColorScheme", Kind::ColorScheme),
    prop("gap", "gap", "Gap", Kind::Gap),
    prop("column_gap", "column-gap", "ColumnGap", Kind::Length),
    prop("grid_template_columns", "grid-template-columns", "GridTemplateColumns", Kind::GridTemplate),
    prop("grid_template_rows", "grid-template-rows", "GridTemplateRows", Kind::GridTemplate),
    prop("grid_auto_flow", "grid-auto-flow", "GridAutoFlow", Kind::GridAutoFlow),
    prop("grid_auto_rows", "grid-auto-rows", "GridAutoRows", Kind::GridTemplate),
    prop("grid_auto_columns", "grid-auto-columns", "GridAutoColumns", Kind::GridTemplate),
    prop("grid_area", "grid-area", "GridArea", Kind::GridLine),
    prop("grid_column", "grid-column", "GridColumn", Kind::GridLine),
    prop("grid_row", "grid-row", "GridRow", Kind::GridLine),
    prop("padding", "padding", "Padding", Kind::Lengths),
    prop("padding_top", "padding-top", "PaddingTop", Kind::Length),
    prop("padding_right", "padding-right", "PaddingRight", Kind::Length),
    prop("padding_bottom", "padding-bottom", "PaddingBottom", Kind::Length),
    prop("padding_left", "padding-left", "PaddingLeft", Kind::Length),
    prop("margin", "margin", "Margin", Kind::LengthsOrAuto),
    prop("margin_top", "margin-top", "MarginTop", Kind::LengthOrAuto),
    prop("margin_right", "margin-right", "MarginRight", Kind::LengthOrAuto),
    prop("margin_bottom", "margin-bottom", "MarginBottom", Kind::LengthOrAuto),
    prop("margin_left", "margin-left", "MarginLeft", Kind::LengthOrAuto),
    prop("width", "width", "Width", Kind::Sizing),
    prop("height", "height", "Height", Kind::Sizing),
    prop("max_width", "max-width", "MaxWidth", Kind::Sizing),
    prop("min_width", "min-width", "MinWidth", Kind::Sizing),
    prop("min_height", "min-height", "MinHeight", Kind::Sizing),
    prop("max_height", "max-height", "MaxHeight", Kind::Sizing),
    prop("aspect_ratio", "aspect-ratio", "AspectRatio", Kind::Number),
    prop("font_family", "font-family", "FontFamily", Kind::FontFamily),
    prop("font_size", "font-size", "FontSize", Kind::Length),
    prop("font_weight", "font-weight", "FontWeight", Kind::FontWeight),
    prop("line_height", "line-height", "LineHeight", Kind::LineHeight),
    prop("color", "color", "Color", Kind::Color),
    prop("background", "background", "Background", Kind::Background),
    prop("accent_color", "accent-color", "AccentColor", Kind::Color),
    prop("outline", "outline", "Outline", Kind::Border),
    prop("border_width", "border-width", "BorderWidth", Kind::Lengths),
    prop("border_style", "border-style", "BorderStyle", Kind::Keyword(&["none", "solid", "dashed", "dotted"])),
    prop("border_top_width", "border-top-width", "BorderTopWidth", Kind::Length),
    prop("border_top_style", "border-top-style", "BorderTopStyle", Kind::Keyword(&["none", "solid", "dashed", "dotted"])),
    prop("border_right_width", "border-right-width", "BorderRightWidth", Kind::Length),
    prop("border_right_style", "border-right-style", "BorderRightStyle", Kind::Keyword(&["none", "solid", "dashed", "dotted"])),
    prop("border_right_color", "border-right-color", "BorderRightColor", Kind::Color),
    prop("border_bottom_width", "border-bottom-width", "BorderBottomWidth", Kind::Length),
    prop("border_bottom_style", "border-bottom-style", "BorderBottomStyle", Kind::Keyword(&["none", "solid", "dashed", "dotted"])),
    prop("border_bottom_color", "border-bottom-color", "BorderBottomColor", Kind::Color),
    prop("border_left_width", "border-left-width", "BorderLeftWidth", Kind::Length),
    prop("border_left_style", "border-left-style", "BorderLeftStyle", Kind::Keyword(&["none", "solid", "dashed", "dotted"])),
    prop("border_left_color", "border-left-color", "BorderLeftColor", Kind::Color),
    prop("outline_width", "outline-width", "OutlineWidth", Kind::Length),
    prop("outline_style", "outline-style", "OutlineStyle", Kind::Keyword(&["none", "solid", "dashed", "dotted"])),
    prop("outline_color", "outline-color", "OutlineColor", Kind::Color),
    prop("outline_offset", "outline-offset", "OutlineOffset", Kind::Length),
    prop("border", "border", "Border", Kind::Border),
    prop("border_top", "border-top", "BorderTop", Kind::Border),
    prop("border_right", "border-right", "BorderRight", Kind::Border),
    prop("border_left", "border-left", "BorderLeft", Kind::Border),
    prop("border_bottom", "border-bottom", "BorderBottom", Kind::Border),
    prop("border_color", "border-color", "BorderColor", Kind::Color),
    prop("border_top_color", "border-top-color", "BorderTopColor", Kind::Color),
    prop("border_radius", "border-radius", "BorderRadius", Kind::Lengths),
    prop("box_shadow", "box-shadow", "BoxShadow", Kind::Shadow),
    prop("transition", "transition", "Transition", Kind::Transition),
    prop("visibility", "visibility", "Visibility", Kind::Keyword(&["visible", "hidden"])),
    prop("z_index", "z-index", "ZIndex", Kind::Number),
    prop("position", "position", "Position", Kind::Keyword(&["relative", "absolute", "fixed", "sticky", "static"])),
    prop("top", "top", "Top", Kind::LengthOrAuto),
    prop("right", "right", "Right", Kind::LengthOrAuto),
    prop("bottom", "bottom", "Bottom", Kind::LengthOrAuto),
    prop("left", "left", "Left", Kind::LengthOrAuto),
    prop("list_style", "list-style", "ListStyle", Kind::Keyword(&["none", "disc", "decimal"])),
    prop("overflow", "overflow", "Overflow", Kind::Keyword(&["hidden", "auto", "scroll", "visible"])),
    prop("overflow_x", "overflow-x", "OverflowX", Kind::Keyword(&["hidden", "auto", "scroll", "visible"])),
    prop("overflow_y", "overflow-y", "OverflowY", Kind::Keyword(&["hidden", "auto", "scroll", "visible"])),
    prop("overflow_anchor", "overflow-anchor", "OverflowAnchor", Kind::Keyword(&["none", "auto"])),
    prop("text_decoration", "text-decoration", "TextDecoration", Kind::Keyword(&["none", "underline", "line-through"])),
    prop("cursor", "cursor", "Cursor", Kind::Keyword(&["pointer", "default", "text", "grab", "grabbing", "move", "not-allowed", "help"])),
    prop("white_space", "white-space", "WhiteSpace", Kind::Keyword(&["nowrap", "normal", "pre", "pre-wrap"])),
    prop_vendor("appearance", "appearance", "Appearance", "-webkit-appearance", Kind::Keyword(&["none", "auto"])),
    prop("user_select", "user-select", "UserSelect", Kind::Keyword(&["none", "auto", "text"])),
    prop("pointer_events", "pointer-events", "PointerEvents", Kind::Keyword(&["none", "auto"])),
    prop("box_sizing", "box-sizing", "BoxSizing", Kind::Keyword(&["border-box", "content-box"])),
    prop("inset", "inset", "Inset", Kind::LengthsOrAuto),
    prop("text_align", "text-align", "TextAlign", Kind::Keyword(&["left", "center", "right", "justify"])),
    prop("row_gap", "row-gap", "RowGap", Kind::Length),
    prop("letter_spacing", "letter-spacing", "LetterSpacing", Kind::Length),
    prop("text_transform", "text-transform", "TextTransform", Kind::Keyword(&["none", "uppercase", "lowercase", "capitalize"])),
    prop("font_style", "font-style", "FontStyle", Kind::Keyword(&["normal", "italic", "oblique"])),
    prop("font_variant_numeric", "font-variant-numeric", "FontVariantNumeric", Kind::Keyword(&["normal", "tabular-nums", "proportional-nums", "lining-nums", "oldstyle-nums"])),
    prop("text_underline_offset", "text-underline-offset", "TextUnderlineOffset", Kind::Length),
    prop("text_decoration_thickness", "text-decoration-thickness", "TextDecorationThickness", Kind::Length),
    prop("text_wrap", "text-wrap", "TextWrap", Kind::Keyword(&["wrap", "nowrap", "balance", "pretty", "stable"])),
    // Motion (the queue-viz cluster): transforms, offsets, SVG, will-change.
    prop("transform", "transform", "Transform", Kind::Transform),
    prop("filter", "filter", "Filter", Kind::Filter),
    prop("transform_origin", "transform-origin", "TransformOrigin", Kind::Origin),
    prop("will_change", "will-change", "WillChange", Kind::Idents),
    prop("offset_path", "offset-path", "OffsetPath", Kind::OffsetPath),
    prop("offset_distance", "offset-distance", "OffsetDistance", Kind::Length),
    prop("offset_rotate", "offset-rotate", "OffsetRotate", Kind::Angle),
    prop("stroke", "stroke", "Stroke", Kind::Color),
    prop("fill", "fill", "Fill", Kind::Background),
    prop("stroke_width", "stroke-width", "StrokeWidth", Kind::Length),
    prop("stroke_linejoin", "stroke-linejoin", "StrokeLinejoin", Kind::Keyword(&["round", "miter", "bevel"])),
    prop("stroke_linecap", "stroke-linecap", "StrokeLinecap", Kind::Keyword(&["butt", "round", "square"])),
    prop("vector_effect", "vector-effect", "VectorEffect", Kind::Keyword(&["none", "non-scaling-stroke"])),
    prop("opacity", "opacity", "Opacity", Kind::LineHeight),
    prop("animation", "animation", "Animation", Kind::Animation),
    prop_vendor("mask", "mask", "Mask", "-webkit-mask", Kind::Mask),
];

pub fn property(name: &str) -> Option<&'static Property> {
    PROPERTIES.iter().find(|p| p.rust == name)
}

/// The four longhands a box-side shorthand stands for, in CSS order. A shorthand is
/// authoring spelling: an atom's property is always a longhand, so that two atoms
/// touching the same side collide on one key and `merge` can resolve them. Leaving the
/// shorthand intact would let `padding` and `padding-left` both survive and hand the
/// browser a conflict the cascade cannot see.
fn box_sides(property: &Property) -> Option<[&'static str; 4]> {
    match property.rust {
        "padding" => Some(["padding_top", "padding_right", "padding_bottom", "padding_left"]),
        "margin" => Some(["margin_top", "margin_right", "margin_bottom", "margin_left"]),
        "inset" => Some(["top", "right", "bottom", "left"]),
        _ => None,
    }
}

/// The sides a border shorthand speaks for: `border` is all four, `border_left` is one.
fn border_sides(property: &Property) -> Option<&'static [&'static str]> {
    match property.rust {
        "border" => Some(&["top", "right", "bottom", "left"]),
        "border_top" => Some(&["top"]),
        "border_right" => Some(&["right"]),
        "border_bottom" => Some(&["bottom"]),
        "border_left" => Some(&["left"]),
        _ => None,
    }
}

/// A border shorthand as its facets. `border: "none"` is a style alone — it is the
/// style that stops the border being drawn, and a width without a style draws nothing
/// either way, so widening it to three atoms would invent two values the author did
/// not write.
fn border_facets(value: &str) -> Vec<(&'static str, String)> {
    match value_tokens(value).as_slice() {
        [style] => vec![("style", style.to_string())],
        [width, style, color] => vec![
            ("width", width.to_string()),
            ("style", style.to_string()),
            ("color", color.to_string()),
        ],
        // `validate` admits only those two shapes.
        _ => Vec::new(),
    }
}

/// One authored declaration as the longhand atoms it means. CSS's 1–4 token rule:
/// one value applies to every side, two are vertical/horizontal, three leave the left
/// mirroring the right, four are clockwise from the top.
fn expand_shorthand(property: &'static Property, value: &str) -> Vec<(&'static Property, String)> {
    let long = |rust: &str| self::property(rust).expect("an expansion target is in the table");

    if let Some(sides) = border_sides(property) {
        return sides
            .iter()
            .flat_map(|side| {
                border_facets(value)
                    .into_iter()
                    .map(move |(facet, part)| (long(&format!("border_{side}_{facet}")), part))
            })
            .collect();
    }
    if property.rust == "outline" {
        return border_facets(value)
            .into_iter()
            .map(|(facet, part)| (long(&format!("outline_{facet}")), part))
            .collect();
    }
    // `border_width` / `border_style` / `border_color` name one facet on every side.
    if let Some(facet) = property
        .rust
        .strip_prefix("border_")
        .filter(|facet| matches!(*facet, "width" | "style" | "color"))
    {
        let sides = ["top", "right", "bottom", "left"];
        return sides
            .iter()
            .zip(per_side(value).unwrap_or([value; 4]))
            .map(|(side, part)| (long(&format!("border_{side}_{facet}")), part.to_string()))
            .collect();
    }

    let Some(sides) = box_sides(property) else {
        return vec![(property, value.to_string())];
    };
    let Some(per_side) = per_side(value) else {
        return vec![(property, value.to_string())];
    };
    sides
        .iter()
        .zip(per_side)
        .map(|(rust, part)| (long(rust), part.to_string()))
        .collect()
}

/// The 1–4 token rule, as the four sides it means: one value applies to every side, two
/// are vertical/horizontal, three leave the left mirroring the right, four are clockwise
/// from the top.
fn per_side(value: &str) -> Option<[&str; 4]> {
    let parts = value_tokens(value);
    Some(match parts.as_slice() {
        [all] => [all, all, all, all],
        [y, x] => [y, x, y, x],
        [top, x, bottom] => [top, x, bottom, x],
        [top, right, bottom, left] => [top, right, bottom, left],
        // `validate` already rejected any other count for this property's kind.
        _ => return None,
    })
}

fn property_or_error(name: &str, span: Span) -> Result<&'static Property> {
    property(name).ok_or_else(|| {
        Error::new(
            span,
            format!(
                "`{name}` is not in idyll-styles' property table — the table is the typed \
                 surface (no string pass-through); if the property is legitimate, extend \
                 the table in idyll-styles-parse and its marker in idyll-styles"
            ),
        )
    })
}

// ── Conditions ────────────────────────────────────────────────────────────────

pub const MOBILE: &str = "(max-width: 640px)";
pub const DESKTOP: &str = "(min-width: 641px)";
pub const DARK: &str = "(prefers-color-scheme: dark)";

/// The condition a declaration applies under — part of the atom's identity, so the
/// same property under different conditions is different atoms.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Condition {
    None,
    Hover,
    FocusVisible,
    Active,
    Checked,
    Disabled,
    Media(&'static str),
    Element(String),
    /// A media query ∧ a descendant element — the one composed condition (a mobile
    /// type ramp on article typography needs it). Declared media-outside,
    /// element-inside: `mobile: { h1: { … } }`.
    MediaElement(&'static str, String),
    /// A range input's thumb — styled through the vendor pseudo-elements. One block
    /// emits two rules (`::-webkit-slider-thumb`, `::-moz-range-thumb`): a selector
    /// list containing an unknown pseudo is dropped whole, so the twins can't share
    /// one selector.
    SliderThumb,
    /// A range input's thumb while the control is being dragged — the one state a
    /// native slider gives you that says "held".
    ActiveSliderThumb,
    /// A range input's track (`::-webkit-slider-runnable-track`, `::-moz-range-track`).
    SliderTrack,
    /// A range input's thumb in **WebKit only** (`::-webkit-slider-thumb`). The two
    /// engines lay the thumb out differently — WebKit aligns it to the track box's
    /// top and nests it *inside* the track, Firefox centres it and keeps it a
    /// sibling — so the declarations that follow from that layout (a centring
    /// margin, a fill clipped by the track) belong to one engine, not to both.
    WebkitSliderThumb,
    /// `> tag` — a **direct** child. Distinct from [`Element`](Condition::Element)'s
    /// descendant: a stage positions its own children, not its grandchildren.
    Child(String),
    /// `> tag:first-of-type` / `:last-of-type` — the end caps of a child run.
    ChildEdge(String, &'static str),
    /// `> a:<state> + b` — the sibling **immediately after** a state-carrying one.
    /// The subject stops being the class-bearing element, which is why these are
    /// their own conditions rather than nestable pseudos.
    SiblingNext(String, String, &'static str),
    /// `> a:nth-of-type(i):checked ~ b:nth-of-type(i)` for `i in 1..=n` — the
    /// positional pairing that switches a panel from its radio. One condition, a
    /// selector list of `n` pairs.
    CheckedNthPairs(String, String, u32),
    /// `:hover > tag` — the styled element is the trigger, the child is what the
    /// hover reveals. The subject is still this element, but what it selects is not.
    HoverChild(String),
    /// `:focus-within > tag` — the keyboard's half of [`HoverChild`](Condition::HoverChild).
    FocusWithinChild(String),
    /// `@media (max-width: {n}px)` — a breakpoint written as the width it is, for the
    /// one-off steps that no named scale describes.
    MaxWidth(u32),
    /// `@media (pointer: coarse)` — a finger, not a cursor. Touch targets grow.
    PointerCoarse,
}

impl Condition {
    pub fn class_suffix(&self) -> String {
        fn media(query: &'static str) -> &'static str {
            match query {
                MOBILE => "-m",
                DESKTOP => "-d",
                _ => "-k",
            }
        }
        match self {
            Condition::None => String::new(),
            Condition::Hover => "-h".into(),
            Condition::FocusVisible => "-f".into(),
            Condition::Active => "-a".into(),
            Condition::Checked => "-c".into(),
            Condition::Disabled => "-x".into(),
            Condition::Media(query) => media(query).into(),
            Condition::Element(tag) => format!("-e-{tag}"),
            Condition::MediaElement(query, tag) => format!("{}-e-{tag}", media(query)),
            Condition::SliderThumb => "-st".into(),
            Condition::ActiveSliderThumb => "-ast".into(),
            Condition::SliderTrack => "-sk".into(),
            Condition::WebkitSliderThumb => "-wst".into(),
            Condition::Child(tag) => format!("-c-{tag}"),
            Condition::ChildEdge(tag, edge) => {
                format!("-c-{tag}-{}", if *edge == "first-of-type" { "first" } else { "last" })
            }
            Condition::SiblingNext(a, b, state) => {
                format!("-n-{a}-{b}-{}", if *state == "checked" { "ck" } else { "fv" })
            }
            Condition::CheckedNthPairs(a, b, n) => format!("-p-{a}-{b}-{n}"),
            Condition::HoverChild(tag) => format!("-hc-{tag}"),
            Condition::FocusWithinChild(tag) => format!("-fwc-{tag}"),
            Condition::MaxWidth(px) => format!("-w{px}"),
            Condition::PointerCoarse => "-pc".into(),
        }
    }
}

// ── Class identity ────────────────────────────────────────────────────────────

/// The declaring file's identity: `{package}/{path relative to its manifest dir}`,
/// forward-slashed, so the macro (compiling on any OS) and the extractor (walking
/// cargo metadata) name the same file the same way.
pub fn path_identity(package: &str, manifest_dir: &Path, file: &Path) -> Option<String> {
    let file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(file)
    };
    let relative = file.strip_prefix(manifest_dir).ok()?;
    Some(format!(
        "{package}/{}",
        relative.to_string_lossy().replace('\\', "/")
    ))
}

/// The per-file class prefix: `i` + eight hex digits of the path identity's hash.
pub fn class_prefix(path_identity: &str) -> String {
    format!("i{:08x}", fnv1a64(path_identity) as u32)
}

pub fn class_name(
    prefix: &str,
    const_name: &str,
    property: &Prop,
    condition: &Condition,
) -> String {
    format!(
        "{prefix}-{}-{}{}",
        const_name.to_ascii_lowercase().replace('_', "-"),
        property.class_fragment(),
        condition.class_suffix()
    )
}

fn fnv1a64(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ── Var groups ────────────────────────────────────────────────────────────────

/// One `vars! { pub Group { … } }` declaration: a set of CSS custom properties. Var
/// identity is the **crate**, not the file — tokens are a crate-level design
/// vocabulary (one `Palette`, many atom files), so any `css!` in the crate references
/// `Group::field` and both consumers derive the same `--{crate}-{group}-{field}`
/// name syntactically. Existence is proven by compilation: the attribute emits a
/// typed assertion against the group's real const, so a typo'd reference is a
/// compile error, not a parse-time lookup. A var's kind is its value's shape
/// ([`VarKind`]); a color var's value may differ under `dark`; the group lowers to
/// `:root` rules.
pub struct VarGroup {
    /// The group's table identity: `{prefix}-{group}` (kebab), the "class" its
    /// `:root` rule text lives under in the style table.
    pub name: String,
    /// The Rust identifier (`Palette`) — what `css!` references resolve against.
    pub ident: String,
    pub vars: Vec<VarDecl>,
}

pub struct VarDecl {
    /// The Rust field (`page`) referenced as `Group::field`.
    pub field: String,
    /// The custom property: `--{prefix}-{group}-{field}` (kebab).
    pub css_name: String,
    pub default: String,
    pub dark: Option<String>,
    pub kind: VarKind,
    /// `Some(syntax)` when declared as `angle(…)`/`length(…)`/`color(…)` — an
    /// `@property` registration, which is what lets the compositor interpolate the
    /// value (and lets a keyframe animate it) instead of treating it as a string.
    pub registered: Option<&'static str>,
    pub span: Span,
}

/// A var's value kind — read off the declared value's shape, asserted at every
/// reference (`Var<kind::…>`), so a var lands only in properties that accept its
/// kind. `dark:` twins are color-only: the other kinds are geometry/typography,
/// which don't fork on scheme.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VarKind {
    /// `"#0af"`, a color keyword, or space-free `"rgba(…)"`.
    Color,
    /// One CSS length (`"8px"`).
    Length,
    /// A bare number literal (`1.5`).
    Number,
    /// Any other non-empty string — font stacks are freeform by nature.
    FontStack,
    /// A CSS angle (`0deg`) — only reachable through a registered declaration,
    /// since an angle is never a bare style value.
    Angle,
}

impl VarKind {
    /// The `idyll_styles::kind` marker type this kind asserts as.
    pub fn marker(self) -> &'static str {
        match self {
            VarKind::Color => "Color",
            VarKind::Length => "Length",
            VarKind::Number => "Number",
            VarKind::FontStack => "FontStack",
            VarKind::Angle => "Angle",
        }
    }
}

impl VarGroup {
    /// The group's full rule text: the `:root` defaults, plus the dark overrides
    /// under the same media query the `dark {}` condition uses. Custom properties
    /// resolve live, so where this lands in the sheet doesn't matter.
    pub fn rule(&self) -> String {
        // A registered var declares its initial value in its `@property`, so it needs
        // no `:root` entry — and must not have one, or the registration's
        // initial-value is shadowed by an unregistered declaration.
        let registrations: String = self
            .vars
            .iter()
            .filter_map(|v| {
                let syntax = v.registered?;
                Some(format!(
                    "@property {}{{syntax:'{syntax}';inherits:false;initial-value:{};}}",
                    v.css_name, v.default
                ))
            })
            .collect();
        let defaults: String = self
            .vars
            .iter()
            .filter(|v| v.registered.is_none())
            .map(|v| format!("{}:{};", v.css_name, v.default))
            .collect();
        if defaults.is_empty() {
            return registrations;
        }
        let mut rule = format!("{registrations}:root{{{defaults}}}");
        let dark: String = self
            .vars
            .iter()
            .filter_map(|v| Some(format!("{}:{};", v.css_name, v.dark.as_deref()?)))
            .collect();
        if !dark.is_empty() {
            rule.push_str(&format!("@media {DARK}{{:root{{{dark}}}}}"));
        }
        rule
    }

}

/// The crate-level prefix var names hash under: `i` + eight hex digits of the
/// package name. Classes keep file identity; vars are crate vocabulary.
pub fn vars_prefix(package: &str) -> String {
    class_prefix(package)
}

/// The custom-property name `Group::field` derives to — the ONE derivation both
/// consumers (and [`parse_vars`]) share, so a reference and its declaration cannot
/// disagree.
pub fn var_css_name(vars_prefix: &str, group: &str, field: &str) -> String {
    format!("--{vars_prefix}-{}-{}", kebab(group), kebab(field))
}

/// A `vars!` item macro's body, if the item is one — the one definition of what
/// counts as a var group, shared by the attribute's rewrite walk and the extractor.
pub fn vars_macro(item: &syn::Item) -> Option<&syn::ItemMacro> {
    match item {
        syn::Item::Macro(item) if item.mac.path.is_ident("vars") => Some(item),
        _ => None,
    }
}

// ── Keyframes ───────────────────────────────────────────────────────────────

/// One `keyframes! { pub Name { step { … } } }` animation. The name is
/// crate-scoped identity (`k{crate}-{ident}`) — stable and referenceable from any
/// `css!` in the crate (like a var), so the a/b
/// alternation idiom (two declarations, two stable names) works; the css is the
/// full `@keyframes` at-rule, riding the style table like any rule.
pub struct Keyframes {
    /// The animation name (the "class" its rule lives under in the table).
    pub name: String,
    /// The Rust identifier (`Walk`) the handle const is named for.
    pub ident: String,
    /// The full `@keyframes {name} { … }` text.
    pub css: String,
    /// Vars a step assigns — asserted by the attribute, so a renamed var is a
    /// compile error rather than a keyframe that animates nothing.
    pub var_refs: Vec<(String, String, VarKind)>,
    pub span: Span,
}

pub fn keyframes_macro(item: &syn::Item) -> Option<&syn::ItemMacro> {
    match item {
        syn::Item::Macro(item) if item.mac.path.is_ident("keyframes") => Some(item),
        _ => None,
    }
}

/// The declaration-site name for a keyframes handle: `k{prefix}-{ident-kebab}`.
pub fn keyframes_name(prefix: &str, ident: &str) -> String {
    format!("k{prefix}-{}", ident.to_ascii_lowercase().replace('_', "-"))
}

// ── theme! ──────────────────────────────────────────────────────────────

/// One `theme! { pub Name: Group { field: value, … } }` — a set of custom-property
/// declarations that redefine a [`VarGroup`] for the subtree it is applied to.
///
/// A theme is a `Style`: its declarations are atoms like any other, so applying one is
/// `css=[Theme]` and two themes on one element resolve by the same last-wins merge as
/// two ordinary styles. Lowering to a single `.cls{--a:x;--b:y}` rule instead would put
/// that back at the mercy of source order in the sheet.
pub struct Theme {
    /// The Rust identifier the handle const is named for.
    pub ident: String,
    pub atoms: Vec<Atom>,
    /// Overrides whose value is a literal: the field, and the kind the value declares
    /// itself to be. Asserted against the group's real const.
    pub var_refs: Vec<(String, String, VarKind)>,
    /// Overrides whose value is another var — `(field, source_group, source_field)`.
    /// The two are asserted to be the *same* kind without naming it, so a remap is
    /// checked without the macro knowing either group's fields.
    pub remaps: Vec<(String, String, String)>,
    /// The group being overridden.
    pub group: String,
}

pub fn theme_macro(item: &syn::Item) -> Option<&syn::ItemMacro> {
    match item {
        syn::Item::Macro(item) if item.mac.path.is_ident("theme") => Some(item),
        _ => None,
    }
}

/// The themes of a `#[styles]` module, in declaration order.
pub fn theme_list(module: &syn::ItemMod, prefix: &str, vars_prefix: &str) -> Result<Vec<Theme>> {
    let Some((_, items)) = &module.content else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in items.iter().filter_map(theme_macro) {
        out.extend(parse_theme(prefix, vars_prefix, item.mac.tokens.clone())?);
    }
    Ok(out)
}

/// The kind a theme value declares itself to be. The group is not looked up — nothing
/// here knows `Palette`'s fields, exactly as a `css!` var reference does not. The value
/// names its own kind and the emitted `Var<kind::…>` assertion is what checks it
/// against the group, so a theme may override a group from any module or crate.
fn theme_value_kind(value: &str, span: Span) -> Result<VarKind> {
    if is_color(value, &["transparent", "currentcolor", "inherit"]) {
        return Ok(VarKind::Color);
    }
    if is_length(value) {
        return Ok(VarKind::Length);
    }
    if value.parse::<f64>().is_ok() {
        return Ok(VarKind::Number);
    }
    if value.contains(',') || value.contains(' ') {
        return Ok(VarKind::FontStack);
    }
    Err(Error::new(
        span,
        format!("`{value}` is not a colour, a length, a number, or a font stack"),
    ))
}

pub fn parse_theme(prefix: &str, vars_prefix: &str, tokens: TokenStream) -> Result<Vec<Theme>> {
    let parser = |input: ParseStream| {
        let mut themes = Vec::new();
        while !input.is_empty() {
            input.call(syn::Attribute::parse_outer)?;
            input.parse::<Token![pub]>()?;
            let ident: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            let group: Ident = input.parse()?;
            let body;
            braced!(body in input);

            let mut atoms = Vec::new();
            let mut var_refs = Vec::new();
            let mut remaps = Vec::new();
            while !body.is_empty() {
                body.call(syn::Attribute::parse_outer)?;
                let field = body.call(Ident::parse_any)?;
                body.parse::<Token![:]>()?;

                // A literal states a new value; a handle remaps the field onto another
                // var, which is how a region says "here, `page` means `panel`" without
                // copying the colour.
                let value = if body.peek(LitStr) {
                    let literal: LitStr = body.parse()?;
                    let text = literal.value();
                    let kind = theme_value_kind(&text, literal.span())?;
                    var_refs.push((group.to_string(), field.to_string(), kind));
                    text
                } else {
                    let source_group: Ident = body.parse()?;
                    body.parse::<Token![::]>()?;
                    let source_field = body.call(Ident::parse_any)?;
                    let name = var_css_name(
                        vars_prefix,
                        &source_group.to_string(),
                        &source_field.to_string(),
                    );
                    remaps.push((
                        field.to_string(),
                        source_group.to_string(),
                        source_field.to_string(),
                    ));
                    format!("var({name})")
                };

                let css = var_css_name(vars_prefix, &group.to_string(), &field.to_string());
                let readable =
                    format!("{}-{}", kebab(&group.to_string()), kebab(&field.to_string()));
                let property = Prop::Var { css, ident: readable };
                let condition = Condition::None;
                atoms.push(Atom {
                    class: class_name(prefix, &ident.to_string(), &property, &condition),
                    property,
                    condition,
                    value,
                    var_refs: Vec::new(),
                    span: field.span(),
                });

                if !body.is_empty() {
                    body.parse::<Token![,]>()?;
                }
            }

            themes.push(Theme {
                ident: ident.to_string(),
                atoms,
                var_refs,
                remaps,
                group: group.to_string(),
            });
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(themes)
    };
    parser.parse2(tokens)
}

// ── document! ─────────────────────────────────────────────────────────────────

/// A rule on one of the document's own elements — the two surfaces no component
/// owns, so no class can reach them. Everything else in a page is a component
/// wearing its own styles.
pub struct DocumentRule {
    /// The rule's table identity: `{prefix}-document-{selector}`. The prefix is the
    /// declaring file, so two files may each say their piece about the document.
    pub name: String,
    pub css: String,
    pub var_refs: Vec<(String, String, VarKind)>,
}

pub fn document_macro(item: &syn::Item) -> Option<&syn::ItemMacro> {
    match item {
        syn::Item::Macro(item) if item.mac.path.is_ident("document") => Some(item),
        _ => None,
    }
}

/// The document rules of a `#[styles]` module, in declaration order.
pub fn document_list(
    module: &syn::ItemMod,
    prefix: &str,
    vars_prefix: &str,
) -> Result<Vec<DocumentRule>> {
    let Some((_, items)) = &module.content else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in items.iter().filter_map(document_macro) {
        out.extend(parse_document(prefix, vars_prefix, item.mac.tokens.clone())?);
    }
    Ok(out)
}

/// `document! { root { … }, body { … } }` — flat declarations on the document's own
/// elements. No conditions: a document element's variation is a token's business
/// (a var with a `dark:` twin), so the rule itself never branches.
pub fn parse_document(
    prefix: &str,
    vars_prefix: &str,
    tokens: proc_macro2::TokenStream,
) -> Result<Vec<DocumentRule>> {
    let parser = |input: ParseStream| {
        let mut rules = Vec::new();
        while !input.is_empty() {
            input.call(syn::Attribute::parse_outer)?;
            let selector = input.call(Ident::parse_any)?;
            let css_selector = match selector.to_string().as_str() {
                "root" => ":root",
                "body" => "body",
                _ => {
                    return Err(Error::new(
                        selector.span(),
                        "the document's elements are `root` (`:root`) and `body`",
                    ))
                }
            };
            let inner;
            braced!(inner in input);
            let mut decls = Vec::new();
            parse_object(&inner, Condition::None, vars_prefix, &mut decls)?;

            let body: Vec<String> = decls
                .iter()
                .map(|decl| match decl.property.css_twin() {
                    Some(twin) => {
                        format!("{}:{};{}:{}", decl.property.css(), decl.value, twin, decl.value)
                    }
                    None => format!("{}:{}", decl.property.css(), decl.value),
                })
                .collect();
            rules.push(DocumentRule {
                name: format!("{prefix}-document-{}", selector.to_string().replace('_', "-")),
                css: format!("{css_selector}{{{}}}", body.join(";")),
                var_refs: decls.into_iter().flat_map(|decl| decl.var_refs).collect(),
            });

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(rules)
    };
    parser.parse2(tokens)
}

/// The keyframes of a `#[styles]` module, in declaration order.
pub fn keyframes_list(module: &syn::ItemMod, vars_prefix: &str) -> Result<Vec<Keyframes>> {
    let Some((_, items)) = &module.content else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in items.iter().filter_map(keyframes_macro) {
        out.extend(parse_keyframes(vars_prefix, item.mac.tokens.clone())?);
    }
    Ok(out)
}

/// One keyframe step's declarations. A key is either a property from the typed
/// table or a `Group::field` var handle — animating a *registered* custom property
/// is how a sub-frame excursion (the gate's gap widening) interpolates at all.
fn parse_keyframe_decls(
    input: ParseStream,
    vars_prefix: &str,
    refs: &mut Vec<(String, String, VarKind)>,
) -> Result<String> {
    let mut out = String::new();
    while !input.is_empty() {
        if input.peek(Ident) && input.peek2(Token![::]) {
            let group: Ident = input.parse()?;
            input.parse::<Token![::]>()?;
            let field = input.call(Ident::parse_any)?;
            let (group, field) = (group.to_string(), field.to_string());
            let name = var_css_name(vars_prefix, &group, &field);
            input.parse::<Token![:]>()?;
            let lit: LitStr = input
                .parse()
                .map_err(|_| input.error("a var assignment's value is a string"))?;
            let value = lit.value();
            let kind = if is_length(&value) {
                VarKind::Length
            } else if is_angle(&value) {
                VarKind::Angle
            } else if is_color(&value, &["transparent", "inherit", "currentcolor"]) {
                VarKind::Color
            } else {
                return Err(Error::new(
                    lit.span(),
                    format!("`{value}` is not a length, percentage, angle, or colour"),
                ));
            };
            refs.push((group, field, kind));
            out.push_str(&format!("{name}:{value};"));
        } else {
            let (key, key_span) = parse_key(input)?;
            let property = property_or_error(&key, key_span)?;
            input.parse::<Token![:]>()?;
            let (value, mut value_refs) = parse_value(input, property, vars_prefix)?;
            refs.append(&mut value_refs);
            out.push_str(&format!("{}:{};", property.css, value));
        }
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
    }
    Ok(out)
}

/// Parse a `keyframes! { pub Name { from { … } "40%" { … } } … }` body.
pub fn parse_keyframes(vars_prefix: &str, body: TokenStream) -> Result<Vec<Keyframes>> {
    Parser::parse2(
        |input: ParseStream| {
            let mut out = Vec::new();
            let mut refs = Vec::new();
            while !input.is_empty() {
                // Doc comments (and any outer attribute) on a keyframes declaration.
                let _attrs = input.call(syn::Attribute::parse_outer)?;
                let _vis: syn::Visibility = input.parse()?;
                let ident: Ident = input.parse()?;
                let name = ident.to_string();
                if !name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    return Err(Error::new(ident.span(), "keyframes are CamelCase (`Walk`)"));
                }
                let steps_body;
                braced!(steps_body in input);
                let mut steps = String::new();
                while !steps_body.is_empty() {
                    // A step key: `from`, `to`, or a `"40%"` percentage string.
                    let (key, key_span) = parse_key(&steps_body)?;
                    let selector = match key.as_str() {
                        "from" | "to" => key.clone(),
                        pct if pct.ends_with('%')
                            && pct.trim_end_matches('%').parse::<f64>().is_ok() =>
                        {
                            pct.to_string()
                        }
                        _ => {
                            return Err(Error::new(
                                key_span,
                                "a keyframe step is `from`, `to`, or a `\"40%\"` percentage",
                            ))
                        }
                    };
                    let decls_body;
                    braced!(decls_body in steps_body);
                    let text = parse_keyframe_decls(&decls_body, vars_prefix, &mut refs)?;
                    steps.push_str(&format!("{selector}{{{text}}}"));
                    if steps_body.peek(Token![,]) {
                        steps_body.parse::<Token![,]>()?;
                    }
                }
                let kf_name = keyframes_name(vars_prefix, &name);
                out.push(Keyframes {
                    css: format!("@keyframes {kf_name}{{{steps}}}"),
                    name: kf_name,
                    ident: name,
                    var_refs: std::mem::take(&mut refs),
                    span: ident.span(),
                });
                if input.peek(Token![,]) {
                    input.parse::<Token![,]>()?;
                }
            }
            Ok(out)
        },
        body,
    )
}

/// The var groups of a `#[styles]` module, in declaration order.
pub fn var_groups(module: &syn::ItemMod, prefix: &str) -> Result<Vec<VarGroup>> {
    let Some((_, items)) = &module.content else {
        return Err(Error::new(
            module.ident.span(),
            "#[styles] needs an inline module body (`mod styles { … }`)",
        ));
    };
    items
        .iter()
        .filter_map(vars_macro)
        .map(|item| parse_vars(prefix, item.mac.tokens.clone()))
        .collect()
}

/// Parse one `vars!` body: `pub Group { field: "#hex", other: { "#hex", dark: "#hex" } }`.
/// Values are colors (the working subset's one var kind), validated like any color.
pub fn parse_vars(prefix: &str, body: TokenStream) -> Result<VarGroup> {
    Parser::parse2(
        |input: ParseStream| {
            // Doc comments (and any outer attribute) on the group declaration.
            let _attrs = input.call(syn::Attribute::parse_outer)?;
            let _vis: syn::Visibility = input.parse()?;
            let ident: Ident = input.parse()?;
            let group = ident.to_string();
            if !group.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                return Err(Error::new(ident.span(), "var groups are CamelCase (`Palette`)"));
            }
            let group_kebab = kebab(&group);
            let body;
            braced!(body in input);
            let mut vars = Vec::new();
            while !body.is_empty() {
                let field = body.call(Ident::parse_any)?;
                body.parse::<Token![:]>()?;
                // `angle("0deg")` and friends register an `@property`; anything else
                // is the plain form, whose value's shape names its kind.
                let (default, dark, kind, registered) = if body.peek(Ident) && body.peek2(token::Paren)
                {
                    let (syntax, initial, kind) = parse_registered(&body)?;
                    (initial, None, kind, Some(syntax))
                } else {
                    let (default, dark, kind) = parse_var_value(&body)?;
                    (default, dark, kind, None)
                };
                let name = field.to_string();
                if vars.iter().any(|v: &VarDecl| v.field == name) {
                    return Err(Error::new(field.span(), format!("`{name}` is declared twice")));
                }
                vars.push(VarDecl {
                    css_name: var_css_name(prefix, &group, &name),
                    field: name,
                    default,
                    dark,
                    kind,
                    registered,
                    span: field.span(),
                });
                if !body.is_empty() {
                    body.parse::<Token![,]>()?;
                }
            }
            if !input.is_empty() {
                return Err(input.error("vars! takes exactly one group"));
            }
            if vars.is_empty() {
                return Err(Error::new(ident.span(), "an empty var group declares nothing"));
            }
            Ok(VarGroup { name: format!("{prefix}-{group_kebab}"), ident: group, vars })
        },
        body,
    )
}

/// A **registered** var: `angle("0deg")`, `length("0px")`, `color("transparent")`.
/// Registration is what the compositor needs — an `@property` declaration with a
/// syntax and an initial value, so the browser interpolates the custom property
/// (and a keyframe can animate it) rather than treating it as an opaque string.
/// Returns `(syntax, initial, kind)`.
fn parse_registered(input: ParseStream) -> Result<(&'static str, String, VarKind)> {
    let ty: Ident = input.parse()?;
    let (syntax, kind) = match ty.to_string().as_str() {
        "angle" => ("<angle>", VarKind::Angle),
        "length" => ("<length>", VarKind::Length),
        "color" => ("<color>", VarKind::Color),
        other => {
            return Err(Error::new(
                ty.span(),
                format!(
                    "`{other}` is not a registered var type — `angle`, `length`, or `color`"
                ),
            ))
        }
    };
    let inner;
    parenthesized!(inner in input);
    let lit: LitStr = inner.parse().map_err(|_| {
        inner.error("a registered var takes its initial value as a string, e.g. `angle(\"0deg\")`")
    })?;
    let initial = lit.value();
    // A registration's syntax is the browser's contract: a `%` initial under
    // `<length>` makes the whole `@property` invalid, so the shapes must not overlap.
    let ok = match kind {
        VarKind::Angle => is_angle(&initial),
        VarKind::Length => is_length(&initial) && !is_percentage(&initial),
        VarKind::Color => is_color(&initial, &["transparent", "inherit", "currentcolor"]),
        _ => false,
    };
    if !ok {
        return Err(Error::new(
            lit.span(),
            format!("`{initial}` is not a valid initial value for `{}`", ty),
        ));
    }
    Ok((syntax, initial, kind))
}

/// One var's value: a literal whose **shape names its kind** — a color string, a
/// length string, a bare number, or any other string (a font stack) — or, for
/// colors only, a block `{ "<color>", dark: "<color>" }`.
fn parse_var_value(input: ParseStream) -> Result<(String, Option<String>, VarKind)> {
    let value = |input: ParseStream| -> Result<(String, VarKind)> {
        if input.peek(Lit) && !input.peek(LitStr) {
            let lit: Lit = input.parse()?;
            let (Lit::Int(_) | Lit::Float(_)) = &lit else {
                return Err(Error::new(lit.span(), "a var's value is a string or a bare number"));
            };
            let number = match &lit {
                Lit::Int(int) if int.suffix().is_empty() => int.base10_digits().to_string(),
                Lit::Float(float) if float.suffix().is_empty() => float.base10_digits().to_string(),
                _ => return Err(Error::new(lit.span(), "numbers here are plain (no type suffix)")),
            };
            return Ok((number, VarKind::Number));
        }
        let lit: LitStr = input.parse().map_err(|_| {
            input.error("a var's value is a color, a length, a bare number, or a font stack")
        })?;
        let text = lit.value();
        if is_color(&text, &["transparent", "inherit", "currentcolor"]) {
            Ok((text, VarKind::Color))
        } else if is_length(&text) {
            Ok((text, VarKind::Length))
        } else if !text.trim().is_empty() {
            Ok((text.trim().to_string(), VarKind::FontStack))
        } else {
            Err(Error::new(lit.span(), "an empty string declares no value"))
        }
    };
    if !input.peek(token::Brace) {
        let (default, kind) = value(input)?;
        return Ok((default, None, kind));
    }
    let block;
    braced!(block in input);
    let (default, kind) = value(&block)?;
    let mut dark = None;
    while !block.is_empty() {
        block.parse::<Token![,]>()?;
        if block.is_empty() {
            break;
        }
        let key = block.call(Ident::parse_any)?;
        if key != "dark" {
            return Err(Error::new(key.span(), "`dark` is the one per-var condition (the working subset)"));
        }
        if kind != VarKind::Color {
            return Err(Error::new(
                key.span(),
                "`dark` twins are for color vars — geometry and typography don't fork on scheme",
            ));
        }
        block.parse::<Token![:]>()?;
        let (twin, twin_kind) = value(&block)?;
        if twin_kind != VarKind::Color {
            return Err(Error::new(key.span(), format!("`{twin}` is not a color")));
        }
        if dark.replace(twin).is_some() {
            return Err(Error::new(key.span(), "`dark` is declared twice"));
        }
    }
    Ok((default, dark, kind))
}

fn kebab(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else if c == '_' {
            out.push('-');
        } else {
            out.push(c);
        }
    }
    out
}

// ── The parsed atom and its rule text ─────────────────────────────────────────

/// One declaration of a style const: a single-class selector for one property under
/// one condition, with its validated CSS value text.
pub struct Atom {
    pub class: String,
    pub property: Prop,
    pub condition: Condition,
    pub value: String,
    /// Every var this value references (a gradient stop list holds several) — the
    /// kind is what the position demands, and the attribute type-asserts
    /// `Var<kind::…>` against the group's real const, so a renamed or mis-kinded
    /// var is a compile error at the reference.
    pub var_refs: Vec<(String, String, VarKind)>,
    pub span: Span,
}

impl Atom {
    /// The atom's full rule text — the one place a value becomes CSS. A
    /// vendor-twinned property is two declarations; a vendor pseudo-element is two
    /// rules (the browser prefixes don't share a selector).
    pub fn rule(&self) -> String {
        let class = &self.class;
        let decl = match self.property.css_twin() {
            Some(twin) => format!("{}:{};{}:{}", self.property.css(), self.value, twin, self.value),
            None => format!("{}:{}", self.property.css(), self.value),
        };
        match &self.condition {
            Condition::None => format!(".{class}{{{decl}}}"),
            Condition::Hover => format!(".{class}:hover{{{decl}}}"),
            Condition::FocusVisible => format!(".{class}:focus-visible{{{decl}}}"),
            Condition::Active => format!(".{class}:active{{{decl}}}"),
            Condition::Checked => format!(".{class}:checked{{{decl}}}"),
            Condition::Disabled => format!(".{class}:disabled{{{decl}}}"),
            Condition::Media(query) => format!("@media {query}{{.{class}{{{decl}}}}}"),
            Condition::Element(tag) => format!(".{class} {tag}{{{decl}}}"),
            Condition::MediaElement(query, tag) => {
                format!("@media {query}{{.{class} {tag}{{{decl}}}}}")
            }
            Condition::SliderThumb => format!(
                ".{class}::-webkit-slider-thumb{{{decl}}}.{class}::-moz-range-thumb{{{decl}}}"
            ),
            Condition::ActiveSliderThumb => format!(
                ".{class}:active::-webkit-slider-thumb{{{decl}}}                 .{class}:active::-moz-range-thumb{{{decl}}}"
            ),
            Condition::SliderTrack => format!(
                ".{class}::-webkit-slider-runnable-track{{{decl}}}.{class}::-moz-range-track{{{decl}}}"
            ),
            Condition::WebkitSliderThumb => {
                format!(".{class}::-webkit-slider-thumb{{{decl}}}")
            }
            Condition::HoverChild(tag) => format!(".{class}:hover > {tag}{{{decl}}}"),
            Condition::FocusWithinChild(tag) => format!(".{class}:focus-within > {tag}{{{decl}}}"),
            Condition::MaxWidth(px) => format!("@media (max-width: {px}px){{.{class}{{{decl}}}}}"),
            Condition::PointerCoarse => format!("@media (pointer: coarse){{.{class}{{{decl}}}}}"),
            Condition::Child(tag) => format!(".{class} > {tag}{{{decl}}}"),
            Condition::ChildEdge(tag, edge) => format!(".{class} > {tag}:{edge}{{{decl}}}"),
            Condition::SiblingNext(a, b, state) => {
                format!(".{class} > {a}:{state} + {b}{{{decl}}}")
            }
            Condition::CheckedNthPairs(a, b, n) => {
                let pairs: Vec<String> = (1..=*n)
                    .map(|i| format!(".{class} > {a}:nth-of-type({i}):checked ~ {b}:nth-of-type({i})"))
                    .collect();
                format!("{}{{{decl}}}", pairs.join(","))
            }
        }
    }
}

// ── The module walk ───────────────────────────────────────────────────────────

/// A `css! {{ … }}` const found in a `#[styles]` module body. Items that aren't
/// style consts pass through untouched — they're simply not in this list.
pub struct StyleConst<'a> {
    pub item: &'a syn::ItemConst,
    pub name: String,
    pub body: TokenStream,
}

/// A const's `css!` initializer, if it has one — the one definition of what counts
/// as a style const, shared by the attribute's rewrite walk and [`style_consts`].
pub fn css_macro(item: &syn::ItemConst) -> Option<&syn::Macro> {
    match &*item.expr {
        syn::Expr::Macro(expr) if expr.mac.path.is_ident("css") => Some(&expr.mac),
        _ => None,
    }
}

/// The style consts of a `#[styles]` module, in declaration order. Both consumers
/// walk through here, so what counts as a style const cannot drift.
pub fn style_consts(module: &syn::ItemMod) -> Result<Vec<StyleConst<'_>>> {
    let Some((_, items)) = &module.content else {
        return Err(Error::new(
            module.ident.span(),
            "#[styles] needs an inline module body (`mod styles { … }`)",
        ));
    };
    let mut consts = Vec::new();
    for item in items {
        let syn::Item::Const(item) = item else { continue };
        if let Some(mac) = css_macro(item) {
            consts.push(StyleConst {
                item,
                name: item.ident.to_string(),
                body: mac.tokens.clone(),
            });
        }
    }
    Ok(consts)
}

// ── The grammar ───────────────────────────────────────────────────────────────

/// Parse one `css! {{ … }}` body — the tokens between the macro's bang and its end,
/// which is exactly one object literal — into atoms named for their declaration site.
/// `vars_prefix` is the crate's var prefix: a value may reference `Group::field`
/// (colors, the working subset), derived to `var(--…)` by the shared derivation.
pub fn parse_style(
    prefix: &str,
    vars_prefix: &str,
    const_name: &str,
    body: TokenStream,
) -> Result<Vec<Atom>> {
    let declarations = Parser::parse2(
        |input: ParseStream| {
            let object;
            braced!(object in input);
            let mut out = Vec::new();
            parse_object(&object, Condition::None, vars_prefix, &mut out)?;
            if !input.is_empty() {
                return Err(input.error("css! takes exactly one object literal"));
            }
            Ok(out)
        },
        body,
    )?;

    Ok(declarations
        .into_iter()
        .map(|decl| Atom {
            class: class_name(prefix, const_name, &decl.property, &decl.condition),
            property: decl.property,
            condition: decl.condition,
            value: decl.value,
            var_refs: decl.var_refs,
            span: decl.span,
        })
        .collect())
}

struct Declaration {
    property: Prop,
    condition: Condition,
    value: String,
    var_refs: Vec<(String, String, VarKind)>,
    span: Span,
}

fn parse_object(
    input: ParseStream,
    condition: Condition,
    vars_prefix: &str,
    out: &mut Vec<Declaration>,
) -> Result<()> {
    while !input.is_empty() {
        let (key, key_span) = parse_key(input)?;

        if input.peek(token::Paren) {
            let called = parse_call_condition(&key, key_span, input)?;
            if condition != Condition::None {
                return Err(Error::new(
                    key_span,
                    format!("`{key}(…)` blocks sit at the top of a style, not inside another condition"),
                ));
            }
            input.parse::<Token![:]>()?;
            let inner;
            braced!(inner in input);
            parse_object(&inner, called, vars_prefix, out)?;
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
            continue;
        }

        input.parse::<Token![:]>()?;

        if input.peek(token::Brace) {
            let nested = match (&condition, block_condition(&key, key_span)?) {
                (Condition::None, nested) => nested,
                // The one composition: an element block inside a media block —
                // `mobile: { h1: { … } }` — for responsive typography on descendants.
                (Condition::Media(query), Condition::Element(tag)) => {
                    Condition::MediaElement(query, tag)
                }
                (Condition::Element(_), Condition::Media(_)) => {
                    return Err(Error::new(
                        key_span,
                        "media and element compose media-outside (`mobile: { h1: { … } }`)",
                    ));
                }
                _ => {
                    return Err(Error::new(
                        key_span,
                        format!("`{key}` blocks don't nest inside this condition (working subset)"),
                    ));
                }
            };
            let inner;
            braced!(inner in input);
            parse_object(&inner, nested, vars_prefix, out)?;
        } else {
            let property = property_or_error(&key, key_span)?;
            let (value, var_refs) = parse_value(input, property, vars_prefix)?;
            for (property, value) in expand_shorthand(property, &value) {
                let decl = Declaration {
                    property: Prop::Table(property),
                    condition: condition.clone(),
                    value,
                    var_refs: var_refs.clone(),
                    span: key_span,
                };
                // Last-wins within the body, same law as merge: one atom per
                // (property, condition), so re-declaring replaces.
                match out.iter_mut().find(|have| {
                    have.property.identity() == decl.property.identity()
                        && have.condition == decl.condition
                }) {
                    Some(slot) => *slot = decl,
                    None => out.push(decl),
                }
            }
        }

        if !input.is_empty() {
            input.parse::<Token![,]>()?;
        }
    }
    Ok(())
}

/// A key: a bare ident or a quoted string (the JS-object forms). Pseudo-classes can
/// only be quoted — `":hover"` — since `:` can't start a bare key.
fn parse_key(input: ParseStream) -> Result<(String, Span)> {
    if input.peek(LitStr) {
        let key: LitStr = input.parse()?;
        Ok((key.value(), key.span()))
    } else {
        let key = input.call(Ident::parse_any)?;
        Ok((key.to_string(), key.span()))
    }
}

/// The called conditions — the shapes that take arguments: the four that reach past
/// the styled element, and the breakpoint that names its own width.
/// Their arguments are element names, spelled as bare idents:
///
/// ```text
/// child(div)                    .cls > div
/// child(label, first)           .cls > label:first-of-type
/// checked_next(input, label)    .cls > input:checked + label
/// checked_pairs(input, pre, 6)  .cls > input:nth-of-type(i):checked ~ pre:nth-of-type(i), …
/// ```
fn parse_call_condition(key: &str, span: Span, input: ParseStream) -> Result<Condition> {
    let args;
    parenthesized!(args in input);
    let tag = || -> Result<String> {
        let ident = args.call(Ident::parse_any)?;
        let name = ident.to_string();
        if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
            return Err(Error::new(ident.span(), format!("`{name}` is not an element name")));
        }
        Ok(name)
    };
    match key {
        "child" => {
            let element = tag()?;
            if args.is_empty() {
                return Ok(Condition::Child(element));
            }
            args.parse::<Token![,]>()?;
            let edge = args.call(Ident::parse_any)?;
            match edge.to_string().as_str() {
                "first" => Ok(Condition::ChildEdge(element, "first-of-type")),
                "last" => Ok(Condition::ChildEdge(element, "last-of-type")),
                _ => Err(Error::new(edge.span(), "a child run's edges are `first` and `last`")),
            }
        }
        "hover_child" => Ok(Condition::HoverChild(tag()?)),
        "focus_within_child" => Ok(Condition::FocusWithinChild(tag()?)),
        "checked_next" | "focus_next" => {
            let stateful = tag()?;
            args.parse::<Token![,]>()?;
            let styled = tag()?;
            let state = if key == "checked_next" { "checked" } else { "focus-visible" };
            Ok(Condition::SiblingNext(stateful, styled, state))
        }
        "max_width" => {
            let width: LitInt = args.parse()?;
            if width.suffix() != "px" {
                return Err(Error::new(width.span(), "a breakpoint is a px width, e.g. `680px`"));
            }
            Ok(Condition::MaxWidth(width.base10_parse()?))
        }
        "checked_pairs" => {
            let radio = tag()?;
            args.parse::<Token![,]>()?;
            let panel = tag()?;
            args.parse::<Token![,]>()?;
            let count: LitInt = args.parse()?;
            let n: u32 = count.base10_parse()?;
            // The selector list is written out, so the bound is the number of panels a
            // set can hold — a real limit, stated where it is enforced.
            if !(1..=12).contains(&n) {
                return Err(Error::new(count.span(), "a checked set pairs 1 to 12 panels"));
            }
            Ok(Condition::CheckedNthPairs(radio, panel, n))
        }
        _ => Err(Error::new(
            span,
            format!(
                "`{key}` is not a condition — the called forms are `child`, `hover_child`, \
                 `focus_within_child`, `checked_next`, `focus_next`, `checked_pairs`, \
                 and `max_width`"
            ),
        )),
    }
}

fn block_condition(key: &str, span: Span) -> Result<Condition> {
    if let Some(pseudo) = key.strip_prefix(':') {
        return match pseudo {
            "hover" => Ok(Condition::Hover),
            "focus-visible" => Ok(Condition::FocusVisible),
            "active" => Ok(Condition::Active),
            "checked" => Ok(Condition::Checked),
            "disabled" => Ok(Condition::Disabled),
            _ => Err(Error::new(
                span,
                "the pseudo-classes are `\":hover\"`, `\":focus-visible\"`, `\":active\"`, \
                 `\":checked\"`, and `\":disabled\"` — all state on the styled element itself",
            )),
        };
    }
    match key {
        "mobile" => Ok(Condition::Media(MOBILE)),
        "desktop" => Ok(Condition::Media(DESKTOP)),
        "dark" => Ok(Condition::Media(DARK)),
        "pointer_coarse" => Ok(Condition::PointerCoarse),
        "slider_thumb" => Ok(Condition::SliderThumb),
        "active_slider_thumb" => Ok(Condition::ActiveSliderThumb),
        "slider_track" => Ok(Condition::SliderTrack),
        "webkit_slider_thumb" => Ok(Condition::WebkitSliderThumb),
        tag => {
            if property(tag).is_some() {
                return Err(Error::new(
                    span,
                    format!("`{tag}` is a property — it takes a value, not a block"),
                ));
            }
            if tag.is_empty() || !tag.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
                return Err(Error::new(
                    span,
                    format!("`{key}` is not a condition — expected `\":hover\"`, `dark`, `mobile`, `desktop`, or an element name"),
                ));
            }
            Ok(Condition::Element(tag.to_string()))
        }
    }
}

// ── Values ────────────────────────────────────────────────────────────────────

/// Parse a declaration's value — a string validated against the property's kind, a
/// bare number for the unitless kinds, or a `Group::field` var reference for the
/// color-kinded properties — into canonical CSS value text. Every rejection points
/// at the offending tokens.
fn parse_value(
    input: ParseStream,
    property: &Property,
    vars_prefix: &str,
) -> Result<(String, Vec<(String, String, VarKind)>)> {
    if input.peek(Ident) && input.peek2(Token![::]) {
        return var_reference(input, property, vars_prefix);
    }
    // `animation: Handle "0.7s linear infinite"` — a crate-scoped `keyframes!`
    // handle (an ident) then the shorthand tail. The name derives by the shared
    // crate rule, so it resolves cross-file like a var.
    if matches!(property.kind, Kind::Animation) && input.peek(Ident) {
        let handle: Ident = input.parse()?;
        let name = keyframes_name(vars_prefix, &handle.to_string());
        let tail: LitStr = input.parse().map_err(|_| {
            input.error("`animation` is `Handle \"<duration> [easing] [iteration]\"`")
        })?;
        let tail = tail.value();
        if tail.trim().is_empty() {
            return Err(Error::new(handle.span(), "animation needs a duration after the handle"));
        }
        return Ok((format!("{name} {}", tail.trim()), Vec::new()));
    }
    // A transition list whose transitioned property is a *var handle*:
    // `transition: [(Motion::a, "45ms linear"), …]`. Custom-property transitions are
    // what put an interpolation on the compositor, and the handle keeps the name
    // typed — renaming the var breaks the build instead of silently not animating.
    if matches!(property.kind, Kind::Transition) && input.peek(token::Bracket) {
        let list;
        syn::bracketed!(list in input);
        let mut refs = Vec::new();
        let mut parts = Vec::new();
        while !list.is_empty() {
            let entry;
            parenthesized!(entry in list);
            let group: Ident = entry.parse()?;
            entry.parse::<Token![::]>()?;
            let field = entry.call(Ident::parse_any)?;
            let (group, field) = (group.to_string(), field.to_string());
            let name = var_css_name(vars_prefix, &group, &field);
            refs.push((group, field, VarKind::Angle));
            entry.parse::<Token![,]>()?;
            let timing: LitStr = entry.parse().map_err(|_| {
                entry.error("a transition entry is `(Group::field, \"<duration> [easing]\")`")
            })?;
            let timing = timing.value();
            match value_tokens(&timing).as_slice() {
                [duration, easing @ ..]
                    if easing.len() <= 1
                        && is_duration(duration)
                        && easing.iter().all(|e| {
                            matches!(*e, "ease" | "ease-in" | "ease-out" | "ease-in-out" | "linear")
                        }) => {}
                _ => return Err(Error::new(input.span(), "a transition entry's timing is `\"<duration> [easing]\"`")),
            }
            parts.push(format!("{name} {timing}"));
            if list.peek(Token![,]) {
                list.parse::<Token![,]>()?;
            }
        }
        if parts.is_empty() {
            return Err(input.error("a transition list needs at least one entry"));
        }
        return Ok((parts.join(", "), refs));
    }

    // `transform: translate_y(calc(Knob::at * 100%)) scale(0.9)` — the typed form. A
    // string interior cannot name a var, because a var's CSS name is derived rather than
    // written; parsed as structure, the handle resolves like it does anywhere else.
    if matches!(property.kind, Kind::Transform) && input.peek(Ident) && input.peek2(token::Paren) {
        let mut refs = Vec::new();
        let mut parts = Vec::new();
        while input.peek(Ident) && input.peek2(token::Paren) {
            parts.push(parse_transform_fn(input, vars_prefix, &mut refs)?);
        }
        return Ok((parts.join(" "), refs));
    }

    // Structured values: `conic_gradient(…)`/`radial_gradient(…)` and `calc(…)`.
    if input.peek(Ident) && input.peek2(token::Paren) {
        let mut refs = Vec::new();
        let func = input.fork().parse::<Ident>()?.to_string();
        let value = match func.as_str() {
            "conic_gradient" | "radial_gradient" => parse_gradient(input, vars_prefix, &mut refs)?,
            "calc" => parse_calc(input, vars_prefix, &mut refs, calc_kind(&property.kind))?,
            other => {
                return Err(Error::new(
                    input.span(),
                    format!("`{other}(…)` is not a value form — `calc`, `conic_gradient`, `radial_gradient`"),
                ))
            }
        };
        if !matches!(property.kind, Kind::Background | Kind::Mask) && func != "calc" {
            return Err(Error::new(
                input.span(),
                format!("`{}` does not take a gradient", property.rust),
            ));
        }
        return Ok((value, refs));
    }
    let lit: Lit = input.parse().map_err(|_| {
        input.error(format!("`{}` expects a string value (or a bare number for the unitless properties)", property.rust))
    })?;
    let value = literal_value(&lit, property)?;
    Ok((value, Vec::new()))
}

// ── calc and gradients ────────────────────────────────────────────────────────
//
// Both are parsed as *structure*, never as text: unit terms are Rust suffixed
// literals (`0deg`, `5.5px`), var terms are the same typed handles the rest of the
// grammar uses, and every operator is a real token. So a malformed expression or a
// renamed var is a compile error at its span — the interior of a gradient is as
// checked as any other value.

/// One term of a `calc(…)`: a suffixed literal (`0deg`), a `Group::field` var, a
/// bare number, or a parenthesised sub-expression.
fn parse_calc_term(
    input: ParseStream,
    vars_prefix: &str,
    refs: &mut Vec<(String, String, VarKind)>,
    kind: VarKind,
) -> Result<String> {
    if input.peek(token::Paren) {
        let inner;
        parenthesized!(inner in input);
        return Ok(format!("({})", parse_calc_expr(&inner, vars_prefix, refs, kind)?));
    }
    if input.peek(Ident) && input.peek2(Token![::]) {
        let group: Ident = input.parse()?;
        input.parse::<Token![::]>()?;
        let field = input.call(Ident::parse_any)?;
        let (group, field) = (group.to_string(), field.to_string());
        let name = var_css_name(vars_prefix, &group, &field);
        // A calc term is arithmetic; which dimension is set by where the calc sits
        // (an angle inside a conic's `from:`, a length in a length property).
        refs.push((group, field, kind));
        return Ok(format!("var({name})"));
    }
    let lit: Lit = input
        .parse()
        .map_err(|_| input.error("a calc term is `0deg`, `1.5px`, a number, or `Group::field`"))?;
    let (digits, suffix) = match &lit {
        Lit::Int(int) => (int.base10_digits().to_string(), int.suffix().to_string()),
        Lit::Float(float) => (float.base10_digits().to_string(), float.suffix().to_string()),
        other => return Err(Error::new(other.span(), "a calc term is a number, with or without a unit")),
    };
    // `100%` lexes as `100` then `%` — a percentage is a unit, not a suffix.
    let mut term = format!("{digits}{suffix}");
    if suffix.is_empty() && input.peek(Token![%]) {
        input.parse::<Token![%]>()?;
        term.push('%');
    } else if !suffix.is_empty() && !(is_length(&term) || is_angle(&term)) {
        return Err(Error::new(lit.span(), format!("`{suffix}` is not a length or angle unit")));
    }
    Ok(term)
}

/// A calc term, paired with the index of the var it is — a lone handle is the one shape a
/// neighbouring `*`/`/` reinterprets, and only a lone one, since a parenthesised
/// sub-expression carries the dimension itself.
fn calc_term(
    input: ParseStream,
    vars_prefix: &str,
    refs: &mut Vec<(String, String, VarKind)>,
    kind: VarKind,
) -> Result<(String, Option<usize>)> {
    let lone = (input.peek(Ident) && input.peek2(Token![::])).then_some(refs.len());
    Ok((parse_calc_term(input, vars_prefix, refs, kind)?, lone))
}

/// `term (op term)*` — CSS requires the spaces around `+`/`-`, and we always emit them.
///
/// A factor is a pure number: in `a * b` or `a / b` the dimension belongs to one side and
/// the other counts it, so a var used as one is a `Number` wherever the calc itself sits.
/// Everything additive takes the calc's own dimension.
fn parse_calc_expr(
    input: ParseStream,
    vars_prefix: &str,
    refs: &mut Vec<(String, String, VarKind)>,
    kind: VarKind,
) -> Result<String> {
    let (mut out, mut lhs) = calc_term(input, vars_prefix, refs, kind)?;
    while !input.is_empty() {
        let op = if input.peek(Token![+]) {
            input.parse::<Token![+]>()?;
            "+"
        } else if input.peek(Token![-]) {
            input.parse::<Token![-]>()?;
            "-"
        } else if input.peek(Token![*]) {
            input.parse::<Token![*]>()?;
            "*"
        } else if input.peek(Token![/]) {
            input.parse::<Token![/]>()?;
            "/"
        } else {
            return Err(input.error("expected `+`, `-`, `*` or `/` between calc terms"));
        };
        let factor = matches!(op, "*" | "/");
        if let (true, Some(at)) = (factor, lhs) {
            refs[at].2 = VarKind::Number;
        }
        let (rhs, rhs_var) = calc_term(input, vars_prefix, refs, kind)?;
        if let (true, Some(at)) = (factor, rhs_var) {
            refs[at].2 = VarKind::Number;
        }
        out = format!("{out} {op} {rhs}");
        lhs = rhs_var;
    }
    Ok(out)
}

/// What a `calc(…)` measures is the property's to say, the way a transform function's
/// arguments are the function's: `flex-grow` counts, `offset-rotate` turns, and the rest of
/// the table measures. So a count handle is added to and subtracted from as itself, with no
/// factor standing in for its kind.
fn calc_kind(kind: &Kind) -> VarKind {
    match kind {
        Kind::Number | Kind::LineHeight | Kind::FontWeight => VarKind::Number,
        Kind::Angle => VarKind::Angle,
        _ => VarKind::Length,
    }
}

/// `calc( … )` at a value position.
fn parse_calc(
    input: ParseStream,
    vars_prefix: &str,
    refs: &mut Vec<(String, String, VarKind)>,
    kind: VarKind,
) -> Result<String> {
    input.parse::<Ident>()?; // `calc`
    let inner;
    parenthesized!(inner in input);
    Ok(format!("calc({})", parse_calc_expr(&inner, vars_prefix, refs, kind)?))
}

/// An angle position inside a gradient: a literal, a var, or a `calc(…)`.
fn parse_angle_value(
    input: ParseStream,
    vars_prefix: &str,
    refs: &mut Vec<(String, String, VarKind)>,
) -> Result<String> {
    if input.peek(Ident) && input.peek2(token::Paren) {
        return parse_calc(input, vars_prefix, refs, VarKind::Angle);
    }
    parse_calc_term(input, vars_prefix, refs, VarKind::Angle)
}

/// One `transform` function in the typed form. The name is snake_case in the grammar and
/// camelCase in CSS; which dimension its arguments are is the function's, not the
/// author's — `rotate` turns, everything else measures.
fn parse_transform_fn(
    input: ParseStream,
    vars_prefix: &str,
    refs: &mut Vec<(String, String, VarKind)>,
) -> Result<String> {
    let name: Ident = input.parse()?;
    let (css, kind) = match name.to_string().as_str() {
        "translate" => ("translate", VarKind::Length),
        "translate_x" => ("translateX", VarKind::Length),
        "translate_y" => ("translateY", VarKind::Length),
        "scale" => ("scale", VarKind::Length),
        "rotate" => ("rotate", VarKind::Angle),
        other => {
            return Err(Error::new(
                name.span(),
                format!(
                    "`{other}(…)` is not a transform — `translate`/`translate_x`/`translate_y`/`scale`/`rotate`"
                ),
            ))
        }
    };
    let args;
    parenthesized!(args in input);
    let mut list = Vec::new();
    while !args.is_empty() {
        list.push(parse_transform_arg(&args, vars_prefix, refs, kind)?);
        if args.peek(Token![,]) {
            args.parse::<Token![,]>()?;
        }
    }
    if list.is_empty() {
        return Err(Error::new(name.span(), format!("`{css}(…)` needs an argument")));
    }
    Ok(format!("{css}({})", list.join(", ")))
}

/// One argument of a transform function: a `calc(…)`, or the single term a calc would
/// have held — a length, an angle, a bare number, or a var handle.
fn parse_transform_arg(
    input: ParseStream,
    vars_prefix: &str,
    refs: &mut Vec<(String, String, VarKind)>,
    kind: VarKind,
) -> Result<String> {
    if input.peek(Ident) && input.peek2(token::Paren) {
        let func = input.fork().parse::<Ident>()?;
        if func != "calc" {
            return Err(Error::new(
                func.span(),
                format!("`{func}(…)` is not a transform argument — a length, an angle, a number, `Group::field`, or a `calc(…)`"),
            ));
        }
        return parse_calc(input, vars_prefix, refs, kind);
    }
    parse_calc_term(input, vars_prefix, refs, kind)
}

/// A colour position inside a gradient: a literal (`"transparent"`, `"#4fa3e3"`) or
/// a var handle — a registered var's `initial-value` is its own fallback.
fn parse_color_value(
    input: ParseStream,
    vars_prefix: &str,
    refs: &mut Vec<(String, String, VarKind)>,
) -> Result<String> {
    if input.peek(Ident) && input.peek2(Token![::]) {
        let group: Ident = input.parse()?;
        input.parse::<Token![::]>()?;
        let field = input.call(Ident::parse_any)?;
        let (group, field) = (group.to_string(), field.to_string());
        let name = var_css_name(vars_prefix, &group, &field);
        refs.push((group, field, VarKind::Color));
        return Ok(format!("var({name})"));
    }
    let lit: LitStr = input
        .parse()
        .map_err(|_| input.error("a gradient stop's colour is a string or `Group::field`"))?;
    let value = lit.value();
    if !is_color(&value, &["transparent", "inherit", "currentcolor"]) {
        return Err(Error::new(lit.span(), format!("`{value}` is not a colour")));
    }
    Ok(value)
}

/// `conic_gradient(from: <angle>, stops: [(<colour>, <position>), …])` and
/// `radial_gradient(<extent>, stops: [(<colour>, <position>), …])`.
fn parse_gradient(
    input: ParseStream,
    vars_prefix: &str,
    refs: &mut Vec<(String, String, VarKind)>,
) -> Result<String> {
    let func: Ident = input.parse()?;
    let conic = match func.to_string().as_str() {
        "conic_gradient" => true,
        "radial_gradient" => false,
        other => {
            return Err(Error::new(
                func.span(),
                format!("`{other}` is not a gradient — `conic_gradient` or `radial_gradient`"),
            ))
        }
    };
    let args;
    parenthesized!(args in input);

    let head = if conic {
        let key: Ident = args.parse()?;
        if key != "from" {
            return Err(Error::new(key.span(), "a conic gradient starts with `from: <angle>`"));
        }
        args.parse::<Token![:]>()?;
        format!("from {}", parse_angle_value(&args, vars_prefix, refs)?)
    } else {
        // The extent keyword (`closest-side` &c.), as a string so it stays a value.
        let extent: LitStr = args
            .parse()
            .map_err(|_| args.error("a radial gradient starts with its extent, e.g. `\"closest-side\"`"))?;
        let extent_value = extent.value();
        if !matches!(
            extent_value.as_str(),
            "closest-side" | "closest-corner" | "farthest-side" | "farthest-corner"
        ) {
            return Err(Error::new(extent.span(), "extent is `closest-side`, `closest-corner`, `farthest-side` or `farthest-corner`"));
        }
        extent_value
    };
    args.parse::<Token![,]>()?;

    let key: Ident = args.parse()?;
    if key != "stops" {
        return Err(Error::new(key.span(), "a gradient's colour stops are `stops: [ … ]`"));
    }
    args.parse::<Token![:]>()?;
    let list;
    syn::bracketed!(list in args);
    let mut stops = Vec::new();
    while !list.is_empty() {
        let stop;
        parenthesized!(stop in list);
        let colour = parse_color_value(&stop, vars_prefix, refs)?;
        stop.parse::<Token![,]>()?;
        let position = if conic {
            parse_angle_value(&stop, vars_prefix, refs)?
        } else {
            parse_calc_term(&stop, vars_prefix, refs, VarKind::Length)?
        };
        if !stop.is_empty() {
            return Err(stop.error("a stop is `(<colour>, <position>)`"));
        }
        stops.push(format!("{colour} {position}"));
        if list.peek(Token![,]) {
            list.parse::<Token![,]>()?;
        }
    }
    if stops.is_empty() {
        return Err(Error::new(key.span(), "a gradient needs at least one colour stop"));
    }
    if args.peek(Token![,]) {
        args.parse::<Token![,]>()?;
    }
    let name = if conic { "conic-gradient" } else { "radial-gradient" };
    Ok(format!("{name}({head}, {})", stops.join(", ")))
}

/// A `Group::field` value: lowers to `var(--…)` by the shared derivation — same
/// crate, by construction (var identity is the crate). Existence isn't checked here:
/// the attribute's typed assertion makes a dangling reference a compile error at its
/// span, and the assertion's kind is what the property demands, so a length var in a
/// color property fails to compile.
fn var_reference(
    input: ParseStream,
    property: &Property,
    vars_prefix: &str,
) -> Result<(String, Vec<(String, String, VarKind)>)> {
    let group_ident: Ident = input.parse()?;
    input.parse::<Token![::]>()?;
    let field_ident = input.call(Ident::parse_any)?;
    let kind = match property.kind {
        Kind::Color | Kind::Background => VarKind::Color,
        Kind::Length | Kind::Lengths | Kind::LengthsOrAuto | Kind::Sizing | Kind::LengthOrAuto
        // A `gap` var is a single length; the two-value form is literals only.
        | Kind::Gap => VarKind::Length,
        Kind::LineHeight | Kind::FontWeight | Kind::Number => VarKind::Number,
        Kind::FontFamily => VarKind::FontStack,
        Kind::Keyword(_)
        | Kind::ColorScheme
        | Kind::Border
        | Kind::Shadow
        | Kind::Transition
        | Kind::Angle
        | Kind::Transform
        | Kind::Filter
        | Kind::Idents
        | Kind::OffsetPath
        | Kind::Origin
        | Kind::Animation
        | Kind::Mask
        | Kind::GridTemplate
        | Kind::GridAutoFlow
        | Kind::GridLine => {
            return Err(Error::new(
                group_ident.span(),
                format!(
                    "`{}` cannot take a var — vars are single values (color, length, \
                     number, font stack), not keywords or shorthands",
                    property.rust
                ),
            ));
        }
    };
    let (group, field) = (group_ident.to_string(), field_ident.to_string());
    Ok((
        format!("var({})", var_css_name(vars_prefix, &group, &field)),
        vec![(group, field, kind)],
    ))
}

fn literal_value(lit: &Lit, property: &Property) -> Result<String> {
    match &lit {
        Lit::Str(text) => validate(property, &text.value(), text.span()),
        Lit::Int(_) | Lit::Float(_) => {
            let (number, suffix) = match &lit {
                Lit::Int(int) => (int.base10_digits().to_string(), int.suffix()),
                Lit::Float(float) => (float.base10_digits().to_string(), float.suffix()),
                _ => unreachable!(),
            };
            if !suffix.is_empty() {
                return Err(Error::new(lit.span(), "numbers here are plain (no type suffix)"));
            }
            match property.kind {
                Kind::LineHeight | Kind::Number => Ok(number),
                Kind::FontWeight => {
                    let weight: u32 = number
                        .parse()
                        .map_err(|_| Error::new(lit.span(), "font-weight is 100–900"))?;
                    if !(100..=900).contains(&weight) {
                        return Err(Error::new(lit.span(), "font-weight is 100–900"));
                    }
                    Ok(number)
                }
                _ => Err(Error::new(
                    lit.span(),
                    format!("`{}` takes a string value — only the unitless properties (line_height, font_weight, the flex factors) take bare numbers", property.rust),
                )),
            }
        }
        other => Err(Error::new(other.span(), "expected a string or number value")),
    }
}

/// Split a value into whitespace-separated tokens, but keep a balanced
/// parenthesised group whole — `calc(100% - 816px)` and `conic-gradient(…)` are
/// single tokens even though CSS requires spaces inside them.
/// Split a comma-separated value list on its **top-level** commas — the ones between
/// entries, never the ones inside a function's arguments.
fn top_level_commas(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (i, c) in value.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(value[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(value[start..].trim());
    out
}

fn value_tokens(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start, mut in_tok) = (0i32, 0usize, false);
    for (i, c) in value.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c.is_whitespace() && depth == 0 => {
                if in_tok {
                    out.push(&value[start..i]);
                    in_tok = false;
                }
                continue;
            }
            _ => {}
        }
        if !in_tok {
            start = i;
            in_tok = true;
        }
    }
    if in_tok {
        out.push(&value[start..]);
    }
    out
}

/// A `calc(…)` expression — accepted wherever a length is (the interior is CSS the
/// author owns; we check only the `calc(` wrapper and balanced parens, the same
/// bound as `rgba(…)`).
fn is_calc(part: &str) -> bool {
    part.strip_prefix("calc(").and_then(|r| r.strip_suffix(')')).is_some()
        && part.bytes().filter(|&b| b == b'(').count() == part.bytes().filter(|&b| b == b')').count()
}

fn validate(property: &Property, value: &str, span: Span) -> Result<String> {
    let parts: Vec<&str> = value_tokens(value);
    let err = |message: String| Err(Error::new(span, message));
    let one = |what: &str| -> Result<&str> {
        match parts.as_slice() {
            [part] => Ok(part),
            _ => Err(Error::new(span, format!("`{}` takes one {what}", property.rust))),
        }
    };

    match &property.kind {
        Kind::Keyword(allowed) => {
            let part = one("keyword")?;
            if allowed.contains(&part) {
                Ok(part.to_string())
            } else {
                err(format!("expected one of: {}", allowed.join(", ")))
            }
        }
        Kind::Length => {
            let part = one("length")?;
            if is_length(part) {
                Ok(part.to_string())
            } else {
                err(format!("`{part}` is not a length (`\"2rem\"`, `\"860px\"`, `\"100%\"`, `\"0\"`)"))
            }
        }
        Kind::Lengths | Kind::LengthsOrAuto => {
            let auto = matches!(property.kind, Kind::LengthsOrAuto);
            if parts.is_empty() || parts.len() > 4 {
                return err(format!("`{}` takes 1–4 lengths", property.rust));
            }
            for part in &parts {
                if !(is_length(part) || (auto && *part == "auto")) {
                    return err(format!("`{part}` is not a length{}", if auto { " or `auto`" } else { "" }));
                }
            }
            Ok(parts.join(" "))
        }
        Kind::Sizing => {
            let part = one("length, `\"auto\"`, `\"none\"`, or an intrinsic size")?;
            // The intrinsic sizes are what a box asks its content for — the only
            // honest width for something sized by the text inside it.
            if is_length(part) || matches!(part, "auto" | "none" | "max-content" | "min-content" | "fit-content") {
                Ok(part.to_string())
            } else {
                err(format!("`{part}` is not a length, `auto`, `none`, or an intrinsic size"))
            }
        }
        Kind::LengthOrAuto => {
            let part = one("length or `\"auto\"`")?;
            if is_length(part) || part == "auto" {
                Ok(part.to_string())
            } else {
                err(format!("`{part}` is not a length or `auto`"))
            }
        }
        Kind::Color => {
            let part = one("color")?;
            if is_color(part, &["transparent", "inherit", "currentcolor"]) {
                Ok(part.to_string())
            } else {
                err(format!("`{part}` is not a color (`\"#3ea5b0\"` — 3, 4, 6 or 8 hex digits — or a color keyword)"))
            }
        }
        Kind::Background => {
            let part = one("color")?;
            if is_color(part, &["transparent", "inherit", "currentcolor", "none"]) {
                Ok(part.to_string())
            } else {
                err(format!("`{part}` is not a color (`\"#3ea5b0\"`, a color keyword, or `\"none\"`)"))
            }
        }
        Kind::ColorScheme => {
            let schemes = value_tokens(value);
            match schemes.iter().find(|s| !matches!(**s, "light" | "dark" | "normal")) {
                Some(bad) => err(format!("`{bad}` is not a color scheme (`light`, `dark`, `normal`)")),
                None if schemes.is_empty() => err("color-scheme names at least one scheme".to_string()),
                None => Ok(schemes.join(" ")),
            }
        }
        Kind::Number => {
            let part = one("number")?;
            if part.parse::<f64>().is_ok() {
                Ok(part.to_string())
            } else {
                err(format!("`{part}` is not a number"))
            }
        }
        Kind::LineHeight => {
            let part = one("number or length")?;
            if part.parse::<f64>().is_ok() || is_length(part) {
                Ok(part.to_string())
            } else {
                err(format!("`{part}` is not a line-height (a unitless number or a length)"))
            }
        }
        Kind::FontWeight => {
            let part = one("weight")?;
            if matches!(part, "normal" | "bold") {
                Ok(part.to_string())
            } else {
                err(format!("`{part}` is not a font-weight — `\"normal\"`, `\"bold\"`, or a bare 100–900"))
            }
        }
        Kind::FontFamily => {
            if value.trim().is_empty() {
                err("font-family needs a font stack".into())
            } else {
                Ok(value.trim().to_string())
            }
        }
        Kind::Border => match parts.as_slice() {
            ["none"] => Ok("none".into()),
            [width, style, color]
                if is_length(width)
                    && matches!(*style, "solid" | "dashed" | "dotted")
                    && is_color(color, &["transparent", "currentcolor"]) =>
            {
                Ok(parts.join(" "))
            }
            _ => err("border is `\"none\"` or `\"<length> <solid|dashed|dotted> <color>\"`".into()),
        },
        Kind::Shadow => {
            // An optional leading `inset` (an inner shadow), then 2–4 lengths and a colour.
            let (inset, rest) = match parts.split_first() {
                Some((&"inset", rest)) => (true, rest),
                _ => (false, parts.as_slice()),
            };
            match rest {
                ["none"] if !inset => Ok("none".into()),
                [lengths @ .., color]
                    if (2..=4).contains(&lengths.len())
                        && lengths.iter().all(|part| is_length(part))
                        && is_color(color, &["transparent", "currentcolor"]) =>
                {
                    Ok(parts.join(" "))
                }
                _ => err("box-shadow is `\"none\"`, or optional `inset` then 2–4 lengths then a color".into()),
            }
        }
        Kind::Gap => {
            if (1..=2).contains(&parts.len()) && parts.iter().all(|part| is_length(part)) {
                Ok(parts.join(" "))
            } else {
                err(format!("`{}` takes one or two lengths (`\"12px\"` or `\"1px 12px\"`)", property.rust))
            }
        }
        Kind::GridTemplate => match parts.as_slice() {
            ["none"] => Ok("none".into()),
            ["subgrid"] => Ok("subgrid".into()),
            [] => err(format!("`{}` needs a track list, `\"none\"`, or `\"subgrid\"`", property.rust)),
            tracks if tracks.iter().all(|t| is_track_size(t) || is_line_names(t)) => {
                Ok(parts.join(" "))
            }
            _ => err(format!(
                "`{}` is `\"none\"`, `\"subgrid\"`, or a track list — lengths, `fr`, `auto`, \
                 an intrinsic size, or `minmax(…)`/`repeat(…)`/`fit-content(…)`",
                property.rust
            )),
        },
        Kind::GridAutoFlow => match parts.as_slice() {
            ["row"] | ["column"] | ["dense"] => Ok(parts.join(" ")),
            [flow, "dense"] if matches!(*flow, "row" | "column") => Ok(parts.join(" ")),
            _ => err("grid-auto-flow is `row`/`column`, optionally with `dense`".into()),
        },
        Kind::GridLine => {
            // `<start> / <end>` or a single placement; each side an int line, `span <n>`,
            // a named line, or `auto`.
            let ok_side = |side: &str| {
                let toks = value_tokens(side);
                match toks.as_slice() {
                    [one] => *one == "auto" || one.parse::<i32>().is_ok() || is_css_ident(one),
                    ["span", n] => n.parse::<u32>().is_ok() || is_css_ident(n),
                    _ => false,
                }
            };
            let sides: Vec<&str> = value.split('/').map(str::trim).collect();
            if (1..=4).contains(&sides.len()) && sides.iter().all(|s| ok_side(s)) {
                Ok(sides.join(" / "))
            } else {
                err(format!(
                    "`{}` is `/`-separated grid lines — an integer, `span <n>`, a name, or `auto`",
                    property.rust
                ))
            }
        }
        Kind::Transition => {
            // Comma-separated transitions; each is `<prop> <duration> [easing]`
            // (`prop` may be a custom property `--name` for compositor interpolation).
            let easings = ["ease", "ease-in", "ease-out", "ease-in-out", "linear"];
            // A `cubic-bezier(…)` is four numbers; the two ordinate ones may leave
            // [0,1], which is exactly how a control overshoots and settles.
            let is_easing = |e: &str| {
                easings.contains(&e)
                    || e.strip_prefix("cubic-bezier(")
                        .and_then(|rest| rest.strip_suffix(')'))
                        .is_some_and(|args| {
                            let n: Vec<_> = args.split(',').map(str::trim).collect();
                            n.len() == 4 && n.iter().all(|a| a.parse::<f64>().is_ok())
                        })
            };
            let ok_one = |t: &str| match value_tokens(t).as_slice() {
                ["none"] => true,
                [prop, duration, easing @ ..] => {
                    easing.len() <= 1
                        && (is_css_ident(prop) || prop.starts_with("--"))
                        && is_duration(duration)
                        && easing.iter().all(|e| is_easing(e))
                }
                _ => false,
            };
            // Split on the commas *between* transitions, not the ones inside an
            // easing's argument list.
            let list = top_level_commas(value);
            if list.iter().all(|t| ok_one(t)) {
                Ok(list.join(", "))
            } else {
                err("transition is `\"none\"` or `\"<property> <duration> [<easing>]\"`, comma-separated".into())
            }
        }
        Kind::Angle => {
            let part = one("angle")?;
            if is_angle(part) {
                Ok(part.to_string())
            } else {
                err(format!("`{part}` is not an angle (`\"45deg\"`, `\"0.5turn\"`, or a `calc(…)`)"))
            }
        }
        Kind::Transform => {
            if parts.iter().all(|p| is_transform_fn(p)) && !parts.is_empty() {
                Ok(parts.join(" "))
            } else {
                err("transform is a space-separated list of `scale(…)`/`rotate(…)`/`translate(…)`/`translateX(…)`/`translateY(…)`".into())
            }
        }
        Kind::Filter => {
            if value == "none" {
                Ok("none".into())
            } else if parts.iter().all(|p| is_filter_fn(p)) && !parts.is_empty() {
                Ok(parts.join(" "))
            } else {
                err("filter is `none` or a space-separated list of `brightness(…)`/`saturate(…)`/`blur(…)`/`contrast(…)`/`grayscale(…)`/`sepia(…)`/`invert(…)`/`opacity(…)`/`hue-rotate(…)`/`drop-shadow(…)`".into())
            }
        }
        Kind::Idents => {
            if value.split(',').map(str::trim).all(is_css_ident) {
                Ok(value.split(',').map(str::trim).collect::<Vec<_>>().join(", "))
            } else {
                err("will-change is a comma list of property names".into())
            }
        }
        Kind::OffsetPath => {
            let part = one("`\"none\"` or a `path(\"…\")`")?;
            if part == "none"
                || (part.starts_with("path(") && part.ends_with(')'))
            {
                Ok(part.to_string())
            } else {
                err("offset-path is `\"none\"` or `path(\"…\")`".into())
            }
        }
        Kind::Origin => {
            let ok = |p: &str| {
                matches!(p, "top" | "left" | "right" | "bottom" | "center") || is_length(p)
            };
            if (1..=2).contains(&parts.len()) && parts.iter().all(|p| ok(p)) {
                Ok(parts.join(" "))
            } else {
                err("transform-origin is one or two of `top`/`right`/`bottom`/`left`/`center` or lengths".into())
            }
        }
        // Handled in `parse_value` (a keyframes handle + tail), never a plain string.
        Kind::Animation => {
            err("`animation` is `Handle \"<duration> [easing] [iteration]\"`".into())
        }
        // Handled in `parse_value` (a gradient), never a plain string.
        Kind::Mask => err("`mask` is a `radial_gradient(…)`/`conic_gradient(…)`".into()),
    }
}

/// A single `transform` function token: `scale(…)`, `rotate(…)`, `translate(…)`,
/// `translateX(…)`, `translateY(…)` — the interior is CSS the author owns.
fn is_transform_fn(part: &str) -> bool {
    ["scale(", "rotate(", "translate(", "translateX(", "translateY("]
        .iter()
        .any(|f| part.starts_with(f))
        && part.ends_with(')')
}

/// A single `filter` function token: `brightness(…)`, `saturate(…)`, `blur(…)`, and the
/// rest of the CSS filter primitives — the interior is CSS the author owns.
fn is_filter_fn(part: &str) -> bool {
    [
        "brightness(", "saturate(", "blur(", "contrast(", "grayscale(", "sepia(",
        "invert(", "opacity(", "hue-rotate(", "drop-shadow(",
    ]
    .iter()
    .any(|f| part.starts_with(f))
        && part.ends_with(')')
}

/// One grid track size: a length, an `fr` flex factor, `auto`, an intrinsic size, or a
/// `minmax(…)`/`repeat(…)`/`fit-content(…)` function (its interior is CSS the author owns).
fn is_track_size(part: &str) -> bool {
    is_length(part)
        || matches!(part, "auto" | "min-content" | "max-content" | "fit-content")
        || part
            .strip_suffix("fr")
            .is_some_and(|number| !number.is_empty() && number.parse::<f64>().is_ok())
        || (["minmax(", "repeat(", "fit-content("].iter().any(|f| part.starts_with(f))
            && part.ends_with(')'))
}

/// A grid line-name group, written `[name …]` between tracks.
fn is_line_names(part: &str) -> bool {
    part.starts_with('[') && part.ends_with(']')
}

fn is_length(part: &str) -> bool {
    if is_calc(part) {
        return true;
    }
    let part = part.strip_prefix('-').unwrap_or(part);
    if part == "0" {
        return true;
    }
    for unit in ["px", "rem", "em", "ch", "vw", "vh", "%"] {
        if let Some(number) = part.strip_suffix(unit) {
            return !number.is_empty() && number.parse::<f64>().is_ok();
        }
    }
    false
}

/// A CSS percentage (`40%`, `-10%`) — a numeric with the one unit, never `calc`.
fn is_percentage(part: &str) -> bool {
    let part = part.strip_prefix('-').unwrap_or(part);
    part.strip_suffix('%').is_some_and(|number| !number.is_empty() && number.parse::<f64>().is_ok())
}

/// A CSS angle (`45deg`, `0.5turn`, `1rad`) — or a `calc(…)` producing one.
fn is_angle(part: &str) -> bool {
    if is_calc(part) {
        return true;
    }
    for unit in ["deg", "turn", "rad", "grad"] {
        if let Some(number) = part.strip_suffix(unit) {
            return !number.is_empty() && number.parse::<f64>().is_ok();
        }
    }
    part == "0"
}

fn is_color(part: &str, keywords: &[&str]) -> bool {
    if keywords.contains(&part) {
        return true;
    }
    if let Some(digits) = part.strip_prefix('#') {
        return matches!(digits.len(), 3 | 4 | 6 | 8)
            && digits.chars().all(|c| c.is_ascii_hexdigit());
    }
    // `rgb(…)`/`rgba(…)` — the value tokenizer keeps a parenthesised group whole,
    // so the channels may be spaced as CSS normally writes them.
    let inner = part
        .strip_prefix("rgba(")
        .or_else(|| part.strip_prefix("rgb("))
        .and_then(|rest| rest.strip_suffix(')'));
    inner.is_some_and(|inner| {
        let channels: Vec<&str> = inner.split(',').map(str::trim).collect();
        matches!(channels.len(), 3 | 4)
            && channels[..3]
                .iter()
                .all(|c| c.parse::<u16>().is_ok_and(|v| v <= 255))
            && channels
                .get(3)
                .is_none_or(|a| a.parse::<f64>().is_ok_and(|v| (0.0..=1.0).contains(&v)))
    })
}

fn is_duration(part: &str) -> bool {
    for unit in ["ms", "s"] {
        if let Some(number) = part.strip_suffix(unit) {
            return !number.is_empty() && number.parse::<f64>().is_ok();
        }
    }
    false
}

fn is_css_ident(part: &str) -> bool {
    !part.is_empty()
        && part
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;
    use syn::parse_quote;

    fn style(body: TokenStream) -> Vec<Atom> {
        parse_style("i0badf00d", "v0badf00d", "PAGE", body).unwrap()
    }

    fn style_err(body: TokenStream) -> String {
        parse_style("i0badf00d", "v0badf00d", "PAGE", body).map(drop).unwrap_err().to_string()
    }

    #[test]
    fn a_var_group_lowers_to_root_rules_with_dark_overrides() {
        let group = parse_vars(
            "v0badf00d",
            quote! {
                pub Palette {
                    page: { "#ffffff", dark: "#101214" },
                    accent: "#2a9d90",
                    ink_muted: { "#5b6572", dark: "#98a2ae" },
                }
            },
        )
        .unwrap();
        assert_eq!(group.name, "v0badf00d-palette");
        assert_eq!(group.ident, "Palette");
        assert_eq!(group.vars[0].css_name, "--v0badf00d-palette-page");
        assert_eq!(group.vars[2].css_name, "--v0badf00d-palette-ink-muted");
        assert_eq!(
            group.rule(),
            ":root{--v0badf00d-palette-page:#ffffff;--v0badf00d-palette-accent:#2a9d90;\
             --v0badf00d-palette-ink-muted:#5b6572;}\
             @media (prefers-color-scheme: dark){:root{--v0badf00d-palette-page:#101214;\
             --v0badf00d-palette-ink-muted:#98a2ae;}}"
        );
    }

    /// A registered var is what a keyframe may sweep — the registration is the browser's
    /// contract that the property interpolates, so its initial value must match the syntax
    /// it declares.
    #[test]
    fn a_registered_var_declares_its_syntax_and_a_keyframe_sweeps_it() {
        let group = parse_vars("v0badf00d", quote! { pub Sweep { edge: length("0px") } }).unwrap();
        assert_eq!(group.vars[0].kind, VarKind::Length);
        assert_eq!(
            group.rule(),
            "@property --v0badf00d-sweep-edge\
             {syntax:'<length>';inherits:false;initial-value:0px;}"
        );

        let vars_err = |body| parse_vars("v0badf00d", body).map(drop).unwrap_err().to_string();
        assert!(
            vars_err(quote! { pub Sweep { edge: length("10%") } })
                .contains("not a valid initial value"),
            "a % initial under <length> is an invalid registration and must not parse",
        );

        let kf = keyframes_list(
            &parse_quote! {
                mod styles {
                    keyframes! {
                        pub Slide {
                            from { Sweep::edge: "0px" }
                            to { Sweep::edge: "112px" }
                        }
                    }
                }
            },
            "v0badf00d",
        )
        .unwrap();
        assert_eq!(
            kf[0].var_refs,
            vec![
                ("Sweep".into(), "edge".into(), VarKind::Length),
                ("Sweep".into(), "edge".into(), VarKind::Length),
            ],
            "one assertion per step's reference",
        );
        assert_eq!(
            kf[0].css,
            "@keyframes kv0badf00d-slide\
             {from{--v0badf00d-sweep-edge:0px;}to{--v0badf00d-sweep-edge:112px;}}"
        );
    }

    #[test]
    fn css_var_references_derive_the_shared_name() {
        // Derivation is syntactic — declaration and reference meet at var_css_name,
        // and existence is the attribute's typed assertion (compile-time), so a
        // cross-file same-crate reference needs no lookup here.
        let atoms = style(quote! {{
            color: Palette::accent,
            background: Palette::page,
            dark: { color: Palette::ink_muted },
        }});
        assert_eq!(atoms[0].value, "var(--v0badf00d-palette-accent)");
        assert_eq!(atoms[0].var_refs, vec![("Palette".to_string(), "accent".to_string(), VarKind::Color)]);
        assert_eq!(
            atoms[0].value,
            format!("var({})", var_css_name("v0badf00d", "Palette", "accent"))
        );
        assert_eq!(atoms[1].rule(), ".i0badf00d-page-background{background:var(--v0badf00d-palette-page)}");
        assert_eq!(
            atoms[2].rule(),
            "@media (prefers-color-scheme: dark){.i0badf00d-page-color-k{color:var(--v0badf00d-palette-ink-muted)}}"
        );
    }

    #[test]
    fn gap_takes_a_length_var() {
        // A `gap` var is a single length; the two-value `gap` form is literals only.
        // Regression: widening `gap` to accept two lengths must not stop it taking a var
        // (the server-side extract path re-parses, so this must hold there too).
        let atoms = style(quote! {{ gap: Palette::space }});
        assert_eq!(atoms[0].value, "var(--v0badf00d-palette-space)");
        assert_eq!(
            atoms[0].var_refs,
            vec![("Palette".to_string(), "space".to_string(), VarKind::Length)]
        );
    }

    #[test]
    fn a_transform_takes_a_var_through_calc() {
        // The string form cannot name a var — a var's CSS name is derived, not written — so
        // a transform driven by one is written as structure. `rotate` demands an angle and
        // everything else a length, which is what the *function* means rather than what the
        // author remembered to pass.
        let atoms = style(quote! {{ transform: translate_y(calc(Knob::at * 100%)) }});
        assert_eq!(atoms[0].value, "translateY(calc(var(--v0badf00d-knob-at) * 100%))");
        // A factor counts the other operand rather than measuring anything, so the handle is
        // a `Number` even though the calc it sits in produces a length.
        assert_eq!(
            atoms[0].var_refs,
            vec![("Knob".to_string(), "at".to_string(), VarKind::Number)]
        );
        assert_eq!(
            style(quote! {{ width: calc((100% - 8px) / Knob::n) }})[0].var_refs[0].2,
            VarKind::Number
        );
        // Additive terms still take the calc's own dimension: nothing is being counted.
        assert_eq!(
            style(quote! {{ width: calc(100% - Palette::space) }})[0].var_refs[0].2,
            VarKind::Length
        );

        let listed = style(quote! {{ transform: translate_x(4px) rotate(Motion::a) }});
        assert_eq!(listed[0].value, "translateX(4px) rotate(var(--v0badf00d-motion-a))");
        assert_eq!(listed[0].var_refs[0].2, VarKind::Angle);

        // The string form still holds, so a transform with nothing to resolve stays one.
        assert_eq!(style(quote! {{ transform: "rotate(360deg)" }})[0].value, "rotate(360deg)");

        assert!(style_err(quote! {{ transform: skew(4deg) }}).contains("is not a transform"));
        assert!(style_err(quote! {{ transform: translate_y(min(1px,2px)) }})
            .contains("is not a transform argument"));
    }

    #[test]
    fn a_calc_takes_the_dimension_of_the_property_it_lands_in() {
        // `flex-grow` counts, so a handle added to a bare number is a number where the same
        // handle under `width` measures — and needs no factor standing in for its kind.
        let atoms = style(quote! {{ flex_grow: calc(Grid::clients - 10) }});
        assert_eq!(atoms[0].value, "calc(var(--v0badf00d-grid-clients) - 10)");
        assert_eq!(
            atoms[0].var_refs,
            vec![("Grid".to_string(), "clients".to_string(), VarKind::Number)]
        );
        assert_eq!(
            style(quote! {{ opacity: calc(1 - Fade::at) }})[0].var_refs[0].2,
            VarKind::Number
        );
        assert_eq!(
            style(quote! {{ offset_rotate: calc(Motion::a + 90deg) }})[0].var_refs[0].2,
            VarKind::Angle
        );
    }

    #[test]
    fn media_and_element_compose_media_outside() {
        let atoms = style(quote! {{
            mobile: { h1: { font_size: "1.75rem" } },
        }});
        assert_eq!(atoms[0].class, "i0badf00d-page-font-size-m-e-h1");
        assert_eq!(
            atoms[0].rule(),
            "@media (max-width: 640px){.i0badf00d-page-font-size-m-e-h1 h1{font-size:1.75rem}}"
        );
        assert!(style_err(quote! {{ h1: { mobile: { font_size: "1rem" } } }})
            .contains("media-outside"));
        assert!(style_err(quote! {{ mobile: { ":hover": { color: "#fff" } } }})
            .contains("don't nest"));
    }

    #[test]
    fn var_misuse_fails_loud() {
        assert!(style_err(quote! {{ display: Palette::accent }}).contains("cannot take a var"));
        assert!(style_err(quote! {{ border: Palette::line }}).contains("cannot take a var"));

        let vars_err = |body| parse_vars("v0badf00d", body).map(drop).unwrap_err().to_string();
        assert!(vars_err(quote! { pub palette { a: "#fff" } }).contains("CamelCase"));
        assert!(vars_err(quote! { pub Palette { a: "#fff", a: "#000" } }).contains("declared twice"));
        assert!(vars_err(quote! { pub Palette { a: { "#fff", mobile: "#000" } } })
            .contains("`dark` is the one per-var condition"));
        assert!(vars_err(quote! { pub Space { a: { "8px", dark: "12px" } } })
            .contains("`dark` twins are for color vars"));
        assert!(vars_err(quote! { pub Palette {} }).contains("declares nothing"));
    }

    #[test]
    fn var_kinds_are_the_values_shape_and_references_carry_the_demanded_kind() {
        let group = parse_vars(
            "v0badf00d",
            quote! {
                pub Tokens {
                    accent: "#2a9d90",
                    control_h: "32px",
                    body_line: 1.5,
                    sans: "ui-sans-serif, system-ui",
                    scrim: "rgba(16,18,20,0.6)",
                }
            },
        )
        .unwrap();
        let kinds: Vec<VarKind> = group.vars.iter().map(|v| v.kind).collect();
        assert_eq!(
            kinds,
            [VarKind::Color, VarKind::Length, VarKind::Number, VarKind::FontStack, VarKind::Color]
        );

        let atoms = style(quote! {{
            padding: Tokens::control_h,
            line_height: Tokens::body_line,
            font_family: Tokens::sans,
        }});
        // `padding` is four atoms, one per side, and each carries the reference — the
        // var is demanded as a length by every one of them.
        let kind_of = |suffix: &str| {
            atoms
                .iter()
                .find(|a| a.class.ends_with(suffix))
                .unwrap_or_else(|| panic!("a {suffix} atom"))
                .var_refs[0]
                .2
        };
        assert_eq!(kind_of("-padding-top"), VarKind::Length);
        assert_eq!(kind_of("-padding-left"), VarKind::Length);
        assert_eq!(kind_of("-line-height"), VarKind::Number);
        assert_eq!(kind_of("-font-family"), VarKind::FontStack);
        assert_eq!(
            atoms[0].rule(),
            ".i0badf00d-page-padding-top{padding-top:var(--v0badf00d-tokens-control-h)}"
        );
    }

    #[test]
    fn declarations_become_atoms_with_declaration_site_classes() {
        let atoms = style(quote! {{
            max_width: "860px",
            margin: "0 auto",
            line_height: 1.6,
            font_weight: 600,
            color: "#1c1e21",
        }});

        let classes: Vec<&str> = atoms.iter().map(|a| a.class.as_str()).collect();
        assert_eq!(
            classes,
            [
                "i0badf00d-page-max-width",
                "i0badf00d-page-margin-top",
                "i0badf00d-page-margin-right",
                "i0badf00d-page-margin-bottom",
                "i0badf00d-page-margin-left",
                "i0badf00d-page-line-height",
                "i0badf00d-page-font-weight",
                "i0badf00d-page-color",
            ]
        );
        assert_eq!(atoms[0].rule(), ".i0badf00d-page-max-width{max-width:860px}");
        assert_eq!(atoms[5].value, "1.6");
        assert_eq!(atoms[6].value, "600");
    }

    #[test]
    fn conditions_suffix_the_class_and_shape_the_rule() {
        let atoms = style(quote! {{
            padding: "2rem",
            ":hover": { background: "#f4f6f8" },
            dark: { color: "#e6e8ea" },
            mobile: { padding: "1.25rem 1rem" },
            desktop: { padding: "3rem" },
            pre: { background: "#11151d" },
        }});

        let by_class: Vec<(&str, String)> =
            atoms.iter().map(|a| (a.class.as_str(), a.rule())).collect();
        let rule_of = |class: &str| {
            by_class
                .iter()
                .find(|(have, _)| *have == class)
                .unwrap_or_else(|| panic!("a {class} atom"))
                .1
                .clone()
        };

        // The condition suffixes the class and wraps the rule, and it does so per
        // *longhand*: a shorthand under a condition expands there too, so a mobile
        // `padding` and a base `padding_left` still collide on one key.
        assert_eq!(rule_of("i0badf00d-page-padding-top"), ".i0badf00d-page-padding-top{padding-top:2rem}");
        assert_eq!(
            rule_of("i0badf00d-page-background-h"),
            ".i0badf00d-page-background-h:hover{background:#f4f6f8}"
        );
        assert_eq!(
            rule_of("i0badf00d-page-color-k"),
            "@media (prefers-color-scheme: dark){.i0badf00d-page-color-k{color:#e6e8ea}}"
        );
        assert_eq!(
            rule_of("i0badf00d-page-padding-left-m"),
            "@media (max-width: 640px){.i0badf00d-page-padding-left-m{padding-left:1rem}}"
        );
        assert_eq!(
            rule_of("i0badf00d-page-padding-top-d"),
            "@media (min-width: 641px){.i0badf00d-page-padding-top-d{padding-top:3rem}}"
        );
        assert_eq!(
            rule_of("i0badf00d-page-background-e-pre"),
            ".i0badf00d-page-background-e-pre pre{background:#11151d}"
        );
        assert_eq!(by_class.len(), 15, "four sides in each of three conditions, plus three others");
    }

    #[test]
    fn quoted_keys_and_trailing_commas_are_the_js_object_forms() {
        let atoms = style(quote! {{
            "max_width": "860px",
            "dark": { "color": "#e6e8ea" },
        }});
        assert_eq!(atoms.len(), 2);
        assert_eq!(atoms[0].class, "i0badf00d-page-max-width");
        assert_eq!(atoms[1].class, "i0badf00d-page-color-k");
    }

    #[test]
    fn last_wins_per_property_and_condition_within_a_body() {
        let atoms = style(quote! {{
            padding: "1rem",
            padding: "2rem",
            mobile: { padding: "1rem" },
        }});
        // Four sides in each of two conditions; the re-declared `padding` replaced the
        // first side-for-side rather than landing beside it.
        assert_eq!(atoms.len(), 8);
        assert_eq!(atoms[0].value, "2rem");
        assert_eq!(atoms[4].condition, Condition::Media(MOBILE));
    }

    #[test]
    fn an_admitted_shorthand_carries_an_expansion() {
        // An atom's property must be a longhand, or two atoms touching the same thing
        // collide in the browser instead of in `merge`. A shorthand relationship cannot be
        // read off the names — `color-scheme` is not part of `color`, nor `border-radius`
        // of `border` — so the expansion table is the declaration, and what this pins is
        // that it stays well-formed: every property it names is admitted, and every
        // expansion lands on longhands that do not themselves expand.
        for property in PROPERTIES {
            let sample = match property.kind {
                Kind::Border => "1px solid #ddd",
                _ => "0",
            };
            for (target, _) in expand_shorthand(property, sample) {
                assert!(
                    self::property(target.rust).is_some(),
                    "`{}` expands into `{}`, which is not in the table",
                    property.rust,
                    target.rust
                );
                if target.css != property.css {
                    assert_eq!(
                        expand_shorthand(target, sample).len(),
                        1,
                        "`{}` expands into `{}`, which is itself a shorthand",
                        property.rust,
                        target.rust
                    );
                }
            }
        }

        // A `Kind::Border` value is three facets in one token stream, so a property of that
        // kind is a shorthand by construction and must never reach the cascade intact —
        // unexpanded, it is indistinguishable from a longhand and `merge` stops seeing the
        // collision. Taken from the kind rather than a list of sides, because a list of
        // sides is the thing that forgets one.
        for property in PROPERTIES.iter().filter(|p| matches!(p.kind, Kind::Border)) {
            assert!(
                expand_shorthand(property, "1px solid #ddd")
                    .iter()
                    .all(|(target, _)| target.css != property.css),
                "`{}` survives expansion as itself",
                property.rust
            );
        }

        // The families that do expand, pinned by their arity so a dropped side or facet
        // is a failure rather than a silently narrower expansion.
        let arity = |rust: &str, value: &str| expand_shorthand(self::property(rust).unwrap(), value).len();
        assert_eq!(arity("padding", "0"), 4);
        assert_eq!(arity("margin", "0 auto"), 4);
        assert_eq!(arity("inset", "0"), 4);
        assert_eq!(arity("border", "1px solid #ddd"), 12);
        assert_eq!(arity("border", "none"), 4);
        assert_eq!(arity("border_left", "1px solid #ddd"), 3);
        assert_eq!(arity("outline", "1px solid #ddd"), 3);
        assert_eq!(arity("border_color", "#ddd"), 4);
    }

    #[test]
    fn every_value_kind_reads_as_css() {
        let atoms = style(quote! {{
            display: "inline-block",
            font_family: "ui-monospace, Menlo, monospace",
            font_size: "0.9em",
            font_weight: "bold",
            line_height: "1.2rem",
            width: "auto",
            margin_top: "-2px",
            border: "1px solid #ddd",
            box_shadow: "0 1px 3px #00000022",
            transition: "background 0.2s ease-in-out",
            background: "none",
            gap: "100%",
            padding: "0",
        }});
        let values: Vec<&str> = atoms.iter().map(|a| a.value.as_str()).collect();
        assert_eq!(
            values,
            [
                "inline-block",
                "ui-monospace, Menlo, monospace",
                "0.9em",
                "bold",
                "1.2rem",
                "auto",
                "-2px",
                // `border: "1px solid #ddd"` is four sides of three facets.
                "1px", "solid", "#ddd",
                "1px", "solid", "#ddd",
                "1px", "solid", "#ddd",
                "1px", "solid", "#ddd",
                "0 1px 3px #00000022",
                "background 0.2s ease-in-out",
                "none",
                "100%",
                // `padding: "0"` is one authored value and four atoms.
                "0",
                "0",
                "0",
                "0",
            ]
        );
    }

    #[test]
    fn bad_values_fail_at_expansion_with_the_reason() {
        assert!(style_err(quote! {{ color: "1c1e21" }}).contains("not a color"));
        assert!(style_err(quote! {{ padding: "2" }}).contains("not a length"));
        assert!(style_err(quote! {{ padding: 2 }}).contains("string value"));
        assert!(style_err(quote! {{ font_weight: 950 }}).contains("100–900"));
        assert!(style_err(quote! {{ display: "banner" }}).contains("expected one of"));
        assert!(style_err(quote! {{ margin: "0 auto 0 auto 0" }}).contains("1–4 lengths"));
        assert!(style_err(quote! {{ border: "1px wavy #ddd" }}).contains("border"));
        assert!(style_err(quote! {{ transition: "background fast" }}).contains("transition"));
    }

    #[test]
    fn unknown_properties_and_conditions_fail_loud() {
        assert!(style_err(quote! {{ float: "left" }}).contains("property table"));
        assert!(style_err(quote! {{ ":focus": { color: "#fff" } }}).contains("\":hover\""));
        assert!(style_err(quote! {{ padding: { top: "1rem" } }}).contains("takes a value, not a block"));
        assert!(style_err(quote! {{ Dark: { color: "#fff" } }}).contains("not a condition"));
        assert!(style_err(quote! {{ dark: { mobile: { color: "#fff" } } }}).contains("don't nest"));
    }

    #[test]
    fn class_prefix_hashes_the_path_identity() {
        let a = class_prefix("todo-app/src/styles.rs");
        let b = class_prefix("todo-app/src/lib.rs");
        assert_eq!(a.len(), 9);
        assert!(a.starts_with('i'));
        assert_ne!(a, b);
        assert_eq!(a, class_prefix("todo-app/src/styles.rs"));
    }

    #[test]
    fn path_identity_is_package_slash_forward_slashed_relative_path() {
        // Each platform's own absolute paths: on Windows this exercises the
        // backslash→`/` normalization, on Unix the already-forward-slashed pass-through.
        // (A `Path` built from the other platform's separators is a single opaque
        // component here, so the paths must be native to the host running the test.)
        let (manifest, file, outside) = if cfg!(windows) {
            (
                Path::new(r"C:\work\idyll\examples\todo\app"),
                Path::new(r"C:\work\idyll\examples\todo\app\src\styles.rs"),
                Path::new(r"C:\elsewhere\styles.rs"),
            )
        } else {
            (
                Path::new("/work/idyll/examples/todo/app"),
                Path::new("/work/idyll/examples/todo/app/src/styles.rs"),
                Path::new("/elsewhere/styles.rs"),
            )
        };
        assert_eq!(
            path_identity("todo-app", manifest, file).as_deref(),
            Some("todo-app/src/styles.rs")
        );
        assert_eq!(path_identity("todo-app", manifest, outside), None);
    }

    #[test]
    fn style_consts_finds_css_initializers_and_skips_the_rest() {
        let module: syn::ItemMod = syn::parse_quote! {
            mod styles {
                use idyll_styles::Style;
                pub const PAGE: Style = css! {{ padding: "2rem" }};
                pub const COLUMNS: usize = 3;
                pub const CARD: Style = css! {{ gap: "1rem" }};
            }
        };
        let consts = style_consts(&module).unwrap();
        assert_eq!(
            consts.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["PAGE", "CARD"]
        );
        let atoms = parse_style("i0badf00d", "v0badf00d", &consts[0].name, consts[0].body.clone()).unwrap();
        assert_eq!(atoms[0].class, "i0badf00d-page-padding-top");
    }

    #[test]
    fn a_module_without_a_body_is_rejected() {
        let module: syn::ItemMod = syn::parse_quote!(mod styles;);
        assert!(style_consts(&module)
            .map(drop)
            .unwrap_err()
            .to_string()
            .contains("inline module body"));
    }
}
