//! Local account identity, stable across pairing and renames.
//!
//! [`AccountId`] is the local slot identifier and intentionally has the same
//! value as the `device.id` row in the shared WhatsApp SQLite database. It is
//! not a phone number, JID, LID, push name, or remote WhatsApp device id.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::str::FromStr;

/// Local account slot, opaque outside the storage boundary.
///
/// The database allocates these ids with SQLite `AUTOINCREMENT`. Keeping the
/// wrapper non-zero and private prevents an unvalidated id from reaching a
/// store, a protocol message, or an account-scoped path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct AccountId(i32);

impl AccountId {
    /// The device row created by the single-account client before multi-account
    /// support. Resetting it keeps the same id; it is never re-minted.
    pub const LEGACY: Self = Self(1);

    /// Wrap an existing database id after validating the account invariant.
    pub const fn new(id: i32) -> Result<Self, InvalidAccountId> {
        if id > 0 {
            Ok(Self(id))
        } else {
            Err(InvalidAccountId)
        }
    }

    /// The value used by `whatsapp-rust` and the SQLite schema.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }

    /// Alias for call sites where the storage representation is explicit.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for AccountId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let id = i32::deserialize(deserializer)?;
        Self::new(id).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for AccountId {
    type Err = InvalidAccountId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = s.parse::<i32>().map_err(|_| InvalidAccountId)?;
        Self::new(id)
    }
}

/// Why [`AccountId::new`] refused an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidAccountId;

impl fmt::Display for InvalidAccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("account id must be a positive SQLite device id")
    }
}

impl std::error::Error for InvalidAccountId {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_positive_and_round_trip() {
        let id = AccountId::new(42).expect("positive id");
        assert_eq!(id.get(), 42);
        assert_eq!(id.as_i32(), 42);
        assert_eq!(id.to_string(), "42");
        assert_eq!("42".parse::<AccountId>().expect("parses"), id);
    }

    #[test]
    fn zero_and_negative_ids_are_refused() {
        assert!(AccountId::new(0).is_err());
        assert!(AccountId::new(-1).is_err());
        assert!("not-an-id".parse::<AccountId>().is_err());
    }

    #[test]
    fn serde_rejects_invalid_ids() {
        let id = AccountId::new(7).expect("positive id");
        let json = serde_json::to_string(&id).expect("serializes");
        assert_eq!(json, "7");
        assert_eq!(
            serde_json::from_str::<AccountId>(&json).expect("deserializes"),
            id
        );
        assert!(serde_json::from_str::<AccountId>("0").is_err());
    }

    #[test]
    fn legacy_slot_is_stable() {
        assert_eq!(AccountId::LEGACY.get(), 1);
    }
}
