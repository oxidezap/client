//! The queue a front end with a reader thread drains.
//!
//! Bounded, and the reader filling it can afford to wait: it is a thread of
//! its own, so a queue with no room is a reason to stop reading rather than a
//! way to deadlock. Nothing else can wait, because nothing else is ever handed
//! the end that is able to.

use tokio::sync::mpsc::error::TrySendError;

use super::{Dropped, Events, FrameReady};
use crate::session::FromDaemon;

/// How many session events may wait for a UI that is busy drawing.
///
/// Bounded on purpose. Unbounded, a stalled window keeps draining the socket
/// and buffering everything the account does, so the daemon sees a reader that
/// is keeping up and never truncates it — and this side grows without limit.
/// Bounded, the reader stops reading, the daemon's own bounded broadcast
/// overruns, and it says `Resync`. That is the recovery this protocol already
/// has; the point is to reach it rather than to hide from it.
const EVENT_QUEUE: usize = 512;

/// The half the front end drains.
pub(super) type Queue = tokio::sync::mpsc::Receiver<FromDaemon>;

/// The half the reader publishes on, and the only one that can wait.
///
/// Not `Clone`: there is one reader, it is the thread this was made for, and
/// being able to block belongs to that thread alone. Everything else gets
/// [`Self::ui`].
pub struct ReaderSink(tokio::sync::mpsc::Sender<FromDaemon>, FrameReady);

/// The half everything on the UI executor publishes on.
///
/// A separate type rather than a rule about which method to call, which is
/// the whole point: the executor holding one is the executor draining the
/// queue, so a publish that waited for room would park the only thread that
/// could make any. There is no method here that could wait.
#[derive(Clone)]
pub struct UiSink(tokio::sync::mpsc::Sender<FromDaemon>, FrameReady);

/// The pair, sized for this platform.
#[must_use]
pub fn channel() -> (ReaderSink, Events) {
    let (tx, rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);
    let (frames, ready) = tokio::sync::mpsc::channel(1);
    (
        ReaderSink(tx, FrameReady(frames)),
        Events {
            ordinary: rx,
            frames: ready,
            ordinary_batch: 0,
        },
    )
}

impl ReaderSink {
    /// Publish, waiting for room.
    ///
    /// Fails only when the front end has gone, which is the reader's signal
    /// to stop.
    ///
    /// # Panics
    ///
    /// This is `blocking_send`, so it panics if it is reached from inside an
    /// async runtime's worker. The one caller is a reader on a thread of its
    /// own, which is what holding one of these means.
    pub fn send(&self, event: FromDaemon) -> Result<(), ()> {
        if matches!(event, FromDaemon::CallFrames) {
            return self.1.send().map_err(|_| ());
        }
        self.0.blocking_send(event).map_err(|_| ())
    }

    /// The end for everything that runs on the UI executor.
    pub fn ui(&self) -> UiSink {
        UiSink(self.0.clone(), self.1.clone())
    }
}

impl UiSink {
    /// Publish without ever waiting.
    ///
    /// For the paths that run *on* the UI executor — a send that failed
    /// before it left this process, a status view that never went out, a
    /// nudge saying a call picture is waiting. Video readiness coalesces on
    /// its own channel. A full ordinary queue drops the event and says so.
    pub fn try_send(&self, event: FromDaemon) -> Result<(), Dropped> {
        if matches!(event, FromDaemon::CallFrames) {
            return self.1.send();
        }
        match self.0.try_send(event) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                log::error!("the window is behind: a session event was dropped");
                Err(Dropped::Full)
            }
            Err(TrySendError::Closed(_)) => {
                log::debug!("nothing is listening for session events any more");
                Err(Dropped::Gone)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Dropped, EVENT_QUEUE, channel};
    use crate::session::{Fault, FromDaemon};

    #[test]
    fn delayed_last_frame_survives_an_ordinary_queue_full() {
        let (reader, mut events) = channel();
        let ui = reader.ui();
        for _ in 0..EVENT_QUEUE {
            assert_eq!(ui.try_send(FromDaemon::ShowWindow), Ok(()));
        }
        let decoder = std::thread::spawn(move || ui.try_send(FromDaemon::CallFrames));
        assert_eq!(decoder.join().unwrap(), Ok(()));
        let mut frames = 0;
        let mut ordinary = 0;
        while let Ok(event) = events.try_recv() {
            match event {
                FromDaemon::CallFrames => frames += 1,
                FromDaemon::ShowWindow => ordinary += 1,
                _ => panic!("unexpected event"),
            }
        }
        assert_eq!(ordinary, EVENT_QUEUE);
        assert_eq!(frames, 1);
    }

    /// A publish from the executor's end never waits, and says what it lost.
    ///
    /// The hazard this type split exists for cannot be reproduced here — it
    /// needs the GPUI executor, and a deadlock is the absence of an event
    /// rather than one. What is checkable is the property the split rests on:
    /// this end returns rather than parking, whatever the queue looks like.
    #[test]
    fn a_ui_publish_on_a_full_queue_drops_rather_than_waits() {
        let (reader, mut events) = channel();
        let ui = reader.ui();
        for _ in 0..EVENT_QUEUE {
            assert_eq!(ui.try_send(FromDaemon::ShowWindow), Ok(()));
        }
        // No room, and this is the call the UI executor makes. It returns.
        assert_eq!(ui.try_send(FromDaemon::ShowWindow), Err(Dropped::Full));
        // Draining one makes room for one.
        assert!(events.blocking_recv().is_some());
        assert_eq!(ui.try_send(FromDaemon::ShowWindow), Ok(()));
    }

    /// And a front end that has gone is the other answer, not the same one.
    #[test]
    fn a_ui_publish_with_nobody_listening_says_gone() {
        let (reader, events) = channel();
        let ui = reader.ui();
        drop(events);
        assert_eq!(ui.try_send(FromDaemon::ShowWindow), Err(Dropped::Gone));
    }

    /// The reader's end waits for room instead of losing the frame.
    ///
    /// Which is the half of this that must keep working: the reader is a
    /// thread of its own, and back pressure onto it is how the daemon learns
    /// this side is behind.
    #[test]
    fn a_reader_publish_waits_for_room() {
        let (reader, mut events) = channel();
        for _ in 0..EVENT_QUEUE {
            assert!(reader.send(FromDaemon::ShowWindow).is_ok());
        }
        let reading = std::thread::spawn(move || {
            // Parks: the queue is full and nothing has drained it yet.
            let ended = FromDaemon::Ended(Fault::unreachable("done"));
            assert!(reader.send(ended).is_ok());
        });
        for _ in 0..EVENT_QUEUE {
            assert!(events.blocking_recv().is_some());
        }
        reading.join().expect("the reader's publish returned");
        // The frame it was holding arrived rather than being dropped.
        assert!(matches!(events.blocking_recv(), Some(FromDaemon::Ended(_))));
    }
}
