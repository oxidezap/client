//! Fixed geometry.
//!
//! Dimensions that do not vary with window size. Anything that should adapt to
//! the viewport belongs in [`crate::responsive`] instead.

/// Used by InputAreaView, which does not receive a `ResponsiveLayout`.
pub const INPUT_AREA_HEIGHT: f32 = 62.0;

pub const QR_CODE_SIZE: f32 = 256.0;

pub const RADIUS_SMALL: f32 = 4.0;
pub const RADIUS_MEDIUM: f32 = 8.0;
pub const RADIUS_LARGE: f32 = 20.0;

// Message bubble metrics. These are duplicated by `calculate_message_height`,
// which the virtual list needs in order to size a row before rendering it, so
// a change here must be reflected there or rows overlap.
pub const MSG_PADDING_TOP_FIRST: f32 = 8.0;
pub const MSG_PADDING_TOP_GROUPED: f32 = 6.0;
pub const MSG_PADDING_BOTTOM: f32 = 4.0;
pub const MSG_BUBBLE_PADDING_Y: f32 = 8.0;
pub const MSG_BUBBLE_PADDING_X: f32 = 12.0;
pub const MSG_CONTENT_GAP: f32 = 4.0;
pub const MSG_TEXT_LINE_HEIGHT: f32 = 22.0;
pub const MSG_TIME_ROW_HEIGHT: f32 = 24.0;
pub const MSG_SENDER_NAME_HEIGHT: f32 = 22.0;
pub const MSG_REACTION_MARGIN_TOP: f32 = 4.0;
pub const MSG_REACTION_HEIGHT: f32 = 28.0;
