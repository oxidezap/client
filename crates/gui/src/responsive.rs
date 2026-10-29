use gpui::{Pixels, Size};

use crate::theme::Metrics;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breakpoint {
    Mobile,
    Tablet,
    Desktop,
}

impl Breakpoint {
    /// Which layout a window this wide can carry.
    ///
    /// The thresholds come from [`Metrics`] rather than from device pixels,
    /// because "is there room for two panes" is a question about the content:
    /// the same 700px window holds two at the reference base and one at
    /// double it. That also means the viewport fit moves them — a handheld
    /// that shrank the design to fit is a window with proportionally *more*
    /// room, not less, and a fixed threshold would have denied it the layout
    /// its own scale had just made room for.
    pub fn from_width(width: Pixels, metrics: &Metrics) -> Self {
        if width < metrics.breakpoint_mobile() {
            Self::Mobile
        } else if width < metrics.breakpoint_tablet() {
            Self::Tablet
        } else {
            Self::Desktop
        }
    }

    pub fn is_mobile(&self) -> bool {
        matches!(self, Self::Mobile)
    }

    #[expect(dead_code, reason = "the vocabulary is the point; see the note above")]
    pub fn is_tablet(&self) -> bool {
        matches!(self, Self::Tablet)
    }

    #[expect(dead_code, reason = "the vocabulary is the point; see the note above")]
    pub fn is_desktop(&self) -> bool {
        matches!(self, Self::Desktop)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MobilePanel {
    #[default]
    ChatList,
    Chat,
}

impl MobilePanel {
    pub fn is_chat_list(&self) -> bool {
        matches!(self, Self::ChatList)
    }

    pub fn is_chat(&self) -> bool {
        matches!(self, Self::Chat)
    }
}

/// The viewport facts and the resolved design scale, together.
///
/// Render helpers are already threaded this value, so carrying [`Metrics`]
/// here is what lets a component read a density- and zoom-aware dimension
/// without every signature growing a second parameter. The two stay separately
/// *defined*: this type answers "how wide is the window", `Metrics` answers
/// "how big is a row".
#[derive(Debug, Clone, Copy)]
pub struct ResponsiveLayout {
    breakpoint: Breakpoint,
    mobile_panel: MobilePanel,
    /// The whole window, not just its width: anything positioned against the
    /// window's edges — a card floating over the conversation — needs both.
    viewport: Size<Pixels>,
    metrics: Metrics,
}

impl ResponsiveLayout {
    /// How much of a phone's width one bubble, or one picture, may take.
    ///
    /// The two ratios are the only dimensions here that are a share of the
    /// window rather than a size: where the conversation *is* the window,
    /// "not the full width" is the whole requirement, and a fixed number
    /// would either crowd a wide phone or overflow a narrow one.
    const MAX_BUBBLE_WIDTH_MOBILE_RATIO: f32 = 0.85;
    const MAX_MEDIA_SIZE_MOBILE_RATIO: f32 = 0.75;

    pub fn new(viewport: Size<Pixels>, mobile_panel: MobilePanel, metrics: Metrics) -> Self {
        Self {
            breakpoint: Breakpoint::from_width(viewport.width, &metrics),
            mobile_panel,
            viewport,
            metrics,
        }
    }

    /// The window this layout describes.
    pub fn viewport(&self) -> Size<Pixels> {
        self.viewport
    }

    /// The active design scale: spacing, radii, type steps and control frames.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    #[expect(
        dead_code,
        reason = "one vocabulary for the layout, whether or not every word is spoken today"
    )]
    pub fn breakpoint(&self) -> Breakpoint {
        self.breakpoint
    }

    pub fn is_mobile(&self) -> bool {
        self.breakpoint.is_mobile()
    }

    #[expect(
        dead_code,
        reason = "one vocabulary for the layout, whether or not every word is spoken today"
    )]
    pub fn is_tablet(&self) -> bool {
        self.breakpoint.is_tablet()
    }

    #[expect(
        dead_code,
        reason = "one vocabulary for the layout, whether or not every word is spoken today"
    )]
    pub fn is_desktop(&self) -> bool {
        self.breakpoint.is_desktop()
    }

    #[expect(
        dead_code,
        reason = "one vocabulary for the layout, whether or not every word is spoken today"
    )]
    pub fn is_compact(&self) -> bool {
        self.is_mobile() || self.is_tablet()
    }

    #[expect(
        dead_code,
        reason = "one vocabulary for the layout, whether or not every word is spoken today"
    )]
    pub fn mobile_panel(&self) -> MobilePanel {
        self.mobile_panel
    }

    pub fn show_sidebar(&self) -> bool {
        match self.breakpoint {
            Breakpoint::Desktop | Breakpoint::Tablet => true,
            Breakpoint::Mobile => self.mobile_panel.is_chat_list(),
        }
    }

    pub fn show_chat_area(&self) -> bool {
        match self.breakpoint {
            Breakpoint::Desktop | Breakpoint::Tablet => true,
            Breakpoint::Mobile => self.mobile_panel.is_chat(),
        }
    }

    pub fn show_back_button(&self) -> bool {
        self.is_mobile() && self.mobile_panel.is_chat()
    }

    /// Whether the header has room to show its actions as buttons.
    ///
    /// Below this they are still reachable — the overflow menu carries every
    /// one of them — so this decides presentation, never availability.
    pub fn show_call_buttons(&self) -> bool {
        self.viewport.width >= self.metrics.breakpoint_header_actions()
    }

    pub fn sidebar_width(&self) -> Pixels {
        match self.breakpoint {
            Breakpoint::Desktop => self.metrics.sidebar_width(),
            Breakpoint::Tablet => (self.viewport.width * 0.35).clamp(
                self.metrics.sidebar_width_min(),
                self.metrics.sidebar_width_compact(),
            ),
            Breakpoint::Mobile => self.viewport.width,
        }
    }

    /// The conversation header. Taller than the sidebar's own header because
    /// it carries a subtitle — presence, member count, who is typing.
    pub fn header_height(&self) -> Pixels {
        if self.is_mobile() {
            self.metrics.mobile_header_height()
        } else {
            self.metrics.header_height()
        }
    }

    pub fn chat_item_height(&self) -> Pixels {
        self.metrics.chat_row_height()
    }

    pub fn avatar_size(&self) -> Pixels {
        self.metrics.avatar_row()
    }

    /// The composer's height.
    ///
    /// `InputAreaView` reads this rather than a constant of its own. The two
    /// used to disagree by 6px on mobile, and the composer drew itself into a
    /// shorter slot than it claimed.
    pub fn input_area_height(&self) -> Pixels {
        if self.is_mobile() {
            self.metrics.composer_height_mobile()
        } else {
            self.metrics.composer_height()
        }
    }

    pub fn max_bubble_width(&self) -> Pixels {
        match self.breakpoint {
            Breakpoint::Desktop => self.metrics.bubble_max_width(),
            Breakpoint::Tablet => self.metrics.bubble_max_width_compact(),
            Breakpoint::Mobile => (self.viewport.width * Self::MAX_BUBBLE_WIDTH_MOBILE_RATIO)
                .min(self.metrics.bubble_max_width_phone()),
        }
    }

    pub fn max_media_size(&self) -> f32 {
        f32::from(match self.breakpoint {
            Breakpoint::Desktop => self.metrics.media_max_size(),
            Breakpoint::Tablet => self.metrics.media_max_size_compact(),
            Breakpoint::Mobile => (self.viewport.width * Self::MAX_MEDIA_SIZE_MOBILE_RATIO)
                .min(self.metrics.media_max_size_compact()),
        })
    }

    /// A fixed sticker canvas, capped to the space beside the hover actions.
    /// Reserve the largest action strip so captions do not resize stickers.
    pub fn sticker_size(&self) -> f32 {
        let metrics = &self.metrics;
        let actions = metrics.icon_button() * 3.0 + metrics.space_xxs() * 2.0;
        // The chat-area estimate includes the navigation rail on two-pane
        // layouts; it is outside the rendered conversation's available width.
        let rail = if self.is_mobile() {
            0.0
        } else {
            f32::from(metrics.icon_button() + metrics.space_lg())
        };
        let available = self.chat_area_width()
            - rail
            - f32::from(self.conversation_padding() * 2.0 + actions + metrics.space_md());
        let content = available.min(f32::from(self.max_bubble_width()))
            - f32::from(metrics.bubble_padding_x() * 2.0);
        f32::from(metrics.sticker_size()).min(content.max(0.0))
    }

    pub fn chat_area_width(&self) -> f32 {
        f32::from(match self.breakpoint {
            Breakpoint::Desktop | Breakpoint::Tablet => self.viewport.width - self.sidebar_width(),
            Breakpoint::Mobile => self.viewport.width,
        })
    }

    pub fn message_list_width(&self) -> f32 {
        self.chat_area_width() - f32::from(self.padding()) * 2.0 - f32::from(self.gap())
    }

    /// The smallest comfortable pointer target. Touch needs more room than a
    /// mouse, so this is the floor every action in a header or composer is
    /// sized against.
    pub fn min_touch_target(&self) -> Pixels {
        if self.is_mobile() {
            self.metrics.touch_target()
        } else {
            self.metrics.icon_button()
        }
    }

    pub fn icon_button_size(&self) -> Pixels {
        if self.is_mobile() {
            self.metrics.touch_target()
        } else {
            self.metrics.icon_button()
        }
    }

    pub fn padding(&self) -> Pixels {
        if self.is_mobile() {
            self.metrics.space_lg()
        } else {
            self.metrics.space_xl()
        }
    }

    /// The conversation's own gutter, which is wider than a panel's padding.
    ///
    /// A bubble is a shape with an edge, and an edge sitting 16px from the
    /// window reads as clipped rather than placed. The timeline gets room on
    /// both sides for the same reason it gets room above the composer: this
    /// is the surface the reader spends their time in.
    pub fn conversation_padding(&self) -> Pixels {
        if self.is_mobile() {
            self.metrics.space_lg()
        } else {
            self.metrics.space_xxxl()
        }
    }

    /// Breathing room at the head and foot of the timeline.
    ///
    /// Without it the newest message sits on the composer and the oldest
    /// under the header, which is what made the whole pane read as crowded.
    pub fn conversation_gap(&self) -> Pixels {
        self.metrics.space_xl()
    }

    #[expect(
        dead_code,
        reason = "one vocabulary for the layout, whether or not every word is spoken today"
    )]
    pub fn padding_small(&self) -> Pixels {
        if self.is_mobile() {
            self.metrics.space_md()
        } else {
            self.metrics.space_lg()
        }
    }

    pub fn gap(&self) -> Pixels {
        if self.is_mobile() {
            self.metrics.space_md()
        } else {
            self.metrics.space_lg()
        }
    }
}

#[cfg(test)]
mod sticker_tests {
    use super::*;
    use crate::theme::metrics::Density;
    use gpui::{px, size};

    #[test]
    fn sticker_canvas_follows_zoom_and_fits_narrow_conversations() {
        for rem in [10.0, 16.0, 24.0, 32.0] {
            for density in Density::ALL {
                let metrics = Metrics::new(rem, density);
                let wide =
                    ResponsiveLayout::new(size(px(2400.0), px(1600.0)), MobilePanel::Chat, metrics);
                assert_eq!(wide.sticker_size(), 300.0 * rem / 16.0);
                for width in [280.0, 400.0, 600.0, 700.0, 900.0, 1200.0] {
                    let layout = ResponsiveLayout::new(
                        size(px(width), px(800.0)),
                        MobilePanel::Chat,
                        metrics,
                    );
                    let canvas = layout.sticker_size();
                    assert!(canvas.is_finite() && canvas >= 0.0);
                    assert!(canvas <= f32::from(metrics.sticker_size()));
                    assert!(canvas <= f32::from(layout.max_bubble_width()));
                    if canvas > 0.0 {
                        let rail = if layout.is_mobile() {
                            0.0
                        } else {
                            f32::from(metrics.icon_button() + metrics.space_lg())
                        };
                        assert!(
                            canvas
                                + f32::from(
                                    metrics.bubble_padding_x() * 2.0
                                        + layout.conversation_padding() * 2.0
                                        + metrics.icon_button() * 3.0
                                        + metrics.space_xxs() * 2.0
                                        + metrics.space_md(),
                                )
                                <= layout.chat_area_width() - rail + 0.001
                        );
                    }
                }
            }
        }
    }
}
