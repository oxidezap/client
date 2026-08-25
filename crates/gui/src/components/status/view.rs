//! One person's updates, played back in the pane a conversation would use.
//!
//! No timer. WhatsApp advances on its own because it is competing for a
//! phone's attention; on a desktop an update that vanishes while it is being
//! read is a bug wearing a feature's clothes, so this steps when the reader
//! says so.

use std::sync::Arc;

use gpui::StyledImage as _;
use gpui::{
    App, Entity, Image, ImageSource, IntoElement, ParentElement, SharedString, Styled, div, img,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Icon, IconName, Sizable as _};

use crate::app::WhatsAppApp;
use crate::components::{Avatar, EmptyState, ProductIcon};
use crate::responsive::ResponsiveLayout;
use crate::theme::{ActiveProductTheme as _, Metrics};
use crate::utils::format_status_time;

use oxidezap_core::ChatMessage;

/// Everything the pane needs, resolved by the caller: the decoded picture is
/// the app's to hand over, and this is drawn where the app is already
/// borrowed.
pub struct StatusViewProps {
    pub author_jid: String,
    pub author_name: SharedString,
    pub message: ChatMessage,
    pub image: Option<Arc<Image>>,
    /// A video update's current decoded frame, when one has been produced.
    pub frame: Option<Arc<gpui::RenderImage>>,
    /// Which of the run is on screen, and how many there are.
    pub index: usize,
    pub count: usize,
    /// Whether the bytes for this update are on their way.
    pub is_loading: bool,
}

pub fn render_status_view(
    props: Option<StatusViewProps>,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = *layout.metrics();
    let base = if layout.is_mobile() {
        div().w_full()
    } else {
        div().flex_1().min_w_0()
    };

    let base = base
        .flex()
        .flex_col()
        .h_full()
        .bg(cx.theme().background)
        .overflow_hidden();

    let Some(props) = props else {
        return base
            .justify_center()
            .items_center()
            .p(metrics.space_xxxl())
            .child(
                EmptyState::new("Status")
                    .icon(ProductIcon::CircleDashed)
                    .description("Pick someone on the left to see what they posted."),
            );
    };

    let can_prev = props.index > 0;
    let can_next = props.index + 1 < props.count;
    let prev_entity = entity.clone();
    let next_entity = entity.clone();
    let close_entity = entity;

    base.child(render_segments(props.index, props.count, metrics, cx))
        .child(render_header(
            &props,
            close_entity,
            layout.avatar_size(),
            metrics,
            cx,
        ))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_between()
                .gap(metrics.space_lg())
                .px(metrics.space_xl())
                .pb(metrics.space_xxl())
                .child(step_button(
                    "status-prev",
                    IconName::ChevronLeft,
                    "Previous update",
                    can_prev,
                    move |cx| {
                        prev_entity.update(cx, |app, cx| app.step_status(false, cx));
                    },
                ))
                .child(render_update(&props, metrics, cx))
                .child(step_button(
                    "status-next",
                    IconName::ChevronRight,
                    "Next update",
                    can_next,
                    move |cx| {
                        next_entity.update(cx, |app, cx| app.step_status(true, cx));
                    },
                )),
        )
}

/// A bar per update, the current one lit. This is the only thing that says
/// how much of someone's run is left.
fn render_segments(
    index: usize,
    count: usize,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let spent = cx.product().hsla(cx.product().palette.faint_foreground);
    let lit = cx.theme().primary;

    div()
        .flex_shrink_0()
        .flex()
        .gap(metrics.space_xs())
        .px(metrics.space_xl())
        .pt(metrics.space_lg())
        .children((0..count).map(|at| {
            div()
                .flex_1()
                .h(metrics.selection_bar_width())
                .rounded_full()
                .bg(if at <= index { lit } else { spent })
        }))
}

fn render_header(
    props: &StatusViewProps,
    close_entity: Entity<WhatsAppApp>,
    avatar: gpui::Pixels,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let when = format_status_time(&props.message.timestamp);

    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .px(metrics.space_xl())
        .py(metrics.space_lg())
        .child(
            Avatar::new(
                props.author_jid.clone(),
                &props.author_name,
                avatar - metrics.space_sm(),
            )
            .on(cx.theme().background),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(metrics.text_body())
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(cx.theme().foreground)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(props.author_name.clone()),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_meta())
                        .text_color(cx.product().hsla(cx.product().palette.subtle_foreground))
                        .child(when),
                ),
        )
        .child(
            Button::new("status-close")
                .icon(Icon::new(IconName::Close))
                .ghost()
                .tooltip("Close")
                .on_click(move |_, _window, cx| {
                    close_entity.update(cx, |app, cx| app.close_status(cx));
                }),
        )
}

/// The update itself: a picture, a caption, or a line of text on its own.
fn render_update(props: &StatusViewProps, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    let caption = props
        .message
        .media
        .as_ref()
        .and_then(|media| media.caption.clone())
        .filter(|caption| !caption.is_empty())
        .or_else(|| Some(props.message.content.clone()).filter(|text| !text.is_empty()));

    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(metrics.space_lg())
        .child(match (props.image.clone(), props.frame.clone()) {
            (Some(image), _) => img(ImageSource::Image(image))
                .max_w_full()
                .max_h_full()
                .object_fit(gpui::ObjectFit::Contain)
                .rounded(metrics.radius_lg())
                .into_any_element(),
            // A video update, decoding. The same frames the timeline draws.
            (None, Some(frame)) => img(ImageSource::Render(frame))
                .max_w_full()
                .max_h_full()
                .object_fit(gpui::ObjectFit::Contain)
                .rounded(metrics.radius_lg())
                .into_any_element(),
            // A text status, or media whose bytes are not here yet. Both are
            // the caption drawn large rather than an empty frame.
            (None, None) => div()
                .max_w(metrics.reading_width())
                .px(metrics.space_xxl())
                .py(metrics.space_xxxl())
                .rounded(metrics.radius_lg())
                .bg(cx.theme().secondary)
                .text_size(metrics.text_heading())
                .text_color(cx.theme().foreground)
                .child(caption.clone().unwrap_or_else(|| {
                    if props.is_loading {
                        "Loading this update…".to_string()
                    } else {
                        // Video, or bytes that would not come. Text updates
                        // land here too, and they *are* their caption.
                        "This update cannot be shown here.".to_string()
                    }
                }))
                .into_any_element(),
        })
        .children(
            (props.image.is_some() || props.frame.is_some())
                .then_some(caption)
                .flatten()
                .map(|caption| {
                    div()
                        .max_w(metrics.reading_width())
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().foreground)
                        .child(caption)
                }),
        )
}

fn step_button<F: Fn(&mut App) + 'static>(
    id: &'static str,
    icon: IconName,
    tooltip: &'static str,
    enabled: bool,
    on_click: F,
) -> impl IntoElement + use<F> {
    Button::new(id)
        .icon(Icon::new(icon))
        .ghost()
        .large()
        .tooltip(tooltip)
        .disabled(!enabled)
        .on_click(move |_, _window, cx| on_click(cx))
}
