//! The small pieces every screen is assembled from.
//!
//! Nothing here is a widget and nothing here holds state. Each function is an
//! element chain that had been written out more than once, moved to one place
//! so an edit to the shape lands everywhere it is drawn rather than in five of
//! the six copies. That is the whole ambition: these are spellings, not a
//! layer, and a caller keeps reading as the chain it replaced.
//!
//! Every one of them returns gpui's own builder — [`Div`], [`Button`] — rather
//! than a type of ours, so adopting one is a deletion: the call site goes on
//! chaining `.text_size()`, `.disabled()`, `.on_click()` onto the result
//! exactly as it did when the opener was spelled out above them. A builder
//! method sets one field of a style refinement and the fields are
//! independent, so a chain reordered around one of these draws the same
//! element it drew before.

use gpui::{App, Div, ElementId, Hsla, ParentElement as _, Pixels, SharedString, Styled as _, div};
use gpui_component::ActiveTheme as _;
use gpui_component::Icon;
use gpui_component::button::{Button, ButtonVariants as _};

use crate::theme::ActiveProductTheme as _;

// ---- palette shortcuts --------------------------------------------------
//
// The product palette carries the tokens gpui-component's `Theme` has no
// field for, and reaching one costs `cx.product().hsla(cx.product().palette.x)`
// — a line long enough that call sites bound it to a local just to fit, which
// is how the same colour ended up with four different local names. These say
// the role instead. They are deliberately free functions and deliberately
// qualified as `parts::subtle(cx)` at the call site: a bare `subtle` would
// collide with the local bindings they replace, and the qualification is what
// makes it obvious the colour is a shared token rather than a nearby `let`.

/// Metadata ink: timestamps, counters, the line under a name.
pub fn subtle(cx: &App) -> Hsla {
    cx.product().hsla(cx.product().palette.subtle_foreground)
}

/// Chrome and disabled glyphs — the step below [`subtle`], and the one WCAG
/// exempts from the contrast floor because nothing is read from it.
pub fn faint(cx: &App) -> Hsla {
    cx.product().hsla(cx.product().palette.faint_foreground)
}

/// The wash a picture is shown over.
pub fn scrim(cx: &App) -> Hsla {
    cx.product().hsla(cx.product().palette.scrim)
}

/// Ink for what sits on that wash. Not `foreground`: the theme's inks are
/// picked against a surface, and over a photograph the dark preset would be
/// near-black on near-black and the light one white on white.
pub fn on_scrim(cx: &App) -> Hsla {
    cx.product().hsla(cx.product().palette.on_scrim)
}

// ---- element openers ----------------------------------------------------

/// The column beside a picture: a name over a line about it.
///
/// `flex_1` so it takes the room the avatar and the trailing controls leave,
/// and `min_w_0` so it may be squeezed below its content — without which the
/// row grows instead and pushes the controls off the edge, and the ellipsis
/// its children ask for never has a bound to trigger against.
pub fn detail_stack() -> Div {
    div().flex_1().min_w_0().flex().flex_col()
}

/// One line that ends in an ellipsis rather than wrapping or overflowing.
///
/// All three are needed and none of them is enough alone: `whitespace_nowrap`
/// keeps it on one line, `overflow_hidden` clips what does not fit, and
/// `text_ellipsis` is what marks the clip.
pub fn one_line() -> Div {
    div().overflow_hidden().text_ellipsis().whitespace_nowrap()
}

/// The round emblem a whole-screen message leads with.
///
/// A frame on the surface's own secondary rather than on the tint, so the
/// colour says which kind of message this is without the screen turning into
/// a warning light.
pub fn hero_icon(icon: Icon, frame: Pixels, glyph: Pixels, tint: Hsla, cx: &App) -> Div {
    div()
        .size(frame)
        .rounded_full()
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .flex()
        .items_center()
        .justify_center()
        .child(icon.size(glyph).text_color(tint))
}

/// A headline and the sentence under it, centred, for a screen that is one
/// message.
///
/// Returned as the `Div` that holds them, so a screen with something more to
/// say — the cost of an irreversible action, say — adds a third child rather
/// than reaching for a second helper.
pub fn screen_message(
    headline: impl Into<SharedString>,
    body: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let metrics = cx.product().metrics;

    div()
        .flex()
        .flex_col()
        .items_center()
        // Headline and body are one group and sit closer to each other than
        // to anything else on the screen.
        .gap(metrics.space_md())
        // A ceiling on the line length, so the sentence stays readable in a
        // window that is far wider than a sentence.
        .max_w(metrics.call_card_width_wide())
        .text_center()
        .child(
            div()
                .text_size(metrics.text_heading())
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child(headline.into()),
        )
        .child(
            div()
                .text_size(metrics.text_secondary())
                .text_color(cx.theme().muted_foreground)
                .child(body.into()),
        )
}

/// A quiet icon button in a square frame.
///
/// The frame is passed rather than read from the metrics because the callers
/// do not agree on it and should not: a header sizes its controls from the
/// responsive layout, a call card from `call_control`, a bubble's row from
/// `icon_button`. What they do agree on is everything else, which is what
/// this is.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon: Icon,
    tooltip: impl Into<SharedString>,
    frame: Pixels,
) -> Button {
    Button::new(id)
        .icon(icon)
        .ghost()
        .tooltip(tooltip)
        .w(frame)
        .h(frame)
}
