//! The tray dot the icon-less platforms draw.
//!
//! StatusNotifierItem names icons out of the user's theme, but a notification
//! area and a menu bar carry their own pixels — so Windows and macOS paint
//! the same 32×32 dot instead of naming one: grey while disconnected, green
//! while connected, amber while something waits to be read. Drawn here,
//! once, rather than once per tray, so no platform can disagree with another
//! about which state gets which colour. Dependency-free on purpose: this
//! module compiles everywhere, and its tests run on every platform.

use crate::state::TrayState;

/// The icon's size. Small on purpose: a menu bar renders it at eighteen
/// points, and a dot needs no more than this to stay round there.
pub const SIDE: u32 = 32;

/// Grey while the connection is down: what was last heard is then a number
/// nothing is refreshing, and an icon asking to be looked at over a stale
/// count is worse than one saying the connection is what is wrong.
pub const DISCONNECTED: [u8; 4] = [0x80, 0x80, 0x80, 0xFF];
/// Green while connected with nothing waiting.
pub const CONNECTED: [u8; 4] = [0x2E, 0xA0, 0x43, 0xFF];
/// Amber while something waits to be read.
pub const UNREAD: [u8; 4] = [0xD6, 0x45, 0x41, 0xFF];

/// Which dot a state gets. One function rather than one per tray, so the
/// icon and anything reasoning about it cannot disagree.
#[must_use]
pub fn colour_for(state: &TrayState) -> [u8; 4] {
    match (state.connected, state.shown_unread()) {
        (false, _) => DISCONNECTED,
        (true, 0) => CONNECTED,
        (true, _) => UNREAD,
    }
}

/// A filled circle on transparency: the hosts draw it small, and a square of
/// colour would read as a chip off a theme.
#[must_use]
pub fn dot(colour: [u8; 4]) -> Vec<u8> {
    let side = SIDE as f32;
    let (centre, radius) = ((side - 1.0) / 2.0, (side - 2.0) / 2.0);
    let mut rgba = Vec::with_capacity((SIDE * SIDE * 4) as usize);
    for y in 0..SIDE {
        for x in 0..SIDE {
            let distance = ((x as f32 - centre).powi(2) + (y as f32 - centre).powi(2)).sqrt();
            if distance <= radius {
                rgba.extend_from_slice(&colour);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(connected: bool, unread: u32) -> TrayState {
        TrayState { connected, unread }
    }

    /// The report Linux got first: messages waiting behind an icon that
    /// looked exactly like an idle one. Three colours, and the waiting one
    /// is not the idle one.
    #[test]
    fn unread_reaches_the_icon_itself() {
        assert_ne!(
            dot(colour_for(&state(true, 3))),
            dot(colour_for(&state(true, 0))),
            "an icon with something to read must not look like one without"
        );
        assert_ne!(
            dot(colour_for(&state(false, 3))),
            dot(colour_for(&state(true, 0))),
            "a disconnected icon must not look connected"
        );
    }

    /// A count nothing is refreshing is not news: the icon goes grey rather
    /// than holding a stale colour.
    #[test]
    fn a_disconnected_icon_is_grey_whatever_it_last_heard() {
        assert_eq!(
            dot(colour_for(&state(false, 3))),
            dot(colour_for(&state(false, 0)))
        );
        assert_eq!(colour_for(&state(false, 3)), DISCONNECTED);
    }

    /// Thirty-two by thirty-two of RGBA, with something drawn and something
    /// transparent: a square of colour would read as a chip off a theme.
    #[test]
    fn the_dot_is_round() {
        let pixels = dot(CONNECTED);
        assert_eq!(pixels.len(), 32 * 32 * 4);
        // Corners fall outside the circle.
        assert_eq!(&pixels[0..4], &[0, 0, 0, 0]);
        // The centre is the colour.
        let centre = (16 * 32 + 16) * 4;
        assert_eq!(&pixels[centre..centre + 4], &CONNECTED);
    }
}
