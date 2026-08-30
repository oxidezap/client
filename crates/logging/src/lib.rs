//! How much the client says about itself, and where that answer is kept.
//!
//! Two facts made this a crate rather than a function. The first is that the
//! level has to be changeable while the process runs: `log`'s global maximum
//! already is, but a filter built from `RUST_LOG` at startup is not, so a
//! level raised in Settings was refused by the logger underneath it. The
//! second is that the answer has to survive a restart, and both processes
//! that write logs have to read the same one — a level a person sets in the
//! window and a level `oxidezapd` starts at are one setting, not two.
//!
//! So the shape here is the shape the rest of the tree already has: one
//! interface, two implementations, and no `cfg` above it. A desktop reads
//! `RUST_LOG`, writes to stderr and keeps the choice in a config file; a page
//! reads `?log=`, writes to the browser's console and keeps the choice in
//! `localStorage`. Nothing above this module learns which.
//!
//! The precedence is the same on both and is stated once: an explicit
//! `RUST_LOG` (or `?log=`) wins, because it is the answer somebody gave to
//! *this* run; the stored choice is next; `info` is what is left. What is
//! chosen at runtime through [`set`] always applies, whatever started it —
//! a person changing the level in Settings is asking about now.

use std::sync::atomic::{AtomicUsize, Ordering};

pub use oxidezap_core::LogLevel;

mod store;

pub use store::location;

/// The level in force, as an index into [`LogLevel::ALL`].
///
/// An atomic rather than a lock: it is read on the path of every log record
/// that gets as far as a logger, including from the session's own threads,
/// and there is nothing to serialize — one value, written whole.
static LEVEL: AtomicUsize = AtomicUsize::new(LogLevel::Info as usize);

/// What is being logged right now.
#[must_use]
pub fn current() -> LogLevel {
    LogLevel::ALL
        .get(LEVEL.load(Ordering::Relaxed))
        .copied()
        .unwrap_or_default()
}

/// Turn this platform's logger on, quieting the crates that narrate.
///
/// The floors are per module and the level is global, which is why they are
/// two different mechanisms: the renderer, the bus and the text shaper all
/// narrate at debug about their own business, and turning debug on to read
/// *our* logs should not bury them. An explicit `RUST_LOG` directive for one
/// of these still wins — a person naming `cosmic_text` has said the one thing
/// that could only have been deliberate.
///
/// Call [`activate`] afterwards. They are two steps because a page has to
/// register its logger *before* `gpui_platform::web_init` and set its level
/// *after*, and one function could not be both.
pub fn install(quiet: &'static [&'static str]) {
    imp::install(quiet);
}

/// Apply the level this run starts at, and answer what it turned out to be.
///
/// `RUST_LOG` (or `?log=`), then the stored choice, then `info`.
pub fn activate() -> LogLevel {
    let level = forced().unwrap_or_else(|| stored().unwrap_or_default());
    apply(level);
    level
}

/// The level this run was started with from outside, if it was.
///
/// `RUST_LOG=debug` on a desktop and `?log=debug` in a page. Only the level
/// with no module attached: `RUST_LOG=oxidezap_session=debug` says something
/// about one target and nothing about how loud the process is, and the
/// logger's own filter is what answers that.
#[must_use]
pub fn forced() -> Option<LogLevel> {
    imp::forced()
}

/// What that outside answer is called, for a pane that has to explain why the
/// stored choice is not the one in force.
#[must_use]
pub const fn forced_by() -> &'static str {
    imp::FORCED_BY
}

/// The stored choice, if there is one and it can be read.
///
/// Absent is the ordinary case — nobody has ever changed it — and so is a
/// store this platform will not open. Neither is worth failing over: the
/// product default is a perfectly good answer.
#[must_use]
pub fn stored() -> Option<LogLevel> {
    match store::read() {
        Ok(level) => level,
        Err(e) => {
            // At `install` time there is no logger yet, and this is called
            // again from Settings where there is. A line either way is better
            // than a silent fallback to `info` that reads as the file being
            // ignored.
            log::debug!("could not read the stored log level: {e}");
            None
        }
    }
}

/// Log at this level from now on, without writing it down.
///
/// What the daemon does when a front end asks: applying and remembering are
/// two steps there, because the remembering is a file write that belongs off
/// the runtime.
pub fn apply(level: LogLevel) {
    LEVEL.store(level as usize, Ordering::Relaxed);
    // Both, and in this order. `set_max_level` is what `log`'s macros check
    // before they build a record at all, so a level raised without it costs
    // nothing and changes nothing.
    log::set_max_level(level.filter());
}

/// Write the choice down, so the next start makes it again.
///
/// # Errors
///
/// There is nowhere to keep it — no config directory, a browser with site
/// data switched off — or keeping it failed.
pub fn remember(level: LogLevel) -> Result<(), String> {
    store::write(level)
}

/// Change the level and remember it: what a front end's own process does.
///
/// # Errors
///
/// The level is applied whatever happens; the error is about the *next*
/// start, which is why it is reported rather than returned instead of the
/// change. A caller that cannot persist has still been heard.
pub fn set(level: LogLevel) -> Result<(), String> {
    apply(level);
    remember(level)
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::LogLevel;

    /// What a desktop reads a level out of.
    pub(super) const FORCED_BY: &str = "RUST_LOG";

    /// The environment's own filter, exactly as `env_logger` would take it.
    fn directives() -> Option<String> {
        std::env::var("RUST_LOG")
            .ok()
            .filter(|v| !v.trim().is_empty())
    }

    /// The bare level in `RUST_LOG`, if it names one.
    ///
    /// `env_filter`'s own grammar, which is what `env_logger` parses: a
    /// comma-separated list of directives, each either `target=level` or a
    /// level on its own. The one on its own is the global answer, and the
    /// last one wins.
    pub(super) fn forced() -> Option<LogLevel> {
        global_level(&directives()?)
    }

    /// The last module-less directive in a filter string.
    fn global_level(directives: &str) -> Option<LogLevel> {
        directives
            .split(',')
            .filter(|directive| !directive.contains('='))
            .filter_map(|directive| directive.trim().parse::<LogLevel>().ok())
            .next_back()
    }

    pub(super) fn install(quiet: &'static [&'static str]) {
        let mut builder = env_logger::Builder::new();
        // Floors first, environment second. A later directive replaces an
        // earlier one for the same target, so parsing `RUST_LOG` before these
        // turned `RUST_LOG=cosmic_text=debug` back into `warn` — the one
        // request that could only have been deliberate was the one that was
        // ignored.
        for module in quiet {
            builder.filter_module(module, log::LevelFilter::Warn);
        }
        if let Some(directives) = directives() {
            builder.parse_filters(&directives);
        }
        // And the global level is ours, not this filter's. That is the whole
        // reason there is a logger here rather than `env_logger::init`: a
        // filter built at startup answers with the level it was built with
        // forever, so a level raised at runtime was refused underneath
        // `log`'s own maximum, which had already let it through. Trace here
        // means the inner logger only ever answers the per-module question;
        // `LEVEL` answers the other one, and the `RUST_LOG` level it was
        // built from is still honoured — as this run's starting value, in
        // `activate`.
        builder.filter_level(log::LevelFilter::Trace);
        let logger = Dynamic {
            inner: builder.build(),
        };
        // A refusal means somebody has already installed a logger, which in
        // this process would be a bug rather than a condition to handle —
        // and there is nowhere to report it, since reporting it needs the
        // logger that was not installed.
        let _ = log::set_boxed_logger(Box::new(logger));
    }

    /// `env_logger`'s formatting and per-module filter, under a level that
    /// can move.
    struct Dynamic {
        inner: env_logger::Logger,
    }

    impl log::Log for Dynamic {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= super::current().filter() && self.inner.enabled(metadata)
        }

        fn log(&self, record: &log::Record<'_>) {
            if record.level() > super::current().filter() {
                return;
            }
            // The inner logger checks its own filter, which is where the
            // module floors live.
            self.inner.log(record);
        }

        fn flush(&self) {
            self.inner.flush();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{LogLevel, global_level};

        /// The level with no module on it is the global one, and the last
        /// wins — which is `env_filter`'s rule, not one invented here.
        #[test]
        fn the_bare_directive_is_the_global_level() {
            assert_eq!(global_level("debug"), Some(LogLevel::Debug));
            assert_eq!(
                global_level("warn,oxidezap_session=trace"),
                Some(LogLevel::Warn)
            );
            assert_eq!(global_level("info,debug"), Some(LogLevel::Debug));
        }

        /// A filter that only names targets says nothing about how loud the
        /// process is, so the stored choice is still the answer.
        #[test]
        fn a_filter_of_targets_alone_forces_nothing() {
            assert_eq!(global_level("oxidezap_session=debug"), None);
            assert_eq!(global_level("gpui=warn,naga=off"), None);
            assert_eq!(global_level(""), None);
        }
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use std::sync::OnceLock;

    use wasm_bindgen::JsValue;

    use super::LogLevel;

    /// What a page reads a level out of.
    ///
    /// In the query rather than the fragment, beside `backend`: the fragment
    /// is where the daemon token goes precisely because it never leaves the
    /// browser, and a log level is not a secret.
    pub(super) const FORCED_BY: &str = "?log=";

    /// The modules held at `warn` however loud the rest is asked to be.
    ///
    /// A `static` because the logger `log` holds has to be `'static` and
    /// carries no state of its own.
    static QUIET: OnceLock<&'static [&'static str]> = OnceLock::new();

    /// The browser's console, with the desktop's module floors.
    static CONSOLE: Console = Console;

    pub(super) fn install(quiet: &'static [&'static str]) {
        let _ = QUIET.set(quiet);
        // Ours first, and that order is the whole of it. `log` accepts one
        // logger, and `gpui_platform::web_init` installs one — so
        // registering after it fails silently and leaves a logger with no
        // module floors in force, which is exactly what makes a raised level
        // useless: gpui, wgpu and the text shaper narrate every frame, and
        // the twenty lines worth reading arrive buried in thousands that are
        // not.
        let _ = log::set_logger(&CONSOLE);
    }

    pub(super) fn forced() -> Option<LogLevel> {
        // An unreadable value is no answer rather than an error: this runs
        // before there is anywhere to report one.
        parameter("log")?.parse().ok()
    }

    /// One `name=value` out of the page's query string.
    pub(crate) fn parameter(name: &str) -> Option<String> {
        let search = web_sys::window()
            .and_then(|window| window.location().search().ok())
            .unwrap_or_default();
        search
            .trim_start_matches('?')
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{name}="))?.to_string().into())
    }

    /// What a page has instead of stderr.
    ///
    /// `log`'s global level is one number with no per-target part, so a
    /// filter that knows targets has to live in a logger.
    struct Console;

    impl log::Log for Console {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            if metadata.level() > super::current().filter() {
                return false;
            }
            let quieted = QUIET.get().is_some_and(|quiet| {
                quiet.iter().any(|module| {
                    metadata.target() == *module
                        || metadata.target().starts_with(&format!("{module}::"))
                })
            });
            !quieted || metadata.level() <= log::Level::Warn
        }

        fn log(&self, record: &log::Record<'_>) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let line = JsValue::from_str(&format!(
                "[{}] {}: {}",
                record.level(),
                record.target(),
                record.args()
            ));
            match record.level() {
                log::Level::Error => web_sys::console::error_1(&line),
                log::Level::Warn => web_sys::console::warn_1(&line),
                log::Level::Info => web_sys::console::info_1(&line),
                log::Level::Debug | log::Level::Trace => web_sys::console::log_1(&line),
            }
        }

        fn flush(&self) {}
    }
}
