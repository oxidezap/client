//! The remaining Settings sections.
//!
//! Several of these describe capabilities the library does not have yet
//! (device selection, notification routing). They say so rather than offering
//! a control that quietly does nothing — a switch that does not switch is a
//! worse answer than an honest note about what is missing.

use gpui::{
    AnyElement, App, Entity, IntoElement, ParentElement, SharedString, Styled, div,
    prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Icon, IconName, Selectable as _, Sizable as _};

use oxidezap_core::LogLevel;

use crate::app::{SettingsSection, WhatsAppApp};
use crate::components::ProductIcon;
use crate::components::parts;
use crate::theme::Metrics;

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
        SettingsSection::Advanced => advanced(entity, metrics, cx),
        SettingsSection::Plugins => plugins(app, entity, metrics, cx),
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

/// Every plugin this daemon knows about, and the two things that change the
/// list.
///
/// One list, in one shape. It used to be two — the plugins that published a
/// surface, drawn as cards, and below them the files that did not load, drawn
/// as a table of key/value rows with a column of identical Remove buttons
/// underneath it and nothing tying a button to a row. A module that failed to
/// load is still a plugin in the folder; the difference is a word in its
/// header, not a second layout.
fn plugins(
    app: &WhatsAppApp,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> AnyElement {
    let ctx = crate::components::PluginContext {
        entity: entity.clone(),
        metrics,
    };
    let home = crate::platform::plugins::home();

    // Everything in the folder that published nothing. A module that fails to
    // parse, answers the wrong ABI version or traps in `oxi_init` has no
    // surface at all, so a screen drawn from the surfaces alone leaves the
    // one file somebody most needs to remove with no control anywhere — and
    // it goes on spending the folder's budget at every load.
    let unloaded: Vec<String> = app
        .installed_plugins()
        .unwrap_or_default()
        .iter()
        .filter(|id| !app.plugins().iter().any(|surface| &surface.id == *id))
        .cloned()
        .collect();

    let rows = app.plugins().len() + unloaded.len();
    let list = div()
        .flex()
        .flex_col()
        .gap(metrics.space_lg())
        .children(app.plugins().iter().map(|surface| {
            crate::components::plugin_ui::settings_entry(
                surface,
                app,
                &ctx,
                removal(&surface.id, home, app, entity.clone(), metrics, cx),
                cx,
            )
            .into_any_element()
        }))
        .children(unloaded.iter().map(|id| {
            crate::components::plugin_ui::unloaded_entry(
                id,
                removal(id, home, app, entity.clone(), metrics, cx),
                metrics,
                cx,
            )
            .into_any_element()
        }));

    group(
        label("PLUGINS", metrics, cx),
        div()
            .flex()
            .flex_col()
            .gap(metrics.space_lg())
            // Prose, not a key/value row: "None loaded" against a sentence
            // read as a setting whose value happened to be an instruction.
            .child(if rows == 0 {
                empty(home.nothing_loaded(), metrics, cx).into_any_element()
            } else {
                list.into_any_element()
            })
            .child(plugin_controls(home, entity, metrics, cx)),
        metrics,
    )
    .into_any_element()
}

/// The control that takes one plugin back out of the folder, where this front
/// end has a folder to take it out of.
///
/// Inside the plugin's own card, which is the whole of what makes it
/// unambiguous. `None` twice over: where the folder is another process's, and
/// where this plugin's file has already gone — a Remove button over a file
/// that is not there is one whose second press answers "not found".
///
/// Absent from a folder that has been *read*. A listing nobody has answered
/// yet is not the folder saying no — the read is a task, so the first frame
/// after Settings opens has none of it, and taking that for absence would
/// tell somebody every plugin they are running had been removed.
fn removal(
    id: &str,
    home: crate::platform::plugins::Home,
    app: &WhatsAppApp,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> Option<gpui::AnyElement> {
    if !home.can_install() {
        return None;
    }
    if app
        .installed_plugins()
        .is_some_and(|ids| !ids.iter().any(|f| f == id))
    {
        // Said rather than left blank: a control that vanishes tells nobody
        // anything, which is the same reason a stopped plugin's widgets stay
        // on screen beside their reason.
        return Some(
            div()
                .flex_shrink_0()
                .text_size(metrics.text_meta())
                .text_color(cx.theme().muted_foreground)
                .child("Removed")
                .into_any_element(),
        );
    }
    let id = id.to_owned();
    Some(
        Button::new(gpui::SharedString::from(format!("remove-plugin-{id}")))
            .label("Remove")
            .ghost()
            .small()
            .on_click(move |_, _window, cx| {
                let id = id.clone();
                entity.update(cx, |app, cx| app.remove_plugin(id, cx));
            })
            .flex_shrink_0()
            .into_any_element(),
    )
}

/// What stands where the list would be.
fn empty(what: &'static str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .w_full()
        .rounded(metrics.radius_md())
        .border_1()
        .border_color(cx.theme().border)
        .px(metrics.space_lg())
        .py(metrics.space_xl())
        .text_size(metrics.text_small())
        .text_color(cx.theme().muted_foreground)
        .child(what)
}

/// The row under the list: reload, and — where this front end has a folder of
/// its own — add.
///
/// Reload is drawn everywhere and Add is not, and the asymmetry is the whole
/// of what each is about. The folder belongs to whichever daemon is running
/// the plugins, so only a front end that *is* that daemon can put a file in
/// it; but asking it to read the folder again is a request on the wire, which
/// every front end can make. A desktop window's Reload is for somebody who
/// has just dropped a `.wasm` beside `oxidezapd`; a follower tab's is for
/// somebody who installed one into an origin whose host is another tab.
///
/// With a line saying what Reload does, because "reload" over a list of
/// running things is a word somebody is right to hesitate over.
fn plugin_controls(
    home: crate::platform::plugins::Home,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let installer = entity.clone();
    div()
        .flex()
        .items_center()
        .gap(metrics.space_md())
        .px(metrics.space_xs())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(metrics.text_meta())
                .text_color(cx.theme().muted_foreground)
                .child("Reloading stops every plugin and starts what is in the folder now."),
        )
        .child(
            Button::new("reload-plugins")
                .label("Reload plugins")
                .ghost()
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |app, cx| app.reload_plugins(cx));
                }),
        )
        .children(home.can_install().then(|| {
            Button::new("install-plugin")
                .label("Add a plugin…")
                .outline()
                .on_click(move |_, _window, cx| {
                    installer.update(cx, |app, cx| app.install_plugin(cx));
                })
        }))
}

/// A short all-caps section label.
pub fn label(text: &'static str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(metrics.text_micro())
        .text_color(parts::subtle(cx))
        .child(text)
}

/// A block of label/value lines, drawn as one surface.
///
/// Bordered rather than a bare list. Three lines on the background with
/// hairlines under them read as the leftovers of a table someone deleted;
/// a settings pane is a set of panels, and this is one panel.
pub fn card(lines: Vec<(String, String)>, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    let last = lines.len().saturating_sub(1);
    div()
        .w_full()
        .flex()
        .flex_col()
        .rounded(metrics.radius_md())
        .overflow_hidden()
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .children(
            lines
                .into_iter()
                .enumerate()
                .map(|(ix, (key, value))| row(key, value, ix == last, metrics, cx)),
        )
}

/// One label/value line.
fn row(
    key: String,
    value: String,
    is_last: bool,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap(metrics.space_xl())
        .px(metrics.space_lg())
        .py(metrics.space_lg())
        // The panel's own edge is the last line's rule.
        .when(!is_last, |el| {
            el.border_b_1().border_color(cx.theme().border)
        })
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
            parts::one_line()
                .flex_1()
                .min_w_0()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_meta())
                .text_color(cx.theme().foreground)
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

    let mut lines = vec![(
        "Name".to_string(),
        account
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_else(|| "—".to_string()),
    )];
    if let Some(jid) = app.account_jid() {
        lines.push((
            "Number".to_string(),
            // The user part alone: the server suffix is noise to a reader
            // checking which account this is.
            jid.split('@').next().unwrap_or(jid).to_string(),
        ));
    }
    lines.push((
        "Status".to_string(),
        account
            .as_ref()
            .map(|a| a.status.clone())
            .unwrap_or_else(|| "not linked".to_string()),
    ));

    group(
        label("LINKED DEVICE", metrics, cx),
        card(lines, metrics, cx),
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
                        // Prefixed by the screen it is on, like every other
                        // control here. The logged-out screen has a button
                        // with this action too, and gpui keys interaction
                        // state (hover, press, focus) by element id: two
                        // surfaces drawn in one frame would share it, for the
                        // one action in the app that erases the account.
                        Button::new("settings-pair-again")
                            .label("Clear data and pair again")
                            .danger()
                            .outline()
                            .on_click(move |_, window, cx| {
                                entity.update(cx, |app, cx| app.reset_and_pair_again(window, cx));
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
            card(
                vec![
                    (
                        "Messages and keys".to_string(),
                        // Until the first answer arrives. The daemon measures,
                        // and it is another process: there is a frame or two
                        // where the honest thing to show is that nobody has
                        // counted yet.
                        usage.map_or_else(
                            || "measuring…".to_string(),
                            |u| format_bytes(u.database_bytes),
                        ),
                    ),
                    (
                        "Downloaded media".to_string(),
                        usage.map_or_else(
                            || "measuring…".to_string(),
                            |u| {
                                format!(
                                    "{} · {}",
                                    format_bytes(u.media_bytes),
                                    files(u.media_files)
                                )
                            },
                        ),
                    ),
                ],
                metrics,
                cx,
            ),
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

/// Diagnostics: how loud the client is, and what is drawing it.
///
/// The level is a control rather than a line of text, which is the whole of
/// this section's reason to exist. It used to report `RUST_LOG` — a variable
/// a person can only set by restarting the client from a terminal they may
/// not have, and one a page has never had at all. What is drawn now takes
/// effect in this window and in the daemon at once, and is remembered.
fn advanced(entity: Entity<WhatsAppApp>, metrics: Metrics, cx: &App) -> AnyElement {
    let active = oxidezap_logging::current();
    // A level given for this run from outside — `RUST_LOG` on a desktop,
    // `?log=` in a page — is what the process *started* at. Said rather than
    // hidden: without it, a stored choice that a launch argument overrode
    // looks like a control that did not work.
    let forced = oxidezap_logging::forced();
    let kept = oxidezap_logging::location();

    group(
        label("DIAGNOSTICS", metrics, cx),
        div()
            .flex()
            .flex_col()
            .gap(metrics.space_lg())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(metrics.space_md())
                    // `Button`s rather than styled `div`s, for the reason the
                    // density control uses them: picking a level changes
                    // application state, and a `div` carries no focus handle
                    // and no keyboard activation.
                    .children(LogLevel::ALL.into_iter().map(|level| {
                        let entity = entity.clone();
                        Button::new(SharedString::from(format!("log-level-{}", level.id())))
                            .label(level.label())
                            .outline()
                            .selected(level == active)
                            .px(metrics.space_xl())
                            .py(metrics.space_md())
                            .rounded(metrics.radius_md())
                            .text_size(metrics.text_small())
                            .on_click(move |_, _window, cx| {
                                entity.update(cx, |app, cx| app.set_log_level(level, cx));
                            })
                    })),
            )
            .child(
                div()
                    .text_size(metrics.text_meta())
                    .text_color(cx.theme().muted_foreground)
                    .child(active.note()),
            )
            .child(card(
                vec![
                    ("Log level".to_string(), active.label().to_string()),
                    (
                        "Kept in".to_string(),
                        kept.unwrap_or_else(|| "nowhere — it lasts for this run".to_string()),
                    ),
                    ("Renderer".to_string(), "GPUI".to_string()),
                ],
                metrics,
                cx,
            ))
            .when_some(forced, |el, forced| {
                el.child(
                    div()
                        .text_size(metrics.text_meta())
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{} asked for {forced} when this started; the level above is \
                             what is in force now.",
                            oxidezap_logging::forced_by(),
                        )),
                )
            }),
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
