//! The shape every "there is nothing here" screen takes.
//!
//! An empty state that only says what is missing leaves the reader where they
//! started. Each one here has a title naming the condition, a line explaining
//! it, and — wherever there is one — the action that resolves it. Keeping that
//! in one component is what stops the seven of them in this app from each
//! inventing a slightly different answer.

use std::rc::Rc;

use gpui::{
    App, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::Icon;
use gpui_component::button::Button;

use crate::theme::ActiveProductTheme as _;

type OnAction = Rc<dyn Fn(&mut Window, &mut App)>;

/// A keyboard route to the same place the action goes.
pub struct Shortcut {
    pub keys: SharedString,
    pub description: SharedString,
}

#[derive(IntoElement)]
pub struct EmptyState {
    title: SharedString,
    description: Option<SharedString>,
    icon: Option<Icon>,
    action: Option<(SharedString, OnAction)>,
    shortcuts: Vec<Shortcut>,
    /// Tightened for a sidebar or a small pane, where the full breathing room
    /// would push the text out of view.
    compact: bool,
}

impl EmptyState {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            description: None,
            icon: None,
            action: None,
            shortcuts: Vec::new(),
            compact: false,
        }
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// The one action that resolves the condition.
    pub fn action(
        mut self,
        label: impl Into<SharedString>,
        on_action: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.action = Some((label.into(), Rc::new(on_action)));
        self
    }

    pub fn shortcut(
        mut self,
        keys: impl Into<SharedString>,
        description: impl Into<SharedString>,
    ) -> Self {
        self.shortcuts.push(Shortcut {
            keys: keys.into(),
            description: description.into(),
        });
        self
    }

    pub fn compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let metrics = cx.product().metrics;
        let product = cx.product();
        let subtle = product.hsla(product.palette.subtle_foreground);
        let icon_frame = if self.compact {
            metrics.avatar_header()
        } else {
            metrics.avatar_call()
        };

        div()
            .flex()
            .flex_col()
            .items_center()
            .text_center()
            .max_w(metrics.call_card_width())
            .gap(if self.compact {
                metrics.space_lg()
            } else {
                metrics.space_xl()
            })
            .children(self.icon.map(|icon| {
                div()
                    .size(icon_frame)
                    .rounded_full()
                    .bg(cx.theme().secondary)
                    .border_1()
                    .border_color(cx.theme().border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        icon.size(icon_frame * 0.42)
                            .text_color(cx.theme().muted_foreground),
                    )
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    // Title and description are one group, so they sit closer
                    // to each other than to anything else on the screen.
                    .gap(metrics.space_md())
                    .child(
                        div()
                            .text_size(if self.compact {
                                metrics.text_strong()
                            } else {
                                metrics.text_heading()
                            })
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground)
                            .child(self.title),
                    )
                    .children(self.description.map(|description| {
                        div()
                            .text_size(metrics.text_secondary())
                            .text_color(cx.theme().muted_foreground)
                            .child(description)
                    })),
            )
            .children(self.action.map(|(label, on_action)| {
                Button::new("empty-state-action")
                    .label(label)
                    .outline()
                    .on_click(move |_, window, cx| on_action(window, cx))
            }))
            .when(!self.shortcuts.is_empty(), |el| {
                el.child(div().flex().flex_col().gap(metrics.space_md()).children(
                    self.shortcuts.into_iter().map(|shortcut| {
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(metrics.space_md())
                            .text_size(metrics.text_small())
                            .text_color(subtle)
                            .child(
                                div()
                                    .px(metrics.space_sm())
                                    .rounded(metrics.radius_sm())
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_size(metrics.text_meta())
                                    .child(shortcut.keys),
                            )
                            .child(shortcut.description)
                    }),
                ))
            })
    }
}
