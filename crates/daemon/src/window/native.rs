//! Asking for a window, and starting one when there is nobody to ask.
//!
//! The daemon has no window of its own, so "Open" has always been a request
//! relayed to whoever owns one. That is the whole answer only while a front
//! end is attached — and the tray exists precisely for when one is not. A
//! request published to nobody left the menu item doing nothing at the one
//! moment it is worth clicking.
//!
//! Starting one here is the mirror of the front end starting a daemon it
//! cannot find (`session::connect_or_start`): the two binaries ship in one
//! directory and each knows how to reach for the other.

use std::process::{Child, Command};
use std::sync::{Mutex, PoisonError};

use oxidezap_ipc::DaemonMessage;

use crate::state::StateHub;

/// The front end this daemon started, kept only to tell "still coming up"
/// from "gone".
///
/// Clicking Open twice is the case it answers: the first launch takes a
/// moment to attach, so during that moment there is still no signal receiver
/// and the naive answer is to launch a second window. Asking the child
/// whether it is alive needs no clock and reaps it when it is not.
static LAUNCHED: Mutex<Option<Child>> = Mutex::new(None);

/// Raise the front end's window, starting a front end if none is attached.
///
/// The request goes out first, always: a front end that owns a window raises
/// it, which is cheaper and less surprising than a second process. Only when
/// nobody owns one does this become a launch.
///
/// Who owns one is what the clients said in their hello, not who is
/// subscribed: every client reads the signal channel, so a TUI or a notifier
/// watching summaries would otherwise stand in for a window that is not
/// there — and the tray's Open would go back to doing nothing.
pub fn show(hub: &StateHub) {
    let Some(program) = front_end_program() else {
        // Said rather than guessed at: with nothing beside this binary there
        // is no front end this daemon ships with, and the answer is to name
        // one rather than to search the environment for something called
        // `oxidezap`.
        hub.signal(&DaemonMessage::ShowWindow);
        log::warn!(
            "no front end beside this binary; set {FRONT_END_ENV} to name one \
             if there is no window to raise"
        );
        return;
    };
    show_program(hub, &program);
}

/// [`show`], with the program named. Split so a test can point it somewhere
/// harmless: the decision worth exercising is when a launch happens, not
/// which binary it is.
fn show_program(hub: &StateHub, program: &std::path::Path) {
    hub.signal(&DaemonMessage::ShowWindow);
    if hub.windows_attached() {
        return;
    }

    let mut launched = LAUNCHED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(child) = launched.as_mut() {
        match child.try_wait() {
            // Alive but not attached yet: this *is* the window being opened.
            Ok(None) => {
                log::debug!("a front end is already starting; not starting another");
                return;
            }
            // `try_wait` is also what reaps it, which is why the handle is
            // kept at all: nothing else here would ever call `wait`.
            Ok(Some(status)) => log::debug!("the front end we started has exited ({status})"),
            Err(e) => log::debug!("could not tell whether the front end is still running: {e}"),
        }
    }

    match Command::new(program).spawn() {
        Ok(child) => {
            log::info!("no front end attached; started {}", program.display());
            *launched = Some(child);
        }
        Err(e) => {
            log::error!("could not start {}: {e}", program.display());
            *launched = None;
        }
    }
}

/// The environment variable that names a front end other than the one that
/// ships beside the daemon.
///
/// The daemon serves front ends and knows nothing about them, which is the
/// point of the split — so the one name it does have to know is the one thing
/// worth making overridable. A TUI, or a second GUI, says here what to start;
/// everyone else gets the pair the release packages together.
const FRONT_END_ENV: &str = "OXIDEZAP_FRONT_END";

/// Where to find the front end.
///
/// [`FRONT_END_ENV`] first, then beside this binary: the two ship together
/// and a release directory is not on anybody's `PATH`. The mirror of the
/// front end's own `daemon_program`.
///
/// `None` rather than the bare name, which is the whole of this function's
/// history: a name with no directory in it is resolved through `PATH`, and
/// the daemon inherits the environment of whoever started it — a tray icon
/// launched from a session where `PATH` had been arranged runs whatever that
/// arranged. Same user, so nothing is escalated; what it is is a silent
/// execution path, and a release directory is not on anybody's `PATH`
/// anyway. A `cargo`-run build finds its sibling in `target/debug` without
/// any of this.
fn front_end_program() -> Option<std::path::PathBuf> {
    const NAME: &str = if cfg!(windows) {
        "oxidezap.exe"
    } else {
        "oxidezap"
    };
    if let Some(named) = std::env::var_os(FRONT_END_ENV).filter(|value| !value.is_empty()) {
        // The user's own variable naming their own program: theirs to
        // resolve however they wrote it.
        return Some(std::path::PathBuf::from(named));
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(NAME)))
        .filter(|path| path.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The release ships `oxidezap` and `oxidezapd` in one directory, so the
    /// daemon's own location is the first place to look — and the name it
    /// looks for is the platform's.
    #[test]
    fn looks_beside_this_binary_first() {
        // A developer running the suite with a front end of their own named
        // is not testing this default.
        if std::env::var_os(FRONT_END_ENV).is_some_and(|value| !value.is_empty()) {
            return;
        }

        // The test binary's directory holds no `oxidezap`, so this exercises
        // the answer for that: nothing. A bare name here would be resolved
        // through the `PATH` this process inherited.
        assert_eq!(front_end_program(), None);
    }

    /// A subscriber is not a window. Reading the signal channel is what every
    /// client does; owning something to raise is what only a front end does,
    /// and only the client itself can say which it is.
    #[test]
    fn a_subscriber_is_not_a_window() {
        let hub = StateHub::new();
        let _watcher = hub.subscribe_signals();
        assert!(!hub.windows_attached(), "a subscriber alone is not one");

        let window = hub.attach_window();
        assert!(hub.windows_attached());

        drop(window);
        assert!(!hub.windows_attached(), "and it is gone when it leaves");
    }

    /// Unix only because the stand-in front end is a shell script. What it
    /// stands for is not: the three outcomes below are the whole of `show`,
    /// and none of them is platform-specific.
    #[cfg(unix)]
    mod launching {
        use super::*;
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        /// `LAUNCHED` is one slot for the whole process, which is right in a
        /// daemon and a hazard in a test binary that runs its tests in
        /// parallel. Held for the body of each test below, and the slot is
        /// emptied under it so no test inherits another's child.
        ///
        /// The crate's own mutex rather than one of this module's, because
        /// these are the tests that fork and something else in the binary
        /// takes a file lock — see [`crate::one_at_a_time`].
        fn exclusive() -> std::sync::MutexGuard<'static, ()> {
            let guard = crate::one_at_a_time();
            if let Some(mut stale) = take_launched() {
                let _ = stale.kill();
                let _ = stale.wait();
            }
            guard
        }

        /// A stand-in front end: it records that it ran, then stays up like
        /// a window would.
        fn fake_front_end(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
            let dir =
                std::env::temp_dir().join(format!("oxidezap-window-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let marker = dir.join("ran");
            let program = dir.join("front-end");
            let mut file = std::fs::File::create(&program).unwrap();
            // The marker is best-effort on purpose: a test that has already
            // killed this child may have taken its directory with it, and a
            // failed `touch` printing to the test log is pure noise.
            writeln!(
                file,
                "#!/bin/sh\ntouch '{}' 2>/dev/null\nsleep 30",
                marker.display()
            )
            .unwrap();
            drop(file);
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
            (program, marker)
        }

        fn take_launched() -> Option<Child> {
            LAUNCHED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        }

        /// Waits for a file the child writes on startup: `spawn` returns
        /// before the program has run, so the marker is the only honest
        /// evidence either way, and its absence has to be waited out too.
        fn ran_within(marker: &std::path::Path, wait: std::time::Duration) -> bool {
            let deadline = wacore::time::Instant::now() + wait;
            while wacore::time::Instant::now() < deadline {
                if marker.exists() {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            false
        }

        /// The bug this module was written for: the tray's Open reached
        /// nobody, and stopped there.
        #[test]
        fn a_request_nobody_can_answer_starts_a_front_end() {
            let _exclusive = exclusive();
            let (program, marker) = fake_front_end("starts");
            let hub = StateHub::new();

            show_program(&hub, &program);

            let mut child = take_launched().expect("a front end was started");
            assert!(ran_within(&marker, std::time::Duration::from_secs(5)));
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(program.parent().unwrap());
        }

        /// An attached front end is a window that exists. Starting a second
        /// process over it is the failure in the other direction.
        #[test]
        fn an_attached_front_end_is_asked_rather_than_duplicated() {
            let _exclusive = exclusive();
            let (program, marker) = fake_front_end("attached");
            let hub = StateHub::new();
            let _client = hub.subscribe_signals();
            let _window = hub.attach_window();

            show_program(&hub, &program);

            assert!(take_launched().is_none(), "nothing was started");
            assert!(
                !ran_within(&marker, std::time::Duration::from_millis(200)),
                "the stand-in front end never ran"
            );
            let _ = std::fs::remove_dir_all(program.parent().unwrap());
        }

        /// Open clicked twice while the first window is still coming up. The
        /// launch is not attached yet, so the signal still reaches nobody —
        /// and the second click must not become a second window.
        #[test]
        fn a_front_end_that_is_still_starting_is_not_started_again() {
            let _exclusive = exclusive();
            let (program, _marker) = fake_front_end("twice");
            let hub = StateHub::new();

            show_program(&hub, &program);
            let first = LAUNCHED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(Child::id)
                .expect("a front end was started");

            show_program(&hub, &program);

            let mut child = take_launched().expect("still holding the first one");
            assert_eq!(child.id(), first, "the same process, not a second one");
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(program.parent().unwrap());
        }
    }
}
