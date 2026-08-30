//! The one thing this crate exists for: a level raised while the process runs
//! is a level the logger underneath actually lets through.
//!
//! Its own test binary, because installing a logger is a process-wide act
//! that happens once — a `#[test]` beside others would race whichever of them
//! ran first.
//!
//! What it guards against is the arrangement this replaced. `env_logger`'s
//! filter is built at startup from `RUST_LOG` and answers with that level for
//! the life of the process, so raising `log::set_max_level` alone changed
//! nothing: the macro let the record through and the logger dropped it. That
//! failure is invisible from the outside — the setting moves, Settings redraws,
//! and no line appears — which is exactly the kind worth a test.

use std::sync::{Arc, Mutex};

use oxidezap_logging::LogLevel;

/// Everything the logger let through, in order.
static WRITTEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A logger of our own, since the real one writes to stderr.
///
/// It answers the same two questions the crate's own does — is this level in
/// force, and is this target one of the quiet ones — by asking
/// [`oxidezap_logging::current`], which is the value under test.
struct Recorder {
    quiet: &'static [&'static str],
}

impl log::Log for Recorder {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        if metadata.level() > oxidezap_logging::current().filter() {
            return false;
        }
        let quieted = self.quiet.iter().any(|module| {
            metadata.target() == *module || metadata.target().starts_with(&format!("{module}::"))
        });
        !quieted || metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        WRITTEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("{}: {}", record.target(), record.args()));
    }

    fn flush(&self) {}
}

fn written() -> Vec<String> {
    WRITTEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[test]
fn a_level_raised_at_runtime_reaches_the_logger() {
    let recorder: &'static Recorder = Box::leak(Box::new(Recorder {
        quiet: &["noisy_renderer"],
    }));
    log::set_logger(recorder).expect("nothing else installs a logger here");

    // Where every process starts, and what nobody has to ask for.
    oxidezap_logging::apply(LogLevel::Info);
    log::debug!("before");
    log::info!("during");
    assert_eq!(written(), vec!["the_level_moves: during".to_string()]);

    // The whole feature, in three lines: no restart, no environment
    // variable, and the record that was dropped a moment ago is kept.
    oxidezap_logging::apply(LogLevel::Debug);
    log::debug!("after");
    assert_eq!(
        written().last().map(String::as_str),
        Some("the_level_moves: after")
    );

    // And `log`'s own maximum moved with it. Without this the macro never
    // builds the record at all, so a logger that would have kept it never
    // sees one.
    assert_eq!(log::max_level(), log::LevelFilter::Debug);

    // Down again, because "any level" includes the quiet ones — a person who
    // has finished debugging asks for silence, and `off` has to mean it.
    oxidezap_logging::apply(LogLevel::Off);
    let before = written().len();
    log::error!("silenced");
    assert_eq!(written().len(), before);

    // A module held at `warn` stays there however loud the rest is: turning
    // debug on to read our own logs must not bury them under a renderer's.
    oxidezap_logging::apply(LogLevel::Trace);
    let quiet: Arc<str> = Arc::from("noisy_renderer");
    log::debug!(target: &quiet, "every frame");
    log::warn!(target: &quiet, "something is actually wrong");
    assert_eq!(
        written().last().map(String::as_str),
        Some("noisy_renderer: something is actually wrong")
    );
}
