//! Handing the application to whoever owns the run loop.
//!
//! The two platforms differ in *who blocks*. On a desktop `Platform::run`
//! blocks for the life of the process, and `Application::run`'s own stack
//! frame is what keeps the app alive. In a browser the run loop belongs to
//! the browser: `run` invokes the launch callback and returns immediately, so
//! the app would be dropped on the way out — which showed up as a canvas that
//! never appeared and one line reading "app was released".
//!
//! One function, two implementations behind it, and `main` never learns which
//! it got.

use gpui::{App, Application};

/// Start the application and give it `launch` to run.
///
/// Returns only when the process is over, wherever there is a process to be
/// over.
pub fn run(application: Application, launch: impl FnOnce(&mut App) + 'static) {
    imp::run(application, launch);
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use gpui::{App, Application};

    pub(super) fn run(application: Application, launch: impl FnOnce(&mut App) + 'static) {
        application.run(launch);
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use gpui::{App, Application};

    pub(super) fn run(application: Application, launch: impl FnOnce(&mut App) + 'static) {
        // `run_embedded` is gpui's answer for a run loop it does not own, and
        // the handle it returns is what holds the app. Leaked deliberately:
        // the page *is* the process here, so the app lives until the tab
        // closes and there is nothing left to hand it back to. Dropping the
        // handle would release the app, which is the bug this replaced.
        std::mem::forget(application.run_embedded(launch));
    }
}
