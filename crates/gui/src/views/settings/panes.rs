//! The remaining Settings sections.
//!
//! Several of these describe capabilities the library does not have yet
//! (device selection, notification routing). They say so rather than offering
//! a control that quietly does nothing — a switch that does not switch is a
//! worse answer than an honest note about what is missing.

use gpui::{AnyElement, App, Entity, IntoElement, ParentElement, Styled, div};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Icon, IconName};

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
        SettingsSection::Privacy => privacy(entity.clone(), metrics, cx),
        SettingsSection::Storage => storage(app, entity, metrics, cx),
        SettingsSection::Advanced => advanced(metrics, cx),
        // Rendered by its own module.
        SettingsSection::Appearance => div().into_any_element(),
    };

    div()
        .flex()
        .flex_col()
        .gap(metrics.space_xxl())
        .max_w(metrics.reading_width())
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
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap(metrics.space_xl())
        .py(metrics.space_lg())
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .flex_shrink_0()
                .text_size(metrics.text_secondary())
                .text_color(cx.theme().muted_foreground)
                .child(key),
        )
        .child(
            // The value is the side that gives: a long push name or a path
            // should end in an ellipsis rather than push the label off screen.
            div()
                .flex_1()
                .min_w_0()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_meta())
                .text_color(cx.theme().foreground)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_right()
                .child(value),
        )
}

/// What a section is waiting on, stated plainly.
fn pending(text: &'static str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .w_full()
        .flex()
        // Top, not centre: these run to two or three lines, and an icon
        // floating in the middle of a paragraph reads as unattached to it.
        .items_start()
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
            // Without a shrinkable flex child the sentence lays out to its
            // natural width and runs off the pane; this is what makes it wrap.
            div()
                .flex_1()
                .min_w_0()
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
            .children(app.account_jid().map(|jid| {
                row(
                    "Number".to_string(),
                    // The user part alone: the server suffix is noise to a
                    // reader checking which account this is.
                    jid.split('@').next().unwrap_or(jid).to_string(),
                    metrics,
                    cx,
                )
            }))
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
                .w_full()
                .flex()
                .items_start()
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
                        .flex_1()
                        .min_w_0()
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
                             contacts, keys and downloaded media — and starts a new link \
                             from the QR code.",
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

fn storage(
    app: &WhatsAppApp,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> AnyElement {
    let usage = app.storage_usage();

    div()
        .flex()
        .flex_col()
        .gap(metrics.space_xxl())
        .child(group(
            label("ON DISK", metrics, cx),
            div()
                .flex()
                .flex_col()
                .child(row(
                    "Messages and keys".to_string(),
                    // Until the first answer arrives. The daemon measures, and
                    // it is another process: there is a frame or two where the
                    // honest thing to show is that nobody has counted yet.
                    usage.map_or_else(
                        || "measuring…".to_string(),
                        |u| format_bytes(u.database_bytes),
                    ),
                    metrics,
                    cx,
                ))
                .child(row(
                    "Downloaded media".to_string(),
                    usage.map_or_else(
                        || "measuring…".to_string(),
                        |u| format!("{} · {}", format_bytes(u.media_bytes), files(u.media_files)),
                    ),
                    metrics,
                    cx,
                )),
            metrics,
        ))
        .child(group(
            label("MEDIA CACHE", metrics, cx),
            div()
                .flex()
                .flex_col()
                .gap(metrics.space_lg())
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .text_color(cx.theme().muted_foreground)
                        // What it costs, so the button is a decision rather
                        // than a dare: the history is untouched and every
                        // message keeps the means to fetch its media again.
                        .child(
                            "Clearing the cache keeps every message. Anything you \
                             open again is downloaded again.",
                        ),
                )
                .child(
                    div().flex().child(
                        Button::new("clear-media-cache")
                            .label("Clear cached media")
                            .outline()
                            .disabled(usage.is_none_or(|u| u.media_files == 0))
                            .on_click(move |_, _window, cx| {
                                entity.update(cx, |app, cx| app.clear_media_cache(cx));
                            }),
                    ),
                ),
            metrics,
        ))
        .into_any_element()
}

/// A size a person can read. Binary units, because that is what a filesystem
/// reports and a number that disagrees with `du` invites a bug report.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    // One decimal below ten, none above: "1.4 MiB" is useful, "847.3 MiB" is
    // three digits of noise.
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

fn files(count: u64) -> String {
    if count == 1 {
        "1 file".to_string()
    } else {
        format!("{count} files")
    }
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

#[cfg(test)]
mod tests {
    use super::format_bytes;

    /// The unit is binary and says so. A settings row that reports 1000-based
    /// "MB" over a filesystem that counts in 1024s is a number nobody can
    /// check against `du`, and the second formatter this file used to carry
    /// did exactly that — in KB, tested, and reachable from nothing.
    #[test]
    fn sizes_scale_to_a_readable_binary_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(900), "900 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(10_485_760), "10 MiB");
    }

    /// One decimal below ten and none above: "1.4 MiB" is useful, "847.3 MiB"
    /// is three digits of noise.
    #[test]
    fn the_decimal_goes_where_it_carries_information() {
        assert_eq!(format_bytes(1_572_864), "1.5 MiB");
        assert_eq!(format_bytes(245_366_784), "234 MiB");
    }
}
