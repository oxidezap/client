//! Drawing what a plugin declared.
//!
//! A plugin names a widget and a slot; this decides what that looks like. The
//! split is the whole point — nothing in a plugin's tree can express a
//! colour, a size or a position, so a plugin cannot put a literal outside the
//! theme's reach, and a front end that is not a window renders the same tree
//! its own way.
//!
//! Every control here is a real `Button` or `Switch` rather than a styled
//! `div`, for the reason the rest of this crate gives: a command needs focus,
//! keyboard activation and the theme's own states, and none of those come
//! with a clickable rectangle.

use gpui::{
    App, Entity, IntoElement, ParentElement, SharedString, Styled, div, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::switch::Switch;
use gpui_component::{Disableable as _, Sizable as _};
use oxidezap_core::{PluginNode, PluginSlot, PluginSurface, PluginWidget};

use crate::app::WhatsAppApp;
use crate::components::parts;
use crate::theme::Metrics;

/// What a widget needs beyond the tree itself.
///
/// Passed down rather than reached for, because the render helpers in this
/// crate take `&App` and retain nothing borrowed.
#[derive(Clone)]
pub struct PluginContext {
    pub entity: Entity<WhatsAppApp>,
    pub metrics: Metrics,
}

/// Everything in one slot, across every plugin, in the order they loaded.
///
/// Returns nothing at all when no plugin drew in this slot, so a header with
/// no plugins is exactly the header it was before plugins existed — not one
/// with an empty container in it.
pub fn slot(
    plugins: &[PluginSurface],
    slot: PluginSlot,
    app: &WhatsAppApp,
    ctx: &PluginContext,
    cx: &App,
) -> Vec<gpui::AnyElement> {
    plugins
        .iter()
        .flat_map(|surface| surface.roots_in(slot).map(move |node| (surface, node)))
        .map(|(surface, node)| widget(surface, slot, node, app, ctx, cx).into_any_element())
        .collect()
}

/// One widget and, below it, whatever it holds.
fn widget(
    surface: &PluginSurface,
    slot: PluginSlot,
    node: &PluginNode,
    app: &WhatsAppApp,
    ctx: &PluginContext,
    cx: &App,
) -> gpui::AnyElement {
    let metrics = ctx.metrics;
    // A plugin that has stopped keeps its widgets on screen and loses the
    // ability to act: a control that vanished tells nobody anything, while
    // one drawn inert beside a reason says what happened. Approval is *not*
    // part of this: drawing and keeping its own settings take effect on
    // declaration, so the panel where somebody reads what a plugin does and
    // configures it has to work before they decide whether it may touch the
    // account — which the host goes on refusing either way. This is also why
    // the plugin's own flag is `&&`ed rather than replaced: a widget it
    // disabled stays disabled.
    let live = surface.is_running() && node.enabled;

    match node.widget {
        PluginWidget::Button => {
            let (plugin, action) = (surface.id.clone(), node.id.clone());
            let entity = ctx.entity.clone();
            // `/` and not `-`: both halves may hold a `-`, so a plugin `a`
            // with a widget `b-c` and a plugin `a-b` with a widget `c` would
            // otherwise be one element id in a list that holds every
            // plugin's roots. The same separator `key` uses, for the same
            // reason and with the same guarantee: no plugin id may contain
            // it.
            Button::new(SharedString::from(format!(
                "plugin/{}/{}",
                surface.id, node.id
            )))
            .label(node.label.clone())
            .ghost()
            .small()
            .disabled(!live)
            .on_click(move |_, _window, cx| {
                let (plugin, action) = (plugin.clone(), action.clone());
                entity.update(cx, |app, cx| {
                    app.send_plugin_action(&plugin, &action, None, slot, PluginWidget::Button, cx);
                });
            })
            .into_any_element()
        }

        PluginWidget::Toggle => {
            let (plugin, action) = (surface.id.clone(), node.id.clone());
            let entity = ctx.entity.clone();
            let checked = node.checked;
            row_with_label(
                &node.label,
                Switch::new(SharedString::from(format!(
                    "plugin/{}/{}",
                    surface.id, node.id
                )))
                .checked(checked)
                .disabled(!live)
                .on_click(move |now: &bool, _window, cx| {
                    let (plugin, action) = (plugin.clone(), action.clone());
                    // What the switch is *now*, taken from the press rather
                    // than from the tree it was drawn against. Nothing here
                    // updates optimistically — the plugin republishes and
                    // that is the state — so two presses before the answer
                    // came back both read the same stale `checked` and sent
                    // the same value twice, and the second click vanished.
                    let value = if *now { "1" } else { "0" }.to_string();
                    entity.update(cx, |app, cx| {
                        app.send_plugin_action(
                            &plugin,
                            &action,
                            Some(value),
                            slot,
                            PluginWidget::Toggle,
                            cx,
                        );
                    });
                })
                .into_any_element(),
                metrics,
                cx,
            )
            .into_any_element()
        }

        PluginWidget::TextField => {
            // The plugin owns the value; this window owns what is being
            // typed. A field with no box yet is one whose sync has not run,
            // which happens for exactly one frame after a plugin adds it.
            let state = app.plugin_field(&surface.id, slot, &node.id);
            div()
                .flex()
                .flex_col()
                .gap(metrics.space_xs())
                .when(!node.label.is_empty(), |el| {
                    el.child(field_label(&node.label, metrics, cx))
                })
                .children(
                    state.map(|state: &Entity<InputState>| {
                        Input::new(state).w_full().disabled(!live)
                    }),
                )
                .into_any_element()
        }

        PluginWidget::Label => div()
            .text_size(metrics.text_small())
            .text_color(parts::subtle(cx))
            .child(node.label.clone())
            .into_any_element(),

        PluginWidget::Row => div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap(metrics.space_md())
            .children(children(surface, slot, node, app, ctx, cx))
            .into_any_element(),

        PluginWidget::Column => div()
            .flex()
            .flex_col()
            .gap(metrics.space_md())
            .children(children(surface, slot, node, app, ctx, cx))
            .into_any_element(),

        PluginWidget::Section => div()
            .flex()
            .flex_col()
            .gap(metrics.space_lg())
            .when(!node.label.is_empty(), |el| {
                el.child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_micro())
                        .text_color(parts::subtle(cx))
                        .child(node.label.to_uppercase()),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(metrics.space_lg())
                    .children(children(surface, slot, node, app, ctx, cx)),
            )
            .into_any_element(),
    }
}

fn children(
    surface: &PluginSurface,
    slot: PluginSlot,
    node: &PluginNode,
    app: &WhatsAppApp,
    ctx: &PluginContext,
    cx: &App,
) -> Vec<gpui::AnyElement> {
    node.children
        .iter()
        .map(|child| widget(surface, slot, child, app, ctx, cx))
        .collect()
}

/// A control with its label to the left, which is the shape a settings row
/// takes everywhere else on that screen.
fn row_with_label(
    label: &str,
    control: gpui::AnyElement,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(metrics.space_lg())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(metrics.text_body())
                .text_color(cx.theme().foreground)
                .child(label.to_owned()),
        )
        .child(control)
}

fn field_label(label: &str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .text_size(metrics.text_small())
        .text_color(cx.theme().muted_foreground)
        .child(label.to_owned())
}

/// How a plugin is doing, in one word.
///
/// Three states and not two, because the folder holds more than the host
/// runs: a module that fails to parse, answers the wrong ABI version or traps
/// in `oxi_init` publishes no surface at all, and is exactly the file
/// somebody most needs to find. Drawn in the same list as the rest rather
/// than in a second one below it — two shapes for "a plugin in this folder"
/// is what made this screen hard to read, and the difference between them is
/// a word, not a layout.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Running,
    Stopped,
    NotLoaded,
}

impl Standing {
    const fn word(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Stopped => "Stopped",
            Self::NotLoaded => "Not loaded",
        }
    }
}

/// The frame every plugin is drawn in, whatever its standing.
///
/// One card per plugin and every control that acts on it *inside* it. Remove
/// used to sit outside — a bare button in the gap between two cards, which
/// with more than one plugin reads as belonging to the next one, and under a
/// list of modules that did not load was a column of identical buttons with
/// nothing saying which was which.
fn entry(
    heading: gpui::AnyElement,
    body: Vec<gpui::AnyElement>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .w_full()
        .flex()
        .flex_col()
        .rounded(metrics.radius_md())
        .overflow_hidden()
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(metrics.space_md())
                .p(metrics.space_lg())
                .child(heading),
        )
        // Each section under its own rule, which is what tells "this is what
        // the plugin is" apart from "this is the decision you are being
        // asked for" and from the plugin's own controls. They were four
        // paragraphs of grey text in one box, and the switch was one of them.
        .children(body.into_iter().map(|section| {
            div()
                .w_full()
                .border_t_1()
                .border_color(cx.theme().border)
                .p(metrics.space_lg())
                .child(section)
        }))
}

/// A plugin's name, its id and how it is doing, on one line with whatever
/// acts on it.
fn heading(
    name: String,
    id: &str,
    standing: Standing,
    remove: Option<gpui::AnyElement>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let subtle = parts::subtle(cx);
    // The colour says the same thing the word does, so nothing here depends
    // on reading it: a legend nobody is given is a legend nobody has.
    let tone = match standing {
        Standing::Running => cx.theme().success,
        Standing::Stopped => cx.theme().danger,
        Standing::NotLoaded => cx.theme().warning,
    };
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(metrics.space_md())
        .child(
            parts::detail_stack()
                .gap(metrics.space_xxs())
                .child(
                    div()
                        .text_size(metrics.text_body())
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(cx.theme().foreground)
                        .child(name),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(metrics.space_sm())
                        .child(
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(metrics.text_micro())
                                .text_color(subtle)
                                .child(id.to_owned()),
                        )
                        .child(
                            div()
                                .text_size(metrics.text_micro())
                                .text_color(tone)
                                .child(standing.word()),
                        ),
                ),
        )
        .children(remove)
}

/// One plugin's block on the Settings screen: what it is, what it may do, and
/// whatever it drew for itself.
///
/// `remove` is drawn where the front end has a folder of its own to take it
/// out of, and nowhere else — see `platform::plugins::Home`.
pub fn settings_entry(
    surface: &PluginSurface,
    app: &WhatsAppApp,
    ctx: &PluginContext,
    remove: Option<gpui::AnyElement>,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = ctx.metrics;
    let subtle = parts::subtle(cx);
    let standing = if surface.stopped.is_some() {
        Standing::Stopped
    } else {
        Standing::Running
    };

    // What it does to the *account*, which is the only part anybody is asked
    // about. Phrased before the answer rather than after it: a plugin that
    // has not been allowed is running and refused — it watches, and every
    // gated command it issues comes back denied.
    let permissions = if surface.gated.is_empty() {
        "Watches only. It cannot act on your account.".to_string()
    } else if surface.approved {
        format!("Allowed to: {}", surface.gated.join(", "))
    } else {
        format!("Wants to: {}", surface.gated.join(", "))
    };
    // What it does only to itself, said plainly and never as a question. It
    // holds these by declaring them, so offering a switch over them would be
    // offering a choice that does not exist — but leaving them unsaid would
    // hide half of what a downloaded file is doing.
    let own: Vec<&str> = surface
        .capabilities
        .iter()
        .filter(|c| !surface.gated.contains(c))
        .map(String::as_str)
        .collect();
    let own = (!own.is_empty()).then(|| format!("Also: {}", own.join(", ")));

    let mut body: Vec<gpui::AnyElement> = Vec::new();
    body.push(
        div()
            .flex()
            .flex_col()
            .gap(metrics.space_xs())
            .child(
                div()
                    .text_size(metrics.text_small())
                    .text_color(subtle)
                    .child(permissions),
            )
            .children(own.map(|own| {
                div()
                    .text_size(metrics.text_small())
                    .text_color(subtle)
                    .child(own)
            }))
            // Why it stopped, where the widgets it left behind are. A plugin
            // that simply disappeared would give nobody anything to act on.
            .children(surface.stopped.as_ref().map(|reason| {
                div()
                    .text_size(metrics.text_small())
                    .text_color(cx.theme().danger)
                    .child(reason.clone())
            }))
            .into_any_element(),
    );

    // Drawn by this window and not by the plugin, which is the whole point:
    // a widget id comes from the plugin's own tree, so a control it could
    // publish is a control it could disguise.
    //
    // Only where there is something to withhold. A plugin wanting nothing but
    // to draw and keep its own settings holds those by declaring them, so a
    // switch over it could be turned off and would immediately read as on
    // again — a control that lies about what it does.
    if !surface.gated.is_empty() {
        let id = surface.id.clone();
        let entity = ctx.entity.clone();
        let approved = surface.approved;
        body.push(
            row_with_label(
                "Let it act on your account",
                Switch::new(SharedString::from(format!("plugin-allow-{}", surface.id)))
                    .checked(approved)
                    .on_click(move |now: &bool, _window, cx| {
                        let id = id.clone();
                        // The switch's own state, not the surface this was
                        // drawn against. Nothing here updates optimistically
                        // — the daemon republishes and that is the answer —
                        // so two presses before the round trip both read the
                        // same stale `approved` and sent `!approved` twice: a
                        // grant followed immediately by a withdrawal sent two
                        // grants and left the capability on.
                        let allowed = *now;
                        entity.update(cx, |app, cx| app.approve_plugin(&id, allowed, cx));
                    })
                    .into_any_element(),
                metrics,
                cx,
            )
            .into_any_element(),
        );
    }

    let drawn: Vec<gpui::AnyElement> = surface
        .roots_in(PluginSlot::Settings)
        .map(|node| widget(surface, PluginSlot::Settings, node, app, ctx, cx).into_any_element())
        .collect();
    if !drawn.is_empty() {
        body.push(
            div()
                .flex()
                .flex_col()
                .gap(metrics.space_lg())
                .children(drawn)
                .into_any_element(),
        );
    }

    entry(
        heading(
            surface.name.clone(),
            &surface.id,
            standing,
            remove,
            metrics,
            cx,
        )
        .into_any_element(),
        body,
        metrics,
        cx,
    )
}

/// A file in the folder that the host could not run.
///
/// It has no surface, so there is no name, no permissions and nothing it
/// drew — but it is in the folder, it spends the folder's budget at every
/// load, and it is the one entry somebody is most likely to be looking for.
/// Drawn in the same list and the same frame as the rest, with the one
/// control it can have.
pub fn unloaded_entry(
    id: &str,
    remove: Option<gpui::AnyElement>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let subtle = parts::subtle(cx);
    entry(
        heading(id.to_owned(), id, Standing::NotLoaded, remove, metrics, cx).into_any_element(),
        vec![
            div()
                .text_size(metrics.text_small())
                .text_color(subtle)
                .child(
                    "It is in the plugins folder but the host would not run it. \
                     Either it is not a plugin, or it was built against a different \
                     version of the ABI.",
                )
                .into_any_element(),
        ],
        metrics,
        cx,
    )
}
