//! The todo example's atoms: design tokens and the page chrome's styles. Per-app for now —
//! promotion to a shared crate waits for the third-instance rule.

use idyll_styles::styles;

pub mod button;
pub mod input;
pub mod toast;
pub mod tokens;

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use super::tokens::{Face, Palette, Space};

    /// The page canvas both routes render into.
    pub const PAGE: Style = css! {{
        font_family: Face::sans,
        max_width: "32rem",
        margin: "3rem auto",
        padding: Space::s4,
        color: Palette::ink,
        background: Palette::page,
    }};

    pub const TITLE: Style = css! {{
        font_size: "1.4rem",
    }};

    /// The count pill over the list.
    pub const BADGE: Style = css! {{
        font_weight: 600,
    }};
}
