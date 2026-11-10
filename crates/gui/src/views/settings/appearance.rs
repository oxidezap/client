//! Appearance: the theme, as controls over the same file you can hand-edit.

use gpui::{
    App, Entity, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Icon, IconName, Selectable as _, Sizable as _};

use crate::app::{SettingsState, WhatsAppApp};
use crate::theme::config::{MAX_FONT_SIZE, MIN_FONT_SIZE};
use crate::theme::metrics::Density;
use crate::theme::{ActiveProductTheme as _, Metrics, Palette, Preset};

use super::panes::{group, label};

pub fn render(
    settings: &SettingsState,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(metrics.space_xxl())
        .max_w(metrics.reading_width())
        .child(render_language(settings, entity.clone(), metrics, cx))
        .child(render_presets(settings, entity.clone(), metrics, cx))
        // Each control group gets the reading column. Translated descriptions
        // must wrap within it instead of sizing a neighbouring flex column.
        .child(render_density(settings, entity.clone(), metrics, cx))
        .child(render_font_size(settings, entity.clone(), metrics, cx))
        .child(render_theme_file(settings, entity, metrics, cx))
}

/// The presets, each showing what it actually looks like.
///
/// A swatch rather than a name alone: "Storm" means nothing until you see that
/// it is the same hues on a lifted ground.
fn render_presets(
    settings: &SettingsState,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let active = settings.draft.preset;
    // A palette that no longer matches its preset is a custom theme, whether
    // it got that way through this screen or through the file.
    let is_custom = settings.draft.palette != active.palette();

    group(
        label(crate::tr!(ThemePreset), metrics, cx),
        div()
            .flex()
            .flex_wrap()
            .gap(metrics.space_lg())
            .children(Preset::ALL.into_iter().map(|preset| {
                render_preset_card(
                    preset,
                    preset == active && !is_custom,
                    entity.clone(),
                    metrics,
                    cx,
                )
            }))
            .when(is_custom, |el| el.child(render_custom_card(metrics, cx))),
        metrics,
    )
}

fn render_preset_card(
    preset: Preset,
    is_selected: bool,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let palette = preset.palette();

    // A card, but a command: choosing a preset applies it. As a `div` these
    // were the one settings control a keyboard could not reach, which is a
    // poor showing for the accessibility pane's neighbour.
    //
    // The whole card is one child, and the column lives inside it. A
    // `Button` lays its children out in a centred row of its own — styling
    // the button as a column styles the frame and not the content — so a
    // preview with a relative width had nothing to be relative to and
    // collapsed into a strip beside its own label.
    Button::new(SharedString::from(format!("preset-{}", preset.id())))
        .ghost()
        .selected(is_selected)
        .w(metrics.preset_card_width())
        .h_auto()
        .p(metrics.space_md())
        .rounded(metrics.radius_lg())
        .border_1()
        .map(|el| {
            if is_selected {
                el.border_color(cx.theme().primary)
                    .bg(cx.theme().list_active)
            } else {
                el.border_color(cx.theme().border)
            }
        })
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(metrics.space_md())
                .child(render_swatch(&palette, metrics, cx))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(metrics.space_md())
                        .text_size(metrics.text_small())
                        .text_color(cx.theme().foreground)
                        .child(preset.label())
                        .when(is_selected, |el| {
                            el.child(
                                Icon::new(IconName::Check)
                                    .size(metrics.icon_small())
                                    .text_color(cx.theme().primary),
                            )
                        }),
                ),
        )
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            entity.update(cx, |app, cx| app.set_theme_preset(preset, window, cx));
        })
}

/// A miniature of the app: panel, two bubbles, accent.
fn render_swatch(palette: &Palette, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    let product = cx.product();
    let hsla = |c| product.hsla(c);
    let bar = |colour: Hsla, width: f32| {
        div()
            .h(metrics.space_sm())
            .w(gpui::relative(width))
            .rounded_full()
            .bg(colour)
    };

    div()
        .w_full()
        .h(metrics.preset_preview_height())
        .rounded(metrics.radius_md())
        .overflow_hidden()
        .flex()
        .border_1()
        .border_color(hsla(palette.border))
        .bg(hsla(palette.background))
        .child(
            div()
                .w(gpui::relative(0.32))
                .h_full()
                .bg(hsla(palette.sidebar)),
        )
        .child(
            div()
                .flex_1()
                .h_full()
                .p(metrics.space_md())
                .flex()
                .flex_col()
                .justify_center()
                .gap(metrics.space_sm())
                .child(bar(hsla(palette.message_received), 0.7))
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .child(bar(hsla(palette.message_sent), 0.55)),
                )
                .child(bar(hsla(palette.primary), 0.3)),
        )
}

/// Where the row says the file has gone its own way.
///
/// Not a command — there is nothing to switch *to* — so it is a card and not
/// a button, but it is the same card: same width, same preview height, so
/// the row does not change shape the moment a hand-edit lands.
fn render_custom_card(metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .w(metrics.preset_card_width())
        .flex()
        .flex_col()
        .gap(metrics.space_md())
        .p(metrics.space_md())
        .rounded(metrics.radius_lg())
        .border_1()
        .border_color(cx.theme().primary)
        .bg(cx.theme().list_active)
        .child(
            div()
                .w_full()
                .h(metrics.preset_preview_height())
                .rounded(metrics.radius_md())
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(metrics.space_xs())
                .bg(cx.theme().background)
                .border_1()
                .border_color(cx.theme().border)
                .child(
                    Icon::new(IconName::Palette)
                        .size(metrics.icon())
                        .text_color(cx.theme().primary),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_micro())
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!(EditedByHand)),
                ),
        )
        .child(
            div()
                .w_full()
                .text_size(metrics.text_small())
                .text_color(cx.theme().foreground)
                .child(crate::tr!(Custom)),
        )
}

fn render_density(
    settings: &SettingsState,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let active = settings.draft.density;

    group(
        label(crate::tr!(Density), metrics, cx),
        div()
            .flex()
            .flex_wrap()
            .gap(metrics.space_md())
            // `Button`s, because picking a density changes application
            // state: a styled `div` carries no focus handle and no keyboard
            // activation, and this pane is where someone adjusting the
            // interface to suit them is most likely to be doing it without a
            // pointer.
            .children(Density::ALL.into_iter().map(|density| {
                let entity = entity.clone();
                Button::new(SharedString::from(format!("density-{}", density.id())))
                    .label(density.label())
                    .outline()
                    .selected(density == active)
                    .px(metrics.space_xl())
                    .py(metrics.space_md())
                    .rounded(metrics.radius_md())
                    .text_size(metrics.text_small())
                    .cursor_pointer()
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |app, cx| app.set_theme_density(density, window, cx));
                    })
            })),
        metrics,
    )
}

/// The base font, which is the application's zoom control.
///
/// Stepped rather than a free slider: the interesting range is small, and
/// discrete steps are reachable from the keyboard where a drag is not.
fn render_font_size(
    settings: &SettingsState,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let size = settings.draft.font_size;
    let smaller = entity.clone();
    let larger = entity;

    group(
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(metrics.space_lg())
            .child(label(crate::tr!(BaseFontSize), metrics, cx))
            .child(
                div()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(metrics.text_meta())
                    .text_color(cx.theme().foreground)
                    .child(format!("{size:.0}px")),
            ),
        div()
            .flex()
            .flex_col()
            .gap(metrics.space_md())
            .child(
                div()
                    .debug_selector(|| "settings-font-controls".into())
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(metrics.space_md())
                    .child(
                        Button::new("font-smaller")
                            .icon(IconName::Minus)
                            .outline()
                            .small()
                            .tooltip(crate::tr!(Smaller))
                            .disabled(size <= MIN_FONT_SIZE)
                            .when(size > MIN_FONT_SIZE, |button| button.cursor_pointer())
                            .on_click(move |_, window, cx| {
                                smaller.update(cx, |app, cx| app.step_font_size(-1.0, window, cx));
                            }),
                    )
                    .child(render_scale(size, metrics, cx))
                    .child(
                        Button::new("font-larger")
                            .icon(IconName::Plus)
                            .outline()
                            .small()
                            .tooltip(crate::tr!(Larger))
                            .disabled(size >= MAX_FONT_SIZE)
                            .when(size < MAX_FONT_SIZE, |button| button.cursor_pointer())
                            .on_click(move |_, window, cx| {
                                larger.update(cx, |app, cx| app.step_font_size(1.0, window, cx));
                            }),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "settings-font-description".into())
                    .min_w_0()
                    .text_size(metrics.text_small())
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!(SpacingControlsAndIconsScaleWithThisNotJustText)),
            ),
        metrics,
    )
}

fn render_scale(size: f32, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    let fraction = ((size - MIN_FONT_SIZE) / (MAX_FONT_SIZE - MIN_FONT_SIZE)).clamp(0.0, 1.0);

    div()
        .flex_1()
        .h(metrics.space_xs())
        .rounded_full()
        .bg(cx.theme().secondary)
        .child(
            div()
                .h_full()
                .w(gpui::relative(fraction))
                .rounded_full()
                .bg(cx.theme().primary),
        )
}

/// Where the theme lives, and what it could not honour.
fn render_theme_file(
    settings: &SettingsState,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let path = settings
        .config_location()
        .unwrap_or_else(|| crate::tr!(NowhereToKeepAThemeOnThisPlatform).to_string());
    let problems = settings.draft.problems.clone();
    let is_dirty = settings.is_dirty();

    let save_entity = entity.clone();
    let reload_entity = entity.clone();
    let revert_entity = entity;

    group(
        div()
            .debug_selector(|| "settings-theme-file-header".into())
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(metrics.space_lg())
            .child(label(crate::tr!(ThemeFile), metrics, cx))
            .child(
                div()
                    .debug_selector(|| "settings-theme-file-actions".into())
                    .flex()
                    .flex_wrap()
                    .gap(metrics.space_md())
                    .when(is_dirty, |el| {
                        el.child(
                            Button::new("theme-revert")
                                .label(crate::tr!(Revert))
                                .ghost()
                                .small()
                                .cursor_pointer()
                                .on_click(move |_, window, cx| {
                                    revert_entity
                                        .update(cx, |app, cx| app.revert_theme(window, cx));
                                }),
                        )
                    })
                    .child(
                        Button::new("theme-reload")
                            .label(crate::tr!(Reload))
                            .outline()
                            .small()
                            .tooltip(crate::tr!(ReReadTheFileFromDisk))
                            .cursor_pointer()
                            .on_click(move |_, window, cx| {
                                reload_entity.update(cx, |app, cx| app.reload_theme(window, cx));
                            }),
                    )
                    .child(
                        Button::new("theme-save")
                            .label(crate::tr!(Save))
                            .primary()
                            .small()
                            .disabled(!is_dirty)
                            .when(is_dirty, |button| button.cursor_pointer())
                            .on_click(move |_, _window, cx| {
                                save_entity.update(cx, |app, cx| app.save_theme(cx));
                            }),
                    ),
            ),
        div()
            .flex()
            .flex_col()
            .gap(metrics.space_md())
            .child(
                div()
                    .id("theme-location")
                    .min_w_0()
                    .overflow_x_scroll()
                    .p(metrics.space_lg())
                    .rounded(metrics.radius_md())
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(metrics.text_meta())
                    .text_color(cx.theme().muted_foreground)
                    .child(path),
            )
            // What Save would put in that file. Read-only on purpose: this
            // pane's controls are the way to change it, and a text field
            // would be a second, worse editor of the same thing.
            .child(
                div()
                    .id("theme-json")
                    .min_w_0()
                    .overflow_x_scroll()
                    .max_h(metrics.config_block_height())
                    .overflow_y_scroll()
                    .p(metrics.space_lg())
                    .rounded(metrics.radius_md())
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(metrics.text_micro())
                    .text_color(cx.theme().muted_foreground)
                    .child(settings.draft_json()),
            )
            // Only when there is something to say. The file is usually fine,
            // and an empty "no problems" panel is noise.
            .when(!problems.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(metrics.space_md())
                        .p(metrics.space_lg())
                        .rounded(metrics.radius_md())
                        .bg(cx.theme().warning.opacity(0.12))
                        .border_1()
                        .border_color(cx.theme().warning.opacity(0.4))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(metrics.space_md())
                                .text_size(metrics.text_small())
                                .text_color(cx.theme().warning)
                                .child(
                                    Icon::new(IconName::TriangleAlert).size(metrics.icon_small()),
                                )
                                .child(
                                    // The point of the fallback: nothing broke,
                                    // some keys were simply not applied.
                                    crate::tr!(ThemeWarning),
                                ),
                        )
                        .children(problems.into_iter().map(|problem| {
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(metrics.text_micro())
                                .text_color(cx.theme().muted_foreground)
                                .child(problem)
                        })),
                )
            }),
        metrics,
    )
}

fn render_language(
    settings: &SettingsState,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    group(
        label(crate::tr!(Language), metrics, cx),
        div()
            .flex()
            .flex_col()
            .gap(metrics.space_md())
            .child(div().flex().flex_wrap().gap(metrics.space_md()).children(
                crate::i18n::Language::ALL.into_iter().map(|language| {
                    let entity = entity.clone();
                    Button::new(SharedString::from(format!("language-{}", language.code())))
                        .label(language.name())
                        .outline()
                        .selected(settings.draft.language == language)
                        .on_click(move |_, window, cx| {
                            entity.update(cx, |app, cx| app.set_language(language, window, cx));
                        })
                }),
            ))
            .child(
                div()
                    .text_size(metrics.text_small())
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!(
                        LanguageChangesApplyImmediatelySaveToKeepThisChoice
                    )),
            ),
        metrics,
    )
}
