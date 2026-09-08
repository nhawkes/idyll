//! TextInput — a single-line text field bound to a value signal. The handler maps
//! the field's string to a message (`e.value()` is always a string, so it always
//! delivers).

use idyll::{live_view, Callback, Ctx, Event, Never, Setup, Signal};
use idyll_styles::styles;

use super::tokens::FOCUS;

#[idyll::component]
pub async fn TextInput(
    ctx: Ctx<Setup, Never>,
    name: &'static str,
    placeholder: &'static str,
    value: Signal<String>,
    typed: Callback<String>,
) -> idyll::Result {
    // The field means a string, and says so; the projection off the event is named
    // once, here, rather than in every caller.
    let typed = typed.contra_map(|e: Event| e.value());
    Ok(ctx.render(live_view! {
        input css=[styles::INPUT, FOCUS] type=("text") name=(name) placeholder=(placeholder)
            value=($value) oninput=(typed)
    }).await?)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::{Face, Palette, Space};

    pub const INPUT: Style = css! {{
        font_family: Face::sans,
        font_size: "0.9375rem",
        padding: Space::s2,
        color: Palette::ink,
        background: Palette::page,
        border: "1px solid #cccccc",
        border_radius: "6px",
    }};
}
