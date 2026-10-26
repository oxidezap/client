//! Display-only choices: rendering never performs a business action.
use crate::components::{BubbleText, render_rich_text};
use crate::theme::Metrics;
use gpui::{
    App, InteractiveElement, IntoElement, ParentElement, SharedString, Styled, div,
    prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use oxidezap_core::BusinessContent;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct BusinessText {
    header: BubbleText,
    footer: BubbleText,
    header_key: Arc<str>,
    footer_key: Arc<str>,
    header_id: SharedString,
    footer_id: SharedString,
    button_ids: Arc<[SharedString]>,
}
impl BusinessText {
    pub(crate) fn new(id: &str, content: &BusinessContent) -> Self {
        Self {
            header: BubbleText::of(&content.header),
            footer: BubbleText::of(&content.footer),
            header_key: format!("business-header-{id}").into(),
            footer_key: format!("business-footer-{id}").into(),
            header_id: format!("business-header-{id}").into(),
            footer_id: format!("business-footer-{id}").into(),
            button_ids: content
                .buttons
                .iter()
                .enumerate()
                .map(|(ix, _)| format!("business-button-{id}-{ix}").into())
                .collect(),
        }
    }
    pub(crate) fn link_targets(&self) -> impl Iterator<Item = &SharedString> {
        self.header
            .link_targets()
            .iter()
            .chain(self.footer.link_targets().iter())
    }
}

pub(super) fn render_header(
    parsed: Option<&BusinessText>,
    order: u64,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let id = parsed
        .map(|p| p.header_id.clone())
        .unwrap_or_else(|| "business-header-unavailable".into());
    let selector = id.clone();
    div()
        .id(id)
        .debug_selector(move || selector.to_string())
        .min_w_0()
        .text_size(metrics.text_body())
        .text_color(cx.theme().foreground)
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .when_some(parsed, |el, parsed| {
            el.when(!parsed.header.is_empty(), |el| {
                el.child(render_rich_text(
                    &parsed.header,
                    Arc::clone(&parsed.header_key),
                    order.saturating_mul(3),
                    cx,
                ))
            })
        })
}

pub(super) fn render_footer(
    content: &BusinessContent,
    parsed: Option<&BusinessText>,
    order: u64,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let id = parsed
        .map(|p| p.footer_id.clone())
        .unwrap_or_else(|| "business-footer-unavailable".into());
    let selector = id.clone();
    div()
        .id(id)
        .debug_selector(move || selector.to_string())
        .flex()
        .flex_col()
        .min_w_0()
        .gap(metrics.space_xs())
        .text_size(metrics.text_small())
        .text_color(cx.theme().muted_foreground)
        .when_some(parsed, |el, parsed| {
            el.when(!parsed.footer.is_empty(), |el| {
                el.child(render_rich_text(
                    &parsed.footer,
                    Arc::clone(&parsed.footer_key),
                    order.saturating_mul(3).saturating_add(2),
                    cx,
                ))
            })
        })
        .children(content.buttons.iter().enumerate().map(|(ix, button)| {
            let id = parsed
                .and_then(|p| p.button_ids.get(ix))
                .cloned()
                .unwrap_or_else(|| format!("business-choice-{ix}").into());
            let selector = id.clone();
            div()
                .id(id)
                .debug_selector(move || selector.to_string())
                .flex()
                .flex_col()
                .min_w_0()
                .p(metrics.space_xs())
                .rounded(metrics.radius_sm())
                .border_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .text_color(cx.theme().foreground)
                        .child(button.label.clone()),
                )
                // Visible explanations remain readable without hover or network.
                .child(button.action.unavailable_reason())
        }))
        .children(content.notice.clone())
}
