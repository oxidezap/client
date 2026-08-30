//! The browser's store: IndexedDB, through the relaxed VFS.
//!
//! One name and no directory. The VFS *is* the namespace — it is private to
//! this origin and holds nothing but our database — so a path would be a
//! second naming scheme over a flat store that already has one.
//!
//! # Two stores, and which one this agent got
//!
//! SQLite's durable VFS on the web is OPFS through a synchronous access
//! handle, and that handle is specified to exist in a dedicated worker and
//! nowhere else. [`prepare`] asks for it anyway and falls back, so the
//! question is answered by the runtime rather than assumed: where the handle
//! is reachable the page gets a store with no durability window at all, and
//! where it is not it gets the one below, which is what the window has today.
//!
//! Asking costs one refused call at startup and buys the thing that matters
//! when the session does move into a worker — the move becomes a change of
//! where this runs rather than a change to what it does.
//!
//! # What the fallback costs
//!
//! What it costs is *when* a write lands rather than whether it does. The
//! database is held in memory and changed blocks are written to IndexedDB
//! after the fact, so a tab killed between a commit and its flush loses that
//! commit — which for chat history is a message that comes back on the next
//! hydration, and for Signal state is a ratchet that has to re-establish.
//!
//! That window cannot be closed from here, and a note that used to say
//! otherwise is why this paragraph is explicit. `WaitCommit` comes back from
//! `import_db`, `delete_db` and `clear_all` and from nothing else: the writes
//! SQLite makes are queued with no notifier at all, so there is no ordinary
//! commit to await and no failed flush to hear about. Nothing here can turn
//! that into a guarantee, which is why [`wipe`] is the one operation that
//! waits — it has a management op to wait *on*.
//!
//! What is left is making eviction less likely and a full quota audible:
//! [`request_durability`] asks the browser to keep this origin's storage and
//! reports what is left. Moving the session into a worker and this to OPFS is
//! the real hardening, and it changes nothing above [`super`] — which is the
//! whole reason that interface is shaped the way it is.

use std::cell::OnceCell;

use log::info;
use sqlite_wasm_rs::WasmOsCallback;
use sqlite_wasm_vfs::relaxed_idb::{RelaxedIdbCfg, RelaxedIdbUtil, install};
use sqlite_wasm_vfs::sahpool::{OpfsSAHPoolCfg, OpfsSAHPoolUtil, install as install_sahpool};

use super::DB_FILE;

/// Which of the two stores this agent got.
///
/// Decided once, at [`prepare`], and asked afterwards by everything whose
/// answer depends on it — which is the pragma the connection opens with and
/// how a wipe deletes.
enum Backend {
    /// OPFS through a synchronous access handle: a commit is on the disk when
    /// it returns, so there is no window to lose one in.
    Durable(OpfsSAHPoolUtil),
    /// The database in memory, with changed blocks pushed to IndexedDB after
    /// the fact. See the durability note above.
    Relaxed(RelaxedIdbUtil),
}

thread_local! {
    /// The installed store, kept for the deletions [`wipe`] does.
    ///
    /// Thread-local rather than global because it is neither `Send` nor
    /// `Sync` — it holds JS objects — and because the whole arrangement rests
    /// on one agent owning the database. SQLite is compiled here with
    /// `SQLITE_THREADSAFE=0`, so a second thread reaching for this is not a
    /// race to make unlikely; it is one to make impossible.
    static STORE: OnceCell<Backend> = const { OnceCell::new() };
}

/// The database's name inside the store.
pub fn database_path() -> String {
    DB_FILE.to_string()
}

/// Install the VFS, make it the default, and read the database into memory.
///
/// Before any connection is opened, and once per agent. Both halves matter:
/// SQLite chooses a VFS when a database is opened, so one installed
/// afterwards would hold nothing while the open database sat in memory and
/// vanished with the tab; and IndexedDB reads are asynchronous while SQLite's
/// are not, so the file has to be there before the first query rather than
/// fetched during it.
///
/// # Errors
///
/// The browser refused IndexedDB — a private window with storage disabled,
/// or a quota already spent.
pub async fn prepare() -> Result<(), String> {
    // Before the store rather than after it: a quota already spent is the
    // reason the next line fails, and asking afterwards would report it as a
    // mystery.
    request_durability().await;

    // OPFS first, because it is the one with no durability window at all: a
    // synchronous access handle writes during the commit rather than after
    // it. It is refused wherever the handle is not reachable — the
    // specification puts it in a dedicated worker, and this runs in the
    // window — so the fallback below is not an error path but the ordinary
    // one until the session moves. Trying anyway costs one refused call at
    // startup and is what makes the move a configuration change rather than a
    // rewrite.
    match install_sahpool::<WasmOsCallback>(&OpfsSAHPoolCfg::default(), true).await {
        Ok(pool) => {
            info!("opened a durable OPFS store");
            STORE.with(|cell| {
                let _ = cell.set(Backend::Durable(pool));
            });
            return Ok(());
        }
        Err(e) => info!("no durable OPFS store here, falling back to IndexedDB: {e:?}"),
    }

    let store = install::<WasmOsCallback>(&RelaxedIdbCfg::default(), true)
        .await
        .map_err(|e| format!("the browser would not open a store: {e:?}"))?;
    store
        .preload_db(vec![DB_FILE.to_string()])
        .await
        .map_err(|e| format!("the store is there but would not load: {e:?}"))?;
    info!(
        "opened the browser store, holding {} file(s)",
        store.count()
    );
    // Kept for [`wipe`], and only the first one needs keeping: `install`
    // registers the VFS under `vfs_name` once and every later call finds it
    // registered and hands back another `RelaxedIdbUtil` over the *same*
    // `&'static VfsAppData` — so a second handle is not a second store, and
    // the one already here deletes out of the pool the newest preload filled.
    // Which is why the refused `set` is discarded rather than repaired:
    // `prepare` runs again after "clear data and pair again", and swapping
    // handles there would be swapping a thing for itself.
    STORE.with(|cell| {
        let _ = cell.set(Backend::Relaxed(store));
    });
    Ok(())
}

/// Whether the store this agent opened writes during the commit.
///
/// Read rather than assumed by [`settings`] and [`wipe`], because the two
/// backends differ in exactly the place durability is decided: one accepts
/// the pragma that describes when a write has landed, and the other has no
/// disk at the moment of the write to describe.
fn is_durable() -> bool {
    STORE.with(|cell| matches!(cell.get(), Some(Backend::Durable(_))))
}

/// Ask the browser to keep this origin's storage, and say what is left.
///
/// Two different questions, asked together because both are about the same
/// tab losing an account. `persist()` moves the origin out of the bucket a
/// browser clears under pressure without asking; a browser that declines is
/// not an error, it is one that decides on its own criteria, so it is said
/// once and the session goes on.
///
/// The quota half is the one that matters more, because running out is
/// silent. This VFS holds the database in memory and pushes changed blocks
/// afterwards, so a refused write surfaces nowhere: the page behaves
/// perfectly all session and the account is gone on reload. Knowing the
/// headroom at open is not a fix, but it is the difference between a
/// diagnosable report and an unreproducible one.
///
/// Never fatal. Every one of these APIs is absent or refused in some ordinary
/// configuration — a private window, an older browser, a user who declined —
/// and none of them is a reason not to run.
async fn request_durability() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let storage = window.navigator().storage();

    match wasm_bindgen_futures::JsFuture::from(
        storage
            .persist()
            .unwrap_or_else(|_| js_sys::Promise::resolve(&wasm_bindgen::JsValue::FALSE)),
    )
    .await
    {
        Ok(granted) if granted.is_truthy() => {
            info!("the browser will keep this origin's storage");
        }
        Ok(_) => info!("the browser may evict this origin's storage under pressure"),
        Err(e) => info!("could not ask the browser to keep storage: {e:?}"),
    }

    let Ok(promise) = storage.estimate() else {
        return;
    };
    let Ok(estimate) = wasm_bindgen_futures::JsFuture::from(promise).await else {
        return;
    };
    let number = |key: &str| {
        js_sys::Reflect::get(&estimate, &wasm_bindgen::JsValue::from_str(key))
            .ok()
            .and_then(|v| v.as_f64())
    };
    if let (Some(usage), Some(quota)) = (number("usage"), number("quota")) {
        let left = quota - usage;
        info!(
            "browser storage: {:.1} MiB used of {:.1} MiB",
            usage / 1_048_576.0,
            quota / 1_048_576.0
        );
        // A database that cannot grow is the failure this whole function
        // exists to make audible, and it arrives with no error of its own.
        if left < LOW_STORAGE_BYTES {
            log::warn!(
                "browser storage is nearly full ({:.1} MiB left): writes may be refused, \
                 and this store cannot report a refused write",
                left / 1_048_576.0
            );
        }
    }
}

/// Where "nearly full" starts, in bytes.
///
/// Generous because the cost of a false warning is a log line and the cost of
/// a missed one is the account.
const LOW_STORAGE_BYTES: f64 = 32.0 * 1_048_576.0;

/// Delete the local session.
///
/// Awaited to the flush, unlike an ordinary write: this one is followed by
/// pairing a new device, and a delete still sitting in memory when the tab
/// reloads would put the dead account straight back.
pub async fn wipe() -> std::io::Result<()> {
    let commit = STORE.with(|cell| match cell.get() {
        // Nothing installed, so nothing was ever written.
        None => Ok(None),
        // Nothing to await: this backend's delete has already reached the
        // disk by the time it answers, which is the whole difference.
        Some(Backend::Durable(pool)) => pool
            .delete_db(DB_FILE)
            .map(|_| None)
            .map_err(|e| std::io::Error::other(format!("could not remove {DB_FILE}: {e:?}"))),
        Some(Backend::Relaxed(store)) => store
            .delete_db(DB_FILE)
            .map(Some)
            .map_err(|e| std::io::Error::other(format!("could not remove {DB_FILE}: {e:?}"))),
    })?;
    if let Some(commit) = commit {
        commit
            .await
            .map_err(|e| std::io::Error::other(format!("the deletion did not land: {e:?}")))?;
    }
    info!("Removed {DB_FILE}");
    Ok(())
}

/// How the database is opened here, which is one setting away from the
/// defaults and not a matter of taste.
///
/// `PRAGMA synchronous` is refused by this VFS at anything but `off` — it
/// answers the file-control with "relaxed-idb vfs only supports
/// synchronous=off" — and the store's default asks for `normal`, so the
/// connection was rejected while being configured and the page opened no
/// database at all.
///
/// Refusing it is the honest thing for the VFS to do rather than a limitation
/// to work around: `synchronous` is a promise about when a write has reached
/// the disk, and this store *has no disk at the moment of the write*. The
/// database is memory and the changed blocks go to IndexedDB afterwards, so
/// there is no ordering here for the pragma to describe. Saying `off` is
/// saying what is already true; the durability window is the module's own
/// subject, above.
pub fn settings() -> whatsapp_rust_sqlite_storage::SqliteStoreConfig {
    if is_durable() {
        // The store's own default, which is `normal`: this backend has a disk
        // at the moment of the write, so the pragma describes something real
        // and refusing it would be refusing the durability it exists for.
        return whatsapp_rust_sqlite_storage::SqliteStoreConfig::default();
    }
    whatsapp_rust_sqlite_storage::SqliteStoreConfig {
        synchronous: whatsapp_rust_sqlite_storage::Synchronous::Off,
        ..Default::default()
    }
}
