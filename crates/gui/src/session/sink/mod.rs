//! Where the reader puts what it has read.
//!
//! Two publisher types, because the two publishers are not the same kind of
//! thing. The reader is one of them: a native front end reads on a thread of
//! its own, so a full queue is a reason to *stop reading* — the daemon then
//! overruns its own bounded broadcast and says `Resync`, which is the
//! recovery this protocol already has. Everything else that publishes runs
//! *on* the UI executor, and that executor is the thing that drains this
//! queue: waiting for room there parks the only thread that could make any,
//! and the window stops with no error and nothing in the log.
//!
//! Which of the two a caller was got enforced by comments, and one wrong call
//! in a later edit was a hung window. It is enforced by the types now. The
//! reader is handed a [`ReaderSink`], which is the only thing that can wait
//! and is not `Clone` — there is one reader. Everything above it is handed a
//! [`UiSink`], which cannot wait because it has no method that could, and is
//! cloned into every caller that needs one. A [`UiSink`] comes from
//! [`ReaderSink::ui`] and from nowhere else, so the executor's half is always
//! a view onto the reader's channels.
//!
//! Video readiness has its own capacity-one channel. A full ordinary queue
//! need not contain a video notification, and the last picture has no later
//! frame to wake the window on its behalf.
//!
//! A page has one thread and it is that same thread again, so the queue is
//! unbounded there and neither end can wait anyway. The split still holds on
//! both, because nothing above this may learn which one it is on — so the two
//! files are the same two types with the same two methods, and what they
//! disagree about is only what happens when the queue is full.
//!
//! The platform modules own the ordinary queues. Receiving and video
//! readiness are shared.

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(target_family = "wasm")]
mod web;

#[cfg(not(target_family = "wasm"))]
use native::Queue;
#[cfg(not(target_family = "wasm"))]
pub use native::{ReaderSink, UiSink, channel};
#[cfg(target_family = "wasm")]
use web::Queue;
#[cfg(target_family = "wasm")]
pub use web::{ReaderSink, UiSink, channel};

use crate::session::FromDaemon;
use tokio::sync::mpsc;

/// Ordinary events and coalesced frame readiness.
pub struct Events {
    ordinary: Queue,
    frames: mpsc::Receiver<()>,
    ordinary_batch: usize,
}

const ORDINARY_BATCH: usize = 16;

impl Events {
    pub async fn recv(&mut self) -> Option<FromDaemon> {
        std::future::poll_fn(|cx| {
            use std::task::Poll;
            // Ordinary events retain FIFO order, but cannot postpone video forever.
            if self.ordinary_batch == ORDINARY_BATCH {
                self.ordinary_batch = 0;
                if self.frames.try_recv().is_ok() {
                    return Poll::Ready(Some(FromDaemon::CallFrames));
                }
            }
            let ordinary = self.ordinary.poll_recv(cx);
            if let Poll::Ready(Some(event)) = ordinary {
                self.ordinary_batch += 1;
                return Poll::Ready(Some(event));
            }
            let frames = self.frames.poll_recv(cx);
            if let Poll::Ready(Some(())) = frames {
                self.ordinary_batch = 0;
                return Poll::Ready(Some(FromDaemon::CallFrames));
            }
            if ordinary.is_ready() && frames.is_ready() {
                Poll::Ready(None)
            } else {
                Poll::Pending
            }
        })
        .await
    }

    #[cfg(test)]
    pub fn try_recv(&mut self) -> Result<FromDaemon, mpsc::error::TryRecvError> {
        if self.ordinary_batch == ORDINARY_BATCH {
            self.ordinary_batch = 0;
            if self.frames.try_recv().is_ok() {
                return Ok(FromDaemon::CallFrames);
            }
        }
        let ordinary = self.ordinary.try_recv();
        if ordinary.is_ok() {
            self.ordinary_batch += 1;
            return ordinary;
        }
        match self.frames.try_recv() {
            Ok(()) => {
                self.ordinary_batch = 0;
                Ok(FromDaemon::CallFrames)
            }
            Err(mpsc::error::TryRecvError::Disconnected) => ordinary,
            Err(error) => Err(error),
        }
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub fn blocking_recv(&mut self) -> Option<FromDaemon> {
        futures_lite::future::block_on(self.recv())
    }
}

#[derive(Clone)]
struct FrameReady(mpsc::Sender<()>);

impl FrameReady {
    fn send(&self) -> Result<(), Dropped> {
        match self.0.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(())) => Err(Dropped::Gone),
        }
    }
}

/// An event that was not published, and why.
///
/// Returned rather than swallowed, because the two reasons are not the same
/// news. `Gone` is a front end that has ended, and is the ordinary way a
/// connection stops mattering. `Full` is a window that fell far enough behind
/// that something it was going to be told — a message that failed to send, a
/// page of history that never came — went on the floor, and whatever is
/// waiting on it waits for good. Nothing above can do better than dropping
/// it: the alternative is waiting on the thread that drains the queue. So
/// this exists to be *said*, which [`UiSink::try_send`] does before handing
/// it back, and to let a test tell the two apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// Nobody is draining this queue any more.
    Gone,
    /// The front end is behind and the queue has no room. Desktop only: a
    /// page's queue is unbounded, so it can only fail the other way.
    #[cfg_attr(
        target_family = "wasm",
        expect(dead_code, reason = "a page's queue has no ceiling to hit")
    )]
    Full,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::FromDaemon;

    #[test]
    fn replenished_ordinary_traffic_cannot_starve_a_pending_frame() {
        let (reader, mut events) = channel();
        let ui = reader.ui();
        assert_eq!(ui.try_send(FromDaemon::CallFrames), Ok(()));
        assert_eq!(ui.try_send(FromDaemon::ShowWindow), Ok(()));
        for _ in 0..=ORDINARY_BATCH {
            match futures_lite::future::block_on(events.recv()) {
                Some(FromDaemon::CallFrames) => return,
                Some(FromDaemon::ShowWindow) => {
                    assert_eq!(ui.try_send(FromDaemon::ShowWindow), Ok(()));
                }
                _ => panic!("unexpected event"),
            }
        }
        panic!("ordinary traffic starved video readiness");
    }

    #[test]
    fn yielding_to_video_preserves_ordinary_fifo_order() {
        let (reader, mut events) = channel();
        let ui = reader.ui();
        for id in 0..ORDINARY_BATCH * 2 {
            assert_eq!(
                ui.try_send(FromDaemon::StatusViewLost(vec![id.to_string()])),
                Ok(())
            );
        }
        assert_eq!(ui.try_send(FromDaemon::CallFrames), Ok(()));
        for id in 0..ORDINARY_BATCH * 2 {
            if id == ORDINARY_BATCH {
                assert!(matches!(
                    futures_lite::future::block_on(events.recv()),
                    Some(FromDaemon::CallFrames)
                ));
            }
            match futures_lite::future::block_on(events.recv()) {
                Some(FromDaemon::StatusViewLost(ids)) => assert_eq!(ids, vec![id.to_string()]),
                _ => panic!("ordinary events changed order"),
            }
        }
    }

    #[test]
    fn frame_readiness_is_capacity_one_and_rearms_after_receiving() {
        let (reader, mut events) = channel();
        let ui = reader.ui();
        for _ in 0..100 {
            assert_eq!(ui.try_send(FromDaemon::CallFrames), Ok(()));
        }
        assert!(matches!(events.try_recv(), Ok(FromDaemon::CallFrames)));
        assert!(events.try_recv().is_err());
        assert_eq!(ui.try_send(FromDaemon::CallFrames), Ok(()));
        assert!(matches!(events.try_recv(), Ok(FromDaemon::CallFrames)));
        drop(events);
        assert_eq!(ui.try_send(FromDaemon::CallFrames), Err(Dropped::Gone));
    }

    #[test]
    fn a_delayed_frame_wakes_a_pending_receiver() {
        use std::future::Future;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll, Wake, Waker};

        struct Wakes(AtomicUsize);
        impl Wake for Wakes {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let (reader, mut events) = channel();
        let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wakes));
        let mut cx = Context::from_waker(&waker);
        let mut receive = std::pin::pin!(events.recv());
        assert!(receive.as_mut().poll(&mut cx).is_pending());
        assert_eq!(reader.ui().try_send(FromDaemon::CallFrames), Ok(()));
        assert!(wakes.0.load(Ordering::Relaxed) > 0);
        assert!(matches!(
            receive.as_mut().poll(&mut cx),
            Poll::Ready(Some(FromDaemon::CallFrames))
        ));
    }

    #[test]
    fn ordinary_events_precede_readiness_and_both_channels_close() {
        let (reader, mut events) = channel();
        assert_eq!(reader.ui().try_send(FromDaemon::CallFrames), Ok(()));
        assert_eq!(reader.ui().try_send(FromDaemon::ShowWindow), Ok(()));
        drop(reader);
        assert!(matches!(events.try_recv(), Ok(FromDaemon::ShowWindow)));
        assert!(matches!(events.try_recv(), Ok(FromDaemon::CallFrames)));
        assert!(matches!(
            events.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        assert!(futures_lite::future::block_on(events.recv()).is_none());
    }
}
