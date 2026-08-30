//! `RUST_LOG=some_target=debug` says how loud one target should be and
//! nothing about how loud the process is — so the process stays at the
//! stored level, and that target still has to be heard.
//!
//! Its own test binary for the reason the other one has its own: installing
//! a logger happens once per process, and this one has to set `RUST_LOG`
//! before it happens.
//!
//! The gate this guards is easy to get backwards. Reading the dynamic level
//! as a ceiling over *every* record refuses the very lines such a directive
//! was written to see — twice over, since `log`'s own maximum is checked
//! before a record exists and knows no targets at all.

use oxidezap_logging::LogLevel;

#[test]
fn a_target_named_in_the_environment_is_heard_at_its_own_level() {
    // Before the logger exists, and before anything else in this binary
    // could be running: `set_var` is sound only while this process is still
    // single-threaded, which at the top of the only test in a binary it is.
    unsafe {
        std::env::set_var("RUST_LOG", "a_named_target=debug");
    }

    oxidezap_logging::install(&["a_quiet_crate"]);
    // `RUST_LOG` names no bare level, so this run starts wherever the stored
    // choice or the default puts it — not at `debug`.
    let level = oxidezap_logging::activate();
    assert!(oxidezap_logging::forced().is_none());

    // `log`'s own maximum has to clear the named target, or the macro never
    // builds the record for the filter to answer about.
    assert!(log::max_level() >= log::LevelFilter::Debug);

    // The directive is honoured...
    assert!(log::log_enabled!(target: "a_named_target", log::Level::Debug));
    // ...and it is a statement about that target alone.
    if level.filter() < log::LevelFilter::Debug {
        assert!(!log::log_enabled!(target: "somebody_else", log::Level::Debug));
    }

    // The floors still hold: a directive raises what it names and nothing
    // else, whatever the process level is.
    oxidezap_logging::apply(LogLevel::Trace);
    assert!(!log::log_enabled!(target: "a_quiet_crate", log::Level::Debug));
    assert!(log::log_enabled!(target: "a_quiet_crate", log::Level::Warn));

    // And a level chosen at runtime is still the answer for everything the
    // environment did not name.
    oxidezap_logging::apply(LogLevel::Error);
    assert!(!log::log_enabled!(target: "somebody_else", log::Level::Warn));
    assert!(log::log_enabled!(target: "a_named_target", log::Level::Debug));
}
