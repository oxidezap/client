//! What distinguishes this front end from another one on the same daemon.
//!
//! Both windows and tabs talk to one daemon, and the daemon broadcasts every
//! assignment it makes to all of them. Anything a front end mints locally and
//! then has renamed — an optimistic bubble, the placeholder an outgoing call
//! is drawn under — therefore has to carry something that says which front
//! end minted it, or one front end's answer lands on another's row.

/// A number that names this front end, drawn once and kept.
///
/// A process id on the desktop, where two windows are two processes. Two tabs
/// are *one* process — and in a browser, one that reports the same id in every
/// tab — so a random number stands in there.
#[must_use]
pub fn front_end_id() -> u64 {
    imp::front_end_id()
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    pub(super) fn front_end_id() -> u64 {
        u64::from(std::process::id())
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use portable_atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    /// Without this, two tabs starting their counters at zero would mint the
    /// same optimistic id within a millisecond of each other, and the daemon
    /// broadcasts every assignment to both — so one tab's send would rename
    /// or dedup the other's bubble.
    pub(super) fn front_end_id() -> u64 {
        static TAB: AtomicU64 = AtomicU64::new(0);
        let known = TAB.load(Ordering::Relaxed);
        if known != 0 {
            return known;
        }
        // The browser's own generator: seeded properly, and already reached
        // for by everything under `wacore` that needs randomness.
        let mut bytes = [0u8; 8];
        // A tab that cannot be told apart from another is worse than one
        // whose number is a clock reading, so a refused draw still produces
        // something rather than zero.
        let drawn = match getrandom::fill(&mut bytes) {
            Ok(()) => u64::from_le_bytes(bytes),
            Err(e) => {
                log::warn!("no randomness for this tab's id ({e}); using the clock");
                wacore::time::now_millis().cast_unsigned()
            }
        };
        // Never zero, which is the "not drawn yet" marker.
        let drawn = drawn | 1;
        TAB.store(drawn, Ordering::Relaxed);
        drawn
    }
}
