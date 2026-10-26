//! Evidence that a sink advanced, independent of seeking or decoder completion.
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub(crate) struct PlaybackStart {
    observed: AtomicBool,
    reported: AtomicBool,
}

impl PlaybackStart {
    #[cfg(any(not(target_family = "wasm"), test))]
    pub(crate) fn samples(&self, consumed: usize) {
        if consumed > 0 {
            self.observed.store(true, Ordering::Release);
        }
    }

    #[cfg(any(target_family = "wasm", test))]
    pub(crate) fn clock(&self, playing: bool, now: f64, started_at: f64) {
        if playing && now > started_at {
            self.observed.store(true, Ordering::Release);
        }
    }

    pub(crate) fn take(&self, accepted: bool) -> bool {
        accepted
            && self.observed.load(Ordering::Acquire)
            && !self.reported.swap(true, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_requires_consumed_samples_and_an_accepted_stream() {
        let start = PlaybackStart::default();
        assert!(!start.take(true)); // load or successful stream installation alone
        start.samples(0);
        assert!(!start.take(true)); // empty callback or seek to the end
        start.samples(24);
        assert!(!start.take(false)); // output start failed
        assert!(start.take(true));
        start.samples(48);
        assert!(!start.take(true)); // repeated callbacks and polling
    }

    #[test]
    fn web_requires_running_clock_and_reports_short_clips_after_end() {
        let start = PlaybackStart::default();
        start.clock(true, 5.0, 5.0); // suspended context, including seek offset
        start.clock(false, 6.0, 5.0); // paused or failed node
        assert!(!start.take(true));
        start.clock(true, 6.001, 6.0); // actual playing or natural ended callback
        assert!(start.take(true));
        start.clock(true, 8.0, 7.0); // resume or seek replaces the node
        assert!(!start.take(true));
        assert!(!PlaybackStart::default().take(true)); // stopped/new clip
    }
}
