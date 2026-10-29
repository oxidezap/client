//! The Status destination's sidebar: mine at the top, everyone else's below.
//!
//! Modelled on what WhatsApp itself shows, because that is what the reader
//! already knows: one row per person rather than one per update, ordered by
//! who has something unwatched.

use gpui::prelude::*;
use gpui::{
    App, Entity, IntoElement, ParentElement, Pixels, SharedString, Size,
    StatefulInteractiveElement, Styled, div, size,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::scroll::Scrollbar;
use gpui_component::{Disableable as _, Icon, IconName, Selectable as _, Sizable as _};
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use std::{cell::RefCell, rc::Rc};

use crate::app::WhatsAppApp;
use crate::components::parts;
use crate::components::status::status_ring;
use crate::components::{EmptyState, ProductIcon};
use crate::responsive::ResponsiveLayout;
use crate::theme::Metrics;
use crate::utils::format_status_time;

use oxidezap_core::{StatusAuthor, StatusFeed};

pub struct StatusListProps {
    pub feed: StatusFeed,
    /// Whose updates are open, so the row reads as selected.
    pub selected: StatusSelection,
    /// Where this list is scrolled to, which is also what its scrollbar
    /// paints itself over.
    pub scroll: gpui::ScrollHandle,
}

/// Whose run the reader has open.
///
/// Named rather than an `Option<String>` in which the account's own updates
/// are the empty string. That is how the feed keys them and it is fine there;
/// as a *selection* it meant any path producing an empty JID (an author
/// without one, a half-cleared reset) drew "My status" as the row being
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
    _cx: &App,
) -> impl IntoElement + use<> {
    StatusList {
        props,
        entity,
        layout,
    }
}

#[derive(IntoElement)]
struct StatusList {
    props: StatusListProps,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
}

impl gpui::RenderOnce for StatusList {
    fn render(self, window: &mut gpui::Window, cx: &mut App) -> impl IntoElement {
        let Self {
            props,
            entity,
            layout,
        } = self;
        let focus = window
            .use_keyed_state("status-list-focus", cx, |_, cx| {
                cx.focus_handle().tab_stop(false)
            })
            .read(cx)
            .clone();
        // Keep the complete focus/accessibility tree while it is being used.
        // A mouse move must not unmount a keyboard-focused contact.
        let eager = window.last_input_was_keyboard()
            || window.is_a11y_active()
            || focus.contains_focused(window, cx);
        let metrics = *layout.metrics();
        let scroll = props.scroll.clone();

        let base = if layout.is_mobile() {
            div().w_full()
        } else {
            div().w(layout.sidebar_width())
        };

        base.id("status-list-focus")
            .track_focus(&focus)
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
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(render_rows(props, entity, layout, eager, cx))
                    // Like both other lists: the bar belongs to whatever scrolls,
                    // and it paints over the bounds its handle reports rather than
                    // over the element it was hung from — so the overlay only has
                    // to be here. With twenty contacts posting, nothing else said
                    // the list continued.
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .child(Scrollbar::vertical(&scroll)),
                    ),
            )
    }
}

/// Headers scroll with contacts. Unfocused pointer navigation builds only
/// visible buttons, formatted subtitles and avatar rings.
fn render_rows(
    props: StatusListProps,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    eager: bool,
    cx: &App,
) -> gpui::AnyElement {
    let metrics = *layout.metrics();
    // Virtualization has a fixed cost. Keep the existing layout when all
    // contacts fit in roughly one viewport.
    let authors = props.feed.authors().len();
    let content_height = layout.chat_item_height() * (authors as f32 + 1.)
        + metrics.space_sm()
        + metrics.space_lg() * 2.
        + section_height(metrics);
    let long_list =
        authors > 0 && content_height > layout.viewport().height - metrics.sidebar_header_height();
    if eager || !long_list {
        return div()
            .id("status-list")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&props.scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .px(metrics.space_md())
                    .pb(metrics.space_lg())
                    .child(render_mine(&props, entity.clone(), layout, long_list, cx))
                    .children(render_recent(&props, entity, layout, cx)),
            )
            .into_any_element();
    }
    let sizes = status_row_sizes(props.feed.authors().len(), layout);
    let scroll = VirtualListScrollHandle::from(props.scroll.clone());
    let rows_entity = entity.clone();
    v_virtual_list(entity, "status-list", sizes, move |_, range, _, cx| {
        range
            .map(|ix| {
                let row = match ix {
                    0 => render_mine(&props, rows_entity.clone(), layout, true, cx)
                        .into_any_element(),
                    1 => section_label("Recent", metrics, cx).into_any_element(),
                    _ => {
                        let author = &props.feed.authors()[ix - 2];
                        render_author_row(
                            author,
                            props.selected.is(&author.jid),
                            None,
                            rows_entity.clone(),
                            layout,
                            cx,
                        )
                        .into_any_element()
                    }
                };
                div()
                    .px(metrics.space_md())
                    .when(ix == props.feed.authors().len() + 1, |el| {
                        el.pb(metrics.space_lg())
                    })
                    .child(row)
            })
            .collect()
    })
    .track_scroll(&scroll)
    .size_full()
    .into_any_element()
}

fn section_height(metrics: Metrics) -> Pixels {
    metrics.text_meta() * 1.5 + metrics.space_sm()
}

/// Cache geometry across repaints, including font and density changes.
fn status_row_sizes(authors: usize, layout: ResponsiveLayout) -> Rc<Vec<Size<Pixels>>> {
    type Cached = (usize, Pixels, Metrics, Rc<Vec<Size<Pixels>>>);
    thread_local! { static SIZES: RefCell<Option<Cached>> = const { RefCell::new(None) }; }
    let width = layout.sidebar_width();
    let metrics = *layout.metrics();
    SIZES.with(|held| {
        let mut held = held.borrow_mut();
        if let Some((count, old_width, old_metrics, sizes)) = held.as_ref()
            && *count == authors
            && *old_width == width
            && *old_metrics == metrics
        {
            return Rc::clone(sizes);
        }
        let mut sizes = vec![size(width, layout.chat_item_height()); authors + 2];
        sizes[0].height += metrics.space_sm() + metrics.space_lg();
        sizes[1].height = section_height(metrics);
        sizes[authors + 1].height += metrics.space_lg();
        let sizes = Rc::new(sizes);
        *held = Some((authors, width, metrics, Rc::clone(&sizes)));
        sizes
    })
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
    bounded: bool,
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
            None => render_placeholder_row(
                subtitle,
                metrics,
                bounded.then(|| layout.chat_item_height()),
                cx,
            )
            .into_any_element(),
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
        .h(section_height(metrics))
        .line_height(metrics.text_meta() * 1.5)
        .px(metrics.space_md())
        .pb(metrics.space_sm())
        .text_size(metrics.text_meta())
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(parts::subtle(cx))
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
    #[cfg(test)]
    {
        tests::ROWS.with(|rows| rows.set(rows.get() + 1));
        tests::CAPTURE.with(|capture| {
            if let Some(ids) = capture.borrow_mut().as_mut() {
                ids.push(author.jid.clone());
            }
        });
    }
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
        .debug_selector(|| format!("status-{jid}"))
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
        .cursor_pointer()
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
            parts::detail_stack()
                .gap(metrics.space_xs())
                .child(
                    parts::one_line()
                        .text_size(metrics.text_body())
                        .text_color(cx.theme().foreground)
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(name),
                )
                .child(
                    parts::one_line()
                        .text_size(metrics.text_small())
                        .text_color(if author.has_unseen() {
                            cx.theme().primary
                        } else {
                            parts::subtle(cx)
                        })
                        .child(subtitle),
                ),
        )
}

/// "My status", with nothing behind it yet.
fn render_placeholder_row(
    subtitle: SharedString,
    metrics: Metrics,
    row_height: Option<Pixels>,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .debug_selector(|| "status-placeholder".into())
        .when_some(row_height, |el, height| el.h(height).overflow_hidden())
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
            parts::detail_stack()
                .gap(metrics.space_xs())
                .child(
                    div()
                        .when(row_height.is_some(), |el| {
                            el.whitespace_nowrap().overflow_hidden().text_ellipsis()
                        })
                        .text_size(metrics.text_body())
                        .text_color(cx.theme().foreground)
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child("My status"),
                )
                .child(
                    div()
                        .when(row_height.is_some(), |el| {
                            el.whitespace_nowrap().overflow_hidden().text_ellipsis()
                        })
                        .text_size(metrics.text_small())
                        .text_color(parts::subtle(cx))
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
    use super::*;
    thread_local! {
        pub(super) static ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        pub(super) static CAPTURE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    }

    #[test]
    fn status_geometry_tracks_count_and_metrics() {
        let layout = ResponsiveLayout::new(
            size(gpui::px(320.), gpui::px(600.)),
            crate::responsive::MobilePanel::ChatList,
            Metrics::default(),
        );
        let first = status_row_sizes(1000, layout);
        assert!(Rc::ptr_eq(&first, &status_row_sizes(1000, layout)));
        assert_eq!(first.len(), 1002);
        assert_eq!(
            first[0].height,
            layout.chat_item_height() + layout.metrics().space_sm() + layout.metrics().space_lg()
        );
        assert_eq!(
            first[1001].height,
            layout.chat_item_height() + layout.metrics().space_lg()
        );
        assert!(!Rc::ptr_eq(&first, &status_row_sizes(999, layout)));
        let zoomed = ResponsiveLayout::new(
            layout.viewport(),
            crate::responsive::MobilePanel::ChatList,
            Metrics::new(24., layout.metrics().density()),
        );
        assert_ne!(first[2].height, status_row_sizes(1000, zoomed)[2].height);
    }

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
        assert!(
            !StatusSelection::Mine.is(""),
            "and our own is not a contact"
        );
        assert!(StatusSelection::Author("a@s.whatsapp.net".to_string()).is("a@s.whatsapp.net"));
    }
}

#[cfg(test)]
mod visible_cost {
    use super::*;
    use crate::responsive::MobilePanel;
    use gpui::{Context, Render, TestAppContext, Window, px, size};

    struct Pane {
        feed: StatusFeed,
        app: Entity<WhatsAppApp>,
        scroll: gpui::ScrollHandle,
        metrics: Metrics,
        selected: StatusSelection,
    }
    impl Render for Pane {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let layout =
                ResponsiveLayout::new(window.viewport_size(), MobilePanel::ChatList, self.metrics);
            render_status_list(
                StatusListProps {
                    feed: self.feed.clone(),
                    selected: self.selected.clone(),
                    scroll: self.scroll.clone(),
                },
                self.app.clone(),
                layout,
                cx,
            )
        }
    }

    struct PlaceholderPane(Metrics);
    impl Render for PlaceholderPane {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            render_placeholder_row(
                "Posting is not available yet".into(),
                self.0,
                Some(self.0.chat_row_height()),
                cx,
            )
        }
    }

    #[gpui::test]
    fn narrow_placeholder_stays_inside_the_virtual_row(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        for density in crate::theme::metrics::Density::ALL {
            let metrics = Metrics::new(24., density);
            let (_, visual) = cx.add_window_view(|_, _| PlaceholderPane(metrics));
            visual.simulate_resize(size(px(120.), px(400.)));
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
            });
            let actual = visual
                .debug_bounds("status-placeholder")
                .unwrap()
                .size
                .height;
            let expected = metrics.chat_row_height();
            // GPUI snaps fractional layout dimensions to device pixels.
            assert!(actual >= expected - px(0.5) && actual <= expected + px(0.5));
        }
    }

    #[gpui::test]
    fn status_constructs_visible_contacts_and_can_scroll_to_last(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let mut chat = oxidezap_core::Chat::new("status@broadcast".into());
        for n in 0..1000 {
            chat.messages.push(oxidezap_core::ChatMessage::new_incoming(
                format!("S-{n}"),
                format!("contact-{n}@s.whatsapp.net"),
                "Synthetic update".into(),
            ));
        }
        let feed = StatusFeed::from_chat(&chat);
        let first = feed.authors().first().unwrap().jid.clone();
        let last = feed.authors().last().unwrap().jid.clone();
        let scroll = gpui::ScrollHandle::new();
        let window = cx.open_window(size(px(320.), px(600.)), |_, cx| Pane {
            feed,
            app: cx.new(WhatsAppApp::new),
            metrics: Metrics::default(),
            selected: StatusSelection::None,
            scroll: scroll.clone(),
        });
        cx.run_until_parked();
        super::tests::CAPTURE.with(|capture| *capture.borrow_mut() = Some(Vec::new()));
        cx.update_window(window.into(), |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let visible = super::tests::CAPTURE.with(|capture| capture.borrow_mut().take().unwrap());
        assert!(visible.contains(&first));
        assert!(!visible.contains(&last));
        assert!(
            visible.len() < 20,
            "built {} contacts for one viewport",
            visible.len()
        );
        let max = scroll.max_offset();
        assert!(max.y > px(0.));
        scroll.set_offset(gpui::point(px(0.), -max.y));
        super::tests::CAPTURE.with(|capture| *capture.borrow_mut() = Some(Vec::new()));
        cx.update_window(window.into(), |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let visible = super::tests::CAPTURE.with(|capture| capture.borrow_mut().take().unwrap());
        assert!(visible.contains(&last), "last contact cannot be reached");
        assert!(visible.len() < 20);
        assert!(scroll.offset().y < px(0.));

        // Expiry can replace a scrolled virtual list with the empty state.
        window
            .update(cx, |pane, _, cx| {
                pane.feed = StatusFeed::default();
                cx.notify();
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(scroll.offset().y, px(0.));
    }

    #[gpui::test]
    fn status_modality_and_resize_preserve_focus_selection_and_scroll(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let mut chat = oxidezap_core::Chat::new("status@broadcast".into());
        for n in 0..100 {
            chat.messages.push(oxidezap_core::ChatMessage::new_incoming(
                format!("S-{n}"),
                format!("contact-{n}@s.whatsapp.net"),
                "Synthetic update".into(),
            ));
        }
        // Multiple updates group into one row; our own updates add a separate
        // focusable header without changing the contact geometry.
        for n in 0..100 {
            chat.messages.push(oxidezap_core::ChatMessage::new_incoming(
                format!("second-{n}"),
                format!("contact-{n}@s.whatsapp.net"),
                "Second synthetic update".into(),
            ));
        }
        chat.messages.push(oxidezap_core::ChatMessage::new_outgoing(
            "mine".into(),
            "My synthetic update".into(),
        ));
        let feed = StatusFeed::from_chat(&chat);
        assert_eq!(feed.authors().len(), 100);
        assert!(feed.authors().iter().all(|author| author.count() == 2));
        assert_eq!(feed.mine().unwrap().count(), 1);
        let selected = StatusSelection::Author(feed.authors()[1].jid.clone());
        let scroll = gpui::ScrollHandle::new();
        let window = cx.open_window(size(px(320.), px(600.)), |_, cx| Pane {
            feed,
            app: cx.new(WhatsAppApp::new),
            scroll: scroll.clone(),
            metrics: Metrics::default(),
            selected: selected.clone(),
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(window.into(), "tab");
        let focused = cx
            .update_window(window.into(), |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
                window.focus_next(cx);
                window.focused(cx).unwrap()
            })
            .unwrap();
        let expected_next = cx
            .update_window(window.into(), |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
                window.focus_next(cx);
                let next = window.focused(cx).unwrap();
                window.focus_prev(cx);
                assert_eq!(window.focused(cx).as_ref(), Some(&focused));
                next
            })
            .unwrap();
        super::tests::CAPTURE.with(|capture| *capture.borrow_mut() = Some(Vec::new()));
        scroll.set_offset(gpui::point(px(0.), -px(1000.)));
        let before = scroll.offset();
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                    position: gpui::point(px(10.), px(10.)),
                    modifiers: gpui::Modifiers::default(),
                    pressed_button: None,
                }),
                cx,
            );
            window.refresh();
            window.draw(cx).clear(cx);
            assert!(!window.last_input_was_keyboard());
            assert_eq!(window.focused(cx).as_ref(), Some(&focused));
        })
        .unwrap();
        let mounted = super::tests::CAPTURE.with(|capture| capture.borrow_mut().take().unwrap());
        assert_eq!(
            mounted.len(),
            101,
            "focused rows must remain mounted after a mouse move"
        );
        assert_eq!(scroll.offset(), before);
        window
            .update(cx, |pane, _, cx| {
                assert_eq!(pane.selected, selected);
                pane.metrics = Metrics::new(24., crate::theme::metrics::Density::Compact);
                cx.notify();
            })
            .unwrap();
        cx.simulate_window_resize(window.into(), size(px(240.), px(450.)));
        cx.update_window(window.into(), |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
            assert_eq!(window.focused(cx).as_ref(), Some(&focused));
        })
        .unwrap();
        window
            .update(cx, |pane, _, _| assert_eq!(pane.selected, selected))
            .unwrap();
        assert!(scroll.offset().y <= px(0.) && -scroll.offset().y <= scroll.max_offset().y);
        cx.simulate_keystrokes(window.into(), "tab");
        cx.update_window(window.into(), |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
            window.focus_next(cx);
            assert_eq!(window.focused(cx).as_ref(), Some(&expected_next));
        })
        .unwrap();
        super::tests::CAPTURE.with(|capture| *capture.borrow_mut() = Some(Vec::new()));
        cx.update_window(window.into(), |_, window, cx| {
            window.blur(cx);
            window.dispatch_event(
                gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                    position: gpui::point(px(10.), px(10.)),
                    modifiers: gpui::Modifiers::default(),
                    pressed_button: None,
                }),
                cx,
            );
            window.refresh();
            window.draw(cx).clear(cx);
            assert!(window.focused(cx).is_none());
        })
        .unwrap();
        let mounted = super::tests::CAPTURE.with(|capture| capture.borrow_mut().take().unwrap());
        assert!(
            mounted.len() < 20,
            "unfocused pointer navigation should virtualize again"
        );
    }

    #[gpui::test]
    fn status_pointer_activation_and_modality_keep_virtual_rows(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let mut chat = oxidezap_core::Chat::new("status@broadcast".into());
        let now = wacore::time::now_utc();
        for n in 0..50 {
            let mut message = oxidezap_core::ChatMessage::new_incoming(
                format!("S-{n}"),
                format!("contact-{n}@s.whatsapp.net"),
                "Synthetic update".into(),
            );
            message.timestamp = now + chrono::Duration::seconds(n);
            chat.messages.push(message);
        }
        let feed = StatusFeed::from_chat(&chat);
        let window = cx.open_window(size(px(320.), px(600.)), |_, cx| Pane {
            feed,
            app: cx.new(WhatsAppApp::new),
            metrics: Metrics::default(),
            selected: StatusSelection::None,
            scroll: gpui::ScrollHandle::new(),
        });
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let original = visual
            .debug_bounds("status-contact-49@s.whatsapp.net")
            .unwrap();
        visual.simulate_keystrokes("tab");
        visual.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        assert_eq!(
            visual
                .debug_bounds("status-contact-49@s.whatsapp.net")
                .unwrap(),
            original
        );
        visual.simulate_mouse_move(
            gpui::point(px(10.), px(10.)),
            None,
            gpui::Modifiers::default(),
        );
        visual.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        assert_eq!(
            visual
                .debug_bounds("status-contact-49@s.whatsapp.net")
                .unwrap(),
            original
        );
        // The pinned Button already prevents pointer focus on mouse down.
        // Exercise activation itself, not just a hover/modality transition.
        visual.simulate_mouse_down(
            original.center(),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        visual.simulate_mouse_up(
            original.center(),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        super::tests::CAPTURE.with(|capture| *capture.borrow_mut() = Some(Vec::new()));
        visual.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
            assert!(
                window.focused(cx).is_none(),
                "pointer activation must not retain list focus"
            );
        });
        let mounted = super::tests::CAPTURE.with(|capture| capture.borrow_mut().take().unwrap());
        assert!(
            mounted.len() < 20,
            "pointer activation must preserve virtualization"
        );
        window
            .update(&mut visual, |pane, _, cx| {
                assert_eq!(
                    pane.app.read(cx).status_pane().author(),
                    Some("contact-49@s.whatsapp.net")
                );
            })
            .unwrap();
    }

    #[gpui::test]
    #[ignore = "manual release performance measurement"]
    fn status_visible_cost(cx: &mut TestAppContext) {
        let count: usize = std::env::var("STATUS_AUTHORS")
            .unwrap_or("1000".into())
            .parse()
            .unwrap();
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let mut chat = oxidezap_core::Chat::new("status@broadcast".into());
        for n in 0..count {
            chat.messages.push(oxidezap_core::ChatMessage::new_incoming(
                format!("S-{n}"),
                format!("contact-{n}@s.whatsapp.net"),
                "Synthetic update".into(),
            ));
        }
        let feed = StatusFeed::from_chat(&chat);
        assert_eq!(feed.authors().len(), count);
        let window = cx.open_window(size(px(320.), px(600.)), |_, cx| Pane {
            feed,
            app: cx.new(WhatsAppApp::new),
            metrics: Metrics::default(),
            selected: StatusSelection::None,
            scroll: gpui::ScrollHandle::new(),
        });
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        for sample in 0..7 {
            super::tests::ROWS.with(|rows| rows.set(0));
            let start = wacore::time::Instant::now();
            for _ in 0..20 {
                cx.update_window(window.into(), |_, window, cx| {
                    window.refresh();
                    window.draw(cx).clear(cx);
                })
                .unwrap();
            }
            let elapsed = start.elapsed();
            let rows = super::tests::ROWS.with(std::cell::Cell::get);
            eprintln!(
                "STATUS_COST authors={count} sample={sample} ns_per_frame={} rows_per_frame={}",
                elapsed.as_nanos() / 20,
                rows / 20
            );
        }
    }
    #[gpui::test]
    fn status_keyboard_navigation_reaches_all_contacts(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let mut chat = oxidezap_core::Chat::new("status@broadcast".into());
        for n in 0..100 {
            chat.messages.push(oxidezap_core::ChatMessage::new_incoming(
                format!("S-{n}"),
                format!("contact-{n}@s.whatsapp.net"),
                "Synthetic update".into(),
            ));
        }
        let feed = StatusFeed::from_chat(&chat);
        let window = cx.open_window(size(px(320.), px(600.)), |_, cx| Pane {
            feed,
            app: cx.new(WhatsAppApp::new),
            metrics: Metrics::default(),
            selected: StatusSelection::None,
            scroll: gpui::ScrollHandle::new(),
        });
        cx.run_until_parked();
        let mut seen = std::collections::HashSet::new();
        cx.simulate_keystrokes(window.into(), "tab");
        for _ in 0..105 {
            cx.update_window(window.into(), |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
                window.focus_next(cx);
                if let Some(handle) = window.focused(cx) {
                    seen.insert(format!("{handle:?}"));
                }
            })
            .unwrap();
        }
        assert_eq!(seen.len(), 100, "Tab must reach offscreen contacts");
    }
}
