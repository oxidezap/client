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
}

impl CallAudioEnding {
    /// The half of the call this ending names, for a log line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlayoutReleased => "the call engine let go of the speaker",
            Self::CaptureReleased => "the call engine let go of the microphone",
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
pub async fn ending(
    playout_released: impl Future<Output = ()>,
    capture_released: impl Future<Output = ()>,
) -> CallAudioEnding {
    futures_lite::future::or(
        async {
            playout_released.await;
            CallAudioEnding::PlayoutReleased
        },
        async {
            capture_released.await;
            CallAudioEnding::CaptureReleased
        },
    )
    .await
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
        ));
        assert_eq!(ending, CallAudioEnding::CaptureReleased);
        // Held to the end, so the speaker arm could not have been what
        // resolved: a test that dropped it would pass for the wrong reason.
        drop(speaker_tx);
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
