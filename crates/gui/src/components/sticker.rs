use std::{sync::Arc, time::Duration};

use gpui::{Corners, ImageId, ObjectFit, RenderImage, SharedString, Styled as _, Task, canvas};
use wacore::time::Instant;

use crate::retained_image::RetainedImage;

const MIN_DELAY: Duration = Duration::from_millis(1);

enum Timing {
    Paused(Option<Duration>),
    Running { deadline: Instant, _wake: Task<()> },
}

struct Playback {
    image: ImageId,
    frame: usize,
    timing: Timing,
}

impl Playback {
    fn new(image: ImageId) -> Self {
        Self {
            image,
            frame: 0,
            timing: Timing::Paused(None),
        }
    }

    fn next_delay(&mut self, image: &RenderImage, now: Instant, playing: bool) -> Option<Duration> {
        match (&self.timing, playing) {
            (Timing::Running { deadline, .. }, false) => {
                self.timing = Timing::Paused(Some(deadline.saturating_duration_since(now)));
                None
            }
            (Timing::Paused(_), false) => None,
            (Timing::Paused(remaining), true) => Some(
                remaining
                    .unwrap_or_else(|| image.delay(self.frame).into())
                    .max(MIN_DELAY),
            ),
            (Timing::Running { deadline, .. }, true) if now < *deadline => None,
            (Timing::Running { deadline, .. }, true) => {
                self.frame = (self.frame + 1) % image.frame_count();
                let duration = Duration::from(image.delay(self.frame)).max(MIN_DELAY);
                let remaining = (*deadline + duration).saturating_duration_since(now);
                // Re-anchor when even the next frame's deadline has passed.
                Some(if remaining.is_zero() {
                    duration
                } else {
                    remaining.max(MIN_DELAY)
                })
            }
        }
    }
}

pub(super) fn sticker(id: SharedString, owner: RetainedImage) -> impl gpui::IntoElement + use<> {
    canvas(
        move |bounds, window, cx| {
            window.with_global_id(id.into(), |id, window| {
                let data = owner.use_data(true, window, cx)?.ok()?;
                if data.frame_count() < 2 {
                    return (data.frame_count() == 1).then_some((data, 0));
                }
                let playing = window.is_visible()
                    && window.is_window_active()
                    && !cx.reduce_motion()
                    && !bounds.is_empty()
                    && !window
                        .content_mask()
                        .bounds
                        .intersect(&ObjectFit::Contain.get_bounds(bounds, data.size(0)))
                        .is_empty();
                let view = window.current_view();
                let frame = window.with_element_state(id, |state: Option<Playback>, window| {
                    let mut state = state
                        .filter(|state| state.image == data.id)
                        .unwrap_or_else(|| Playback::new(data.id));
                    let now = Instant::now();
                    if let Some(delay) = state.next_delay(&data, now, playing) {
                        let deadline = now + delay;
                        state.timing = Timing::Running {
                            deadline,
                            _wake: window.spawn(cx, async move |cx| {
                                let mut remaining =
                                    deadline.saturating_duration_since(Instant::now());
                                while !remaining.is_zero() {
                                    let delay = remaining.max(MIN_DELAY);
                                    #[cfg(not(test))]
                                    crate::platform::clock::sleep(delay).await;
                                    // The GPUI test scheduler rejects wakes from the OS timer thread.
                                    #[cfg(test)]
                                    cx.background_executor().timer(delay).await;
                                    remaining = deadline.saturating_duration_since(Instant::now());
                                }
                                cx.update(|_, cx| cx.notify(view)).ok();
                            }),
                        };
                    }
                    (state.frame, state)
                });
                Some((data, frame))
            })
        },
        |bounds, frame: Option<(Arc<RenderImage>, usize)>, window, _| {
            if let Some((image, frame)) = frame.filter(|_| !bounds.is_empty()) {
                let image_bounds = ObjectFit::Contain.get_bounds(bounds, image.size(frame));
                if let Err(error) = window.paint_image(
                    bounds,
                    image_bounds,
                    Corners::default(),
                    image,
                    frame,
                    false,
                ) {
                    log::error!("could not paint sticker: {error}");
                }
            }
        },
    )
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> RenderImage {
        let frame = |ms| {
            image::Frame::from_parts(
                image::RgbaImage::new(1, 1),
                0,
                0,
                image::Delay::from_numer_denom_ms(ms, 1),
            )
        };
        RenderImage::new(smallvec::smallvec![frame(1000), frame(250)])
    }

    fn arm(state: &mut Playback, now: Instant, delay: Duration) {
        state.timing = Timing::Running {
            deadline: now + delay,
            _wake: Task::ready(()),
        };
    }

    #[test]
    fn waits_for_the_deadline_and_advances_at_most_one_frame_after_a_delay() {
        let image = image();
        let mut state = Playback::new(image.id);
        let start = Instant::ZERO;
        let first = Duration::from_secs(1);
        let second = Duration::from_millis(250);
        assert_eq!(state.next_delay(&image, start, true), Some(first));
        arm(&mut state, start, first);
        assert_eq!(state.next_delay(&image, start + second, true), None);
        assert_eq!(state.frame, 0);

        let late = start + Duration::from_secs(60);
        assert_eq!(state.next_delay(&image, late, true), Some(second));
        assert_eq!(state.frame, 1);
        arm(&mut state, late, second);
        assert_eq!(state.next_delay(&image, late + second, true), Some(first));
        assert_eq!(state.frame, 0);
    }

    #[test]
    fn a_short_repaint_delay_does_not_accumulate_between_frames() {
        let image = image();
        let mut state = Playback::new(image.id);
        let start = Instant::ZERO;
        let first = Duration::from_secs(1);
        arm(&mut state, start, first);
        assert_eq!(
            state.next_delay(&image, start + first + Duration::from_millis(16), true),
            Some(Duration::from_millis(234))
        );
        assert_eq!(state.frame, 1);
    }

    #[test]
    fn pause_cancels_the_deadline_and_resume_preserves_the_remaining_delay() {
        let image = image();
        let mut state = Playback::new(image.id);
        let start = Instant::ZERO;
        let first = Duration::from_secs(1);
        arm(&mut state, start, first);

        assert_eq!(state.next_delay(&image, start + first / 4, false), None);
        assert!(matches!(state.timing, Timing::Paused(_)));
        assert_eq!(
            state.next_delay(&image, start + Duration::from_secs(60), true),
            Some(first * 3 / 4)
        );
        assert_eq!(state.frame, 0);
    }

    #[cfg(not(target_family = "wasm"))]
    #[gpui::test]
    fn dropping_playback_cancels_the_pending_wake(cx: &mut gpui::TestAppContext) {
        use std::{cell::Cell, rc::Rc};

        struct WakeGuard(Rc<Cell<bool>>);
        impl Drop for WakeGuard {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        let dropped = Rc::new(Cell::new(false));
        let guard = WakeGuard(dropped.clone());
        let image = image();
        let mut state = Playback::new(image.id);
        state.timing = Timing::Running {
            deadline: Instant::ZERO + Duration::from_secs(1),
            _wake: cx.update(|cx| {
                cx.spawn(async move |_| {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                })
            }),
        };
        cx.run_until_parked();
        assert!(!dropped.get());
        drop(state);
        cx.run_until_parked();
        assert!(dropped.get());
    }
}
