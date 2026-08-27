//! Projecting a [`Palette`] onto gpui-component's `Theme`.
//!
//! This is the one place that decides which semantic role a colour plays, so
//! our surfaces and the library's own controls resolve the same tokens. Roles
//! that are a fixed relationship to another role — hover states, tints, the
//! ink that has to stay readable on a coloured ground — are derived here
//! rather than listed in the palette, so overriding one key in `theme.json`
//! moves its dependants with it instead of leaving a stale value behind.

use gpui::{App, Hsla, px, rgb};
use gpui_component::theme::{Theme, ThemeMode};

use super::config::ThemeSettings;
use super::palette::{Palette, Rgb};

fn hsla(colour: Rgb) -> Hsla {
    rgb(colour.0).into()
}

/// Install `settings` as the active theme.
///
/// Changes the base theme first so every token exists before it is
/// overridden, then republishes to Base so scrollbars and resize handles
/// follow. Call `window.refresh()` afterwards when reapplying to a live
/// window — the base font is the rem reference and existing frames are laid
/// out against the old one.
pub fn apply(settings: &ThemeSettings, rem_size: f32, cx: &mut App) {
    let mode = if settings.preset.is_dark() {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    Theme::change(mode, None, cx);

    let palette = &settings.palette;
    let theme = cx.global_mut::<Theme>();

    // The rem `Metrics` resolved — the base the *window* can carry, already
    // fitted and already bounded — rather than the one the settings asked
    // for. The library's controls size themselves from this, so any base this
    // side worked out for itself would put our chrome and the library's
    // buttons on two different scales in the same header.
    theme.font_size = px(rem_size);
    // Radii the library resolves for its own controls. Ours come from
    // `Metrics`, which scales them with the base font; these two are the
    // library's equivalents of `radius_md` and `radius_xl` at that base.
    theme.radius = px(10.0 * rem_size / 16.0);
    theme.radius_lg = px(14.0 * rem_size / 16.0);

    apply_palette(&mut theme.colors, palette);

    // Base owns scrollbars and resize handles and holds its own projection of
    // the theme; without this it keeps painting the previous palette.
    Theme::sync_base(cx);
}

fn apply_palette(c: &mut gpui_component::theme::ThemeColor, palette: &Palette) {
    let p = |colour: Rgb| hsla(colour);

    // Surfaces, back to front. The design's whole point is that these are four
    // distinct steps: the conversation sits deepest, panels above it, cards on
    // panels, and popovers above everything.
    c.background = p(palette.background);
    c.sidebar = p(palette.sidebar);
    c.secondary = p(palette.secondary);
    c.popover = p(palette.elevated);
    c.title_bar = p(palette.sidebar);
    c.muted = p(palette.secondary);
    c.input = p(palette.secondary);
    c.list = p(palette.sidebar);
    c.group_box = p(palette.secondary);
    c.tab_bar = p(palette.sidebar);
    c.tab = p(palette.sidebar);
    c.tab_active = p(palette.secondary);
    c.skeleton = p(palette.list_hover);
    c.accordion = p(palette.secondary);

    c.foreground = p(palette.foreground);
    c.muted_foreground = p(palette.muted_foreground);
    c.sidebar_foreground = p(palette.foreground);
    c.popover_foreground = p(palette.foreground);
    c.secondary_foreground = p(palette.foreground);
    c.group_box_foreground = p(palette.foreground);
    c.tab_active_foreground = p(palette.foreground);
    c.description_list_label_foreground = p(palette.muted_foreground);

    c.border = p(palette.border);
    c.sidebar_border = p(palette.border);
    c.title_bar_border = p(palette.border);
    c.list_active_border = p(palette.primary);
    c.drag_border = p(palette.ring);

    // Row states use the list tokens the components already consult, so a
    // library list and ours highlight identically.
    c.list_hover = p(palette.list_hover);
    c.list_active = p(palette.list_active);
    c.list_even = p(palette.sidebar);
    c.list_head = p(palette.secondary);
    c.accent = p(palette.list_hover);
    c.accent_foreground = p(palette.foreground);
    c.sidebar_accent = p(palette.list_hover);
    c.sidebar_accent_foreground = p(palette.foreground);

    // Focus is deliberately not `primary`: a ring that matches the selection
    // colour stops answering "where is the keyboard?".
    c.ring = p(palette.ring);
    c.caret = p(palette.primary);
    c.selection = p(palette.ring.mix(palette.background, 0.65));

    apply_semantic_family(c, palette);
    apply_button_family(c, palette);

    c.scrollbar = p(palette.background.mix(palette.foreground, 0.02));
    c.scrollbar_thumb = p(palette.border);
    c.scrollbar_thumb_hover = p(palette.faint_foreground);
    c.progress_bar = p(palette.primary);
    c.slider_bar = p(palette.primary);
    c.slider_thumb = p(palette.foreground);
    c.switch = p(palette.border);
    c.switch_thumb = p(palette.foreground);
    c.drop_target = p(palette.primary.mix(palette.background, 0.7));
    c.link = p(palette.primary);
    c.link_hover = p(palette.success);
    c.link_active = p(palette.primary);
}

/// The status colours, each with the hover/active pair the library expects.
fn apply_semantic_family(c: &mut gpui_component::theme::ThemeColor, palette: &Palette) {
    let p = |colour: Rgb| hsla(colour);
    // A status colour lightens on hover and darkens on press, relative to the
    // palette's own ink and ground rather than to absolute white and black —
    // that keeps the gesture legible in a light preset too.
    let lift = |colour: Rgb| p(colour.mix(palette.foreground, 0.18));
    let press = |colour: Rgb| p(colour.mix(palette.background, 0.18));

    c.primary = p(palette.primary);
    c.primary_hover = lift(palette.primary);
    c.primary_active = press(palette.primary);
    c.primary_foreground = p(palette.on(palette.primary));

    c.danger = p(palette.danger);
    c.danger_hover = lift(palette.danger);
    c.danger_active = press(palette.danger);
    c.danger_foreground = p(palette.on(palette.danger));

    c.warning = p(palette.warning);
    c.warning_hover = lift(palette.warning);
    c.warning_active = press(palette.warning);
    c.warning_foreground = p(palette.on(palette.warning));

    c.success = p(palette.success);
    c.success_hover = lift(palette.success);
    c.success_active = press(palette.success);
    c.success_foreground = p(palette.on(palette.success));

    c.info = p(palette.info);
    c.info_hover = lift(palette.info);
    c.info_active = press(palette.info);
    c.info_foreground = p(palette.on(palette.info));

    c.chart_bullish = p(palette.success);
    c.chart_bearish = p(palette.danger);
}

/// `Button` reads a mix of the semantic roles and its own `button_*` family;
/// leaving the latter on the base theme is what makes a themed window sprout
/// one differently-coloured control.
fn apply_button_family(c: &mut gpui_component::theme::ThemeColor, palette: &Palette) {
    let p = |colour: Rgb| hsla(colour);
    let lift = |colour: Rgb| p(colour.mix(palette.foreground, 0.18));
    let press = |colour: Rgb| p(colour.mix(palette.background, 0.18));

    // A default button is a card that reacts, so it starts on the card surface
    // and moves through the same row states as a list row.
    c.button = p(palette.secondary);
    c.button_hover = p(palette.list_hover);
    c.button_active = p(palette.list_active);
    c.button_foreground = p(palette.foreground);

    c.button_primary = p(palette.primary);
    c.button_primary_hover = lift(palette.primary);
    c.button_primary_active = press(palette.primary);
    c.button_primary_foreground = p(palette.on(palette.primary));

    c.button_secondary = p(palette.secondary);
    c.button_secondary_hover = p(palette.list_hover);
    c.button_secondary_active = p(palette.list_active);
    c.button_secondary_foreground = p(palette.foreground);

    c.button_danger = p(palette.danger);
    c.button_danger_hover = lift(palette.danger);
    c.button_danger_active = press(palette.danger);
    c.button_danger_foreground = p(palette.on(palette.danger));

    c.button_success = p(palette.success);
    c.button_success_hover = lift(palette.success);
    c.button_success_active = press(palette.success);
    c.button_success_foreground = p(palette.on(palette.success));

    c.button_warning = p(palette.warning);
    c.button_warning_hover = lift(palette.warning);
    c.button_warning_active = press(palette.warning);
    c.button_warning_foreground = p(palette.on(palette.warning));

    c.button_info = p(palette.info);
    c.button_info_hover = lift(palette.info);
    c.button_info_active = press(palette.info);
    c.button_info_foreground = p(palette.on(palette.info));
}
