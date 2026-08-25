//! One conversation in the sidebar list.
//!
//! A row is a surface, not a command: it selects a conversation rather than
//! running an action, so it stays a clickable `div` and the selection itself
//! is what carries the state. Everything it draws is decided in
//! [`crate::app::ChatRow`]; this file only lays it out.

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Entity, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, div,
};
use gpui_component::ActiveTheme as _;
use gpui_component::Icon;

use crate::app::{ChatOpen, ChatRow, Preview, PreviewGlyph, Unread, WhatsAppApp};
use crate::components::{ProductIcon, status_ticks};
use crate::responsive::ResponsiveLayout;
use crate::theme::ActiveProductTheme as _;
use crate::utils::format_list_time;

use super::Avatar;

pub fn render_chat_item(
    row: ChatRow,
    is_selected: bool,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
    // `use<>`: the element reads colours out of the theme but retains nothing
    // borrowed from `cx`, so it must not inherit its lifetime — the virtual
    // list builds rows inside a closure holding `&mut Context`.
) -> impl IntoElement + use<> {
    let metrics = *layout.metrics();
    let jid = row.jid.clone();
    let name: SharedString = row.name.clone().into();
    let has_unread = row.has_unread();

    // The row's own ground, which the avatar's badge rings itself in.
    let ground = if is_selected {
        cx.theme().list_active
    } else {
        cx.theme().sidebar
    };

    div()
        .id(SharedString::from(format!("chat-{jid}")))
        .w_full()
        .h(layout.chat_item_height())
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .px(metrics.chat_row_padding_x())
        .rounded(metrics.radius_lg())
        .relative()
        .cursor_pointer()
        .when(is_selected, |el| el.bg(cx.theme().list_active))
        .when(!is_selected, |el| {
            let hover = cx.theme().list_hover;
            el.hover(move |s| s.bg(hover))
        })
        // Selection is a bar as well as a fill. A fill alone is a small
        // lightness step on a dark palette, and it disappears entirely next
        // to a hovered neighbour.
        .when(is_selected, |el| {
            el.child(
                div()
                    .absolute()
                    .left_0()
                    .top(metrics.space_lg())
                    .bottom(metrics.space_lg())
                    .w(metrics.selection_bar_width())
                    .rounded_r(metrics.selection_bar_width())
                    .bg(cx.theme().primary),
            )
        })
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| {
                this.select_chat(jid.clone(), ChatOpen::ToCompose, window, cx)
            });
        })
        .child(
            Avatar::new(row.jid.clone(), &row.name, layout.avatar_size())
                .group(row.is_group)
                .on(ground),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(metrics.space_xs())
                .overflow_hidden()
                .child(render_name_row(&row, name, has_unread, metrics, cx))
                .child(render_preview_row(&row, has_unread, metrics, cx)),
        )
}

fn render_name_row(
    row: &ChatRow,
    name: SharedString,
    has_unread: bool,
    metrics: crate::theme::Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_baseline()
        .gap(metrics.space_md())
        .min_w_0()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(metrics.text_body())
                .text_color(cx.theme().foreground)
                .font_weight(gpui::FontWeight::MEDIUM)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(name),
        )
        .children(row.timestamp.map(|timestamp| {
            div()
                .flex_shrink_0()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_meta())
                // The time joins the badge in saying "something is waiting
                // here", so the two agree rather than competing.
                .text_color(if has_unread {
                    cx.theme().primary
                } else {
                    cx.product().hsla(cx.product().palette.subtle_foreground)
                })
                .child(format_list_time(&timestamp))
        }))
}

fn render_preview_row(
    row: &ChatRow,
    has_unread: bool,
    metrics: crate::theme::Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_center()
        .gap(metrics.space_sm())
        .min_w_0()
        .child(render_preview(&row.preview, row.is_group, metrics, cx))
        .children(render_unread(&row.unread, has_unread, metrics, cx))
}

fn render_preview(
    preview: &Preview,
    is_group: bool,
    metrics: crate::theme::Metrics,
    cx: &App,
) -> AnyElement {
    let product = cx.product();
    let line = || {
        div()
            .flex()
            .items_center()
            .gap(metrics.space_sm())
            .flex_1()
            .min_w_0()
            .text_size(metrics.text_secondary())
    };
    let text = |content: String| {
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(content)
    };

    match preview {
        Preview::Empty => line()
            .text_color(product.hsla(product.palette.subtle_foreground))
            .italic()
            .child("No messages")
            .into_any_element(),

        // Typing is the one preview that is happening right now, so it takes
        // the accent colour the rest of the row spends on unread state.
        Preview::Typing(summary) => line()
            .text_color(cx.theme().primary)
            .child(text(summary.compact_label(is_group)))
            .into_any_element(),

        Preview::Draft(draft) => line()
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(cx.theme().warning)
                    .child("Draft:"),
            )
            .text_color(cx.theme().muted_foreground)
            .child(text(draft.clone()))
            .into_any_element(),

        Preview::Message {
            prefix,
            glyph,
            text: body,
            status,
        } => line()
            .text_color(cx.theme().muted_foreground)
            .children(status.map(|status| status_ticks(status, metrics.icon_small(), cx)))
            .children(prefix.as_ref().map(|prefix| {
                div()
                    .flex_shrink_0()
                    .text_color(product.hsla(product.palette.subtle_foreground))
                    .child(format!("{prefix}:"))
            }))
            .children(glyph.map(|glyph| {
                Icon::new(icon_for(glyph))
                    .size(metrics.icon_small())
                    .flex_shrink_0()
                    .text_color(product.hsla(product.palette.subtle_foreground))
            }))
            .child(text(body.clone()))
            .into_any_element(),
    }
}

fn render_unread(
    unread: &Unread,
    has_unread: bool,
    metrics: crate::theme::Metrics,
    cx: &App,
) -> Option<AnyElement> {
    if !has_unread {
        return None;
    }
    let badge = div()
        .flex_shrink_0()
        .rounded_full()
        .bg(cx.theme().primary)
        .flex()
        .items_center()
        .justify_center();

    Some(match unread {
        Unread::Count(count) => badge
            .min_w(metrics.space_xxl())
            .h(metrics.space_xxl())
            .px(metrics.space_sm())
            .font_family(cx.theme().mono_font_family.clone())
            .text_size(metrics.text_meta())
            .text_color(cx.theme().primary_foreground)
            .child(format_count(*count))
            .into_any_element(),
        // Marked unread by hand has no number. A pill containing a bullet
        // pretends to be a count of something; a dot does not.
        Unread::Marked => badge.size(metrics.space_md()).into_any_element(),
        Unread::None => return None,
    })
}

/// Cap the badge so a long-neglected group cannot widen the row.
fn format_count(count: u32) -> String {
    if count > 999 {
        "999+".to_string()
    } else {
        count.to_string()
    }
}

fn icon_for(glyph: PreviewGlyph) -> ProductIcon {
    match glyph {
        PreviewGlyph::Image => ProductIcon::Image,
        PreviewGlyph::Video => ProductIcon::Film,
        PreviewGlyph::Audio => ProductIcon::Mic,
        PreviewGlyph::Document => ProductIcon::FileText,
        PreviewGlyph::Sticker => ProductIcon::Sticker,
    }
}

#[cfg(test)]
mod tests {
    use super::format_count;

    #[test]
    fn a_neglected_group_cannot_widen_the_row() {
        assert_eq!(format_count(65), "65");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(4_312), "999+");
    }
}
