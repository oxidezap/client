//! The voice message player.
//!
//! A voice note is the one attachment you cannot skim, so the bars carry the
//! only summary there is: where the loud parts are, and how far in you have
//! got. That makes the waveform a control rather than decoration — it is the
//! scrub bar, and clicking it seeks.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, Bounds, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Pixels, SharedString, Styled, canvas, div, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Icon, IconName, Sizable as _};

use crate::app::WhatsAppApp;
use crate::components::ProductIcon;
use crate::components::parts;
use crate::theme::{ActiveProductTheme as _, Metrics};
use oxidezap_core::MediaContent;

/// How many bars the player draws.
///
/// `oxidezap-audio` emits 64 buckets and WhatsApp ships 64 too, so this is a
/// straight read at the common case; anything else is resampled onto it.
const BARS: usize = 48;

/// Playback speeds, in the order the chip cycles through them.
pub const SPEEDS: [f32; 3] = [1.0, 1.5, 2.0];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioLoadState {
    Ready,
    Downloadable,
    Downloading,
    Preparing,
    Unavailable,
}

impl AudioLoadState {
    fn from_presence(
        has_data: bool,
        can_download: bool,
        is_downloading: bool,
        is_preparing: bool,
    ) -> Self {
        if is_preparing {
            Self::Preparing
        } else if is_downloading {
            Self::Downloading
        } else if has_data {
            Self::Ready
        } else if can_download {
            Self::Downloadable
        } else {
            Self::Unavailable
        }
    }

    fn status(self) -> Option<&'static str> {
        match self {
            Self::Ready => None,
            Self::Downloadable => Some("Download"),
            Self::Downloading => Some("Downloading…"),
            Self::Preparing => Some("Preparing…"),
            Self::Unavailable => Some("Unavailable"),
        }
    }

    fn can_activate(self) -> bool {
        matches!(self, Self::Ready | Self::Downloadable | Self::Preparing)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_audio_player(
    media_content: MediaContent,
    message_id: String,
    is_playing: bool,
    // Progress only means anything for the clip that is actually loaded; a
    // second voice note in the same conversation must not borrow its
    // position, so the list hands this row `None` unless it is the one
    // playing. Read out there rather than here: the app is already leased to
    // build this row, and reading it again panics.
    audio: Option<super::AudioProgress>,
    speed: f32,
    is_downloading: bool,
    is_preparing: bool,
    entity: Entity<WhatsAppApp>,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = cx.product().metrics;
    let has_data = media_content.has_data();
    let state = AudioLoadState::from_presence(
        has_data,
        media_content.can_download(),
        is_downloading,
        is_preparing,
    );

    let progress = audio.map_or(0.0, |audio| audio.fraction);
    let elapsed = audio
        .filter(|audio| audio.fraction > 0.0)
        .map(|audio| audio.elapsed_secs as u32);
    let duration = media_content.duration_secs;
    let bars = resample(media_content.waveform.as_ref().map(|w| w.as_slice()), BARS);

    div()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .child(render_play_button(
            &media_content,
            message_id.clone(),
            is_playing,
            state,
            entity.clone(),
            metrics,
            cx,
        ))
        .child(render_waveform(
            bars,
            progress,
            message_id.clone(),
            has_data && state == AudioLoadState::Ready,
            entity.clone(),
            metrics,
            cx,
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .items_end()
                .flex_shrink_0()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_meta())
                .text_color(cx.theme().muted_foreground)
                // Counts up while playing, shows the total at rest — the same
                // number a listener wants at each moment.
                .child(format_clock(elapsed.or(duration)))
                .children(state.status().map(|status| {
                    div()
                        .text_size(metrics.text_micro())
                        .text_color(cx.theme().muted_foreground)
                        .child(status)
                })),
        )
        .child(render_speed_chip(&message_id, speed, entity, metrics, cx))
}

fn render_play_button(
    media_content: &MediaContent,
    message_id: String,
    is_playing: bool,
    state: AudioLoadState,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    _cx: &App,
) -> impl IntoElement + use<> {
    let data = media_content.data.clone();
    let downloadable = media_content.downloadable.clone();
    let id: SharedString = format!("play-{message_id}").into();
    let icon = match state {
        AudioLoadState::Ready => Icon::from(if is_playing {
            ProductIcon::Pause
        } else {
            ProductIcon::Play
        }),
        AudioLoadState::Downloadable => Icon::new(IconName::ArrowDown),
        AudioLoadState::Downloading => Icon::new(IconName::LoaderCircle),
        // The preparation is still cancellable as a playback intent: a
        // second tap must be able to pause it before the stream exists.
        AudioLoadState::Preparing => Icon::from(if is_playing {
            ProductIcon::Pause
        } else {
            ProductIcon::Play
        }),
        AudioLoadState::Unavailable => Icon::from(ProductIcon::Play),
    };
    let tooltip = match state {
        AudioLoadState::Ready if is_playing => "Pause",
        AudioLoadState::Ready => "Play",
        AudioLoadState::Downloadable => "Download audio",
        AudioLoadState::Downloading => "Downloading audio…",
        AudioLoadState::Preparing if is_playing => "Pause when audio is ready",
        AudioLoadState::Preparing => "Play when audio is ready",
        AudioLoadState::Unavailable => "Audio unavailable",
    };

    Button::new(id)
        .icon(icon.size(metrics.icon_small()))
        .primary()
        .rounded_full()
        .w(metrics.avatar_inline())
        .h(metrics.avatar_inline())
        .disabled(!state.can_activate())
        .tooltip(tooltip)
        .when(state.can_activate(), |button| button.cursor_pointer())
        .on_click(move |_, _window, cx| {
            let message_id = message_id.clone();
            entity.update(cx, |app, cx| {
                if !data.is_empty() {
                    app.toggle_audio(message_id, (*data).clone(), cx);
                } else if let Some(dl) = downloadable.clone() {
                    app.toggle_audio_lazy(message_id, dl, cx);
                }
            });
        })
}

/// The bars, and the scrub they double as.
///
/// The click is handled on the strip rather than per bar: a bar is 2px wide,
/// and a target that narrow would make seeking a game of precision.
fn render_waveform(
    bars: Vec<u8>,
    progress: f32,
    message_id: String,
    can_seek: bool,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let id: SharedString = format!("waveform-{message_id}").into();
    let seek_id = message_id.to_string();
    let height = metrics.waveform_height();
    let played = cx.theme().primary;
    let remaining = parts::faint(cx);
    let playhead = cx.theme().foreground;
    let count = bars.len().max(1);

    // A pointer position is in window coordinates; turning it into a fraction
    // needs the strip's own resolved bounds, which only exist after layout.
    // The canvas captures them in prepaint for the handler to read.
    let strip: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));
    let measured = strip.clone();

    div()
        .id(id)
        .relative()
        .flex_1()
        .min_w_0()
        .h(height)
        .flex()
        .items_center()
        .gap(metrics.waveform_bar_gap())
        .child(
            canvas(move |bounds, _, _| measured.set(bounds), |_, _, _, _| {})
                .absolute()
                .size_full(),
        )
        .when(can_seek, move |el| {
            el.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                move |event: &MouseDownEvent, _w, cx| {
                    let fraction = seek_fraction(event.position.x, strip.get());
                    entity.update(cx, |app, cx| app.seek_audio(&seek_id, fraction, cx));
                },
            )
        })
        .children(bars.into_iter().enumerate().map(move |(ix, level)| {
            let position = (ix as f32 + 0.5) / count as f32;
            div()
                .w(metrics.waveform_bar_width())
                // A floor so silence still reads as part of the clip rather
                // than a gap in it.
                .h((height * (level as f32 / 100.0)).max(metrics.bar_thin()))
                .rounded_full()
                .bg(if position <= progress {
                    played
                } else {
                    remaining
                })
        }))
        // Only while there is something to point at: a playhead parked at zero
        // on an unplayed note is noise.
        .when(progress > 0.0, |el| {
            el.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(gpui::relative(progress))
                    .w(metrics.bar_thin())
                    .rounded_full()
                    .bg(playhead),
            )
        })
}

/// 1× / 1.5× / 2×, cycled by clicking.
///
/// A `Button`, because changing playback speed is a command: a styled `div`
/// carries no focus handle and no keyboard activation, so the control was
/// reachable with a pointer and by nothing else.
fn render_speed_chip(
    message_id: &str,
    speed: f32,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let is_default = (speed - 1.0).abs() < f32::EPSILON;

    Button::new(
        // Per message: a GPUI id is scoped to its first identified ancestor,
        // and every voice note in a conversation is a sibling of the rest, so
        // one constant made them all the same element.
        SharedString::from(format!("playback-speed-{message_id}")),
    )
    .label(format_speed(speed))
    .outline()
    .xsmall()
    .rounded_full()
    .flex_shrink_0()
    .font_family(cx.theme().mono_font_family.clone())
    .text_size(metrics.text_micro())
    // Quiet at 1×, lit once it departs from it: the chip only needs to draw
    // attention when it is doing something.
    .text_color(if is_default {
        cx.theme().muted_foreground
    } else {
        cx.theme().primary
    })
    .tooltip("Playback speed")
    .cursor_pointer()
    .on_click(move |_, _window, cx| {
        entity.update(cx, |app, cx| app.cycle_playback_speed(cx));
    })
}

/// Where a click at `x` falls along `bounds`, as `0.0..=1.0`.
pub fn seek_fraction(x: Pixels, bounds: Bounds<Pixels>) -> f32 {
    let width = f32::from(bounds.size.width);
    if width <= 0.0 {
        return 0.0;
    }
    ((f32::from(x) - f32::from(bounds.origin.x)) / width).clamp(0.0, 1.0)
}

/// Fit an envelope of any length onto `target` bars.
///
/// Both producers emit 64 buckets, so the common path is a plain resample;
/// the general form exists because the field is `bytes` on the wire and a
/// sender is free to send any length. No envelope at all yields a flat row
/// rather than an invented shape.
pub fn resample(source: Option<&[u8]>, target: usize) -> Vec<u8> {
    const FLAT: u8 = 30;

    let Some(source) = source.filter(|s| !s.is_empty()) else {
        return vec![FLAT; target];
    };
    (0..target)
        .map(|ix| {
            // Every output bar averages the input range it covers, so
            // downsampling cannot drop a peak between two sampled points.
            let start = ix * source.len() / target;
            let end = ((ix + 1) * source.len())
                .div_ceil(target)
                .clamp(start + 1, source.len());
            let slice = &source[start..end];
            let sum: u32 = slice.iter().map(|&v| u32::from(v)).sum();
            (sum / slice.len() as u32).min(100) as u8
        })
        .collect()
}

/// `m:ss`, or a placeholder when the length is unknown.
fn format_clock(secs: Option<u32>) -> String {
    match secs {
        Some(secs) => oxidezap_core::format_duration(secs),
        None => "--:--".to_string(),
    }
}

fn format_speed(speed: f32) -> String {
    // No trailing `.0`: `1×` reads as a speed, `1.0×` reads as a measurement.
    if (speed.fract()).abs() < f32::EPSILON {
        format!("{speed:.0}×")
    } else {
        format!("{speed:.1}×")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    #[test]
    fn audio_control_shows_fetch_and_preparation_as_distinct_states() {
        use AudioLoadState as State;

        assert_eq!(
            State::from_presence(false, true, false, false),
            State::Downloadable
        );
        assert_eq!(
            State::from_presence(false, true, true, false),
            State::Downloading
        );
        assert_eq!(
            State::from_presence(true, true, false, true),
            State::Preparing
        );
        assert_eq!(State::from_presence(true, true, false, false), State::Ready);
        assert_eq!(
            State::from_presence(false, false, false, false),
            State::Unavailable
        );
        assert_eq!(State::Downloading.status(), Some("Downloading…"));
        assert_eq!(State::Preparing.status(), Some("Preparing…"));
        assert_eq!(State::Ready.status(), None);
        assert!(State::Downloadable.can_activate());
        assert!(State::Ready.can_activate());
        assert!(!State::Downloading.can_activate());
        assert!(State::Preparing.can_activate());
        assert!(!State::Unavailable.can_activate());
    }

    #[test]
    fn audio_released_from_memory_is_fetchable_again() {
        // A retained chat may release bytes after its reader leaves; the
        // daemon can still have them, but this row does not promise that.
        let state = AudioLoadState::from_presence(false, true, false, false);
        assert_eq!(state.status(), Some("Download"));
    }

    #[test]
    fn an_absent_envelope_draws_flat_rather_than_inventing_a_shape() {
        let bars = resample(None, 8);
        assert_eq!(bars.len(), 8);
        assert!(bars.iter().all(|&b| b == bars[0]));
    }

    #[test]
    fn an_empty_envelope_is_treated_as_absent() {
        assert_eq!(resample(Some(&[]), 4), resample(None, 4));
    }

    #[test]
    fn the_common_case_is_a_straight_resample() {
        let source: Vec<u8> = (0..64).map(|i| (i * 100 / 63) as u8).collect();
        let bars = resample(Some(&source), 48);
        assert_eq!(bars.len(), 48);
        // Monotonic in, monotonic out.
        assert!(bars.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn downsampling_averages_rather_than_dropping_peaks() {
        // A peak sitting between two sampled points would vanish under naive
        // nearest-neighbour picking.
        let source = [0, 100, 0, 0, 0, 100, 0, 0];
        let bars = resample(Some(&source), 4);
        assert_eq!(bars.len(), 4);
        assert!(bars.iter().any(|&b| b > 0), "the peaks survived");
    }

    #[test]
    fn upsampling_a_short_envelope_fills_every_bar() {
        let bars = resample(Some(&[10, 90]), 8);
        assert_eq!(bars.len(), 8);
        assert!(bars.iter().all(|&b| b > 0));
    }

    #[test]
    fn levels_stay_inside_the_drawable_range() {
        let bars = resample(Some(&[255, 255, 255]), 4);
        assert!(bars.iter().all(|&b| b <= 100));
    }

    fn strip() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(100.0), px(0.0)),
            size: size(px(200.0), px(28.0)),
        }
    }

    #[test]
    fn a_click_maps_to_its_position_along_the_strip() {
        assert!((seek_fraction(px(100.0), strip()) - 0.0).abs() < f32::EPSILON);
        assert!((seek_fraction(px(200.0), strip()) - 0.5).abs() < f32::EPSILON);
        assert!((seek_fraction(px(300.0), strip()) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_drag_past_either_edge_clamps() {
        assert_eq!(seek_fraction(px(-50.0), strip()), 0.0);
        assert_eq!(seek_fraction(px(9_999.0), strip()), 1.0);
    }

    #[test]
    fn a_zero_width_strip_cannot_divide_by_zero() {
        let collapsed = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(0.0), px(28.0)),
        };
        assert_eq!(seek_fraction(px(10.0), collapsed), 0.0);
    }

    #[test]
    fn speeds_read_as_speeds() {
        assert_eq!(format_speed(1.0), "1×");
        assert_eq!(format_speed(1.5), "1.5×");
        assert_eq!(format_speed(2.0), "2×");
    }

    #[test]
    fn an_unknown_length_shows_a_placeholder_not_zero() {
        assert_eq!(format_clock(None), "--:--");
        assert_eq!(format_clock(Some(14)), "0:14");
    }
}
