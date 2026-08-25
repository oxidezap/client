//! The window's destinations.
//!
//! Two of them today, Chats and Status, and the rail exists because Status is
//! not a conversation: it has no other party and nothing is sent to it, so
//! putting it in the list of people to talk to made it read as one.
//!
//! One list of destinations, drawn along whichever axis there is room for: a
//! strip down the side of a window, a bar across the foot of a phone. The
//! alternative — two functions — is two places to add the third destination.

use gpui::prelude::*;
use gpui::{App, Entity, IntoElement, ParentElement, Styled, div};
use gpui_component::ActiveTheme as _;
use gpui_component::Icon;

use crate::app::{Destination, WhatsAppApp};
use crate::responsive::ResponsiveLayout;
use crate::theme::{ActiveProductTheme as _, Metrics};

pub fn render_nav_rail(
    current: Destination,
    unread_chats: usize,
    unseen_status: usize,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = *layout.metrics();
    let is_mobile = layout.is_mobile();
    let thickness = metrics.touch_target() + metrics.space_lg();

    let base = div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .gap(metrics.space_sm())
        .bg(cx.theme().sidebar)
        .border_color(cx.theme().border);

    let base = if is_mobile {
        base.w_full().h(thickness).flex_row().border_t_1()
    } else {
        base.h_full().w(thickness).flex_col().border_r_1()
    };

    base.children(Destination::ALL.into_iter().map(|destination| {
        let badge = match destination {
            Destination::Chats => unread_chats,
            Destination::Status => unseen_status,
        };
        render_destination(
            destination,
            destination == current,
            badge,
            entity.clone(),
            metrics,
            cx,
        )
    }))
}

/// One destination. A clickable `div` rather than a `Button`: it selects where
/// the window is, the way a chat row selects a conversation, and the selected
/// one has to stay visibly selected — which is state a button does not carry.
fn render_destination(
    destination: Destination,
    is_current: bool,
    badge: usize,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let icon: Icon = match destination {
        Destination::Chats => crate::components::ProductIcon::MessageSquare.into(),
        Destination::Status => crate::components::ProductIcon::CircleDashed.into(),
    };

    div()
        .id(destination.id())
        .relative()
        .size(metrics.touch_target())
        .flex()
        .items_center()
        .justify_center()
        .rounded(metrics.radius_md())
        .cursor_pointer()
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(destination.label()).build(window, cx)
        })
        .when(is_current, |el| el.bg(cx.theme().list_active))
        .when(!is_current, |el| {
            let hover = cx.theme().list_hover;
            el.hover(move |s| s.bg(hover))
        })
        .on_click(move |_, _window, cx| {
            entity.update(cx, |app, cx| app.set_destination(destination, cx));
        })
        .child(icon.size(metrics.icon()).text_color(if is_current {
            cx.theme().foreground
        } else {
            cx.product().hsla(cx.product().palette.subtle_foreground)
        }))
        // A count, not a dot: how many conversations are waiting is the thing
        // worth knowing before deciding to look.
        .when(badge > 0, |el| {
            el.child(
                div()
                    .absolute()
                    .top(metrics.space_xs())
                    .right(metrics.space_xs())
                    .min_w(metrics.space_lg())
                    .h(metrics.space_lg())
                    .px(metrics.space_xxs())
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(cx.theme().primary)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(metrics.text_micro())
                    .text_color(cx.theme().primary_foreground)
                    .child(if badge > 99 {
                        "99+".to_string()
                    } else {
                        badge.to_string()
                    }),
            )
        })
}
