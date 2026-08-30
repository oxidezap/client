//! The bar above a conversation: who it is, and what can be done with them.

use gpui::{
    App, Entity, IntoElement, ParentElement, SharedString, Styled, div, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{Icon, IconName};

use crate::app::WhatsAppApp;
use crate::components::avatar::Presence;
use crate::responsive::ResponsiveLayout;
use crate::theme::Metrics;
use oxidezap_core::{Availability, Chat, TypingSummary};

use super::{Avatar, ProductIcon};

/// Only plain PN/LID user JIDs can receive a call (not groups, broadcast
/// lists, status or newsletters).
fn is_callable_user(jid: &str) -> bool {
    jid.parse::<wacore_binary::jid::Jid>()
        .map(|j| j.is_pn() || j.is_lid())
        .unwrap_or(false)
}

/// What the header says under the name.
///
/// Ordered by which fact is most current: someone typing now beats a presence
/// reading, which beats the static member count.
///
/// A group with nobody typing has no subtitle at all: see below.
fn subtitle(
    chat: &Chat,
    typing: Option<&TypingSummary>,
    availability: Option<&Availability>,
) -> Option<(String, bool)> {
    if let Some(summary) = typing {
        return Some((summary.compact_label(chat.is_group), true));
    }
    if chat.is_group {
        // No member count. `participants` is not a roster — it is filled only
        // when a live message supplies that sender's name, so a fifty-person
        // group with one recently observed sender reported "1 members".
        // Nothing in the library's public surface answers the real question
        // yet, and saying nothing beats saying something false.
        return None;
    }
    match availability {
        Some(Availability::Online) => Some(("online".to_string(), false)),
        Some(Availability::LastSeen(at)) => Some((
            format!("last seen {}", crate::utils::format_list_time(at)),
            false,
        )),
        Some(Availability::Unknown) | None => None,
    }
}

// Nine, and each one is a fact about this header that the app does not hand
// over as a unit: what the conversation is, what it is doing, and what this
// window may do to it. A struct here would be a struct with nine fields and
// one construction site.
#[expect(
    clippy::too_many_arguments,
    reason = "nine facts, none of them a group"
)]
pub fn render_chat_header(
    chat: &Chat,
    typing: Option<&TypingSummary>,
    availability: Option<&Availability>,
    // Whether this conversation is with the account's own number, which is
    // the one case where the name alone does not identify it.
    is_own_number: bool,
    // Whether this window can reach the network at all. Offline, a call
    // button that still looked live would place a call into nothing.
    can_send: bool,
    entity: Entity<WhatsAppApp>,
    // What plugins want in this bar, already rendered. Elements rather than
    // the trees they came from: this component knows where a plugin's button
    // goes and nothing about what a plugin is.
    plugin_actions: Vec<gpui::AnyElement>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = *layout.metrics();
    // One definition of the marker, shared with the list row. Two copies had
    // already drifted — that one collapses whitespace in the name first, this
    // one did not — and a user-visible label with two implementations drifts
    // again.
    let name: SharedString = crate::app::chat_row::display_name(&chat.name, is_own_number).into();
    let subtitle = subtitle(chat, typing, availability);
    // No badge until presence actually arrives. A contact who has told us
    // nothing is not "away": most contacts are in that state before the first
    // presence event, and a dot claiming otherwise is a made-up fact.
    let presence = if chat.is_group {
        None
    } else {
        match availability {
            Some(Availability::Online) => Some(Presence::Online),
            Some(Availability::LastSeen(_)) => Some(Presence::Away),
            // `Unknown` is "offline and not saying when", which the subtitle
            // already declines to describe — a dot with no line under it
            // explaining it asserts more than the app knows.
            Some(Availability::Unknown) | None => None,
        }
    };

    div()
        .h(layout.header_height())
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_between()
        .gap(metrics.space_lg())
        .pl(layout.padding())
        .pr(metrics.space_lg())
        .bg(cx.theme().sidebar)
        .border_b_1()
        .border_color(cx.theme().border)
        .child(render_identity(
            chat, name, subtitle, presence, &entity, layout, metrics, cx,
        ))
        .child(render_actions(
            chat,
            can_send,
            plugin_actions,
            entity,
            layout,
            metrics,
            cx,
        ))
}

#[allow(clippy::too_many_arguments)]
fn render_identity(
    chat: &Chat,
    name: SharedString,
    subtitle: Option<(String, bool)>,
    presence: Option<Presence>,
    entity: &Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let back_entity = entity.clone();

    div()
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap(metrics.space_lg())
        .overflow_hidden()
        .when(layout.show_back_button(), |el| {
            // Back is a command, so it is a Button: that is what carries
            // focus, keyboard activation and the theme's button states,
            // none of which a styled div gets.
            el.child(
                Button::new("back")
                    .icon(IconName::ArrowLeft)
                    .ghost()
                    .tooltip("Back to chats")
                    .on_click(move |_, _window, cx| {
                        back_entity.update(cx, |app, cx| app.navigate_back(cx));
                    }),
            )
        })
        .child(
            Avatar::new(chat.jid.clone(), &chat.name, metrics.avatar_header())
                .group(chat.is_group)
                .presence(presence)
                .on(cx.theme().sidebar),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(metrics.space_xxs())
                .child(
                    div()
                        .text_size(metrics.text_strong())
                        .text_color(cx.theme().foreground)
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(name),
                )
                .children(subtitle.map(|(text, is_typing)| {
                    div()
                        .text_size(metrics.text_small())
                        .text_color(if is_typing {
                            cx.theme().primary
                        } else {
                            cx.theme().muted_foreground
                        })
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(text)
                })),
        )
}

#[allow(clippy::too_many_arguments)]
fn render_actions(
    chat: &Chat,
    can_send: bool,
    plugin_actions: Vec<gpui::AnyElement>,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    metrics: Metrics,
    _cx: &App,
) -> impl IntoElement + use<> {
    // Calls are 1:1 only: gate on a parsed PN/LID user JID, since !is_group
    // alone would still offer calls to status/broadcast and newsletter rows.
    // Offline is as much a reason a call cannot be placed as the JID being a
    // group: both mean the button would do nothing.
    let callable = is_callable_user(&chat.jid) && can_send;
    let call_jid = chat.jid.clone();
    let video_jid = chat.jid.clone();
    let overflow_jid = chat.jid.clone();
    let call_entity = entity.clone();
    let video_entity = entity.clone();
    let overflow_entity = entity.clone();
    let search_entity = entity;

    // Every native control, and none of them shrinks: the row around them
    // does, so that what gives way on a narrow header is the plugins' region
    // rather than Call or the overflow menu.
    let action = |id: &'static str, icon: Icon, tip: &'static str| {
        Button::new(id)
            .icon(icon)
            .ghost()
            .flex_shrink_0()
            .tooltip(tip)
            .w(layout.icon_button_size())
            .h(layout.icon_button_size())
    };

    div()
        .flex()
        .min_w_0()
        .items_center()
        .gap(metrics.space_xxs())
        // Plugin controls come first so they read as belonging to the
        // conversation rather than to the window's chrome — but in a region
        // that may *shrink*, while the native buttons after it may not.
        // Ordering alone would not protect them: a row that cannot shrink
        // simply grows, taking its `min_w_0` child's bound with it, and Call
        // and the overflow menu go off the edge of a narrow header. So the
        // row yields, every native control refuses to, and the plugins'
        // region is the only thing here that gives way.
        .child(
            div()
                .flex()
                .min_w_0()
                .items_center()
                .gap(metrics.space_xxs())
                .overflow_hidden()
                .children(plugin_actions),
        )
        .when(layout.show_call_buttons(), |el| {
            el.child(
                action(
                    "search-in-chat",
                    Icon::new(IconName::Search),
                    "Search in conversation",
                )
                .on_click(move |_, window, cx| {
                    search_entity.update(cx, |app, cx| app.toggle_conversation_search(window, cx));
                }),
            )
        })
        .when(callable && layout.show_call_buttons(), |el| {
            el.child(
                action("voice-call", ProductIcon::Phone.into(), "Voice call").on_click(
                    move |_, _window, cx| {
                        call_entity
                            .update(cx, |app, cx| app.start_call(call_jid.clone(), false, cx));
                    },
                ),
            )
            .child(
                action("video-call", ProductIcon::Video.into(), "Video call").on_click(
                    move |_, _window, cx| {
                        video_entity
                            .update(cx, |app, cx| app.start_call(video_jid.clone(), true, cx));
                    },
                ),
            )
        })
        .child(render_overflow_menu(
            callable,
            overflow_jid,
            overflow_entity,
            layout,
        ))
}

/// Everything the header can do, whether or not it has room to show it.
///
/// The menu is not a leftovers drawer: it carries every action the toolbar
/// does, so narrowing the window changes where a command lives and never
/// whether it exists. Below the breakpoint this is the only route to them,
/// which is exactly why it cannot be a subset.
fn render_overflow_menu(
    callable: bool,
    jid: String,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
) -> impl IntoElement + use<> {
    let search_entity = entity.clone();
    let video_entity = entity.clone();
    let call_entity = entity;

    Button::new("chat-menu")
        .icon(Icon::new(IconName::EllipsisVertical))
        .ghost()
        .tooltip("More")
        .w(layout.icon_button_size())
        .h(layout.icon_button_size())
        .dropdown_menu(move |menu, _window, _cx| {
            let search_entity = search_entity.clone();
            let call_entity = call_entity.clone();
            let video_entity = video_entity.clone();
            let video_jid = jid.clone();
            let jid = jid.clone();

            let menu = menu.item(
                PopupMenuItem::new("Search in conversation")
                    .icon(IconName::Search)
                    .on_click(move |_, window, cx| {
                        search_entity
                            .update(cx, |app, cx| app.toggle_conversation_search(window, cx));
                    }),
            );

            if !callable {
                return menu;
            }
            menu.separator()
                .item(
                    PopupMenuItem::new("Voice call")
                        .icon(Icon::from(ProductIcon::Phone))
                        .on_click(move |_, _window, cx| {
                            call_entity
                                .update(cx, |app, cx| app.start_call(jid.clone(), false, cx));
                        }),
                )
                .item(
                    PopupMenuItem::new("Video call")
                        .icon(Icon::from(ProductIcon::Video))
                        .on_click(move |_, _window, cx| {
                            video_entity
                                .update(cx, |app, cx| app.start_call(video_jid.clone(), true, cx));
                        }),
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::{ComposingKind, Typist};

    fn group_with(members: usize) -> Chat {
        let mut chat = Chat::new("group@g.us".to_string());
        for i in 0..members {
            chat.participants
                .insert(format!("{i}@s.whatsapp.net"), format!("Member {i}"));
        }
        chat
    }

    fn direct() -> Chat {
        Chat::new("5521999999999@s.whatsapp.net".to_string())
    }

    fn typing(name: &str) -> TypingSummary {
        TypingSummary {
            typists: vec![Typist {
                jid: format!("{name}@s.whatsapp.net"),
                name: name.to_string(),
            }],
            total: 1,
            kind: ComposingKind::Text,
        }
    }

    #[test]
    fn typing_outranks_every_other_subtitle() {
        let summary = typing("Ana");
        let (text, is_typing) = subtitle(&group_with(4), Some(&summary), None).unwrap();
        assert_eq!(text, "Ana typing…");
        assert!(is_typing);

        let (text, is_typing) =
            subtitle(&direct(), Some(&summary), Some(&Availability::Online)).unwrap();
        assert_eq!(text, "typing…", "a direct chat needs no name");
        assert!(is_typing);
    }

    /// `participants` collects the senders that have been *seen*, so its
    /// length is not the group's size and must never be presented as one.
    #[test]
    fn a_group_does_not_pass_its_known_senders_off_as_a_member_count() {
        assert!(subtitle(&group_with(4), None, None).is_none());
        assert!(subtitle(&group_with(0), None, None).is_none());
    }

    /// What a group *does* say is who is typing, which is a fact about now
    /// and true whatever the roster is.
    #[test]
    fn a_group_still_reports_who_is_typing() {
        let summary = TypingSummary {
            typists: vec![Typist {
                jid: "a@s.whatsapp.net".to_string(),
                name: "Ana".to_string(),
            }],
            total: 1,
            kind: ComposingKind::Text,
        };
        assert_eq!(
            subtitle(&group_with(4), Some(&summary), None).unwrap().0,
            "Ana typing…"
        );
    }

    #[test]
    fn a_contact_reports_presence_only_when_it_is_shared() {
        assert_eq!(
            subtitle(&direct(), None, Some(&Availability::Online))
                .unwrap()
                .0,
            "online"
        );
        assert!(
            subtitle(&direct(), None, Some(&Availability::Unknown)).is_none(),
            "a contact who hides last-seen gets no subtitle, not a guess"
        );
        assert!(subtitle(&direct(), None, None).is_none());
    }

    #[test]
    fn only_a_real_user_jid_can_be_called() {
        assert!(is_callable_user("5521999999999@s.whatsapp.net"));
        assert!(!is_callable_user("group@g.us"));
        assert!(!is_callable_user("status@broadcast"));
        assert!(!is_callable_user("not a jid"));
    }
}
