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

/// How long a credential is good for, and when it was issued.
///
/// Not a constant: the library issues the first QR with a long life and every
/// rotation after it with a short one, so a fixed denominator drew a bar that
/// started a third full and never moved. The issue time rather than a
/// countdown, so the time left is the clock's answer rather than a number
/// nobody decrements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lifetime {
    pub timeout_secs: u64,
    pub issued_at: DateTime<Utc>,
}

impl Lifetime {
    pub fn new(timeout_secs: u64, issued_at: DateTime<Utc>) -> Self {
        Self {
            timeout_secs,
            issued_at,
        }
    }

    /// Seconds left, and how much of the life that is.
    pub fn left_at(&self, now: DateTime<Utc>) -> (u64, f32) {
        if self.timeout_secs == 0 {
            return (0, 0.0);
        }
        let elapsed = (now - self.issued_at).num_seconds().max(0) as u64;
        let left = self.timeout_secs.saturating_sub(elapsed);
        (
            left,
            (left as f32 / self.timeout_secs as f32).clamp(0.0, 1.0),
        )
    }
}

/// A credential and the clock it is running against.
///
/// One clock each, because the two pairing credentials do not share one: a QR
/// rotates on the server's own short cycle while a phone code lives for
/// minutes. Held together they cannot come apart — refreshing one used to
/// reset the deadline the other was being drawn against, so a code could sit
/// on screen long after it had expired under a bar still reporting time left.
#[derive(Debug, Clone, PartialEq)]
pub struct Issued<T> {
    pub value: T,
    pub life: Lifetime,
}

impl<T> Issued<T> {
    pub fn new(value: T, timeout_secs: u64, issued_at: DateTime<Utc>) -> Self {
        Self {
            value,
            life: Lifetime::new(timeout_secs, issued_at),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppState {
    Loading,
    Connecting,
    WaitingForPairing {
        qr_code: Option<Issued<CachedQrCode>>,
        pair_code: Option<Issued<String>>,
    },
    Syncing,
    Connected,
    /// The user chose to stop waiting for a connection and read what is
    /// already on this device.
    ///
    /// Its own state rather than `Connected`, because the difference is the
    /// whole point: history is local and stays readable, but nothing can be
    /// sent, no call can be placed, and a composer that accepted text here
    /// would draw a bubble the daemon has no session to send — pending for
    /// ever, with the failure logged where nobody looks.
    Offline,
    Error(String),
    /// This window may not hold the account, and no amount of waiting
    /// changes that.
    ///
    /// Distinct from [`Error`](Self::Error) for the reason
    /// [`LoggedOut`](Self::LoggedOut) is: that state is an outage, and the
    /// screen it draws promises to keep trying. Here there is nothing to keep
    /// trying — another tab holds this account, or this is a preview that has
    /// not been told it may keep one — and the answer changes only when a
    /// person does something. Drawing it as an outage would make two false
    /// claims at once: that WhatsApp could not be reached, and that we are
    /// still working on it.
    Refused {
        reason: String,
    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn at(secs: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 25, 12, 0, secs).unwrap()
    }

    #[test]
    fn a_fresh_code_fills_the_bar_whatever_its_lifetime() {
        assert_eq!(Lifetime::new(60, at(0)).left_at(at(0)), (60, 1.0));
        assert_eq!(Lifetime::new(20, at(0)).left_at(at(0)), (20, 1.0));
    }

    #[test]
    fn the_bar_drains_against_this_codes_own_lifetime() {
        let (left, fraction) = Lifetime::new(20, at(0)).left_at(at(10));
        assert_eq!(left, 10);
        assert!((fraction - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn an_expired_code_reads_as_empty_rather_than_negative() {
        assert_eq!(Lifetime::new(20, at(0)).left_at(at(45)), (0, 0.0));
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_overfill_it() {
        let (left, fraction) = Lifetime::new(20, at(30)).left_at(at(0));
        assert_eq!(left, 20);
        assert!(fraction <= 1.0);
    }

    /// The two credentials do not share a clock. A QR rotates on the
    /// server's short cycle and a phone code lives for minutes; on one
    /// deadline, refreshing the QR restated the code's remaining life as the
    /// QR's, so a code could sit there expired under a bar with time on it.
    #[test]
    fn each_pairing_credential_keeps_its_own_deadline() {
        let phone = Issued::new("ABCD1234".to_string(), 300, at(0));
        // Twenty seconds on, the QR rotates and the phone code does not.
        let qr = Issued::new("second".to_string(), 20, at(20));

        assert_eq!(qr.life.left_at(at(20)), (20, 1.0));
        assert_eq!(phone.life.left_at(at(20)).0, 280);
    }
}
