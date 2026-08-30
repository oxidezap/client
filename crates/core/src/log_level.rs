//! How much the client says about itself.
//!
//! One enum, because the level is three things at once: a value a person
//! picks in Settings, a value that crosses the daemon socket, and a value
//! written to a file that outlives the process. `log::LevelFilter` is only
//! the first of those — it is neither `serde` nor stable as a wire word — so
//! this is the type everything above the logger names, and the conversion is
//! at the edge.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How much to say.
///
/// The `log` levels and `off`, which is the one a person reaches for after
/// they have finished debugging.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    /// What the client says when nobody has asked for anything else.
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// Quietest first, which is the order a row of buttons draws them in.
    pub const ALL: [Self; 6] = [
        Self::Off,
        Self::Error,
        Self::Warn,
        Self::Info,
        Self::Debug,
        Self::Trace,
    ];

    /// The word this level is written as — in a file, on the wire, and in
    /// `RUST_LOG`. Lowercase, because `RUST_LOG` is where a reader has met
    /// it before.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    /// The word a control is labelled with.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Error => "Error",
            Self::Warn => "Warn",
            Self::Info => "Info",
            Self::Debug => "Debug",
            Self::Trace => "Trace",
        }
    }

    /// One line about what this level costs, for a pane where the wrong
    /// answer is a log nobody can read rather than an error.
    #[must_use]
    pub const fn note(self) -> &'static str {
        match self {
            Self::Off => "Nothing is logged at all.",
            Self::Error => "Only failures.",
            Self::Warn => "Failures and anything suspicious.",
            Self::Info => "The default: connections, sends, and what went wrong.",
            Self::Debug => "Every stanza the library reads, and every step of a pairing.",
            Self::Trace => "Everything, including the protocol's own bookkeeping.",
        }
    }

    /// What `log` is told.
    #[must_use]
    pub const fn filter(self) -> log::LevelFilter {
        match self {
            Self::Off => log::LevelFilter::Off,
            Self::Error => log::LevelFilter::Error,
            Self::Warn => log::LevelFilter::Warn,
            Self::Info => log::LevelFilter::Info,
            Self::Debug => log::LevelFilter::Debug,
            Self::Trace => log::LevelFilter::Trace,
        }
    }

    /// The other direction, for a level read out of `RUST_LOG` by somebody
    /// else's parser.
    #[must_use]
    pub const fn from_filter(filter: log::LevelFilter) -> Self {
        match filter {
            log::LevelFilter::Off => Self::Off,
            log::LevelFilter::Error => Self::Error,
            log::LevelFilter::Warn => Self::Warn,
            log::LevelFilter::Info => Self::Info,
            log::LevelFilter::Debug => Self::Debug,
            log::LevelFilter::Trace => Self::Trace,
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// What an unreadable word was.
///
/// Carried rather than swallowed: a stored level that does not parse is a
/// file somebody edited by hand, and the honest answer is to say which word
/// was not understood and fall back to the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownLogLevel(pub String);

impl fmt::Display for UnknownLogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a log level: {}", self.0)
    }
}

impl std::error::Error for UnknownLogLevel {}

impl FromStr for LogLevel {
    type Err = UnknownLogLevel;

    /// Case-insensitive, and `warning`/`err` are accepted because
    /// `env_logger` accepts them: a level typed into `RUST_LOG` and a level
    /// typed into this file should not mean different things.
    fn from_str(word: &str) -> Result<Self, Self::Err> {
        match word.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Ok(Self::Off),
            "error" | "err" => Ok(Self::Error),
            "warn" | "warning" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err(UnknownLogLevel(word.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LogLevel;

    /// The word is the wire format, the file's contents and what `RUST_LOG`
    /// uses. A round trip through it is what keeps those three the same
    /// thing.
    #[test]
    fn a_level_survives_being_written_down() {
        for level in LogLevel::ALL {
            assert_eq!(level.id().parse::<LogLevel>().expect("parses"), level);
            assert_eq!(
                serde_json::from_str::<LogLevel>(
                    &serde_json::to_string(&level).expect("serializes")
                )
                .expect("deserializes"),
                level
            );
        }
    }

    /// `env_logger` takes these, so a level a person has already typed into
    /// `RUST_LOG` means the same thing here.
    #[test]
    fn the_spellings_env_logger_takes_are_taken_here() {
        assert_eq!(
            "WARNING".parse::<LogLevel>().expect("parses"),
            LogLevel::Warn
        );
        assert_eq!("Err".parse::<LogLevel>().expect("parses"), LogLevel::Error);
        assert!("verbose".parse::<LogLevel>().is_err());
    }

    /// The two conversions are one mapping, not two lists that can drift.
    #[test]
    fn the_filter_round_trips() {
        for level in LogLevel::ALL {
            assert_eq!(LogLevel::from_filter(level.filter()), level);
        }
    }
}
