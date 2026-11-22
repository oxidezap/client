//! Conversions at the SDK boundary. Client IPC and storage retain their existing UTC types.

use chrono::{DateTime, Utc};
use wacore::Timestamp;

/// Convert an SDK timestamp without changing the client's UTC representation.
pub fn from_sdk(timestamp: Timestamp) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(timestamp.as_millis())
        .expect("SDK timestamps fit the UTC calendar range")
}

/// Read the SDK wall clock for client-owned timestamps.
pub fn now_utc() -> DateTime<Utc> {
    from_sdk(wacore::time::now_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_signed_epoch_milliseconds_and_utc() {
        for millis in [
            Timestamp::MIN_MILLIS,
            -1001,
            -1,
            0,
            1,
            1_700_000_000_123,
            Timestamp::MAX_MILLIS,
        ] {
            let sdk = Timestamp::from_millis(millis).unwrap();
            let utc = from_sdk(sdk);
            assert_eq!(utc.timestamp_millis(), millis);
            assert_eq!(utc.timezone(), Utc);
        }
    }
}
