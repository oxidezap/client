//! The window's destinations.
//!
//! Chats, Status, Channels and Communities share the same navigation on both targets.
//!
//! One list of destinations, drawn along whichever axis there is room for: a
//! strip down the side of a window, a bar across the foot of a phone. The
//! axis follows the responsive layout.

use gpui::prelude::*;
use gpui::{App, Entity, IntoElement, ParentElement, Styled, div};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, Selectable as _};

use crate::app::{Destination, WhatsAppApp};
use crate::components::parts;
use crate::responsive::ResponsiveLayout;
use crate::theme::Metrics;

pub fn render_nav_rail(
    current: Destination,
    unread: [usize; 4],
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
        .tab_group()
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

    base.children(
        Destination::ALL
            .into_iter()
            .zip(unread)
            .map(|(destination, badge)| {
                render_destination(
                    destination,
                    destination == current,
                    badge,
                    target,
                    entity.clone(),
                    metrics,
                    cx,
                )
            }),
    )
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
        Destination::Channels => crate::components::ProductIcon::Channels.into(),
        Destination::Communities => crate::components::ProductIcon::Users.into(),
    };

    // A `Button`: destinations need keyboard activation and a focus ring.
    // This is the only route to each list, so a
    // pointer-only rail would put a whole destination out of reach of the
    // keyboard — and the focus ring the theme defines is drawn by the
    // library's controls, not by anything hand-rolled.
    let button = Button::new(destination.id())
        .ghost()
        .selected(is_current)
        .toggled(is_current)
        .accessibility_id(destination.id())
        .accessibility_label(destination.label())
        .debug_selector(move || destination.id().into())
        .relative()
        .size(target)
        .flex()
        .items_center()
        .justify_center()
        .rounded(metrics.radius_md())
        .tooltip(destination.label())
        .when(is_current, |el| el.bg(cx.theme().list_active))
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            entity.update(cx, |app, cx| app.set_destination(destination, window, cx));
        })
        .child(icon.size(metrics.icon()).text_color(if is_current {
            cx.theme().foreground
        } else {
            parts::subtle(cx)
        }));

    // Button clips its internal content to the icon box. Keep the badge beside
    // it so all digits survive on compact desktop buttons too.
    div()
        .relative()
        .size(target)
        .flex_shrink_0()
        .child(button)
        // A count, not a dot: how many conversations are waiting is the thing
        // worth knowing before deciding to look.
        .when(badge > 0, |el| {
            el.child(
                div()
                    .id(gpui::SharedString::from(format!(
                        "nav-badge-{}",
                        destination.id()
                    )))
                    .debug_selector(move || format!("nav-badge-{}", destination.id()))
                    .absolute()
                    .whitespace_nowrap()
                    .top(metrics.space_xs())
                    .right(metrics.space_xs())
                    .min_w(metrics.space_lg())
                    .min_h(metrics.space_lg())
                    .py(metrics.space_xxs())
                    .line_height(gpui::relative(1.0))
                    .px(metrics.space_xxs())
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(cx.theme().primary)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(metrics.text_micro())
                    .text_color(cx.theme().primary_foreground)
                    .child(
                        div()
                            .id("nav-badge-label")
                            .debug_selector(|| "nav-badge-label".into())
                            .child(if badge > 99 {
                                "99+".to_string()
                            } else {
                                badge.to_string()
                            }),
                    ),
            )
        })
}
