//! The delivery indicator on an outgoing message.
//!
//! Five states, and the shape changes with every one of them: a clock while
//! it is in flight, one tick when the server has it, two when it arrived, two
//! in the accent colour when it was read, and a warning when it failed.
//! Colour is never the only difference — read and delivered would otherwise
//! be indistinguishable to a large share of readers, and on a custom palette
//! they might not differ at all.

use gpui::{App, IntoElement, Pixels, Styled};
use gpui_component::ActiveTheme as _;
use gpui_component::{Icon, IconName};
use oxidezap_core::MessageStatus;

use crate::theme::ActiveProductTheme as _;

use super::ProductIcon;

/// The tick for `status`, sized to sit on the same line as its timestamp.
///
/// `on_accent` is for a tick drawn on the outgoing bubble, whose ground is
/// already the brand colour: the ordinary muted ink disappears against it.
pub fn status_ticks(status: MessageStatus, size: Pixels, cx: &App) -> impl IntoElement + use<> {
    ticks(status, size, false, cx)
}

/// As [`status_ticks`], for a tick sitting on an outgoing bubble.
pub fn bubble_status_ticks(
    status: MessageStatus,
    size: Pixels,
    cx: &App,
) -> impl IntoElement + use<> {
    ticks(status, size, true, cx)
}

fn ticks(
    status: MessageStatus,
    size: Pixels,
    on_accent: bool,
    cx: &App,
) -> impl IntoElement + use<> {
    let product = cx.product();
    // On the sent bubble the muted ink is unreadable, so the quiet states use
    // a lightened form of the bubble's own colour instead.
    let quiet = if on_accent {
        product.hsla(
            product
                .palette
                .message_sent
                .mix(product.palette.foreground, 0.55),
        )
    } else {
        product.hsla(product.palette.subtle_foreground)
    };

    let (icon, colour): (Icon, _) = match status {
        MessageStatus::Pending => (ProductIcon::Clock.into(), quiet),
        MessageStatus::Sent => (Icon::new(IconName::Check), quiet),
        MessageStatus::Delivered => (ProductIcon::CheckCheck.into(), quiet),
        // The one state that earns colour: it is the answer to "did they see
        // it", which is the question the ticks exist for.
        MessageStatus::Read => (ProductIcon::CheckCheck.into(), cx.theme().ring),
        MessageStatus::Failed => (Icon::new(IconName::TriangleAlert), cx.theme().danger),
    };

    icon.size(size).flex_shrink_0().text_color(colour)
}
