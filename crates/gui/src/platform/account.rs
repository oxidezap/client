//! The account this frontend resumes after reopening.

use oxidezap_core::AccountId;
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AccountSelection {
    pub active: AccountId,
    pub retained: Option<AccountId>,
}

impl Default for AccountSelection {
    fn default() -> Self {
        Self {
            active: AccountId::LEGACY,
            retained: None,
        }
    }
}

impl AccountSelection {
    pub fn load() -> Self {
        if cfg!(test) {
            return Self::default();
        }
        match imp::read() {
            Ok(Some(raw)) => match serde_json::from_str::<Self>(&raw) {
                Ok(selection) if selection.retained != Some(selection.active) => selection,
                _ => {
                    log::warn!("ignoring invalid local account selection");
                    Self::default()
                }
            },
            Ok(None) => Self::default(),
            Err(error) => {
                log::warn!("could not read local account selection: {error}");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        if cfg!(test) {
            return Ok(());
        }
        let raw = serde_json::to_string(self).map_err(|error| error.to_string())?;
        imp::write(&raw)
    }
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use portable_atomic::{AtomicU64, Ordering};
    use std::path::PathBuf;

    fn path() -> Option<PathBuf> {
        let dir = if cfg!(windows) {
            PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })?
        };
        Some(dir.join("oxidezap").join("account-selection.json"))
    }

    pub(super) fn read() -> Result<Option<String>, String> {
        let Some(path) = path() else { return Ok(None) };
        read_from(&path)
    }

    fn read_from(path: &std::path::Path) -> Result<Option<String>, String> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(Some(raw)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    pub(super) fn write(raw: &str) -> Result<(), String> {
        let path = path().ok_or("no local configuration directory")?;
        write_to(&path, raw)
    }

    fn write_to(path: &std::path::Path, raw: &str) -> Result<(), String> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(path.parent().expect("configuration has a parent"))
            .map_err(|error| error.to_string())?;
        let temporary = path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&temporary, raw).map_err(|error| error.to_string())?;
        if let Err(error) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(temporary);
            return Err(error.to_string());
        }
        Ok(())
    }

    #[test]
    fn selection_documents_survive_reopening_and_atomic_replacement() {
        let path = std::env::temp_dir().join(format!(
            "oxidezap-selection-fixture-{}.json",
            std::process::id()
        ));
        let first = r#"{"active":2,"retained":1}"#;
        write_to(&path, first).unwrap();
        assert_eq!(read_from(&path).unwrap().as_deref(), Some(first));
        let selection: super::AccountSelection =
            serde_json::from_str(&read_from(&path).unwrap().unwrap()).unwrap();
        assert_eq!(selection.active, oxidezap_core::AccountId::new(2).unwrap());
        assert_eq!(selection.retained, Some(oxidezap_core::AccountId::LEGACY));
        let second = r#"{"active":1,"retained":null}"#;
        write_to(&path, second).unwrap();
        assert_eq!(read_from(&path).unwrap().as_deref(), Some(second));
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    fn storage() -> Result<(web_sys::Storage, String), String> {
        let window = web_sys::window().ok_or("no browser window")?;
        let storage = window
            .session_storage()
            .map_err(|_| "session storage is unavailable")?
            .ok_or("session storage is unavailable")?;
        let location = window.location();
        let key = format!(
            "oxidezap:account-selection:{}{}",
            location.pathname().unwrap_or_default(),
            location.search().unwrap_or_default()
        );
        Ok((storage, key))
    }

    fn preferred() -> Result<web_sys::Storage, String> {
        web_sys::window()
            .ok_or("no browser window")?
            .local_storage()
            .map_err(|_| "local storage is unavailable")?
            .ok_or_else(|| "local storage is unavailable".into())
    }

    pub(super) fn read() -> Result<Option<String>, String> {
        let (storage, key) = storage()?;
        let held = storage
            .get_item(&key)
            .map_err(|_| "could not read the selected account")?;
        if held.is_some() {
            return Ok(held);
        }
        // Existing tabs keep their own choice; new tabs inherit this route's preference.
        let raw = match preferred()?
            .get_item(&key)
            .map_err(|_| "could not read the preferred account")?
        {
            Some(raw) => raw,
            None => serde_json::to_string(&super::AccountSelection::default())
                .map_err(|error| error.to_string())?,
        };
        storage
            .set_item(&key, &raw)
            .map_err(|_| "could not retain this tab's account")?;
        Ok(Some(raw))
    }

    pub(super) fn write(raw: &str) -> Result<(), String> {
        let (storage, key) = storage()?;
        storage
            .set_item(&key, raw)
            .map_err(|_| "could not save the selected account")?;
        preferred()?
            .set_item(&key, raw)
            .map_err(|_| "could not save the preferred account".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reloaded_selection_keeps_the_active_and_retained_accounts_distinct() {
        let selection = AccountSelection {
            active: AccountId::new(2).unwrap(),
            retained: Some(AccountId::LEGACY),
        };
        let raw = serde_json::to_string(&selection).unwrap();
        assert_eq!(
            serde_json::from_str::<AccountSelection>(&raw).unwrap(),
            selection
        );
        assert!(serde_json::from_str::<AccountSelection>(r#"{"active":0,"retained":1}"#).is_err());
    }
}
