//! The browser's store: OPFS, through the pool VFS.
//!
//! One name and no directory. The pool *is* the namespace — it is private to
//! this origin and holds nothing but our database — so a path would be a
//! second naming scheme over a flat pool that already has one.

use std::cell::OnceCell;

use log::info;
use sqlite_wasm_rs::WasmOsCallback;
use sqlite_wasm_vfs::sahpool::{OpfsSAHPoolCfg, OpfsSAHPoolUtil, install};

use super::DB_FILE;

thread_local! {
    /// The installed pool, kept for the deletions [`wipe`] does.
    ///
    /// Thread-local rather than global because it is neither `Send` nor
    /// `Sync` — it holds JS objects — and because the whole point of the
    /// arrangement is that one worker owns the store. Anything reaching for
    /// this from another thread is a bug this makes impossible rather than
    /// unlikely.
    static POOL: OnceCell<OpfsSAHPoolUtil> = const { OnceCell::new() };
}

/// The database's name inside the pool.
pub fn database_path() -> String {
    DB_FILE.to_string()
}

/// Install the OPFS pool VFS and make it the default.
///
/// Before any connection is opened, and once per worker: SQLite chooses a
/// VFS when a database is opened, so a pool installed afterwards would hold
/// nothing while the open database sat in memory, losing everything at the
/// end of the tab's life with no error anywhere.
///
/// # Errors
///
/// The handle is unavailable — which is what a page gets instead of a
/// dedicated worker, and what a browser without OPFS gets everywhere.
pub async fn prepare() -> Result<(), String> {
    let util = install::<WasmOsCallback>(&OpfsSAHPoolCfg::default(), true)
        .await
        .map_err(|e| format!("the browser would not give us a durable store: {e:?}"))?;
    info!("opened the OPFS store, holding {} file(s)", util.count());
    POOL.with(|pool| {
        let _ = pool.set(util);
    });
    Ok(())
}

/// Delete the local session.
///
/// The three names rather than the pool: `clear_all` would be the same thing
/// today and the wrong thing the moment anything else is kept beside the
/// database.
pub fn wipe() -> std::io::Result<()> {
    POOL.with(|pool| {
        let Some(pool) = pool.get() else {
            // Nothing installed, so nothing was ever written.
            return Ok(());
        };
        for suffix in ["", "-wal", "-shm"] {
            let name = format!("{DB_FILE}{suffix}");
            match pool.delete_db(&name) {
                Ok(true) => info!("Removed {name}"),
                Ok(false) => {}
                Err(e) => {
                    return Err(std::io::Error::other(format!(
                        "could not remove {name}: {e:?}"
                    )));
                }
            }
        }
        Ok(())
    })
}
