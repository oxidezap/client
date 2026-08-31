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

/// Tell the library what time it is, where nothing else will.
///
/// `wacore` reads the wall clock through a provider it registers a default
/// for — `chrono::Utc::now()` on a desktop, and on `wasm32` a stub that
/// returns *epoch* and warns once, because there is no clock std can offer
/// there. Nothing fails visibly under it: messages are stamped 1970, receipts
/// sort against each other wrongly, and a history load looks like an account
/// that has been quiet for fifty-six years.
///
/// It is registered here because it has to be first. The provider is a
/// `OnceLock` behind `get_or_init`, so the *first read* installs the default
/// permanently and every later `set` is refused — and the first read happens
/// somewhere in the first frame. There is nothing earlier than this in the
/// process on either platform.
pub fn clocks() {
    imp::clocks();
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
        // An explicit `RUST_LOG` still wins over the stored choice, and a
        // module named in it wins over the floors below: see
        // `oxidezap_logging`, which is where both rules are stated once for
        // this process and the daemon alike.
        oxidezap_logging::install(QUIET);
        oxidezap_logging::activate();
    }

    /// Crates that narrate, held at `warn` however loud the rest is asked to
    /// be.
    const QUIET: &[&str] = &[
        "blade_graphics",
        "naga",
        "zbus",
        "tracing",
        "gpui",
        "cosmic_text",
        "wgpu_core",
        "wgpu_hal",
        "font_kit",
    ];

    /// Nothing to install: `wacore` defaults to `chrono` here, which has a
    /// clock behind it.
    pub(super) fn clocks() {}

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
    /// There is no environment to read a filter out of and no stderr to write
    /// to, so the level comes from `?log=` or from what was last chosen in
    /// Settings, and the destination is the browser's console. `web_init`
    /// also installs the panic hook that turns a Rust panic into a readable
    /// trace rather than "unreachable executed".
    pub(super) fn logging() {
        // Ours first, and that order is the whole of it. `log` accepts one
        // logger, and `web_init` installs one — so registering after it fails
        // silently and leaves a logger with no module floors in force, which
        // is exactly what makes a raised level useless: gpui, wgpu and the
        // text shaper narrate every frame, and the twenty lines worth reading
        // arrive buried in thousands that are not. Registering first makes
        // `web_init`'s own `set_logger` the one that fails, and it is written
        // to tolerate that (`.ok()`); the panic hook it installs — the thing
        // that turns a Rust panic into a trace rather than "unreachable
        // executed" — is what it is still called for.
        oxidezap_logging::install(QUIET);
        gpui_platform::web_init();
        // After `web_init`, never before: it sets a level of its own, so a
        // level set ahead of it is the one that gets overwritten.
        let level = oxidezap_logging::activate();
        if level.filter() > log::LevelFilter::Info {
            match oxidezap_logging::forced() {
                Some(_) => log::info!("logging at {level}, asked for by the URL"),
                None => log::info!("logging at {level}, the level this page was left at"),
            }
        }
    }

    /// Crates that narrate, held at `warn` however loud the rest is asked to
    /// be. Named rather than derived, and the same list the desktop quiets.
    const QUIET: &[&str] = &[
        "naga",
        "gpui",
        "gpui_web",
        "gpui_wgpu",
        "gpui_platform",
        "cosmic_text",
        "wgpu_core",
        "wgpu_hal",
    ];

    /// One `name=value` out of the page's query string.
    fn parameter(name: &str) -> Option<String> {
        let search = web_sys::window()
            .and_then(|window| window.location().search().ok())
            .unwrap_or_default();
        search
            .trim_start_matches('?')
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}="))?.to_string().into())
    }

    /// `Date.now()` for the wall clock, `performance.now()` for the
    /// monotonic one.
    ///
    /// Two clocks because they answer different questions and fail
    /// differently. The wall clock says when something happened and may jump
    /// when the machine's time is corrected; the monotonic one measures how
    /// long something took and may not. `wacore` would otherwise derive the
    /// second from the first, which is only as monotonic as the user's time
    /// zone changes let it be — and `performance.now()` is the browser's own
    /// answer, in fractional milliseconds from the page's start.
    pub(super) fn clocks() {
        struct DateNow;
        impl wacore::time::TimeProvider for DateNow {
            fn now_millis(&self) -> i64 {
                js_sys::Date::now() as i64
            }
        }

        struct PerformanceNow;
        impl wacore::time::MonotonicProvider for PerformanceNow {
            fn now_nanos(&self) -> u64 {
                let millis = web_sys::window()
                    .and_then(|window| window.performance())
                    .map_or(0.0, |performance| performance.now());
                (millis.max(0.0) * 1_000_000.0) as u64
            }
        }

        // A refusal means something read the clock before this ran, which is
        // the one thing this function exists to be earlier than. Worth a line
        // in the console, and not worth failing the page over: a wrong clock
        // draws a wrong timestamp, and no clock at all draws nothing.
        if wacore::time::set_time_provider(DateNow).is_err() {
            log::warn!("the wall clock was already resolved before it could be set");
        }
        if wacore::time::set_monotonic_provider(PerformanceNow).is_err() {
            log::warn!("the monotonic clock was already resolved before it could be set");
        }
    }

    pub(super) fn application() -> gpui::Application {
        gpui_platform::application_with_web_backend(requested_backend())
    }

    /// WebGL by default, and WebGPU for anyone who asks.
    ///
    /// Not the browser's preference, which is what `Auto` asks for and what
    /// this used to pass. On a machine whose WebGPU *probes* fine, `Auto`
    /// selects it and the first pipeline it builds can still fail —
    /// `CreateGraphicsPipelines failed with VK_ERROR_UNKNOWN … [RenderPipeline
    /// "quads"]` — which reaches wgpu as an "Unexpected error" panic and takes
    /// the device with it. Measured, on an ordinary Intel/Mesa laptop through
    /// a released Chrome: the session behind it went on opening the store and
    /// building the client, into a window that would never draw again.
    ///
    /// A probe cannot catch that, because the probe passed. So the default is
    /// the backend that has worked everywhere it has been tried, and the
    /// faster one is a query away for anyone who wants it. Both directions
    /// are named, because "the window is black" is otherwise unactionable
    /// from either side.
    fn requested_backend() -> gpui_platform::WebBackendPreference {
        match parameter("backend").as_deref() {
            Some("webgpu") => gpui_platform::WebBackendPreference::WebGpu,
            Some("auto") => gpui_platform::WebBackendPreference::Auto,
            _ => gpui_platform::WebBackendPreference::WebGl,
        }
    }
}
