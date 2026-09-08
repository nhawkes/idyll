//! Button — the todo example's action atom. Same shape as the blog's (kind
//! variant enum, fn-pointer handler): the design system is per-app for now, but
//! the API is uniform so promotion to a shared crate is mechanical.

use idyll::{live_view, Callback, Ctx, Event, Never, Setup};
use idyll_styles::styles;

use super::tokens::FOCUS;

pub enum ButtonKind {
    /// The filled call-to-action (Add).
    Primary,
}

#[idyll::component]
pub async fn Button(
    ctx: Ctx<Setup, Never>,
    kind: ButtonKind,
    label: &'static str,
    pressed: Callback<Event>,
) -> idyll::Result {
    let variant = match kind {
        ButtonKind::Primary => styles::PRIMARY,
    };
    Ok(ctx.render(live_view! {
        button css=[styles::BASE, FOCUS, variant] onclick=(pressed) { (label) }
    }).await?)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::{Face, Palette, Space};

    pub const BASE: Style = css! {{
        font_family: Face::sans,
        font_size: "0.9375rem",
        cursor: "pointer",
    }};

    pub const PRIMARY: Style = css! {{
        color: Palette::page,
        background: Palette::ink,
        border: "none",
        border_radius: "6px",
        padding: Space::s2,
        ":active": { background: Palette::muted },
    }};
}
