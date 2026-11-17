//! Unicode emoji stay text, including selectors, modifiers and joiners.
//! Only visible rows are built; filtering never runs on a paint pass.

use emojis::{Emoji, SkinTone};
use gpui::{
    Entity, EventEmitter, Focusable as _, UniformListScrollHandle, div, prelude::*, uniform_list,
};
use gpui_component::{
    ActiveTheme as _, Selectable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
};

use crate::theme::ActiveProductTheme as _;

/// What the grid starts at, before the popover that holds it says how much
/// room there is. See [`EmojiPicker::fit`].
const COLUMNS: usize = 10;
const VISIBLE_ROWS: f32 = 7.;
const TONES: [SkinTone; 6] = [
    SkinTone::Default,
    SkinTone::Light,
    SkinTone::MediumLight,
    SkinTone::Medium,
    SkinTone::MediumDark,
    SkinTone::Dark,
];

pub(super) struct EmojiPicked(pub &'static str);

pub(super) struct EmojiPicker {
    columns: usize,
    rows: f32,
    /// The side of one emoji cell, when the popover has chosen one.
    cell: Option<gpui::Pixels>,
    search: Entity<InputState>,
    matches: Vec<&'static Emoji>,
    tone: SkinTone,
    scroll: UniformListScrollHandle,
    language: crate::i18n::Language,
}

impl EventEmitter<EmojiPicked> for EmojiPicker {}

fn search(query: &str, tone: SkinTone) -> Vec<&'static Emoji> {
    let query = query.trim().trim_matches(':').to_lowercase();
    emojis::iter()
        .filter(|emoji| emoji.emoji_version() <= crate::emoji::VERSION)
        .filter(|emoji| {
            query.is_empty()
                || emoji.as_str() == query
                || emoji
                    .with_skin_tone(tone)
                    .is_some_and(|e| e.as_str() == query)
                || query.split_whitespace().all(|word| {
                    emoji.name().to_lowercase().contains(word)
                        || emoji.shortcodes().any(|code| code.contains(word))
                })
        })
        .map(|emoji| emoji.with_skin_tone(tone).unwrap_or(emoji))
        .collect()
}

impl EmojiPicker {
    pub fn new(window: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(crate::tr!(SearchEmoji)));
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => this.filter(cx),
            InputEvent::PressEnter { .. } => {
                if let Some(emoji) = this.matches.first() {
                    cx.emit(EmojiPicked(emoji.as_str()));
                }
            }
            _ => {}
        })
        .detach();
        Self {
            columns: COLUMNS,
            rows: VISIBLE_ROWS,
            cell: None,
            search: input,
            matches: search("", SkinTone::Default),
            tone: SkinTone::Default,
            scroll: UniformListScrollHandle::new(),
            language: crate::i18n::Language::current(),
        }
    }

    /// Take the room the popover has: as many columns and rows as fit, so the
    /// picker is as large as the window allows rather than one fixed size.
    ///
    /// Reports whether anything changed, because a view is drawn from its own
    /// cache until it is told it is dirty: a size set without a notify left
    /// the grid drawn at the old one, with only the gaps between cells new.
    pub fn fit(&mut self, columns: usize, rows: f32, cell: gpui::Pixels) -> bool {
        let (columns, rows) = (columns.max(1), rows.max(3.0));
        let changed = self.columns != columns || self.rows != rows || self.cell != Some(cell);
        self.columns = columns;
        self.rows = rows;
        self.cell = Some(cell);
        changed
    }

    pub fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        self.search.read(cx).focus_handle(cx)
    }

    fn filter(&mut self, cx: &mut gpui::Context<Self>) {
        self.matches = search(&self.search.read(cx).text().to_string(), self.tone);
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        cx.notify();
    }
}

impl Render for EmojiPicker {
    fn render(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        let language = crate::i18n::Language::current();
        if self.language != language {
            self.language = language;
            self.search.update(cx, |input, cx| {
                input.set_placeholder(crate::tr!(SearchEmoji), window, cx)
            });
        }
        let metrics = cx.product().metrics;
        let small = metrics.icon_button();
        let cell = self.cell.unwrap_or(small);
        let entity = cx.entity();
        div()
            .flex()
            .flex_col()
            .gap(metrics.space_sm())
            .w(cell * self.columns as f32)
            .text_color(cx.theme().popover_foreground)
            .child(crate::tr!(ChooseEmoji))
            .child(Input::new(&self.search).w_full())
            .child(crate::tr!(EmojiSkinTone))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(metrics.space_xs())
                    .children(TONES.into_iter().enumerate().map(|(ix, tone)| {
                        let emoji = emojis::get("👋").unwrap().with_skin_tone(tone).unwrap();
                        Button::new(("emoji-tone", ix))
                            .ghost()
                            .p_0()
                            .w(small)
                            .h(small)
                            .selected(self.tone == tone)
                            .child(crate::emoji::picture(
                                emoji.as_str(),
                                small * 0.7,
                                window,
                                cx,
                            ))
                            .accessibility_label(emoji.name())
                            .tooltip(emoji.name())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.tone = tone;
                                this.filter(cx);
                            }))
                    })),
            )
            .when(self.matches.is_empty(), |panel| {
                panel.child(crate::tr!(NoEmojiFound))
            })
            .child(
                uniform_list(
                    "emoji-grid",
                    self.matches.len().div_ceil(self.columns),
                    cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                        range
                            .map(|row| {
                                div().flex().h(cell).children(
                                    this.matches
                                        .iter()
                                        .skip(row * this.columns)
                                        .take(this.columns)
                                        .map(|emoji| {
                                            let emoji = *emoji;
                                            let entity = entity.clone();
                                            Button::new(gpui::SharedString::from(emoji.as_str()))
                                                .ghost()
                                                .p_0()
                                                .w(cell)
                                                .h(cell)
                                                .child(crate::emoji::picture(
                                                    emoji.as_str(),
                                                    cell * 0.7,
                                                    window,
                                                    cx,
                                                ))
                                                .accessibility_label(emoji.name())
                                                .tooltip(emoji.name())
                                                .on_click(move |_, _, cx| {
                                                    entity.update(cx, |_, cx| {
                                                        cx.emit(EmojiPicked(emoji.as_str()))
                                                    });
                                                })
                                        }),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h(cell * self.rows)
                .track_scroll(&self.scroll),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real renderer probe, separate from CI machines without a GPU adapter.
    #[test]
    #[ignore = "requires a WGPU adapter; exports the actual picker when requested"]
    fn picker_artwork_render_probe() {
        use gpui::{AppContext as _, px, size};
        use std::sync::Arc;
        let mut cx = gpui::HeadlessAppContext::with_platform(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("DejaVu Sans")),
            Arc::new(crate::assets::Assets),
            || Ok(Some(Box::new(gpui_wgpu::WgpuHeadlessRenderer::new()?))),
        );
        cx.update(|cx| {
            crate::platform::fonts::fonts(cx);
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let window = cx
            .open_window(size(px(400.), px(460.)), |window, cx| {
                cx.new(|cx| EmojiPicker::new(window, cx))
            })
            .unwrap();
        for _ in 0..4 {
            cx.run_until_parked();
            cx.update_window(*window, |_, window, cx| {
                window.refresh();
                cx.refresh_windows();
            })
            .unwrap();
            let _ = cx.capture_screenshot(*window).unwrap();
        }
        let screenshot = cx.capture_screenshot(*window).unwrap();
        let yellow = screenshot
            .pixels()
            .filter(|p| p[0] > 180 && p[1] > 100 && p[1] < 235 && p[2] < 100 && p[3] > 200)
            .count();
        assert!(
            yellow > 1000,
            "picker did not paint its yellow PNG artwork: {yellow}"
        );
        if let Some(path) = std::env::var_os("OXIDEZAP_PICKER_PREVIEW") {
            screenshot.save(path).unwrap();
        }
    }

    /// The same renderer, with the popover's sizing applied: the glyphs have
    /// to grow with the cell, not only the gaps between them.
    #[test]
    #[ignore = "requires a WGPU adapter; exports the sized picker when requested"]
    fn sized_picker_artwork_probe() {
        use gpui::{AppContext as _, px, size};
        use std::sync::Arc;
        let mut cx = gpui::HeadlessAppContext::with_platform(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("DejaVu Sans")),
            Arc::new(crate::assets::Assets),
            || Ok(Some(Box::new(gpui_wgpu::WgpuHeadlessRenderer::new()?))),
        );
        cx.update(|cx| {
            crate::platform::fonts::fonts(cx);
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let window = cx
            .open_window(size(px(800.), px(700.)), |window, cx| {
                cx.new(|cx| {
                    let mut picker = EmojiPicker::new(window, cx);
                    let _ = picker.fit(11, 6.0, px(70.));
                    picker
                })
            })
            .unwrap();
        for _ in 0..5 {
            cx.run_until_parked();
            cx.update_window(*window, |_, window, cx| {
                window.refresh();
                cx.refresh_windows();
            })
            .unwrap();
            let _ = cx.capture_screenshot(*window).unwrap();
        }
        let screenshot = cx.capture_screenshot(*window).unwrap();
        if let Some(path) = std::env::var_os("OXIDEZAP_PICKER_PREVIEW") {
            screenshot.save(path).unwrap();
        }
    }

    #[test]
    fn search_preserves_sequences_and_supports_names_shortcodes_and_tones() {
        assert!(
            search(":ROCKET:", SkinTone::Default)
                .iter()
                .any(|e| e.as_str() == "🚀")
        );
        assert!(
            search("woman technologist", SkinTone::Medium)
                .iter()
                .any(|e| e.as_str() == "👩🏽‍💻")
        );
        assert!(
            search("flag brazil", SkinTone::Default)
                .iter()
                .any(|e| e.as_str() == "🇧🇷")
        );
        assert!(
            search("👨‍👩‍👧‍👦", SkinTone::Default)
                .iter()
                .any(|e| e.as_str() == "👨‍👩‍👧‍👦")
        );
        assert!(search("no_such_emoji_xyz", SkinTone::Default).is_empty());
    }
}
