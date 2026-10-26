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

#[cfg(any(test, target_family = "wasm"))]
fn selection_key(path: &str, daemon: Option<&str>) -> String {
    let source = daemon.map(|url| url.split('?').next().unwrap_or(url));
    match source {
        Some(url) => format!("oxidezap:account-selection:{path}:daemon:{url}"),
        None => format!("oxidezap:account-selection:{path}:embedded"),
    }
}

#[test]
fn selections_belong_to_the_effective_daemon_without_persisting_its_token() {
    let embedded = selection_key("/app", None);
    let a = selection_key("/app", Some("ws://localhost:9000/ipc?token=secret"));
    let b = selection_key("/app", Some("ws://localhost:9001/ipc?token=secret"));
    assert_ne!(embedded, a);
    assert_ne!(a, b);
    assert_eq!(
        a,
        selection_key("/app", Some("ws://localhost:9000/ipc?token=rotated"))
    );
    assert!(!a.contains("secret"));
}

#[cfg(any(test, target_family = "wasm"))]
mod tab_selection {
    pub(super) trait Documents {
        fn tab(&self) -> Result<Option<String>, String>;
        fn preferred(&self) -> Result<Option<String>, String>;
        fn write_tab(&self, raw: &str) -> Result<(), String>;
        fn write_preferred(&self, raw: &str) -> Result<(), String>;
    }

    pub(super) fn restore(documents: &impl Documents) -> Result<Option<String>, String> {
        if let Some(raw) = documents.tab()? {
            return Ok(Some(raw));
        }
        let raw = match documents.preferred()? {
            Some(raw) => raw,
            None => serde_json::to_string(&super::AccountSelection::default())
                .map_err(|error| error.to_string())?,
        };
        documents.write_tab(&raw)?;
        Ok(Some(raw))
    }

    pub(super) fn save(documents: &impl Documents, raw: &str) -> Result<(), String> {
        documents.write_tab(raw)?;
        documents.write_preferred(raw)
    }

    #[test]
    fn reloads_keep_each_tabs_choice_and_new_sessions_inherit_the_preference() {
        use std::cell::RefCell;
        use std::rc::Rc;
        struct Tab {
            held: RefCell<Option<String>>,
            preferred: Rc<RefCell<Option<String>>>,
        }
        impl Documents for Tab {
            fn tab(&self) -> Result<Option<String>, String> {
                Ok(self.held.borrow().clone())
            }
            fn preferred(&self) -> Result<Option<String>, String> {
                Ok(self.preferred.borrow().clone())
            }
            fn write_tab(&self, raw: &str) -> Result<(), String> {
                *self.held.borrow_mut() = Some(raw.into());
                Ok(())
            }
            fn write_preferred(&self, raw: &str) -> Result<(), String> {
                *self.preferred.borrow_mut() = Some(raw.into());
                Ok(())
            }
        }
        let preferred = Rc::new(RefCell::new(None));
        let new_tab = || Tab {
            held: RefCell::new(None),
            preferred: preferred.clone(),
        };
        let active = |tab: &Tab| {
            serde_json::from_str::<super::AccountSelection>(&restore(tab).unwrap().unwrap())
                .unwrap()
                .active
        };
        let first = new_tab();
        assert_eq!(active(&first), oxidezap_core::AccountId::LEGACY);
        let second = new_tab();
        save(&second, r#"{"active":2,"retained":1}"#).unwrap();
        assert_eq!(active(&first), oxidezap_core::AccountId::LEGACY);
        assert_eq!(active(&second), oxidezap_core::AccountId::new(2).unwrap());
        let third = new_tab();
        assert_eq!(active(&third), oxidezap_core::AccountId::new(2).unwrap());
        save(&second, r#"{"active":1,"retained":null}"#).unwrap();
        assert_eq!(active(&third), oxidezap_core::AccountId::new(2).unwrap());
        assert_eq!(active(&new_tab()), oxidezap_core::AccountId::LEGACY);
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
        let endpoint = match oxidezap_ipc::web::named_daemon() {
            oxidezap_ipc::web::NamedDaemon::Nobody => None,
            oxidezap_ipc::web::NamedDaemon::Named(url) => Some(url),
            oxidezap_ipc::web::NamedDaemon::Rejected(_) => Some("rejected".to_owned()),
        };
        let key = super::selection_key(
            &location.pathname().unwrap_or_default(),
            endpoint.as_deref(),
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

    struct BrowserDocuments {
        storage: web_sys::Storage,
        key: String,
    }

    impl super::tab_selection::Documents for BrowserDocuments {
        fn tab(&self) -> Result<Option<String>, String> {
            self.storage
                .get_item(&self.key)
                .map_err(|_| "could not read the selected account".into())
        }
        fn preferred(&self) -> Result<Option<String>, String> {
            preferred()?
                .get_item(&self.key)
                .map_err(|_| "could not read the preferred account".into())
        }
        fn write_tab(&self, raw: &str) -> Result<(), String> {
            self.storage
                .set_item(&self.key, raw)
                .map_err(|_| "could not retain this tab's account".into())
        }
        fn write_preferred(&self, raw: &str) -> Result<(), String> {
            preferred()?
                .set_item(&self.key, raw)
                .map_err(|_| "could not save the preferred account".into())
        }
    }

    pub(super) fn read() -> Result<Option<String>, String> {
        let (storage, key) = storage()?;
        super::tab_selection::restore(&BrowserDocuments { storage, key })
    }

    pub(super) fn write(raw: &str) -> Result<(), String> {
        let (storage, key) = storage()?;
        super::tab_selection::save(&BrowserDocuments { storage, key }, raw)
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
