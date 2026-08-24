//! Asking the daemon to stop, from somewhere that cannot stop it.
//!
//! The tray's "Quit" item and a client's [`ClientRequest::Shutdown`] both run
//! far from `main`'s teardown, and neither may end the process itself:
//! exiting from a D-Bus callback or a connection task would skip disconnecting
//! the session and closing SQLite. Both raise SIGTERM at this process instead,
//! which lands on the handler `main` already installs, so the daemon keeps
//! exactly one shutdown path however it is asked to leave.
//!
//! [`ClientRequest::Shutdown`]: oxidezap_ipc::ClientRequest::Shutdown

/// Ask this process to shut down, as if a service manager had asked.
pub fn request(reason: &str) {
    log::info!("shutdown requested: {reason}");

    #[cfg(unix)]
    {
        use rustix::process::{Signal, getpid, kill_process};
        // The only failure a signal to ourselves can report is a signal number
        // the kernel does not know, which SIGTERM is not. Logged rather than
        // propagated: the caller has no better answer than the daemon does.
        if let Err(e) = kill_process(getpid(), Signal::TERM) {
            log::error!("could not signal ourselves to stop: {e}");
        }
    }
}
