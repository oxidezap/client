//! Local account identity, stable across pairing and renames.
//!
//! A [`AccountId`] names a local slot, not a person. It exists before pairing,
//! survives re-pairing, and never changes when the push name, JID or LID do.
//! [`oxidezap_ipc::AccountIdentity`](https://docs.rs/) is metadata about the
//! slot; this is the slot itself.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Local account slot, opaque and permanent.
///
/// 128 bits of randomness rendered as 32 lowercase hex characters. Nothing
/// about the person is in it, so a rename, a number change or a re-pair
/// leaves every reference to it intact.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(String);

impl AccountId {
    /// How many random bytes a generated id carries.
    const ID_BYTES: usize = 16;
    /// Longest id [`Self::new`] accepts, keeping directory names readable.
    const MAX_LEN: usize = 64;

    /// The id for a slot that predates multi-account: the migrated store.
    ///
    /// Fixed rather than generated so a migration that runs twice lands on
    /// the same slot instead of minting a second one.
    pub const MIGRATED: &'static str = "migrated-single-account";

    /// Wrap an existing id after checking it cannot escape a directory.
    ///
    /// # Errors
    ///
    /// The string is empty, too long, or carries anything but ASCII
    /// alphanumerics, `-` and `_`.
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidAccountId> {
        let id = id.into();
        if is_valid_account_id(&id) {
            Ok(Self(id))
        } else {
            Err(InvalidAccountId)
        }
    }

    /// Mint a fresh id from the operating system's randomness.
    ///
    /// # Errors
    ///
    /// The platform refused randomness, which on a desktop means the process
    /// has no entropy source left to read.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; Self::ID_BYTES];
        getrandom::fill(&mut bytes)?;
        let mut id = String::with_capacity(Self::ID_BYTES * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(id, "{byte:02x}");
        }
        Ok(Self(id))
    }

    /// The id as stored in `accounts.json` and directory names.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for AccountId {
    type Err = InvalidAccountId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Why [`AccountId::new`] refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidAccountId;

impl fmt::Display for InvalidAccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("account id must be 1-64 ASCII alphanumerics, '-' or '_'")
    }
}

impl std::error::Error for InvalidAccountId {}

fn is_valid_account_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= AccountId::MAX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_hex_and_unique() {
        let a = AccountId::generate().expect("randomness");
        let b = AccountId::generate().expect("randomness");
        assert_eq!(a.as_str().len(), 32);
        assert!(a.as_str().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn traversal_keys_are_refused() {
        for bad in ["", "../x", "a/b", ".hidden", "a.b", "sp ace", "ç", "a".repeat(65).as_str()] {
            assert!(AccountId::new(bad).is_err(), "{bad:?} was allowed");
        }
    }

    #[test]
    fn ordinary_ids_round_trip() {
        let id = AccountId::new("01JAAA-work_1").expect("valid");
        let json = serde_json::to_string(&id).expect("serializes");
        assert_eq!(json, "\"01JAAA-work_1\"");
        let back: AccountId = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(id, back);
        assert_eq!(id.to_string(), "01JAAA-work_1");
    }

    #[test]
    fn migrated_slot_is_stable() {
        let id = AccountId::new(AccountId::MIGRATED).expect("valid");
        assert_eq!(id.as_str(), AccountId::MIGRATED);
    }
}
