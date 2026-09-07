use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxidezap_core::VideoStream;
use wacore::time::Instant;

/// Nonblocking admission to the originating connection's request queue.
/// Retries do not depend on admission succeeding or on a daemon acknowledgement.
pub type RecoverySink = Arc<dyn Fn(&str, VideoStream) -> bool + Send + Sync>;

pub struct Recovery {
    call_id: String,
    stream: VideoStream,
    sink: RecoverySink,
    attempted: Mutex<Option<Instant>>,
}

impl Recovery {
    pub fn new(call_id: String, stream: VideoStream, sink: RecoverySink) -> Self {
        Self {
            call_id,
            stream,
            sink,
            attempted: Mutex::new(None),
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
        let _ = (self.sink)(&self.call_id, self.stream);
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
        assert_eq!(
            *requests.lock().unwrap(),
            vec![("old-call".into(), VideoStream::Remote); 2]
        );
    }
}
