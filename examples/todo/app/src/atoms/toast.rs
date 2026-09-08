//! A transient notice, pinned to the viewport. No portal: a fixed-position element
//! is already out of its parent's flow, so the only thing a portal would buy is a
//! different parent — and nothing here needs one.
//!
//! The notice is `Option<String>` and the view matches it, so the arm that renders
//! the toast is the arm that has the text. There is no "is it showing" flag to keep
//! in step with the message.

use idyll::{live_view, Callback, Ctx, Event, Never, Setup, Signal};
use idyll_styles::styles;

#[idyll::component]
pub async fn Toast(
    ctx: Ctx<Setup, Never>,
    notice: Signal<Option<String>>,
    dismissed: Callback<Event>,
) -> idyll::Result {
    Ok(ctx.render(live_view! {
        @match $notice {
            Some(text) => {
                div css=[styles::REGION] role=("status") {
                    span { (text) }
                    button css=[styles::DISMISS] onclick=(dismissed)
                        aria_label=("Dismiss") { "×" }
                }
            },
            None => {}
        }
    }).await?)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::{Face, Palette, Space};

    /// `role="status"` makes this a live region, so a screen reader announces the
    /// notice without the focus moving — which is the whole point of a toast.
    pub const REGION: Style = css! {{
        position: "fixed",
        bottom: Space::s4,
        left: Space::s4,
        z_index: 10,
        display: "flex",
        align_items: "center",
        gap: Space::s3,
        max_width: "min-content",
        padding: "0.6rem 0.75rem",
        background: Palette::ink,
        color: Palette::page,
        border_radius: "6px",
        font_family: Face::sans,
        font_size: "0.875rem",
        line_height: 1.4,
    }};

    pub const DISMISS: Style = css! {{
        background: "none",
        border: "none",
        color: Palette::page,
        font_size: "1rem",
        line_height: 1,
        padding: "0",
        cursor: "pointer",
    }};
}
