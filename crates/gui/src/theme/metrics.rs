//! The product's own spacing, sizing and radius scale.
//!
//! gpui-component projects a fixed default spacing scale from its global
//! `Theme` and does not persist a custom one, so a product that wants its own
//! scale has to own the snapshot and pass it to its components. [`Metrics`] is
//! that snapshot: it is threaded through render code on
//! [`crate::responsive::ResponsiveLayout`], next to the viewport facts, so a
//! component receives one value and reads both.
//!
//! Every dimension resolves from `rem`, never from a literal. The numbers in
//! this file are the design's measurements at the 16px reference base; at any
//! other base font they scale with it, which is what makes the base font the
//! application's zoom control. Anything cached from these values — virtual
//! list row heights above all — must therefore key on [`Metrics::rem_size`].

use std::fmt;
use std::str::FromStr;

use gpui::{Pixels, px};

/// The base font the design was measured against.
const REFERENCE_REM: f32 = 16.0;

/// How much room the interface spends per unit of content.
///
/// A tier changes vertical rhythm and control frames together — never one
/// isolated control — so the whole window reads as one density.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Density {
    /// More rows on screen, for people who keep many conversations open.
    Compact,
    /// The default: the design as drawn.
    #[default]
    Comfortable,
}

impl Density {
    pub const ALL: [Self; 2] = [Self::Compact, Self::Comfortable];

    pub fn id(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Comfortable => "comfortable",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Comfortable => "Comfortable",
        }
    }

    /// Applied to vertical rhythm and control frames, not to type: shrinking
    /// the text as well is a zoom change, which the base font already owns.
    fn scale(self) -> f32 {
        match self {
            Self::Compact => 0.86,
            Self::Comfortable => 1.0,
        }
    }
}

impl FromStr for Density {
    type Err = UnknownDensityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|density| density.id() == s)
            .ok_or_else(|| UnknownDensityError(s.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownDensityError(pub String);

impl fmt::Display for UnknownDensityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown density \"{}\"; expected \"compact\" or \"comfortable\"",
            self.0
        )
    }
}

impl std::error::Error for UnknownDensityError {}

/// A resolved scale: one base font, one density.
///
/// Cheap to copy, and copied rather than borrowed so render helpers can take
/// it by value and stay `use<>`-free of `cx`'s lifetime.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    rem_size: f32,
    density: Density,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new(REFERENCE_REM, Density::default())
    }
}

impl Metrics {
    pub fn new(rem_size: f32, density: Density) -> Self {
        Self {
            // A zero or negative base would collapse every dimension to
            // nothing; clamp rather than trust a value that reached us from a
            // hand-edited file.
            rem_size: rem_size.max(1.0),
            density,
        }
    }

    /// The base font in force. Include this in the invalidation key of
    /// anything derived from these metrics and cached across frames.
    pub fn rem_size(&self) -> f32 {
        self.rem_size
    }

    pub fn density(&self) -> Density {
        self.density
    }

    /// A length the design specified as `design_px` at the reference base,
    /// scaled to the current base font. Type and hairlines use this directly.
    fn scaled(&self, design_px: f32) -> Pixels {
        px(design_px * self.rem_size / REFERENCE_REM)
    }

    /// As [`Self::scaled`], and additionally responsive to density. Vertical
    /// rhythm, control frames and padding use this.
    fn dense(&self, design_px: f32) -> Pixels {
        px(design_px * self.rem_size / REFERENCE_REM * self.density.scale())
    }

    // ---- spacing ------------------------------------------------------
    //
    // The semantic steps from the design system, named for the relationship
    // they express rather than their size.

    /// Optical correction: an icon baseline, a separator nudge.
    pub fn space_xxs(&self) -> Pixels {
        self.dense(2.0)
    }
    /// Parts of one control: a title and its description.
    pub fn space_xs(&self) -> Pixels {
        self.dense(4.0)
    }
    /// Closely related controls: an icon and its label.
    pub fn space_sm(&self) -> Pixels {
        self.dense(6.0)
    }
    /// One content group.
    pub fn space_md(&self) -> Pixels {
        self.dense(8.0)
    }
    /// Separate groups within a section.
    pub fn space_lg(&self) -> Pixels {
        self.dense(12.0)
    }
    /// Separate sections.
    pub fn space_xl(&self) -> Pixels {
        self.dense(16.0)
    }
    /// A major region boundary.
    pub fn space_xxl(&self) -> Pixels {
        self.dense(24.0)
    }
    /// Empty-state breathing room.
    pub fn space_xxxl(&self) -> Pixels {
        self.dense(32.0)
    }

    // ---- radii --------------------------------------------------------

    /// Chips, ticks, small inline surfaces.
    pub fn radius_sm(&self) -> Pixels {
        self.scaled(8.0)
    }
    /// Fields, icon buttons, thumbnails.
    pub fn radius_md(&self) -> Pixels {
        self.scaled(10.0)
    }
    /// Message bubbles and panels.
    pub fn radius_lg(&self) -> Pixels {
        self.scaled(12.0)
    }
    /// Floating cards: the call card, dialogs.
    pub fn radius_xl(&self) -> Pixels {
        self.scaled(14.0)
    }
    /// The tight corner that marks the authored side of a bubble.
    pub fn radius_bubble_tail(&self) -> Pixels {
        self.scaled(4.0)
    }

    // ---- type ---------------------------------------------------------
    //
    // Steps around the base rather than absolute sizes, so hierarchy survives
    // zoom.

    /// Screen and dialog titles.
    pub fn text_title(&self) -> Pixels {
        self.scaled(19.0)
    }
    /// Section headings, a caller's name on the call card.
    pub fn text_heading(&self) -> Pixels {
        self.scaled(17.0)
    }
    /// Names in a header.
    pub fn text_strong(&self) -> Pixels {
        self.scaled(15.0)
    }
    /// Body text.
    pub fn text_body(&self) -> Pixels {
        self.scaled(15.0)
    }
    /// Secondary text: previews, subtitles.
    pub fn text_secondary(&self) -> Pixels {
        self.scaled(13.5)
    }
    /// Chips and compact controls.
    pub fn text_small(&self) -> Pixels {
        self.scaled(12.5)
    }
    /// Monospace metadata: timestamps, counters, shortcuts.
    pub fn text_meta(&self) -> Pixels {
        self.scaled(11.0)
    }
    /// The smallest step: a tick's timestamp, a date divider.
    pub fn text_micro(&self) -> Pixels {
        self.scaled(10.5)
    }

    // ---- chat list ----------------------------------------------------

    /// A conversation row. The design lifted this from 72 to 78 so the name,
    /// preview and badge stop crowding each other.
    pub fn chat_row_height(&self) -> Pixels {
        self.dense(78.0)
    }
    /// Gap between rows: rows read as cards, not as a ruled table.
    pub fn chat_row_gap(&self) -> Pixels {
        self.dense(4.0)
    }
    pub fn chat_row_padding_x(&self) -> Pixels {
        self.dense(12.0)
    }
    /// The teal bar that marks the selected row.
    pub fn selection_bar_width(&self) -> Pixels {
        self.scaled(3.0)
    }
    pub fn avatar_row(&self) -> Pixels {
        self.dense(44.0)
    }
    pub fn avatar_header(&self) -> Pixels {
        self.dense(38.0)
    }
    /// Beside a typing bubble, and in the sidebar footer.
    pub fn avatar_inline(&self) -> Pixels {
        self.dense(28.0)
    }
    pub fn avatar_call(&self) -> Pixels {
        self.dense(64.0)
    }
    /// The presence dot on an avatar.
    pub fn presence_dot(&self) -> Pixels {
        self.scaled(12.0)
    }

    // ---- chrome -------------------------------------------------------

    /// The sidebar's own header, which holds the title and its actions.
    pub fn sidebar_header_height(&self) -> Pixels {
        self.dense(56.0)
    }
    /// The account row at the foot of the sidebar.
    pub fn sidebar_footer_height(&self) -> Pixels {
        self.dense(56.0)
    }
    /// The conversation header: taller than the sidebar's, because it carries
    /// a subtitle under the name.
    pub fn header_height(&self) -> Pixels {
        self.dense(60.0)
    }
    pub fn mobile_header_height(&self) -> Pixels {
        self.dense(56.0)
    }
    /// The composer's slot. `InputAreaView` is given this rather than owning a
    /// constant of its own, which is what used to clip it by 6px on mobile.
    pub fn composer_height(&self) -> Pixels {
        self.dense(62.0)
    }
    /// Mobile keeps a full touch target plus its padding, and no more.
    pub fn composer_height_mobile(&self) -> Pixels {
        self.dense(56.0)
    }
    /// The floor for a pointer target on touch.
    pub fn touch_target(&self) -> Pixels {
        self.dense(48.0)
    }
    pub fn search_field_height(&self) -> Pixels {
        self.dense(38.0)
    }
    pub fn filter_chip_height(&self) -> Pixels {
        self.dense(26.0)
    }
    /// A quiet icon button in a header or toolbar.
    pub fn icon_button(&self) -> Pixels {
        self.dense(34.0)
    }
    /// The glyph inside an icon button.
    pub fn icon(&self) -> Pixels {
        self.scaled(17.0)
    }
    pub fn icon_small(&self) -> Pixels {
        self.scaled(14.0)
    }

    // ---- timeline -----------------------------------------------------

    pub fn bubble_padding_x(&self) -> Pixels {
        self.dense(13.0)
    }
    pub fn bubble_padding_y(&self) -> Pixels {
        self.dense(9.0)
    }
    /// Between consecutive bubbles from the same author.
    pub fn bubble_gap_grouped(&self) -> Pixels {
        self.dense(2.0)
    }
    /// Where authorship changes.
    pub fn bubble_gap_authored(&self) -> Pixels {
        self.dense(8.0)
    }
    pub fn date_divider_height(&self) -> Pixels {
        self.dense(34.0)
    }
    pub fn typing_row_height(&self) -> Pixels {
        self.dense(46.0)
    }
    /// Reactions overlap the bubble's lower edge by this much.
    pub fn reaction_overlap(&self) -> Pixels {
        self.dense(6.0)
    }
    pub fn reaction_height(&self) -> Pixels {
        self.dense(22.0)
    }
    /// One line of body text in a bubble.
    pub fn line_height(&self) -> Pixels {
        self.scaled(22.0)
    }

    // ---- call card ----------------------------------------------------

    pub fn call_card_width(&self) -> Pixels {
        self.dense(340.0)
    }
    /// Video and group need room for the picture, so they are wider.
    pub fn call_card_width_wide(&self) -> Pixels {
        self.dense(380.0)
    }
    /// The drag handle strip along the card's top edge.
    pub fn call_drag_handle_height(&self) -> Pixels {
        self.dense(26.0)
    }
    pub fn call_action_height(&self) -> Pixels {
        self.dense(42.0)
    }
    /// A round call control: mute, hang up, camera.
    pub fn call_control(&self) -> Pixels {
        self.dense(44.0)
    }

    // ---- audio --------------------------------------------------------

    /// The scrubbable waveform's hit area, taller than the bars it draws so
    /// the pointer target stays comfortable.
    pub fn waveform_height(&self) -> Pixels {
        self.dense(28.0)
    }
    pub fn waveform_bar_width(&self) -> Pixels {
        self.scaled(2.0)
    }
    pub fn waveform_bar_gap(&self) -> Pixels {
        self.scaled(2.0)
    }

    // ---- other --------------------------------------------------------

    pub fn qr_size(&self) -> Pixels {
        self.dense(256.0)
    }
    /// A hairline stays one device pixel at any zoom: multiplying it makes the
    /// whole interface read as heavier rather than larger.
    pub fn hairline(&self) -> Pixels {
        px(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_base_reproduces_the_designed_measurements() {
        let metrics = Metrics::new(16.0, Density::Comfortable);
        assert_eq!(metrics.chat_row_height(), px(78.0));
        assert_eq!(metrics.avatar_row(), px(44.0));
        assert_eq!(metrics.radius_lg(), px(12.0));
        assert_eq!(metrics.text_body(), px(15.0));
    }

    #[test]
    fn a_larger_base_font_scales_every_dimension() {
        let base = Metrics::new(16.0, Density::Comfortable);
        let zoomed = Metrics::new(24.0, Density::Comfortable);
        assert_eq!(zoomed.chat_row_height(), px(78.0 * 1.5));
        assert_eq!(zoomed.text_body(), px(15.0 * 1.5));
        assert!(zoomed.space_lg() > base.space_lg());
    }

    #[test]
    fn hairlines_do_not_zoom() {
        // Scaling a separator makes the UI heavier, not bigger.
        assert_eq!(Metrics::new(24.0, Density::Comfortable).hairline(), px(1.0));
    }

    #[test]
    fn compact_tightens_rhythm_but_not_type() {
        let comfortable = Metrics::new(16.0, Density::Comfortable);
        let compact = Metrics::new(16.0, Density::Compact);
        assert!(compact.chat_row_height() < comfortable.chat_row_height());
        assert_eq!(
            compact.text_body(),
            comfortable.text_body(),
            "density is not zoom"
        );
    }

    #[test]
    fn a_nonsensical_base_font_cannot_collapse_the_layout() {
        let metrics = Metrics::new(0.0, Density::Comfortable);
        assert!(metrics.chat_row_height() > px(0.0));
    }

    #[test]
    fn density_ids_round_trip() {
        for density in Density::ALL {
            assert_eq!(density.id().parse::<Density>().unwrap(), density);
        }
        assert!("cosy".parse::<Density>().is_err());
    }
}
