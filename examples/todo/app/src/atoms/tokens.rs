//! The todo example's design tokens — crate vocabulary, Astryx-neutral, dark twins on every
//! surface color. Same shape as the blog's tokens; the values are this app's own.

use idyll_styles::styles;

#[styles]
pub mod styles {
    use idyll_styles::Style;

    vars! {
        pub Palette {
            page:  { "#ffffff", dark: "#101214" },
            ink:   { "#1c1e21", dark: "#e6e8ea" },
            muted: { "#8a9099", dark: "#6b7280" },
            accent: "#2a9d90",
        }
    }

    vars! {
        pub Space {
            s1: "4px",
            s2: "8px",
            s3: "12px",
            s4: "16px",
            s6: "24px",
        }
    }

    vars! {
        pub Face {
            sans: "system-ui, sans-serif",
        }
    }

    /// The focus ring. Every focusable surface wears the same one, so it is a style
    /// composed in rather than a block each component repeats.
    pub const FOCUS: Style = css! {{
        ":focus-visible": {
            outline_width: "2px",
            outline_style: "solid",
            outline_color: Palette::accent,
            outline_offset: "1px",
        },
    }};
}

pub use styles::*;
