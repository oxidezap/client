//! Application state enum.

use std::sync::Arc;

use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct CachedQrCode {
    pub data: String,
    pub png_bytes: Arc<Vec<u8>>,
}

impl PartialEq for CachedQrCode {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppState {
    Loading,
    Connecting,
    WaitingForPairing {
        qr_code: Option<CachedQrCode>,
        pair_code: Option<String>,
        /// How long *this* code is good for. Not a constant: the library
        /// issues the first QR with a long life and every rotation after it
        /// with a short one, so a fixed denominator drew a bar that started
        /// a third full and never moved.
        timeout_secs: u64,
        /// When it was issued, so the time left is the clock's answer rather
        /// than a number nobody decrements.
        issued_at: DateTime<Utc>,
    },
    Syncing,
    Connected,
    Error(String),
    /// The server ended the session (401 and friends). Distinct from
    /// [`Error`](Self::Error) because retrying is useless: the stored
    /// credentials are dead and only a fresh pairing can recover, which means
    /// wiping local state rather than reconnecting with it.
    LoggedOut {
        message: String,
    },
}

#[allow(dead_code)]
impl AppState {
    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading | Self::Connecting)
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Connected)
    }

    pub fn needs_pairing(&self) -> bool {
        matches!(self, Self::WaitingForPairing { .. })
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }

    pub fn error_message(&self) -> Option<&str> {
        if let Self::Error(msg) = self {
            Some(msg)
        } else {
            None
        }
    }
}
