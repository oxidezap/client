//! Asking the daemon to stop, from somewhere that cannot stop it.
//!
//! The tray's "Quit" item and a client's [`ClientRequest::Shutdown`] both run
//! far from `main`'s teardown, and neither may end the process itself:
//! exiting from a D-Bus callback or a connection task would skip disconnecting
//! the session and closing SQLite.
//!
//! So they ask instead, and `main` is the only thing that acts. A signal was
//! the obvious way to carry that ask — the daemon already had to handle
//! SIGTERM for a service manager — but a signal is not something Windows has,
//! which would have left an IPC `Shutdown` inert there. One in-process
//! notification serves both, and the signal handler now feeds the same one
//! rather than being a second route to the same place.
//!
//! [`ClientRequest::Shutdown`]: oxidezap_ipc::ClientRequest::Shutdown

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// Raised once, waited on by `main`.
///
/// `notify_one` stores a permit, so an ask that arrives before `main` is
/// watching is not lost — which is the case whenever the daemon fails fast
/// during startup.
static STOP: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Ask this process to shut down.
pub fn request(reason: &str) {
    log::info!("shutdown requested: {reason}");
    STOP.notify_one();
}

/// Resolve once somebody has asked.
pub async fn requested() {
    STOP.notified().await;
}

/// The outside signal that asked this process to stop.
///
/// The daemon binary maps what the operating system delivers onto this; the
/// gate below decides what a repeated one means. Carried explicitly rather
/// than as a raw number so the escalation code beside it cannot drift from
/// the signal it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGINT, Ctrl-C where there are no signals: the terminal.
    Interrupt,
    /// SIGTERM: the service manager, and what an operating-system shutdown
    /// sends.
    Terminate,
}

impl Signal {
    /// The process exit status when this signal ends the run without the
    /// graceful teardown running to completion: 128 plus the signal number,
    /// which is the convention a supervisor reads back.
    pub fn escalation_code(self) -> i32 {
        match self {
            Signal::Interrupt => 128 + 2,
            Signal::Terminate => 128 + 15,
        }
    }

    /// The name the log line carries, so a repeated signal reads as one.
    pub fn name(self) -> &'static str {
        match self {
            Signal::Interrupt => "SIGINT",
            Signal::Terminate => "SIGTERM",
        }
    }
}

/// What a caught signal means, given what came before it.
///
/// The first one drives the graceful shutdown: stop accepting work, drain
/// the connections, disconnect the session and close the store. One that
/// arrives while that is still running escalates instead — the teardown is
/// abandoned and the process exits on the status beside it — because a
/// second signal is somebody saying the first one is taking too long, and
/// answering that by waiting longer is the hang the handler exists to
/// prevent. The teardown has joins without a deadline (a reload in flight,
/// the plugin retire, the publisher drain), which is what makes "still
/// running" reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalDecision {
    /// Drive the graceful shutdown.
    Graceful,
    /// Leave the teardown unfinished and exit on the status carried here.
    Escalate(i32),
}

/// Counts caught signals so the first drives teardown and the second ends it.
///
/// A struct rather than a flag the binary keeps because the policy is what
/// the tests hold onto: constructed per test, driven without delivering a
/// real signal to the test runner, and asserting the decision rather than
/// the exit. One atomic swap, so observing can neither block nor panic —
/// there is no lock to poison and no channel to close on this path.
#[derive(Debug, Default)]
pub struct SignalGate {
    seen: AtomicBool,
}

impl SignalGate {
    /// A gate that has seen nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one caught signal.
    ///
    /// The first observation answers [`SignalDecision::Graceful`]; every one
    /// after it answers [`SignalDecision::Escalate`], whichever signal each
    /// was — SIGTERM then SIGINT is somebody escalating just as much as
    /// SIGTERM twice.
    pub fn observe(&self, signal: Signal) -> SignalDecision {
        if self.seen.swap(true, Ordering::SeqCst) {
            SignalDecision::Escalate(signal.escalation_code())
        } else {
            SignalDecision::Graceful
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The terminal's signal starts the graceful shutdown.
    #[test]
    fn the_first_sigint_is_graceful() {
        assert_eq!(
            SignalGate::new().observe(Signal::Interrupt),
            SignalDecision::Graceful
        );
    }

    /// The service manager's signal starts the graceful shutdown.
    #[test]
    fn the_first_sigterm_is_graceful() {
        assert_eq!(
            SignalGate::new().observe(Signal::Terminate),
            SignalDecision::Graceful
        );
    }

    /// A repeated SIGTERM does not wait out the teardown: it exits on the
    /// conventional status for a process ended by SIGTERM.
    #[test]
    fn a_second_sigterm_escalates() {
        let gate = SignalGate::new();
        assert_eq!(gate.observe(Signal::Terminate), SignalDecision::Graceful);
        assert_eq!(
            gate.observe(Signal::Terminate),
            SignalDecision::Escalate(128 + 15)
        );
    }

    /// A repeated SIGINT exits on the conventional status for a process
    /// ended by SIGINT.
    #[test]
    fn a_second_sigint_escalates() {
        let gate = SignalGate::new();
        assert_eq!(gate.observe(Signal::Interrupt), SignalDecision::Graceful);
        assert_eq!(
            gate.observe(Signal::Interrupt),
            SignalDecision::Escalate(128 + 2)
        );
    }

    /// Escalation is about the repetition, not the sameness: SIGTERM then
    /// SIGINT is somebody asking twice, and answers with the second
    /// signal's status.
    #[test]
    fn a_different_second_signal_escalates_too() {
        let gate = SignalGate::new();
        assert_eq!(gate.observe(Signal::Terminate), SignalDecision::Graceful);
        assert_eq!(
            gate.observe(Signal::Interrupt),
            SignalDecision::Escalate(128 + 2)
        );
    }

    /// And the mirror: SIGINT then SIGTERM answers with SIGTERM's status.
    #[test]
    fn sigint_then_sigterm_escalates_with_sigterms_status() {
        let gate = SignalGate::new();
        assert_eq!(gate.observe(Signal::Interrupt), SignalDecision::Graceful);
        assert_eq!(
            gate.observe(Signal::Terminate),
            SignalDecision::Escalate(128 + 15)
        );
    }

    /// Escalation is stable: further signals keep answering exit rather
    /// than returning to graceful or panicking, so holding the key down
    /// cannot wedge the decision itself.
    #[test]
    fn further_signals_stay_escalated() {
        let gate = SignalGate::new();
        assert_eq!(gate.observe(Signal::Terminate), SignalDecision::Graceful);
        assert_eq!(
            gate.observe(Signal::Terminate),
            SignalDecision::Escalate(128 + 15)
        );
        assert_eq!(
            gate.observe(Signal::Interrupt),
            SignalDecision::Escalate(128 + 2)
        );
    }
}
