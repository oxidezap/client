use gpui::{Pixels, Size, px};

use crate::theme::Metrics;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breakpoint {
    Mobile,
    Tablet,
    Desktop,
}

impl Breakpoint {
    pub const MOBILE_MAX: f32 = 600.0;
    pub const TABLET_MAX: f32 = 900.0;

    pub fn from_width(width: f32) -> Self {
        if width < Self::MOBILE_MAX {
            Self::Mobile
        } else if width < Self::TABLET_MAX {
            Self::Tablet
        } else {
            Self::Desktop
        }
    }

    pub fn is_mobile(&self) -> bool {
        matches!(self, Self::Mobile)
    }

    pub fn is_tablet(&self) -> bool {
        matches!(self, Self::Tablet)
    }

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
    viewport_width: f32,
    metrics: Metrics,
}

impl ResponsiveLayout {
    // Widths the design fixes in device pixels because they answer "how much
    // window does this pane get", not "how big is a control". They are the
    // viewport's own geometry, so unlike the design scale they do not follow
    // the base font.
    const SIDEBAR_WIDTH_DESKTOP: f32 = 340.0;
    const SIDEBAR_WIDTH_TABLET: f32 = 280.0;
    const SIDEBAR_WIDTH_MIN: f32 = 240.0;

    const MAX_BUBBLE_WIDTH_DESKTOP: f32 = 520.0;
    const MAX_BUBBLE_WIDTH_TABLET: f32 = 420.0;
    const MAX_BUBBLE_WIDTH_MOBILE_RATIO: f32 = 0.85;

    const MAX_MEDIA_SIZE_DESKTOP: f32 = 300.0;
    const MAX_MEDIA_SIZE_TABLET: f32 = 280.0;
    const MAX_MEDIA_SIZE_MOBILE_RATIO: f32 = 0.75;

    /// Below this the header has no room for its action row, so the actions
    /// move into the overflow menu rather than disappearing.
    const CALL_BUTTON_MIN_WIDTH: f32 = 400.0;

    pub fn new(viewport: Size<Pixels>, mobile_panel: MobilePanel, metrics: Metrics) -> Self {
        let width: f32 = viewport.width.into();

        Self {
            breakpoint: Breakpoint::from_width(width),
            mobile_panel,
            viewport_width: width,
            metrics,
        }
    }

    /// The active design scale: spacing, radii, type steps and control frames.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn breakpoint(&self) -> Breakpoint {
        self.breakpoint
    }

    pub fn is_mobile(&self) -> bool {
        self.breakpoint.is_mobile()
    }

    pub fn is_tablet(&self) -> bool {
        self.breakpoint.is_tablet()
    }

    pub fn is_desktop(&self) -> bool {
        self.breakpoint.is_desktop()
    }

    pub fn is_compact(&self) -> bool {
        self.is_mobile() || self.is_tablet()
    }

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
        self.viewport_width >= Self::CALL_BUTTON_MIN_WIDTH
    }

    pub fn sidebar_width(&self) -> Pixels {
        px(match self.breakpoint {
            Breakpoint::Desktop => Self::SIDEBAR_WIDTH_DESKTOP,
            Breakpoint::Tablet => {
                let proportional = self.viewport_width * 0.35;
                proportional.clamp(Self::SIDEBAR_WIDTH_MIN, Self::SIDEBAR_WIDTH_TABLET)
            }
            Breakpoint::Mobile => self.viewport_width,
        })
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
        px(match self.breakpoint {
            Breakpoint::Desktop => Self::MAX_BUBBLE_WIDTH_DESKTOP,
            Breakpoint::Tablet => Self::MAX_BUBBLE_WIDTH_TABLET,
            Breakpoint::Mobile => {
                (self.viewport_width * Self::MAX_BUBBLE_WIDTH_MOBILE_RATIO).min(350.0)
            }
        })
    }

    pub fn max_media_size(&self) -> f32 {
        match self.breakpoint {
            Breakpoint::Desktop => Self::MAX_MEDIA_SIZE_DESKTOP,
            Breakpoint::Tablet => Self::MAX_MEDIA_SIZE_TABLET,
            Breakpoint::Mobile => {
                (self.viewport_width * Self::MAX_MEDIA_SIZE_MOBILE_RATIO).min(280.0)
            }
        }
    }

    pub fn chat_area_width(&self) -> f32 {
        match self.breakpoint {
            Breakpoint::Desktop | Breakpoint::Tablet => {
                self.viewport_width - f32::from(self.sidebar_width())
            }
            Breakpoint::Mobile => self.viewport_width,
        }
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
