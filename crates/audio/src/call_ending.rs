//! Which half of a call's audio the engine let go of, and what that means.
//!
//! A call's devices are handed to the library as two channel ends: a receiver
//! it takes microphone frames from, and a sender it puts the peer's audio
//! into. Nothing here holds the call open — the *engine* does, by keeping
//! those two ends alive — so this side learns a call is over by watching them
//! go.
//!
//! That makes the ending a piece of evidence rather than a formality. An
//! engine that runs a call and stops releases both ends when its driver
//! returns, and one that never really started releases them the same way at
//! the same moment. From inside the audio graph the two are identical, and
//! the only thing that tells them apart is what the transport saw in between.
//! Naming the ending is what lets a log line say which happened instead of
//! reporting a device that closed for no stated reason — which is exactly how
//! a browser call that ended a moment after it connected read for three
//! separate reports.
//!
//! Portable, and deliberately: the rule is about channel ends and holds on
//! both platforms, so it is stated once and tested where `cargo test` already
//! runs rather than only inside a browser.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What this side knows about one call's audio when a channel end goes.
///
/// Shared with whoever hands the endpoints to the engine, because the two
/// questions an ending turns on are both answered outside the graph: whether
/// the engine ever received these channels, and whether the capture end was
/// closed from here. Both are one bit, set once, read once.
///
/// Portable rather than a platform split: `Arc<AtomicBool>` is `Send` where a
/// desktop call task needs it to be and costs a browser nothing.
#[derive(Clone, Default, Debug)]
pub struct CallAudioFacts {
    handed_to_engine: Arc<AtomicBool>,
    capture_ended_locally: Arc<AtomicBool>,
}

impl CallAudioFacts {
    /// The engine has the endpoints; from here their release is its doing.
    ///
    /// Marked by the caller the moment the engine has accepted them — after
    /// a `start()` that returned, never before it, since a builder dropped on
    /// the way to one takes the endpoints with it and no driver ever ran.
    pub fn hand_to_engine(&self) {
        // One flag, no data published behind it, and nothing orders against
        // a second atomic: `Relaxed` is the whole requirement.
        self.handed_to_engine.store(true, Ordering::Relaxed);
    }

    /// This side is closing the capture channel because the track ended.
    pub fn capture_ended(&self) {
        self.capture_ended_locally.store(true, Ordering::Relaxed);
    }

    fn engine_has_them(&self) -> bool {
        self.handed_to_engine.load(Ordering::Relaxed)
    }

    fn microphone_went(&self) -> bool {
        self.capture_ended_locally.load(Ordering::Relaxed)
    }
}

/// Why one call's audio graph is being torn down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallAudioEnding {
    /// Nothing is putting the peer's audio anywhere any more.
    ///
    /// The engine dropped the sender this side plays out of. Reached whenever
    /// the driver stops, whether it ran a whole conversation or returned
    /// without ever using its transport.
    PlayoutReleased,
    /// Nothing is taking this microphone's frames any more.
    ///
    /// The engine dropped the receiver this side captures into.
    CaptureReleased,
    /// The microphone itself ended, and this side closed the channel.
    ///
    /// Unplugged, revoked in the site settings, or taken by the operating
    /// system. The engine may still be holding its end perfectly happily —
    /// what went is the device — so this is the one capture ending that is
    /// *not* evidence about the driver, and naming it as one would put the
    /// blame for a local fault on the far side of the call.
    CaptureLost,
    /// The endpoints were dropped before the engine ever received them.
    ///
    /// A call ended while its devices were opening — hung up here, hung up by
    /// the caller, or answered on another device — takes this exit, and the
    /// permission prompt in front of `getUserMedia` makes that window seconds
    /// long rather than instants. Nothing here is evidence about a driver:
    /// there was no driver. Naming it as one would put an ordinary
    /// cancellation in the same log line as the failure being hunted.
    NeverHandedOver,
}

impl CallAudioEnding {
    /// The half of the call this ending names, for a log line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlayoutReleased => "the call engine let go of the speaker",
            Self::CaptureReleased => "the call engine let go of the microphone",
            Self::CaptureLost => "this microphone ended; the call engine did not release it",
            Self::NeverHandedOver => "the call ended before its audio reached the engine",
        }
    }
}

/// Wait for the engine to let go of either half, and say which it was.
///
/// Both arms are the same ending as far as the devices are concerned — the
/// graph closes either way — so this exists for the name rather than for the
/// control flow. `playout` is biased only because a driver that returns drops
/// its whole channel set at once and something has to be reported first;
/// nothing downstream may depend on which of a simultaneous pair wins.
///
/// `facts` is what keeps the naming honest, and it answers two ways of
/// attributing to the far side something this side did.
///
/// A microphone that is unplugged or revoked is closed *by this side*, from
/// the track's own `ended` handler, and the sender closing is the same
/// observation whichever end let go. And endpoints dropped before the engine
/// received them — a call cancelled while its devices were opening — release
/// both ends at once in exactly the way a driver returning does. Neither is
/// evidence about a driver, and reporting either as one is the false
/// evidence this module exists to stop producing.
///
/// Both are read once an arm has won, never before: what matters is what was
/// true at the ending.
pub async fn ending(
    playout_released: impl Future<Output = ()>,
    capture_released: impl Future<Output = ()>,
    facts: &CallAudioFacts,
) -> CallAudioEnding {
    let released = futures_lite::future::or(
        async {
            playout_released.await;
            CallAudioEnding::PlayoutReleased
        },
        async {
            capture_released.await;
            if facts.microphone_went() {
                CallAudioEnding::CaptureLost
            } else {
                CallAudioEnding::CaptureReleased
            }
        },
    )
    .await;
    match released {
        // A device that went on this side is a fact about this side, and it
        // holds whether or not an engine ever had the endpoints. A microphone
        // unplugged while the *camera* was still opening is exactly that: the
        // call was not cancelled, the registry may go on to `start()`, and
        // the local loss is the only evidence there is. Letting the handoff
        // gate overwrite it would throw the specific answer away for a vaguer
        // one that is also wrong.
        CallAudioEnding::CaptureLost => CallAudioEnding::CaptureLost,
        // The rest name the engine, so they are asked over the arm rather
        // than beside it: an ending with no engine behind it says nothing
        // about which half went first, and answering that question anyway is
        // what would make a cancellation read as a fault.
        named if facts.engine_has_them() => named,
        _ => CallAudioEnding::NeverHandedOver,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One call's four channel ends: what this side keeps, and what it hands
    /// the engine.
    struct Endpoints {
        /// Held here; the capture callback writes into it.
        mic_tx: async_channel::Sender<Vec<i16>>,
        /// Handed to the engine, which takes microphone frames from it.
        mic_rx: async_channel::Receiver<Vec<i16>>,
        /// Handed to the engine, which puts the peer's audio into it.
        speaker_tx: async_channel::Sender<Vec<i16>>,
        /// Held here; the playout ring drains it.
        speaker_rx: async_channel::Receiver<Vec<i16>>,
    }

    /// A call whose endpoints reached the engine, which is every ending this
    /// module is evidence about.
    fn handed_over() -> CallAudioFacts {
        let facts = CallAudioFacts::default();
        facts.hand_to_engine();
        facts
    }

    fn endpoints() -> Endpoints {
        let (mic_tx, mic_rx) = async_channel::bounded::<Vec<i16>>(4);
        let (speaker_tx, speaker_rx) = async_channel::bounded::<Vec<i16>>(4);
        Endpoints {
            mic_tx,
            mic_rx,
            speaker_tx,
            speaker_rx,
        }
    }

    /// The reproduction, and the reason this module exists.
    ///
    /// A call engine whose driver returns immediately — before it has sent a
    /// single packet — drops the endpoint pair it was handed. From the audio
    /// graph's side that is indistinguishable from a call that ran and ended:
    /// both halves go at once. What this asserts is that it *is* reported,
    /// because a teardown nobody names is the failure that produced three
    /// evidence-free bug reports.
    #[test]
    fn a_driver_that_returns_without_using_the_call_still_ends_the_audio() {
        let Endpoints {
            mic_tx,
            mic_rx,
            speaker_tx,
            speaker_rx,
        } = endpoints();

        // The engine takes both ends and its driver returns at once.
        let engine = (mic_rx, speaker_tx);
        drop(engine);

        let ending = futures_lite::future::block_on(ending(
            async {
                // Nothing will ever play here again.
                while speaker_rx.recv().await.is_ok() {}
            },
            mic_tx.closed(),
            &handed_over(),
        ));
        assert_eq!(ending, CallAudioEnding::PlayoutReleased);
    }

    /// The microphone going on its own: the engine is still playing the peer,
    /// so only the capture end is released.
    #[test]
    fn a_released_microphone_is_named_as_one() {
        let Endpoints {
            mic_tx,
            mic_rx,
            speaker_tx,
            speaker_rx,
        } = endpoints();
        drop(mic_rx);

        let ending = futures_lite::future::block_on(ending(
            async { while speaker_rx.recv().await.is_ok() {} },
            mic_tx.closed(),
            &handed_over(),
        ));
        assert_eq!(ending, CallAudioEnding::CaptureReleased);
        // Held to the end, so the speaker arm could not have been what
        // resolved: a test that dropped it would pass for the wrong reason.
        drop(speaker_tx);
    }

    /// A microphone unplugged mid-call closes the channel from *this* side,
    /// which is the same observation as the engine dropping its receiver. The
    /// two must not read the same in a log: one is evidence about the driver
    /// and the other is a device that went away.
    #[test]
    fn a_microphone_that_ended_locally_is_not_blamed_on_the_engine() {
        let Endpoints {
            mic_tx,
            mic_rx,
            speaker_tx,
            speaker_rx,
        } = endpoints();

        // The engine still holds both of its ends, exactly as it would with
        // the call running; what closed the channel is the track's `ended`
        // handler on this side.
        let facts = handed_over();
        facts.capture_ended();
        mic_tx.close();

        let ending = futures_lite::future::block_on(ending(
            async { while speaker_rx.recv().await.is_ok() {} },
            mic_tx.closed(),
            &facts,
        ));
        assert_eq!(ending, CallAudioEnding::CaptureLost);
        drop((mic_rx, speaker_tx));
    }

    /// A call cancelled while its devices were opening drops the endpoints
    /// with no engine anywhere behind them. Both ends go at once, exactly as
    /// they do when a driver returns — and that is the whole hazard: an
    /// ordinary cancellation would otherwise be logged as the engine
    /// releasing a call it never held.
    #[test]
    fn endpoints_dropped_before_the_engine_saw_them_are_not_its_doing() {
        let Endpoints {
            mic_tx,
            mic_rx,
            speaker_tx,
            speaker_rx,
        } = endpoints();

        // Never handed over: the caller hung up while `getUserMedia` was
        // still in front of a permission prompt, so these are dropped on the
        // way to a `start()` that never happened.
        drop((mic_rx, speaker_tx));

        let ending = futures_lite::future::block_on(ending(
            async { while speaker_rx.recv().await.is_ok() {} },
            mic_tx.closed(),
            &CallAudioFacts::default(),
        ));
        assert_eq!(ending, CallAudioEnding::NeverHandedOver);
    }

    /// A microphone lost while the camera was still opening is still a lost
    /// microphone. The endpoints have not reached an engine yet, but nothing
    /// was cancelled either — the call may go on to start — and the local
    /// device is the only evidence there is, so the handoff gate may not
    /// overwrite it with a vaguer answer that is also wrong.
    #[test]
    fn a_microphone_lost_before_the_handoff_is_still_a_lost_microphone() {
        let Endpoints {
            mic_tx,
            mic_rx,
            speaker_tx,
            speaker_rx,
        } = endpoints();

        // Nothing has been handed over: the registry is still awaiting the
        // camera. The track ends anyway.
        let facts = CallAudioFacts::default();
        facts.capture_ended();
        mic_tx.close();

        let ending = futures_lite::future::block_on(ending(
            async { while speaker_rx.recv().await.is_ok() {} },
            mic_tx.closed(),
            &facts,
        ));
        assert_eq!(ending, CallAudioEnding::CaptureLost);
        drop((mic_rx, speaker_tx));
    }

    /// A call that is running holds both ends, and neither arm may resolve —
    /// the graph closing while a call is live is a microphone going dead
    /// mid-conversation.
    #[test]
    fn a_live_call_ends_nothing() {
        let Endpoints {
            mic_tx,
            mic_rx,
            speaker_tx,
            speaker_rx,
        } = endpoints();

        let ended = futures_lite::future::block_on(futures_lite::future::or(
            async {
                Some(
                    ending(
                        async { while speaker_rx.recv().await.is_ok() {} },
                        mic_tx.closed(),
                        &handed_over(),
                    )
                    .await,
                )
            },
            // The engine is alive and holding both; nothing should win, so
            // this arm is what the test actually finishes on.
            async { None },
        ));
        assert_eq!(ended, None, "a live call released a device");
        drop((mic_rx, speaker_tx));
    }
}
