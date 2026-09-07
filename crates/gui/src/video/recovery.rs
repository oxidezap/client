use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxidezap_core::VideoStream;
use wacore::time::Instant;

#[path = "h264.rs"]
pub(super) mod h264;

/// Nonblocking admission to the originating connection's request queue.
/// Retries do not depend on admission succeeding or on a daemon acknowledgement.
pub type RecoverySink = Arc<dyn Fn(&str, VideoStream) -> bool + Send + Sync>;

#[derive(Default)]
struct Diagnostic {
    attempt: u64,
    admitted: Option<u64>,
    awaiting_output: bool,
}

pub struct Recovery {
    call_id: String,
    stream: VideoStream,
    sink: RecoverySink,
    attempted: Mutex<Option<Instant>>,
    diagnostic: Mutex<Diagnostic>,
}

impl Recovery {
    pub fn new(call_id: String, stream: VideoStream, sink: RecoverySink) -> Self {
        Self {
            call_id,
            stream,
            sink,
            attempted: Mutex::new(None),
            diagnostic: Mutex::new(Diagnostic::default()),
        }
    }

    pub fn request(&self) {
        self.request_at(Instant::now());
    }

    fn request_at(&self, now: Instant) {
        let mut attempted = self
            .attempted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if attempted
            .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(1))
        {
            return;
        }
        // Bound failed admissions too. Waiting delta units retry after the interval.
        *attempted = Some(now);
        drop(attempted);
        let attempt = {
            let mut state = self
                .diagnostic
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.attempt = state.attempt.saturating_add(1);
            state.attempt
        };
        let queued = (self.sink)(&self.call_id, self.stream);
        log::debug!(
            "video recovery call={} stream={:?} attempt={attempt} queued={queued}",
            self.call_id,
            self.stream
        );
    }

    pub fn admitted(&self) {
        let mut state = self
            .diagnostic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.admitted == Some(state.attempt) {
            return;
        }
        state.admitted = Some(state.attempt);
        state.awaiting_output = true;
        log::debug!(
            "video recovery call={} stream={:?} attempt={} IDR admitted",
            self.call_id,
            self.stream,
            state.attempt
        );
    }

    pub fn output(&self) {
        let mut state = self
            .diagnostic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.awaiting_output {
            return;
        }
        state.awaiting_output = false;
        log::debug!(
            "video recovery call={} stream={:?} attempt={} first output",
            self.call_id,
            self.stream,
            state.admitted.unwrap_or(0)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_coalesces_and_retries_failed_admission_with_original_identity() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = requests.clone();
        let recovery = Recovery::new(
            "old-call".into(),
            VideoStream::Remote,
            Arc::new(move |id, stream| {
                received.lock().unwrap().push((id.to_owned(), stream));
                false
            }),
        );
        let now = Instant::now();
        for _ in 0..20 {
            recovery.request_at(now);
        }
        recovery.request_at(now + Duration::from_millis(999));
        assert_eq!(requests.lock().unwrap().len(), 1);
        recovery.request_at(now + Duration::from_secs(1));
        recovery.request_at(now + Duration::from_millis(999));
        assert_eq!(
            *requests.lock().unwrap(),
            vec![("old-call".into(), VideoStream::Remote); 2]
        );
        recovery.admitted();
        assert!(recovery.diagnostic.lock().unwrap().awaiting_output);
        recovery.output();
        recovery.admitted();
        recovery.output();
        let state = recovery.diagnostic.lock().unwrap();
        assert_eq!(state.attempt, 2);
        assert_eq!(state.admitted, Some(2));
        assert!(!state.awaiting_output);
    }
}
