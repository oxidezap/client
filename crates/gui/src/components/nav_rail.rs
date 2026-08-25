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
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, Selectable as _};

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
    // A phone's tab bar is sized by the finger; a desktop strip is sized by
    // the icon it holds. Using the touch target at every width put a 60px rail
    // beside 34px header icons — two control scales in one window, and the
    // phone's minimum applied where nothing else in the app uses it.
    let target = if is_mobile {
        metrics.touch_target()
    } else {
        metrics.icon_button()
    };
    let thickness = target + metrics.space_lg();

    let base = div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(metrics.space_sm())
        .bg(cx.theme().sidebar)
        .border_color(cx.theme().border);

    let base = if is_mobile {
        // A phone's tab bar spans the foot, so its items spread across it.
        base.w_full()
            .h(thickness)
            .flex_row()
            .justify_center()
            .border_t_1()
    } else {
        // A strip beside a list starts where the list starts. Centred down the
        // window, two icons float in the middle of nothing and line up with
        // no part of what they switch between.
        base.h_full()
            .w(thickness)
            .flex_col()
            .justify_start()
            .pt(metrics.space_lg())
            .border_r_1()
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
            target,
            entity.clone(),
            metrics,
            cx,
        )
    }))
}

/// One destination. It selects where the window is, the way a chat row selects
/// a conversation, and the selected one stays visibly selected — which is what
/// `Button::selected` carries.
#[allow(clippy::too_many_arguments)]
fn render_destination(
    destination: Destination,
    is_current: bool,
    badge: usize,
    // What the rail sized itself for: a finger below the breakpoint, an icon
    // above it.
    target: gpui::Pixels,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let icon: Icon = match destination {
        Destination::Chats => crate::components::ProductIcon::MessageSquare.into(),
        Destination::Status => crate::components::ProductIcon::CircleDashed.into(),
    };

    // A `Button`, not a styled `div`: this is the only route to Status, so a
    // pointer-only rail would put a whole destination out of reach of the
    // keyboard — and the focus ring the theme defines is drawn by the
    // library's controls, not by anything hand-rolled.
    Button::new(destination.id())
        .ghost()
        .selected(is_current)
        .relative()
        .size(target)
        .flex()
        .items_center()
        .justify_center()
        .rounded(metrics.radius_md())
        .tooltip(destination.label())
        .when(is_current, |el| el.bg(cx.theme().list_active))
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
