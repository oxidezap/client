//! The remaining Settings sections.
//!
//! Several of these describe capabilities the library does not have yet
//! (device selection, notification routing). They say so rather than offering
//! a control that quietly does nothing — a switch that does not switch is a
//! worse answer than an honest note about what is missing.

use gpui::{AnyElement, App, Entity, IntoElement, ParentElement, Styled, div};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, IconName};

use crate::app::{SettingsSection, WhatsAppApp};
use crate::components::ProductIcon;
use crate::theme::{ActiveProductTheme as _, Metrics};

pub fn render(
    section: SettingsSection,
    app: &WhatsAppApp,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> AnyElement {
    let body = match section {
        SettingsSection::Account => account(app, metrics, cx),
        SettingsSection::Notifications => notifications(metrics, cx),
        SettingsSection::AudioVideo => audio_video(metrics, cx),
        SettingsSection::Privacy => privacy(entity, metrics, cx),
        SettingsSection::Storage => storage(app, metrics, cx),
        SettingsSection::Advanced => advanced(metrics, cx),
        // Rendered by its own module.
        SettingsSection::Appearance => div().into_any_element(),
    };

    div()
        .flex()
        .flex_col()
        .gap(metrics.space_xxl())
        .max_w(gpui::px(720.0))
        .child(body)
        .into_any_element()
}

/// A titled block: a label, then its content.
pub fn group(
    heading: impl IntoElement,
    content: impl IntoElement,
    metrics: Metrics,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        // The heading belongs to what follows it, so it sits closer to its
        // content than the block does to the next block.
        .gap(metrics.space_lg())
        .child(heading)
        .child(content)
}

/// A short all-caps section label.
pub fn label(text: &'static str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(metrics.text_micro())
        .text_color(cx.product().hsla(cx.product().palette.subtle_foreground))
        .child(text)
}

/// One label/value line.
fn row(key: String, value: String, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(metrics.space_xl())
        .py(metrics.space_lg())
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .text_size(metrics.text_secondary())
                .text_color(cx.theme().muted_foreground)
                .child(key),
        )
        .child(
            div()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_meta())
                .text_color(cx.theme().foreground)
                .child(value),
        )
}

/// What a section is waiting on, stated plainly.
fn pending(text: &'static str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .p(metrics.space_lg())
        .rounded(metrics.radius_md())
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            Icon::new(IconName::Info)
                .size(metrics.icon_small())
                .flex_shrink_0()
                .text_color(cx.theme().muted_foreground),
        )
        .child(
            div()
                .text_size(metrics.text_small())
                .text_color(cx.theme().muted_foreground)
                .child(text),
        )
}

fn account(app: &WhatsAppApp, metrics: Metrics, cx: &App) -> AnyElement {
    let account = app.account_summary();

    group(
        label("LINKED DEVICE", metrics, cx),
        div()
            .flex()
            .flex_col()
            .child(row(
                "Name".to_string(),
                account
                    .as_ref()
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| "—".to_string()),
                metrics,
                cx,
            ))
            .child(row(
                "Status".to_string(),
                account
                    .as_ref()
                    .map(|a| a.status.clone())
                    .unwrap_or_else(|| "not linked".to_string()),
                metrics,
                cx,
            )),
        metrics,
    )
    .into_any_element()
}

fn notifications(metrics: Metrics, cx: &App) -> AnyElement {
    group(
        label("NOTIFICATIONS", metrics, cx),
        pending(
            "This client does not raise desktop notifications yet. The daemon \
             carries a tray presence; routing messages through it is the next step.",
            metrics,
            cx,
        ),
        metrics,
    )
    .into_any_element()
}

fn audio_video(metrics: Metrics, cx: &App) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(metrics.space_xxl())
        .child(group(
            label("DEVICES", metrics, cx),
            pending(
                "Calls use the system's default input and output. Choosing a \
                 device needs enumeration the audio layer does not expose yet.",
                metrics,
                cx,
            ),
            metrics,
        ))
        .child(group(
            label("VIDEO", metrics, cx),
            pending(
                "The VoIP facade is audio-only and 1:1. The call card already \
                 has the layouts for video and for groups; the controls turn on \
                 when the library does.",
                metrics,
                cx,
            ),
            metrics,
        ))
        .into_any_element()
}

fn privacy(entity: Entity<WhatsAppApp>, metrics: Metrics, cx: &App) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(metrics.space_xxl())
        .child(group(
            label("ENCRYPTION", metrics, cx),
            div()
                .flex()
                .items_center()
                .gap(metrics.space_lg())
                .p(metrics.space_lg())
                .rounded(metrics.radius_md())
                .bg(cx.theme().secondary)
                .border_1()
                .border_color(cx.theme().border)
                .child(
                    Icon::new(ProductIcon::Lock)
                        .size(metrics.icon_small())
                        .flex_shrink_0()
                        .text_color(cx.theme().success),
                )
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            "Messages are end-to-end encrypted. Keys live in this \
                             device's store and never leave it.",
                        ),
                ),
            metrics,
        ))
        .child(group(
            label("START OVER", metrics, cx),
            div()
                .flex()
                .flex_col()
                .gap(metrics.space_lg())
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .text_color(cx.theme().muted_foreground)
                        // Named before it is offered: this is the one action
                        // here that cannot be undone.
                        .child(
                            "Unlinking clears this device's local data — messages, \
                             contacts and keys — and starts a new link from the QR code.",
                        ),
                )
                .child(
                    div().flex().child(
                        // Outline, not filled: destructive and deliberate, but
                        // not the emphasis of the screen.
                        Button::new("pair-again")
                            .label("Clear data and pair again")
                            .danger()
                            .outline()
                            .on_click(move |_, _window, cx| {
                                entity.update(cx, |app, cx| app.reset_and_pair_again(cx));
                            }),
                    ),
                ),
            metrics,
        ))
        .into_any_element()
}

fn storage(app: &WhatsAppApp, metrics: Metrics, cx: &App) -> AnyElement {
    let database = app
        .database_size()
        .map(format_size)
        .unwrap_or_else(|| "—".to_string());

    div()
        .flex()
        .flex_col()
        .gap(metrics.space_xxl())
        .child(group(
            label("ON DISK", metrics, cx),
            div()
                .flex()
                .flex_col()
                .child(row("Message store".to_string(), database, metrics, cx))
                .child(row(
                    "Media cache".to_string(),
                    "not cached".to_string(),
                    metrics,
                    cx,
                )),
            metrics,
        ))
        .child(group(
            label("MEDIA", metrics, cx),
            pending(
                "Downloaded media is held in memory for the session and fetched \
                 again after a restart. An on-disk cache is the fix, and is what \
                 would give this section something to clear.",
                metrics,
                cx,
            ),
            metrics,
        ))
        .into_any_element()
}

fn advanced(metrics: Metrics, cx: &App) -> AnyElement {
    group(
        label("DIAGNOSTICS", metrics, cx),
        div()
            .flex()
            .flex_col()
            .child(row(
                "Log level".to_string(),
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
                metrics,
                cx,
            ))
            .child(row("Renderer".to_string(), "GPUI".to_string(), metrics, cx)),
        metrics,
    )
    .into_any_element()
}

/// A size for a settings row: whole units, since nobody acts on the decimal.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::format_size;

    #[test]
    fn sizes_scale_to_a_readable_unit() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(900), "900 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(10_485_760), "10.0 MB");
    }
}
