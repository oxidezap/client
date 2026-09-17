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

use tokio::sync::watch;

/// One flag, broadcast to every account runtime rather than to a single
/// waiter.
///
/// A plain [`tokio::sync::Notify`] used to carry this: `notify_one` stores a
/// permit, so an ask that arrives before anybody is watching is not lost —
/// which matters whenever the daemon fails fast during startup — but it
/// wakes exactly one waiter. That was fine while there was one session task
/// to wake; a multi-account daemon has one per live account, all of which
/// have to stop on the same ask. `watch` keeps both properties at once: the
/// value survives with zero receivers (an early `request` is not lost), and
/// every receiver — however many are subscribed, and however late a new one
/// subscribes — observes the same `true`.
struct ShutdownSignal {
    tx: watch::Sender<bool>,
}

impl ShutdownSignal {
    fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self { tx }
    }

    /// Not `send`: that fails with zero receivers, which is exactly the
    /// startup window this exists to survive. `send_replace` sets the value
    /// unconditionally and is what `AccountRegistry`'s own snapshot channel
    /// uses for the same reason.
    fn request(&self) {
        self.tx.send_replace(true);
    }

    /// Resolve immediately if already requested, otherwise wait for it.
    ///
    /// Every call subscribes its own receiver, so any number of concurrent
    /// callers — one per live account runtime, plus `main`'s own signal
    /// handling — each see the flip independently rather than racing one
    /// shared waiter for it.
    async fn requested(&self) {
        let mut rx = self.tx.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                // The sender is `'static` in production; only reachable if a
                // test's own signal is dropped mid-wait.
                return;
            }
        }
    }
}

/// Raised once, waited on by every account runtime and by `main`'s own
/// signal handling.
static STOP: LazyLock<ShutdownSignal> = LazyLock::new(ShutdownSignal::new);

/// Ask this process to shut down.
pub fn request(reason: &str) {
    log::info!("shutdown requested: {reason}");
    STOP.request();
}

/// Resolve once somebody has asked, for however many callers are waiting.
pub async fn requested() {
    STOP.requested().await;
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

    /// One request wakes every concurrent waiter, not just one of them —
    /// the property multiple account runtimes now depend on to all stop on
    /// the same ask. A local signal, not the process-global one: the real
    /// `STOP` is `'static` and monotonic, so a test that set it would break
    /// `requested()` for every other test sharing this binary.
    #[tokio::test]
    async fn one_request_wakes_every_waiter() {
        let signal = std::sync::Arc::new(ShutdownSignal::new());
        let waiters: Vec<_> = (0..4)
            .map(|_| {
                let signal = std::sync::Arc::clone(&signal);
                tokio::spawn(async move { signal.requested().await })
            })
            .collect();
        // Let every spawned task actually reach its `.await` before the
        // request lands, rather than racing the scheduler.
        tokio::task::yield_now().await;
        signal.request();

        // Bounded: with the `Notify::notify_one` semantics this replaced, at
        // most one of these would ever resolve and the rest would hang
        // forever rather than fail loudly.
        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for waiter in waiters {
                waiter.await.expect("a waiter task panicked");
            }
        })
        .await;
        assert!(
            joined.is_ok(),
            "one request() must resolve every concurrent requested() waiter"
        );

        // A waiter that subscribes *after* the request still resolves at
        // once — the value survived, exactly as `Notify::notify_one`'s
        // stored permit used to for the single waiter it served.
        signal.requested().await;
    }

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
