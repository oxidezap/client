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
//!
//! The window has a say in that base as well as the user: a viewport smaller
//! than the canvas the design was drawn on multiplies it by [`viewport_fit`].
//! That is deliberately the *only* thing a small screen changes here. One
//! factor on the rem moves type, rhythm, control frames and the layout
//! thresholds together, so a 480×640 handheld gets the design it was drawn —
//! at its size — rather than a second design assembled out of special cases.

use std::fmt;
use std::str::FromStr;

use gpui::{Pixels, Size, px};

/// The base font the design was measured against.
const REFERENCE_REM: f32 = 16.0;

/// The smallest window the design was drawn to fit at full size.
///
/// Not a breakpoint and not a minimum: it is the canvas every fixed dimension
/// in this file was measured against. A window smaller than this in either
/// axis gets the same design at a smaller scale — see [`viewport_fit`] — which
/// is the one lever that keeps a 480×640 handheld showing the whole screen
/// instead of the top-left corner of it.
const DESIGN_VIEWPORT: Size<f32> = Size {
    width: 400.0,
    height: 720.0,
};

/// How far the fit may shrink the design.
///
/// Past this the type stops being legible and shrinking further buys a screen
/// nobody can read; what does not fit below the floor is what the scroll
/// containers are for.
const FIT_MIN: f32 = 0.7;

/// How coarsely the fit is quantised.
///
/// A continuous factor would move every cached row height on every pixel of a
/// window drag — the timeline keys its measurements on [`Metrics::rem_size`],
/// which the fit multiplies. Twentieths are fine enough that no step is
/// visible and coarse enough that a resize crosses only a handful of them.
const FIT_STEP: f32 = 20.0;

/// The design scale a window of this size can carry, as a factor on the base
/// font.
///
/// One number for both axes, taken from whichever is tighter: a screen is too
/// small in the axis that runs out first, and scaling only that one would
/// stretch the design rather than shrink it. Never above 1.0 — a large window
/// is a window with room to spare, not an instruction to magnify.
pub fn viewport_fit(viewport: Size<Pixels>) -> f32 {
    let width = f32::from(viewport.width) / DESIGN_VIEWPORT.width;
    let height = f32::from(viewport.height) / DESIGN_VIEWPORT.height;
    let raw = width.min(height).min(1.0);
    if !raw.is_finite() {
        return 1.0;
    }
    ((raw * FIT_STEP).floor() / FIT_STEP).clamp(FIT_MIN, 1.0)
}

/// The narrowest and widest base font the layout stays usable at.
///
/// Both ends matter: below the floor every dimension collapses, and above the
/// ceiling the window outgrows the controls that would let anyone put it back
/// — including the field the number was typed into.
const REM_MIN: f32 = 10.0;
const REM_MAX: f32 = 32.0;

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
            // A range, not a floor. The value reaches here from a hand-edited
            // file that is hot-reloaded, so both ends have to hold: zero
            // collapses every dimension to nothing, and a large one scales the
            // whole window past the controls that would let anyone undo it —
            // including the field the number was typed into. NaN compares
            // false against everything, so `clamp` would panic on it: it is
            // caught first and falls back to the reference size.
            rem_size: if rem_size.is_finite() {
                rem_size.clamp(REM_MIN, REM_MAX)
            } else {
                REFERENCE_REM
            },
            density,
        }
    }

    /// The scale a window of this size can carry, from the base font the user
    /// asked for.
    ///
    /// The fit multiplies the base rather than sitting beside it, so
    /// everything derived from the rem — every token in this file, and the
    /// cache keys taken from [`Self::rem_size`] — follows the window without
    /// a single call site learning that windows have sizes.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the window resolves its own metrics; this is how the scale is stated"
        )
    )]
    pub fn for_viewport(rem_size: f32, density: Density, viewport: Size<Pixels>) -> Self {
        Self::new(rem_size * viewport_fit(viewport), density)
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
}

/// Declare the scale: one line per token, naming the rule it follows and the
/// number the design specified at the reference base.
///
/// Every accessor below used to be four lines of the same shape, which put the
/// rule an accessor follows — [`Metrics::scaled`] or [`Metrics::dense`], the
/// one distinction that matters here — in the middle of a stanza rather than
/// beside the name. Written as a table it reads as what it is: the design's
/// measurements. The generated functions are the hand-written ones, name for
/// name and value for value, and nothing about how a caller reads them
/// changed — which matters more here than anywhere else, because this file is
/// the mechanism the whole rem rule rests on.
macro_rules! tokens {
    ($(
        $(#[$meta:meta])*
        $name:ident = $rule:ident($design_px:literal);
    )*) => {
        impl Metrics {
            $(
                $(#[$meta])*
                pub fn $name(&self) -> Pixels {
                    self.$rule($design_px)
                }
            )*
        }
    };
}

tokens! {
    // ---- spacing ------------------------------------------------------
    //
    // The semantic steps from the design system, named for the relationship
    // they express rather than their size.

    /// Optical correction: an icon baseline, a separator nudge.
    space_xxs = dense(2.0);
    /// Parts of one control: a title and its description.
    space_xs = dense(4.0);
    /// Closely related controls: an icon and its label.
    space_sm = dense(6.0);
    /// One content group.
    space_md = dense(8.0);
    /// Separate groups within a section.
    space_lg = dense(12.0);
    /// Separate sections.
    space_xl = dense(16.0);
    /// A major region boundary.
    space_xxl = dense(24.0);
    /// Empty-state breathing room.
    space_xxxl = dense(32.0);

    // ---- radii --------------------------------------------------------

    /// Chips, ticks, small inline surfaces.
    radius_sm = scaled(8.0);
    /// Fields, icon buttons, thumbnails.
    radius_md = scaled(10.0);
    /// Message bubbles and panels.
    radius_lg = scaled(12.0);
    /// Floating cards: the call card, dialogs.
    radius_xl = scaled(14.0);
    /// The tight corner that marks the authored side of a bubble.
    radius_bubble_tail = scaled(4.0);

    // ---- type ---------------------------------------------------------
    //
    // Steps around the base rather than absolute sizes, so hierarchy survives
    // zoom.

    /// Screen and dialog titles.
    text_title = scaled(19.0);
    /// Section headings, a caller's name on the call card.
    text_heading = scaled(17.0);
    /// Names in a header.
    text_strong = scaled(16.0);
    /// Body text.
    text_body = scaled(15.0);
    /// Secondary text: previews, subtitles.
    text_secondary = scaled(13.5);
    /// Chips and compact controls.
    text_small = scaled(12.5);
    /// Monospace metadata: timestamps, counters, shortcuts.
    text_meta = scaled(11.0);
    /// The smallest step: a tick's timestamp, a date divider.
    text_micro = scaled(10.5);

    // ---- chat list ----------------------------------------------------

    /// A conversation row. The design lifted this from 72 to 78 so the name,
    /// preview and badge stop crowding each other.
    chat_row_height = dense(78.0);
    /// Gap between rows: rows read as cards, not as a ruled table.
    chat_row_gap = dense(4.0);
    chat_row_padding_x = dense(12.0);
    /// The teal bar that marks the selected row.
    selection_bar_width = scaled(3.0);
    avatar_row = dense(44.0);
    avatar_header = dense(38.0);
    /// Beside a typing bubble, and in the sidebar footer.
    avatar_inline = dense(28.0);
    avatar_call = dense(64.0);
    /// The presence dot on an avatar.
    #[expect(
        dead_code,
        reason = "a step nothing draws today is still the step between the ones that do"
    )]
    presence_dot = scaled(12.0);

    // ---- chrome -------------------------------------------------------

    /// The sidebar's own header, which holds the title and its actions.
    sidebar_header_height = dense(56.0);
    /// The account row at the foot of the sidebar.
    sidebar_footer_height = dense(56.0);
    /// The conversation header: taller than the sidebar's, because it carries
    /// a subtitle under the name.
    header_height = dense(60.0);
    mobile_header_height = dense(56.0);
    /// The composer's slot. `InputAreaView` is given this rather than owning a
    /// constant of its own, which is what used to clip it by 6px on mobile.
    composer_height = dense(62.0);
    /// Mobile keeps a full touch target plus its padding, and no more.
    composer_height_mobile = dense(56.0);
    /// The floor for a pointer target on touch.
    touch_target = dense(48.0);
    #[expect(
        dead_code,
        reason = "a step nothing draws today is still the step between the ones that do"
    )]
    search_field_height = dense(38.0);
    filter_chip_height = dense(26.0);
    /// A quiet icon button in a header or toolbar.
    icon_button = dense(34.0);
    /// The glyph inside an icon button.
    icon = scaled(17.0);
    icon_small = scaled(14.0);
    /// A glyph over media: bigger than a toolbar's, because the wash behind
    /// it is a picture rather than a surface.
    icon_media = scaled(20.0);
    /// The play control at the centre of a video.
    icon_media_large = scaled(32.0);
    /// The same, once a video is playing and the control gives way to it.
    icon_media_playing = scaled(24.0);

    // ---- fine geometry ------------------------------------------------
    //
    // Small enough to look like constants and not be: they are drawn beside
    // type and rows that move with the base font, so a hairline that stayed
    // one pixel at double the base is a line that has quietly halved.

    /// The narrowest a drawn bar gets, and the floor under a bar whose height
    /// is a level: a waveform column at silence is still a column.
    bar_thin = scaled(2.0);
    /// A level meter's column.
    bar = scaled(3.0);
    /// A dot in a row of them, as the connecting indicator draws.
    dot_small = scaled(3.0);
    /// A single status dot beside a label.
    dot = scaled(6.0);
    /// The floor under a badge that is otherwise a fraction of its avatar.
    badge_min = scaled(10.0);
    /// The round control drawn over a video: play, retry, or the spinner.
    media_control = scaled(48.0);
    /// A status ring's stroke, and the breathing room between it and the
    /// avatar inside. Rem-derived like the row height the ring is sized
    /// against.
    ring_thickness = scaled(2.0);
    ring_gap = scaled(3.0);

    // ---- timeline -----------------------------------------------------

    bubble_padding_x = dense(13.0);
    bubble_padding_y = dense(9.0);
    /// Between consecutive bubbles from the same author.
    bubble_gap_grouped = dense(2.0);
    /// Where authorship changes.
    bubble_gap_authored = dense(8.0);
    #[expect(
        dead_code,
        reason = "a step nothing draws today is still the step between the ones that do"
    )]
    date_divider_height = dense(34.0);
    #[expect(
        dead_code,
        reason = "a step nothing draws today is still the step between the ones that do"
    )]
    typing_row_height = dense(46.0);
    /// Reactions overlap the bubble's lower edge by this much.
    reaction_overlap = dense(6.0);
    #[expect(
        dead_code,
        reason = "a step nothing draws today is still the step between the ones that do"
    )]
    reaction_height = dense(22.0);
    /// How far beyond the viewport the timeline keeps rows laid out.
    ///
    /// About a screen either way, which is a claim about the rows rather than
    /// about the glass: at double the base font a fixed 800px is no longer a
    /// screen, and a flick lands on rows nobody has measured.
    timeline_overdraw = scaled(800.0);

    /// One line of body text in a bubble.
    #[expect(
        dead_code,
        reason = "a step nothing draws today is still the step between the ones that do"
    )]
    line_height = scaled(22.0);

    /// How wide a column of prose is allowed to get.
    ///
    /// Scales with the base font rather than sitting at a pixel count: the
    /// limit exists so a line stays a comfortable number of characters, and
    /// that is a property of the text size, not of the window.
    reading_width = scaled(720.0);

    /// How wide a transient notice is allowed to get.
    ///
    /// The last `px` literal in the interface lived here, and it was the same
    /// mistake the reading width exists to name: the ceiling is there so a
    /// long message wraps into a readable line rather than spanning the
    /// window, and how many characters fit on a line is a property of the
    /// text size. Fixed at 360 device pixels it was three words at double the
    /// base font. `scaled` and not `dense` for the same reason as the reading
    /// width: this is a bound on content, and density is rhythm and control
    /// frames.
    notice_width = scaled(360.0);

    /// How tall a read-only block of configuration is allowed to get.
    ///
    /// Its own token, not half the reading width. A height borrowed from a
    /// width works right up until someone adjusts the width for the reason it
    /// exists — line length — and silently resizes a panel that has nothing
    /// to do with prose.
    config_block_height = scaled(360.0);

    /// One theme preset's swatch in the Appearance pane.
    ///
    /// Sized for the row of them, not derived from the call card: they share
    /// no reason to be related, and tying them made the call card's width a
    /// remote control for a settings pane.
    preset_card_width = dense(184.0);

    /// The miniature window inside a preset card.
    ///
    /// Its own token rather than an avatar size: the preview is a picture of
    /// a layout, and it wants the shape of a window — wider than tall — not
    /// the shape of a face.
    preset_preview_height = dense(84.0);

    /// One destination in the Settings side navigation.
    ///
    /// Its own token, not an avatar's size: nothing on this row is a picture
    /// of a person, and a list of seven places wants to read as a list rather
    /// than as seven cards.
    nav_item_height = dense(34.0);

    /// The Settings side navigation.
    ///
    /// Not the call card's width. The two were the same number and neither
    /// was chosen for the other, so widening a floating card over a video
    /// call would have moved a settings column.
    settings_nav_width = dense(248.0);

    // ---- call card ----------------------------------------------------

    call_card_width = dense(340.0);
    /// Video and group need room for the picture, so they are wider.
    call_card_width_wide = dense(380.0);
    /// The drag handle strip along the card's top edge.
    call_drag_handle_height = dense(26.0);
    call_action_height = dense(42.0);
    /// A round call control: mute, hang up, camera.
    call_control = dense(44.0);

    // ---- audio --------------------------------------------------------

    /// The scrubbable waveform's hit area, taller than the bars it draws so
    /// the pointer target stays comfortable.
    waveform_height = dense(28.0);
    waveform_bar_width = scaled(2.0);
    waveform_bar_gap = scaled(2.0);

    // ---- layout -------------------------------------------------------
    //
    // How much window a pane gets, and where the layout changes shape. These
    // scale like everything else on purpose: "is there room for two panes"
    // is a question about the content, not about the glass. A window 700px
    // wide holds two panes at the reference base and one at double it, and a
    // threshold fixed in device pixels answered the same either way — which
    // is how a zoomed-in window ended up with a sidebar and a conversation
    // four words wide.

    /// Below this the window shows one pane at a time.
    breakpoint_mobile = scaled(600.0);
    /// Below this the sidebar is narrowed rather than dropped.
    breakpoint_tablet = scaled(900.0);
    /// Below this the conversation header has no room for its action row, so
    /// the actions move into the overflow menu.
    breakpoint_header_actions = scaled(400.0);

    sidebar_width = scaled(340.0);
    sidebar_width_compact = scaled(280.0);
    sidebar_width_min = scaled(240.0);

    /// How wide a bubble may get where there is a conversation pane to spare,
    /// and where the conversation is the whole window.
    bubble_max_width = scaled(520.0);
    bubble_max_width_compact = scaled(420.0);
    bubble_max_width_phone = scaled(350.0);

    media_max_size = scaled(300.0);
    media_max_size_compact = scaled(280.0);

    // ---- other --------------------------------------------------------

    /// The pairing code, which is content rather than chrome.
    ///
    /// `scaled`, not `dense`. Density is defined here as vertical rhythm and
    /// control frames, and a QR code is neither: it has to be readable by a
    /// camera across a desk, and Compact took it to ~138px on a handheld
    /// against 160px in Comfortable, near the limit, for a preference that
    /// was never about content.
    qr_size = scaled(256.0);
}

impl Metrics {
    /// A hairline stays one device pixel at any zoom: multiplying it makes the
    /// whole interface read as heavier rather than larger.
    pub fn hairline(&self) -> Pixels {
        px(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    /// A step in the scale that is not a step is a lie in a token name: the
    /// hierarchy between "a name in a header" and "body copy" then exists only
    /// where a reader cannot see it, and an edit to one silently stops
    /// matching the other. `text_strong` and `text_body` were both 15px.
    #[test]
    fn every_type_token_is_its_own_step() {
        let m = Metrics::new(REFERENCE_REM, Density::Comfortable);
        let steps = [
            ("text_title", m.text_title()),
            ("text_heading", m.text_heading()),
            ("text_strong", m.text_strong()),
            ("text_body", m.text_body()),
            ("text_secondary", m.text_secondary()),
            ("text_small", m.text_small()),
            ("text_meta", m.text_meta()),
            ("text_micro", m.text_micro()),
        ];
        for pair in steps.windows(2) {
            let (above, below) = (pair[0], pair[1]);
            assert!(
                above.1 > below.1,
                "{} ({}) does not sit above {} ({})",
                above.0,
                f32::from(above.1),
                below.0,
                f32::from(below.1),
            );
        }
    }

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
        // Nor is the QR code rhythm or a control frame. It has to be read by
        // a camera across a desk, and Compact took it to ~138px on a handheld
        // against 160px in Comfortable, near the limit of that.
        assert_eq!(
            compact.qr_size(),
            comfortable.qr_size(),
            "density does not touch content"
        );
    }

    #[test]
    fn a_nonsensical_base_font_cannot_collapse_the_layout() {
        let metrics = Metrics::new(0.0, Density::Comfortable);
        assert!(metrics.chat_row_height() > px(0.0));
    }

    /// The window the design was drawn for, and anything larger, gets it at
    /// full size. A factor above 1.0 would magnify a desktop window instead.
    #[test]
    fn a_window_with_room_to_spare_is_not_scaled() {
        assert_eq!(viewport_fit(size(px(1200.0), px(800.0))), 1.0);
        assert_eq!(viewport_fit(size(px(3840.0), px(2160.0))), 1.0);
        assert_eq!(
            viewport_fit(size(px(DESIGN_VIEWPORT.width), px(DESIGN_VIEWPORT.height))),
            1.0
        );
    }

    /// The axis that runs out first is the one that decides, because a screen
    /// is too small in whichever direction it is too small.
    #[test]
    fn the_tighter_axis_decides_the_fit() {
        // A handheld: wide enough for the phone layout, and much too short.
        let handheld = viewport_fit(size(px(480.0), px(640.0)));
        assert!(handheld < 1.0, "a 640px-tall window is short of the design");
        assert_eq!(handheld, viewport_fit(size(px(2000.0), px(640.0))));
    }

    #[test]
    fn the_fit_never_shrinks_past_legibility() {
        assert_eq!(viewport_fit(size(px(120.0), px(90.0))), FIT_MIN);
        assert_eq!(viewport_fit(size(px(0.0), px(0.0))), FIT_MIN);
    }

    /// Quantised, so dragging a window edge does not rebuild every cached row
    /// height on every pixel: the timeline keys its measurements on the rem.
    #[test]
    fn a_pixel_of_resize_does_not_move_the_scale() {
        let at = |h: f32| viewport_fit(size(px(480.0), px(h)));
        assert_eq!(at(640.0), at(641.0));
        assert!(at(400.0) < at(640.0), "a much shorter window does move it");
    }

    /// The whole point of folding the fit into the base font: every token
    /// follows, and so does every cache keyed on the rem.
    #[test]
    fn fitting_a_small_window_scales_the_whole_design() {
        let full = Metrics::for_viewport(16.0, Density::Comfortable, size(px(1200.0), px(800.0)));
        let handheld =
            Metrics::for_viewport(16.0, Density::Comfortable, size(px(480.0), px(640.0)));
        assert_eq!(full.rem_size(), 16.0);
        assert!(handheld.rem_size() < full.rem_size());
        assert!(handheld.text_body() < full.text_body());
        assert!(handheld.chat_row_height() < full.chat_row_height());
        assert!(handheld.qr_size() < full.qr_size());
        // A layout threshold is content-sized too, so a window that shrank
        // the design does not also cross into a layout meant for a wider one.
        assert!(handheld.breakpoint_mobile() < full.breakpoint_mobile());
    }

    /// The floor is reachable from a configured font, which is why the rem
    /// the library's controls are given has to be the one `Metrics` resolved
    /// rather than a second multiplication beside it: the smallest base a
    /// `theme.json` may ask for, at the smallest fit, lands under the floor.
    /// A window that computed it twice drew its own chrome at 10 and the
    /// library's buttons at 7.7, in the same header.
    #[test]
    fn the_smallest_configured_base_at_the_smallest_fit_hits_the_floor() {
        let asked = crate::theme::config::MIN_FONT_SIZE * FIT_MIN;
        assert!(asked < REM_MIN, "the clamp has to bite for this to matter");
        assert_eq!(
            Metrics::new(asked, Density::Comfortable).rem_size(),
            REM_MIN
        );
        // And the other end cannot be reached at all: the fit only shrinks.
        let largest = crate::theme::config::MAX_FONT_SIZE;
        assert!(largest <= REM_MAX);
        assert_eq!(
            Metrics::new(largest, Density::Comfortable).rem_size(),
            largest
        );
    }

    #[test]
    fn density_ids_round_trip() {
        for density in Density::ALL {
            assert_eq!(density.id().parse::<Density>().unwrap(), density);
        }
        assert!("cosy".parse::<Density>().is_err());
    }
}
