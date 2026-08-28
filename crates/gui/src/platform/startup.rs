//! How the process starts, which is the one thing a page and a binary do
//! least alike.
//!
//! A desktop `main` owns its thread, reads `RUST_LOG` out of the environment
//! and writes to stderr. A page's `main` is a start function the browser
//! calls, has no environment and no stderr, and has to be told which
//! rendering backend to try. Both differences used to sit in `main.rs` as
//! `cfg`s, which is the arrangement this module exists to keep out of the
//! rest of the tree — `main.rs` calls two functions now and names no
//! platform.

/// Quiet the crates that narrate, and turn on whichever logger this platform
/// has.
pub fn logging() {
    imp::logging();
}

/// The application, before it is given anything to draw.
#[must_use]
pub fn application() -> gpui::Application {
    imp::application()
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    /// Quiet the crates that narrate, and turn on whichever logger this platform
    /// has.
    ///
    /// The renderer, the bus and the text shaper all narrate at debug level, and
    /// none of it is about this app. `cosmic_text` in particular reports every
    /// family it walks past while looking for a glyph — "failed to find family
    /// 'FreeSans'" is it working, not it failing, and one message with an unusual
    /// script produces a dozen. Turning `RUST_LOG=debug` on to look at *our* logs
    /// should not bury them.
    pub(super) fn logging() {
        // An explicit `RUST_LOG` still wins: these are floors for modules the
        // user did not ask about.
        let mut logger = env_logger::Builder::new();
        // Floors first, environment second. A later directive replaces an earlier
        // one for the same target, so parsing `RUST_LOG` before these turned
        // `RUST_LOG=cosmic_text=debug` back into `warn` — the one request that
        // could only have been deliberate was the one that was ignored.
        for quiet in [
            "blade_graphics",
            "naga",
            "zbus",
            "tracing",
            "gpui",
            "cosmic_text",
            "wgpu_core",
            "wgpu_hal",
            "font_kit",
        ] {
            logger.filter_module(quiet, log::LevelFilter::Warn);
        }
        logger.parse_env(env_logger::Env::default().default_filter_or("info"));
        logger.init();
    }

    /// The application, before it is given anything to draw.
    ///
    /// A desktop window and a canvas differ in one thing the rest of this file
    /// would rather not know about: the web backend has to be chosen, and it is
    /// worth being able to choose it from the URL when a machine's WebGPU is the
    /// thing that is broken.
    pub(super) fn application() -> gpui::Application {
        gpui_platform::application()
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    /// The same, for a page.
    ///
    /// There is no environment to read a filter out of and no stderr to write to,
    /// so the level is fixed and the destination is the browser's console.
    /// `web_init` also installs the panic hook that turns a Rust panic into a
    /// readable trace rather than "unreachable executed".
    pub(super) fn logging() {
        // One initializer, not two. `web_init` installs the panic hook that turns
        // a Rust panic into a readable trace *and* a `log` implementation that
        // writes to the browser console — and `log` accepts only one, so a second
        // one after it silently fails and leaves the first one's level in force.
        // Setting the level afterwards is what actually needs saying.
        gpui_platform::web_init();
        log::set_max_level(log::LevelFilter::Info);
    }

    pub(super) fn application() -> gpui::Application {
        gpui_platform::application_with_web_backend(requested_backend())
    }

    /// WebGPU, WebGL, or whichever the browser prefers.
    ///
    /// `?backend=webgl` forces the fallback. A page is served to machines nobody
    /// has tested on, and "the window is black" is otherwise unactionable.
    fn requested_backend() -> gpui_platform::WebBackendPreference {
        let search = web_sys::window()
            .and_then(|window| window.location().search().ok())
            .unwrap_or_default();
        let asked = |name: &str| {
            search
                .trim_start_matches('?')
                .split('&')
                .any(|parameter| parameter == format!("backend={name}"))
        };
        if asked("webgpu") {
            gpui_platform::WebBackendPreference::WebGpu
        } else if asked("webgl") {
            gpui_platform::WebBackendPreference::WebGl
        } else {
            gpui_platform::WebBackendPreference::Auto
        }
    }
}
