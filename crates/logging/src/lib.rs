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
use std::sync::{Mutex, PoisonError};

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
    // Read before anything is applied and reported after, in that order and
    // for one reason: `log`'s runtime maximum is `Off` until the first
    // `set_max_level`, so a line written about the file here would be
    // discarded by the macro before any logger saw it — and a stored level
    // that failed to parse would then be ignored in total silence, which is
    // the one thing worth saying about it.
    let stored = store::read();
    let level = forced()
        .or_else(|| stored.as_ref().ok().copied().flatten())
        .unwrap_or_default();
    apply(level);
    if let Err(e) = &stored {
        log::warn!("the stored log level was not used ({e}); logging at {level}");
    }
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
    // Silent, and deliberately: the one caller that runs before a logger
    // exists is [`activate`], which reads the store itself so it can say
    // what happened *after* it has established a level to say it at.
    store::read().ok().flatten()
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
    //
    // And never below what `RUST_LOG` named a target for: that maximum is
    // checked before a record exists and knows no targets, so a process at
    // `info` with `RUST_LOG=oxidezap_session=debug` would drop the debug
    // records at the macro and never reach the filter that wants them. The
    // logger is still what decides which of them are written.
    log::set_max_level(level.filter().max(imp::named_ceiling()));
}

/// Write the level in force down, so the next start makes it again.
///
/// What is written is [`current`] rather than a level passed in, and the
/// writes are serialized. Both halves answer the same thing: two front ends
/// can change the level in the same moment, and the daemon writes for each
/// of them on a thread of its own. Ordered by nothing, two writes carrying
/// their own levels can land in either order and leave the file disagreeing
/// with the process — the earlier request restored on the next start. A
/// write that asks what the level *is* converges instead: whichever runs
/// last writes the last level applied.
///
/// # Errors
///
/// There is nowhere to keep it — no config directory, a browser with site
/// data switched off — or keeping it failed.
pub fn remember() -> Result<(), String> {
    static WRITING: Mutex<()> = Mutex::new(());
    // Recovered rather than panicked on: this guards one file write with no
    // invariant spanning anything, so a holder that died mid-write left
    // nothing torn — the write is a temporary and a rename — and a second
    // panic here would hide the first.
    let _serialized = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
    store::write(current())
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
    remember()
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use std::sync::OnceLock;

    use super::LogLevel;

    /// What a desktop reads a level out of.
    pub(super) const FORCED_BY: &str = "RUST_LOG";

    /// The environment's own filter, exactly as `env_logger` would take it.
    fn directives() -> Option<String> {
        std::env::var("RUST_LOG")
            .ok()
            .filter(|v| !v.trim().is_empty())
    }

    /// One directive out of a filter: a level, and the target it is about.
    ///
    /// `None` for the target is the level with nothing attached — the one
    /// that says how loud the process is.
    type Directive = (Option<String>, log::LevelFilter);

    /// Read a filter the way `env_filter` reads it.
    ///
    /// Its grammar and not a subset of it, because every form it accepts is
    /// one somebody may have in a shell profile, and one this crate fails to
    /// recognise is a directive the dynamic gate then refuses. Three of them
    /// are easy to miss and all three are real:
    ///
    /// - `RUST_LOG=oxidezap_session` — a bare word that is not a level names
    ///   a *target*, at `trace`.
    /// - `RUST_LOG=oxidezap_session=` — an empty level does the same.
    /// - `RUST_LOG==debug` — an empty *target* is a prefix that matches
    ///   every target, so it reads as a global level in all but name.
    /// - `RUST_LOG=debug/stanza` — the `/` and everything after it is a
    ///   regular expression over the message, applied by the logger. It is
    ///   not part of the last directive's level, and reading it as one loses
    ///   the level in front of it.
    ///
    /// An unreadable level is skipped, which is what `env_filter` does with
    /// it too.
    fn parse_filter(spec: &str) -> Vec<Directive> {
        // The regex is one suffix over the whole filter rather than
        // something a directive carries, so it comes off first.
        let directives = spec.split('/').next().unwrap_or_default();
        directives
            .split(',')
            .map(str::trim)
            .filter(|directive| !directive.is_empty())
            .filter_map(|directive| match directive.split_once('=') {
                Some((target, level)) => {
                    let (target, level) = (target.trim(), level.trim());
                    // `target=` names a target at its loudest, exactly as a
                    // bare `target` does.
                    let level = if level.is_empty() {
                        log::LevelFilter::Trace
                    } else {
                        level.parse().ok()?
                    };
                    // An empty target is kept rather than dropped: `=debug`
                    // is a prefix that matches everything, which is what
                    // `env_filter` makes of it and so what the inner filter
                    // will answer. Dropping it left the gate refusing records
                    // that filter accepts.
                    Some((Some(target.to_string()), level))
                }
                // A bare word is a level if it reads as one, and a target at
                // its loudest otherwise.
                None => Some(match directive.parse() {
                    Ok(level) => (None, level),
                    Err(_) => (Some(directive.to_string()), log::LevelFilter::Trace),
                }),
            })
            .collect()
    }

    /// The bare level in `RUST_LOG`, if it names one.
    pub(super) fn forced() -> Option<LogLevel> {
        global_level(&directives()?)
    }

    /// The last directive in a filter that names no target.
    ///
    /// The last, because that is `env_filter`'s rule for two answers to one
    /// question.
    fn global_level(spec: &str) -> Option<LogLevel> {
        parse_filter(spec)
            .into_iter()
            .rev()
            .find(|(target, _)| target.is_none())
            .map(|(_, level)| LogLevel::from_filter(level))
    }

    /// The targets a filter names, and how loud each was asked to be.
    ///
    /// `env_filter` parses these too and answers with them, and that answer
    /// is still what runs — this reading exists only to know *that* a target
    /// was named, which is a question its `Filter` does not expose. See
    /// [`Dynamic::named`] for why the difference matters.
    fn named_targets(spec: &str) -> Vec<(String, log::LevelFilter)> {
        parse_filter(spec)
            .into_iter()
            .filter_map(|(target, level)| Some((target?, level)))
            .collect()
    }

    /// The loudest level `RUST_LOG` named a target for.
    ///
    /// `Off` when it named none, which is the ordinary case. Read on every
    /// [`apply`](super::apply) and so memoized — the environment cannot
    /// change under a running process in any way this crate would honour,
    /// since the filter was built from it once.
    pub(super) fn named_ceiling() -> log::LevelFilter {
        static CEILING: OnceLock<log::LevelFilter> = OnceLock::new();
        *CEILING.get_or_init(|| {
            directives()
                .map(|directives| {
                    named_targets(&directives)
                        .into_iter()
                        .map(|(_, level)| level)
                        .max()
                        .unwrap_or(log::LevelFilter::Off)
                })
                .unwrap_or(log::LevelFilter::Off)
        })
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
        let named = match directives() {
            Some(directives) => {
                builder.parse_filters(&directives);
                named_targets(&directives)
            }
            None => Vec::new(),
        };
        // The write style is the environment's too. `parse_env`/`from_env`,
        // which both startup paths used before this crate existed, read
        // `RUST_LOG_STYLE` as well as `RUST_LOG` — so without this a person
        // who had turned colour off, or forced it on through a pipe, quietly
        // lost that when the filter moved in here.
        if let Ok(style) = std::env::var("RUST_LOG_STYLE") {
            builder.parse_write_style(&style);
        }
        // And the global level is ours, not this filter's. That is the whole
        // reason there is a logger here rather than `env_logger::init`: a
        // filter built at startup answers with the level it was built with
        // forever, so a level raised at runtime was refused underneath
        // `log`'s own maximum, which had already let it through. Trace here
        // leaves the inner filter answering only the per-target question —
        // the floors, the `RUST_LOG` directives and its regex — and `LEVEL`
        // answers the other one. The `RUST_LOG` level this was built from is
        // still honoured, as this run's starting value in `activate`.
        builder.filter_level(log::LevelFilter::Trace);
        let logger = Dynamic {
            inner: builder.build(),
            named,
        };
        // A refusal means somebody has already installed a logger, which in
        // this process would be a bug rather than a condition to handle —
        // and there is nowhere to report it, since reporting it needs the
        // logger that was not installed.
        let _ = log::set_boxed_logger(Box::new(logger));
    }

    /// `env_logger`'s formatting and per-target filter, under a level that
    /// can move.
    struct Dynamic {
        inner: env_logger::Logger,
        /// The targets `RUST_LOG` named, and how loud each was asked to be.
        named: Vec<(String, log::LevelFilter)>,
    }

    impl Dynamic {
        /// Whether `RUST_LOG` asked for this record by name.
        ///
        /// The dynamic level is a gate over everything *nobody named*, and
        /// only that. `RUST_LOG=oxidezap_session=debug` says how loud one
        /// target should be and nothing about how loud the process is — so
        /// it leaves `forced` empty and the stored choice in force, and a
        /// gate that treated the stored choice as a ceiling would refuse the
        /// very records that directive was written to see. The inner filter
        /// already answers such a target correctly; this is what lets the
        /// answer through.
        ///
        /// Longest prefix wins, which is `env_filter`'s own rule: a
        /// directive for `oxidezap_session` and one for
        /// `oxidezap_session::whatsapp` are two answers about one record, and
        /// the more specific one is the one that was meant.
        ///
        /// A bare prefix and not one that has to end at a `::`, which is also
        /// `env_filter`'s rule — `foo=debug` matches `foobar` there. Reading
        /// it more strictly here is not a stricter policy but a dropped
        /// record: this gate only decides what the inner filter is *allowed*
        /// to answer about, and a target it accepts that this refuses is a
        /// line the environment asked for and nothing writes.
        fn named(&self, metadata: &log::Metadata<'_>) -> bool {
            let target = metadata.target();
            self.named
                .iter()
                .filter(|(name, _)| target.starts_with(name.as_str()))
                .max_by_key(|(name, _)| name.len())
                .is_some_and(|(_, level)| metadata.level() <= *level)
        }

        /// Whether the level in force lets this record through.
        ///
        /// Two questions, and the inner filter answers the other one: what a
        /// target was *held down* to. Both have to say yes.
        fn allowed(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= super::current().filter() || self.named(metadata)
        }
    }

    impl log::Log for Dynamic {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            self.allowed(metadata) && self.inner.enabled(metadata)
        }

        fn log(&self, record: &log::Record<'_>) {
            if !self.allowed(record.metadata()) {
                return;
            }
            // The inner logger checks its own filter, which is where the
            // floors, the `RUST_LOG` directives and its regex live.
            self.inner.log(record);
        }

        fn flush(&self) {
            self.inner.flush();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{LogLevel, global_level, named_targets};

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

        /// A target named in `RUST_LOG` is one the dynamic gate has to let
        /// through, so it has to be recognised as named — including when it
        /// arrives beside a global level or a regex.
        #[test]
        fn the_targets_a_filter_names_are_read_back() {
            assert_eq!(
                named_targets("info,oxidezap_session=debug"),
                vec![("oxidezap_session".to_string(), log::LevelFilter::Debug)]
            );
            assert_eq!(
                named_targets("gpui=warn,naga=off/some regex"),
                vec![
                    ("gpui".to_string(), log::LevelFilter::Warn),
                    ("naga".to_string(), log::LevelFilter::Off),
                ]
            );
            // A bare level names nothing, and neither does an unreadable one.
            assert!(named_targets("debug").is_empty());
            assert!(named_targets("gpui=verbose").is_empty());
            // An empty target is a prefix that matches everything, which is
            // what `env_filter` makes of it.
            assert_eq!(
                named_targets("=debug"),
                vec![(String::new(), log::LevelFilter::Debug)]
            );
        }

        /// `env_filter`'s shorthands, which are the forms most likely to be
        /// sitting in somebody's shell profile: a bare target is that target
        /// at its loudest, an empty level says the same thing, and a filter
        /// this crate failed to recognise is a directive the gate then
        /// refuses.
        #[test]
        fn a_target_named_without_a_level_is_named_at_its_loudest() {
            assert_eq!(
                named_targets("oxidezap_session"),
                vec![("oxidezap_session".to_string(), log::LevelFilter::Trace)]
            );
            assert_eq!(
                named_targets("oxidezap_session="),
                vec![("oxidezap_session".to_string(), log::LevelFilter::Trace)]
            );
            // And it is a statement about that target, not about the process.
            assert_eq!(global_level("oxidezap_session"), None);
        }

        /// The `/` and what follows it is a regular expression over the
        /// message, applied by the logger. Read as part of the level in front
        /// of it, it loses that level entirely.
        #[test]
        fn a_message_regex_is_not_part_of_the_level() {
            assert_eq!(global_level("debug/stanza"), Some(LogLevel::Debug));
            assert_eq!(
                global_level("info,oxidezap_session=debug/pair-device"),
                Some(LogLevel::Info)
            );
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

    /// A page has no per-target filter to raise anything above the level in
    /// force: `?log=` is one level for everything.
    pub(super) const fn named_ceiling() -> log::LevelFilter {
        log::LevelFilter::Off
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
