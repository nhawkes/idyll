//! The agreement fixture: compiled through `#[styles]` by tests/extract.rs AND read
//! from source by the extractor — the two must produce identical classes and rules.

use idyll_styles::styles;

#[styles]
pub mod styles {
    use idyll_styles::Style;

    vars! {
        pub Palette {
            ink: { "#1c1e21", dark: "#e6e8ea" },
            accent: "#2a9d90",
            scrim: "rgba(16,18,20,0.6)",
        }
    }

    vars! {
        pub Metrics {
            control: "32px",
            body_line: 1.5,
            sans: "ui-sans-serif, system-ui",
        }
    }

    theme! {
        /// A region that redefines the group for its subtree: one field restated, one
        /// remapped onto another var so the value has a single definition.
        pub Inverted: Palette {
            ink: "#ffffff",
            accent: Palette::ink,
        }
    }

    document! {
        root { color_scheme: "light dark" },
        body { margin: "0", background: Palette::ink },
    }

    pub const INKED: Style = css! {{
        color: Palette::ink,
        background: Palette::accent,
        height: Metrics::control,
        line_height: Metrics::body_line,
        font_family: Metrics::sans,
    }};

    pub const BANNER: Style = css! {{
        padding: "1rem 2rem",
        color: "#e6e8ea",
        line_height: 1.5,
        dark: { background: "#101214" },
        ":hover": { color: "#ffffff" },
        mobile: { padding: "0.5rem" },
        code: { font_size: "0.9em" },
    }};

    pub const QUIET: Style = css! {{
        color: "#8a9099",
        user_select: "none",
        box_sizing: "border-box",
        text_align: "center",
        min_width: "4rem",
        ":active": { background: "rgba(0,0,0,0.08)" },
        ":checked": { border_color: "#2a9d90" },
        ":disabled": { pointer_events: "none" },
    }};

    /// The conditions that reach past the element, and the ones that ask about the
    /// viewport rather than the element.
    pub const RESPONSIVE: Style = css! {{
        display: "flex",
        child(input): { opacity: 0 },
        child(label, last): { border_radius: "0 6px 6px 0" },
        checked_next(input, label): { color: "#ffffff" },
        focus_next(input, label): { outline: "2px solid #2a9d90" },
        checked_pairs(input, pre, 2): { display: "block" },
        pointer_coarse: { padding: "9px 14px" },
        max_width(680px): { width: "100%" },
        hover_child(small): { visibility: "visible" },
        focus_within_child(small): { visibility: "visible" },
    }};

    /// The typographic properties: tracked uppercase labels, italics, tabular
    /// figures, and the underline treatment on a plotted number.
    pub const TYPESET: Style = css! {{
        letter_spacing: "0.06em",
        text_transform: "uppercase",
        font_style: "italic",
        font_variant_numeric: "tabular-nums",
        text_underline_offset: "2px",
        text_decoration_thickness: "1.5px",
        text_wrap: "pretty",
        transition: "transform 320ms cubic-bezier(.34,1.4,.5,1), color 240ms",
    }};

    /// Motion value grammar: transforms, offsets, SVG, calc, will-change.
    pub const MOTION: Style = css! {{
        transform: "scale(0.85)",
        transform_origin: "top left",
        margin_left: "calc((100% - 816px) / 2)",
        will_change: "offset-distance, transform",
        offset_rotate: "0deg",
        offset_distance: "100%",
        offset_path: "none",
        opacity: 0.5,
        svg: {
            fill: "none",
            stroke: "#e87b3e",
            stroke_width: "1.5px",
            stroke_linejoin: "round",
        },
    }};
}
