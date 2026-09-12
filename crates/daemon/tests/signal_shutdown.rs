//! The stop-signal contract behind issue #158.
//!
//! Shutting the machine down with the daemon up waited on the daemon to
//! close. The first SIGTERM or SIGINT has to drive the graceful shutdown —
//! stop accepting work, drain the connections, disconnect the session and
//! close the store — and a second one while that drains has to escalate to
//! a prompt exit on the conventional status rather than hang behind it.
//!
//! These drive that orchestration through the public gate without delivering
//! a real signal to the test runner: what is locked in is the decision each
//! arrival produces, which is the whole of the policy the binary acts on.

use oxidezap_daemon::shutdown::{Signal, SignalDecision, SignalGate};

/// A SIGTERM — what an operating-system shutdown sends — starts the graceful
/// shutdown rather than killing the process on the spot.
#[test]
fn sigterm_starts_a_graceful_shutdown() {
    assert_eq!(
        SignalGate::new().observe(Signal::Terminate),
        SignalDecision::Graceful
    );
}

/// A SIGINT — the terminal — starts the same graceful shutdown.
#[test]
fn sigint_starts_a_graceful_shutdown() {
    assert_eq!(
        SignalGate::new().observe(Signal::Interrupt),
        SignalDecision::Graceful
    );
}

/// A repeated signal while draining does not queue behind the teardown: it
/// escalates to a prompt exit, idempotently — holding the key down keeps
/// answering exit rather than wedging the decision or panicking on it.
#[test]
fn a_repeated_signal_escalates_idempotently() {
    let gate = SignalGate::new();
    assert_eq!(gate.observe(Signal::Terminate), SignalDecision::Graceful);
    assert_eq!(
        gate.observe(Signal::Terminate),
        SignalDecision::Escalate(128 + 15)
    );
    assert_eq!(
        gate.observe(Signal::Terminate),
        SignalDecision::Escalate(128 + 15)
    );
}

/// A second signal of the other kind is somebody asking twice just as much,
/// and exits on that signal's own conventional status.
#[test]
fn a_repeated_signal_of_the_other_kind_escalates_too() {
    let gate = SignalGate::new();
    assert_eq!(gate.observe(Signal::Interrupt), SignalDecision::Graceful);
    assert_eq!(
        gate.observe(Signal::Terminate),
        SignalDecision::Escalate(128 + 15)
    );
}

/// The escalation statuses are the conventional 128-plus-signal-number the
/// supervisor reads back, and the graceful decision carries no exit at all.
#[test]
fn escalation_exits_crash_free_on_the_conventional_status() {
    assert_eq!(Signal::Interrupt.escalation_code(), 128 + 2);
    assert_eq!(Signal::Terminate.escalation_code(), 128 + 15);
    // The graceful path returns to `main` normally — status zero — while the
    // escalated path is a plain exit status, never a panic or a core dump.
    let gate = SignalGate::new();
    assert!(matches!(
        gate.observe(Signal::Terminate),
        SignalDecision::Graceful
    ));
    assert!(matches!(
        gate.observe(Signal::Terminate),
        SignalDecision::Escalate(_)
    ));
}
