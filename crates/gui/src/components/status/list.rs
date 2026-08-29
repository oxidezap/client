//! The Status destination's sidebar: mine at the top, everyone else's below.
//!
//! Modelled on what WhatsApp itself shows, because that is what the reader
//! already knows: one row per person rather than one per update, ordered by
//! who has something unwatched.

use gpui::prelude::*;
use gpui::{
    App, Entity, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, div,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Icon, IconName, Selectable as _, Sizable as _};

use crate::app::WhatsAppApp;
use crate::components::status::status_ring;
use crate::components::{EmptyState, ProductIcon};
use crate::responsive::ResponsiveLayout;
use crate::theme::{ActiveProductTheme as _, Metrics};
use crate::utils::format_status_time;

use oxidezap_core::{StatusAuthor, StatusFeed};

pub struct StatusListProps {
    pub feed: StatusFeed,
    /// Whose updates are open, so the row reads as selected.
    pub selected: StatusSelection,
}

/// Whose run the reader has open.
///
/// Named rather than an `Option<String>` in which the account's own updates
/// are the empty string. That is how the feed keys them and it is fine there;
/// as a *selection* it meant any path producing an empty JID — an author
/// without one, a half-cleared reset — drew "My status" as the row being
/// read, with nothing in the code saying that `""` meant anything at all.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StatusSelection {
    /// Nobody's: the list is being browsed.
    #[default]
    None,
    /// The account's own.
    Mine,
    /// A contact's, by JID.
    Author(String),
}

impl StatusSelection {
    /// What the reader has open, as the pane spells it.
    #[must_use]
    pub fn of(author: Option<&str>) -> Self {
        match author {
            None => Self::None,
            Some("") => Self::Mine,
            Some(jid) => Self::Author(jid.to_string()),
        }
    }

    fn is(&self, jid: &str) -> bool {
        matches!(self, Self::Author(open) if open == jid)
    }
}

pub fn render_status_list(
    props: StatusListProps,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = *layout.metrics();

    let base = if layout.is_mobile() {
        div().w_full()
    } else {
        div().w(layout.sidebar_width())
    };

    base
        // The same shell the conversation list draws, for the same reason: the
        // boundary belongs to the sidebar, and the pane beside it draws none.
        .when(!layout.is_mobile(), |el| {
            el.border_r_1().border_color(cx.theme().border)
        })
        .flex()
        .flex_col()
        .h_full()
        .min_h_0()
        .bg(cx.theme().sidebar)
        .child(render_title_bar(metrics, cx))
        .child(
            div()
                .id("status-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(metrics.space_md())
                .pb(metrics.space_lg())
                .child(render_mine(&props, entity.clone(), layout, cx))
                .children(render_recent(&props, entity, layout, cx)),
        )
}

fn render_title_bar(metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .flex_shrink_0()
        .h(metrics.sidebar_header_height())
        .flex()
        .items_center()
        .pl(metrics.space_xl())
        .pr(metrics.space_lg())
        .child(
            div()
                .text_size(metrics.text_title())
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child("Status"),
        )
}

/// Our own updates, whether or not there are any.
///
/// The row is here even when empty because it is also the answer to "where do
/// I post one" — and it says plainly that posting is not built yet rather than
/// offering a button that does nothing.
fn render_mine(
    props: &StatusListProps,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = *layout.metrics();
    let mine = props.feed.mine();
    let subtitle: SharedString = match mine {
        Some(author) => format!(
            "{} · {}",
            update_count(author.count()),
            format_status_time(&author.latest)
        )
        .into(),
        None => "Posting is not available yet".into(),
    };

    div()
        .flex_shrink_0()
        .pt(metrics.space_sm())
        .pb(metrics.space_lg())
        .child(match mine {
            Some(author) => render_author_row(
                author,
                props.selected == StatusSelection::Mine,
                Some(subtitle.clone()),
                entity,
                layout,
                cx,
            )
            .into_any_element(),
            None => render_placeholder_row(subtitle, metrics, cx).into_any_element(),
        })
}

fn render_recent(
    props: &StatusListProps,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> Vec<gpui::AnyElement> {
    let metrics = *layout.metrics();
    let authors = props.feed.authors();

    if authors.is_empty() {
        return vec![
            div()
                .pt(metrics.space_xxxl())
                .flex()
                .justify_center()
                .child(
                    EmptyState::new("No status updates")
                        .icon(ProductIcon::CircleDashed)
                        .description("Updates from your contacts show up here for 24 hours."),
                )
                .into_any_element(),
        ];
    }

    let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(authors.len() + 1);
    rows.push(section_label("Recent", metrics, cx).into_any_element());
    rows.extend(authors.iter().map(|author| {
        let is_selected = props.selected.is(&author.jid);
        render_author_row(author, is_selected, None, entity.clone(), layout, cx).into_any_element()
    }));
    rows
}

fn section_label(label: &'static str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .px(metrics.space_md())
        .pb(metrics.space_sm())
        .text_size(metrics.text_meta())
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(cx.product().hsla(cx.product().palette.subtle_foreground))
        .child(label)
}

/// One person, with the ring that says whether their run is watched.
fn render_author_row(
    author: &StatusAuthor,
    is_selected: bool,
    subtitle: Option<SharedString>,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = *layout.metrics();
    let jid = author.jid.clone();
    let name: SharedString = author.name.clone().into();
    let subtitle: SharedString = subtitle.unwrap_or_else(|| {
        format!(
            "{} · {}",
            update_count(author.count()),
            format_status_time(&author.latest)
        )
        .into()
    });
    let ground = if is_selected {
        cx.theme().list_active
    } else {
        cx.theme().sidebar
    };
    // The ring is drawn at the row's avatar size minus the room the ring
    // itself takes, so a Status row is the same height as a chat row.
    let avatar = layout.avatar_size() - metrics.space_sm();

    // A `Button` rather than a row-shaped `div`: opening someone's status is
    // the only thing this screen does, and a chat row at least has ↑↓ and the
    // list's own selection behind it. This has nothing else.
    Button::new(SharedString::from(format!("status-{jid}")))
        .ghost()
        .selected(is_selected)
        .w_full()
        .h(layout.chat_item_height())
        .flex()
        .items_center()
        .justify_start()
        .gap(metrics.space_lg())
        .px(metrics.chat_row_padding_x())
        .rounded(metrics.radius_lg())
        .when(is_selected, |el| el.bg(cx.theme().list_active))
        .on_click(move |_, _window, cx| {
            entity.update(cx, |app, cx| app.open_status(jid.clone(), cx));
        })
        .child(status_ring(
            &author.jid,
            &author.name,
            avatar,
            author.count(),
            author.unseen,
            ground,
            cx,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(metrics.space_xs())
                .child(
                    div()
                        .text_size(metrics.text_body())
                        .text_color(cx.theme().foreground)
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(name),
                )
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .text_color(if author.has_unseen() {
                            cx.theme().primary
                        } else {
                            cx.product().hsla(cx.product().palette.subtle_foreground)
                        })
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(subtitle),
                ),
        )
}

/// "My status", with nothing behind it yet.
fn render_placeholder_row(
    subtitle: SharedString,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .px(metrics.chat_row_padding_x())
        .py(metrics.space_md())
        .child(
            Button::new("status-add")
                .icon(Icon::new(IconName::Plus))
                .ghost()
                .large()
                .disabled(true)
                .tooltip("Posting a status update is not available yet"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(metrics.space_xs())
                .child(
                    div()
                        .text_size(metrics.text_body())
                        .text_color(cx.theme().foreground)
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child("My status"),
                )
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .text_color(cx.product().hsla(cx.product().palette.subtle_foreground))
                        .child(subtitle),
                ),
        )
}

/// "1 update" / "4 updates" — the count is the row's whole subtitle, so it
/// has to read as a sentence rather than a bare number.
fn update_count(count: usize) -> String {
    if count == 1 {
        "1 update".to_string()
    } else {
        format!("{count} updates")
    }
}

#[cfg(test)]
mod tests {
    use super::StatusSelection;

    /// The account's own updates are keyed by the empty string in the feed,
    /// and as a bare `Option<String>` selection any path producing an empty
    /// JID drew "My status" as the row being read.
    #[test]
    fn an_empty_jid_is_not_a_contact_being_read() {
        assert_eq!(StatusSelection::of(None), StatusSelection::None);
        assert_eq!(StatusSelection::of(Some("")), StatusSelection::Mine);
        assert_eq!(
            StatusSelection::of(Some("a@s.whatsapp.net")),
            StatusSelection::Author("a@s.whatsapp.net".to_string())
        );

        assert!(!StatusSelection::None.is(""), "nothing is open");
        assert!(!StatusSelection::Mine.is(""), "and our own is not a contact");
        assert!(StatusSelection::Author("a@s.whatsapp.net".to_string()).is("a@s.whatsapp.net"));
    }
}
