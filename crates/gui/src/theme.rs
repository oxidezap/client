//! Product theme.
//!
//! The palette is registered once into gpui-component's [`Theme`] global, so
//! every surface — ours and the library's own controls — reads the same tokens
//! through `cx.theme()`. Rendering code must not name a colour directly: a
//! literal in a component is invisible to theme switching and drifts from the
//! rest of the UI the moment either side changes.
//!
//! Two colours have no semantic token because they carry meaning nothing else
//! does — which side of a conversation a bubble belongs to. Those live in
//! [`brand`] and are the only sanctioned exception.

use gpui::{App, Hsla, rgb};
use gpui_component::theme::Theme;

/// Colours with no semantic equivalent in the design system.
///
/// Message bubbles encode authorship, not status or emphasis, so no token
/// means what they mean. Anything that *does* have a token belongs there
/// instead.
pub mod brand {
    /// Outgoing bubble.
    pub const MESSAGE_SENT: u32 = 0x005c4b;
    /// Incoming bubble.
    pub const MESSAGE_RECEIVED: u32 = 0x202c33;
}

/// Overwrite the active theme's tokens with the product palette.
///
/// Called after `Theme::change` so the base theme exists to be overridden.
pub fn apply_brand_palette(cx: &mut App) {
    let hsla = |hex: u32| -> Hsla { rgb(hex).into() };
    let theme = cx.global_mut::<Theme>();
    let c = &mut theme.colors;

    // Surfaces, back to front: the chat pane sits deepest, panels above it.
    c.background = hsla(0x0b141a);
    c.sidebar = hsla(0x111b21);
    c.secondary = hsla(0x202c33);
    c.popover = hsla(0x202c33);
    c.title_bar = hsla(0x202c33);

    c.foreground = hsla(0xe9edef);
    c.muted_foreground = hsla(0x8696a0);
    c.sidebar_foreground = hsla(0xe9edef);
    c.popover_foreground = hsla(0xe9edef);
    c.secondary_foreground = hsla(0xe9edef);

    c.border = hsla(0x2a3942);
    c.sidebar_border = hsla(0x2a3942);
    c.title_bar_border = hsla(0x2a3942);

    // Selection and hover are list states, not decoration, so they use the
    // list tokens the components already consult.
    c.list_hover = hsla(0x2a3942);
    c.list_active = hsla(0x374248);
    c.accent = hsla(0x2a3942);
    c.accent_foreground = hsla(0xe9edef);

    // Green is the primary action colour, which is also what makes the
    // library's `Button::primary()` land in the right place for free.
    c.primary = hsla(0x00a884);
    c.primary_hover = hsla(0x06cf9c);
    c.primary_foreground = hsla(0xffffff);
    c.ring = hsla(0x00a884);

    c.danger = hsla(0xff4444);
    c.danger_foreground = hsla(0xffffff);
}
