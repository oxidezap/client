//! The desktop store: one file, under the user's data directory.

use log::{info, warn};

use super::{DATA_DIR, DB_FILE};

/// Resolve a stable per-user path for the SQLite database.
///
/// A CWD-relative path would silently split state between launch methods
/// (desktop launcher vs terminal), so prefer the platform data dir and only
/// fall back to the working directory when no home is known.
pub fn database_path() -> String {
    database_dir()
        .map(|dir| dir.join(DB_FILE).to_string_lossy().into_owned())
        .unwrap_or_else(|| DB_FILE.to_string())
}

/// Nothing to install: the platform's own VFS is the one we want, and the
/// directory is made where the path is resolved.
///
/// # Errors
///
/// Never, here. The signature is the browser's, where a store can genuinely
/// refuse to open.
pub async fn prepare() -> Result<(), String> {
    Ok(())
}

/// Delete the local session: device identity, Signal state and chat history
/// all live in the one SQLite file.
///
/// `async` for the browser's sake, where the deletion has to be awaited to
/// the flush; here it is ready before it is polled.
///
/// Called after the server ends the session, where reconnecting is pointless
/// — the credentials are dead and pairing mints a new device. A partial wipe
/// is not an option: chat rows are keyed by device id, so keeping them would
/// orphan every one of them behind the new device anyway.
pub async fn wipe() -> std::io::Result<()> {
    let Some(dir) = database_dir() else {
        return Ok(());
    };
    // -wal and -shm hold committed pages SQLite would replay into a fresh file.
    for suffix in ["", "-wal", "-shm"] {
        let path = dir.join(format!("{DB_FILE}{suffix}"));
        match std::fs::remove_file(&path) {
            Ok(()) => info!("Removed {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Per-user data directory, under the platform data root.
fn database_dir() -> Option<std::path::PathBuf> {
    let not_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(std::path::PathBuf::from(v));
    let data_root = if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .and_then(not_empty)
            .map(|home| home.join("Library/Application Support"))
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA")
            .and_then(not_empty)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .and_then(not_empty)
                    .map(|profile| profile.join("AppData").join("Local"))
            })
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .and_then(not_empty)
            .or_else(|| {
                std::env::var_os("HOME")
                    .and_then(not_empty)
                    .map(|home| home.join(".local/share"))
            })
    };

    let dir = data_root.map(|root| root.join(DATA_DIR))?;
    // SQLite won't create missing parent directories itself.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("Failed to create data dir: {e}; using CWD-relative {DB_FILE}");
        return None;
    }
    Some(dir)
}

/// How the database is opened here: the crate's own defaults.
///
/// A real file under a real VFS, so every knob the store offers means what it
/// says.
pub fn settings() -> whatsapp_rust_sqlite_storage::SqliteStoreConfig {
    whatsapp_rust_sqlite_storage::SqliteStoreConfig::default()
}
