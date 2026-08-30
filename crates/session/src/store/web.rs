//! The browser's store: IndexedDB, through the relaxed VFS.
//!
//! One name and no directory. The VFS *is* the namespace — it is private to
//! this origin and holds nothing but our database — so a path would be a
//! second naming scheme over a flat store that already has one.
//!
//! # Why this one and not OPFS
//!
//! SQLite's durable VFS on the web is OPFS through a synchronous access
//! handle, and that handle exists in a dedicated worker and nowhere else.
//! This one works in the window, which is where the session runs today, and
//! it is the reason it can run there at all.
//!
//! What it costs is *when* a write lands rather than whether it does. The
//! database is held in memory and changed blocks are written to IndexedDB
//! after the fact, so a tab killed between a commit and its flush loses that
//! commit — which for chat history is a message that comes back on the next
//! hydration, and for Signal state is a ratchet that has to re-establish.
//!
//! An ordinary commit is also not *observable*: the VFS hands back a
//! `WaitCommit` for an import, a deletion and a clear, and nothing at all for
//! the writes a session actually makes. So a quota the browser refuses to go
//! past has nowhere to be reported — the database keeps behaving perfectly
//! all session and the account is gone on the next load. What this module can
//! do about that is say the headroom out loud before it runs out; see
//! [`report_headroom`]. Closing it properly means either a VFS that answers
//! for a commit or the move to OPFS, where the write is the call.
//!
//! Moving the session into a worker and this to OPFS is the hardening, and it
//! changes nothing above [`super`] — which is the whole reason that interface
//! is shaped the way it is. [`prepare`] already *asks* for OPFS before
//! falling back here, so both backends are written and the pragma and the
//! wipe already dispatch on which one answered: the move becomes a change of
//! where this runs rather than a change to what it does. In the window the
//! ask is normally refused, since the synchronous access handle is specified
//! to live in a dedicated worker.

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
    /// it returns, so the durability window above does not exist here and
    /// there is nothing for the headroom warning to be about.
    Durable(OpfsSAHPoolUtil),
    /// The database in memory, with changed blocks pushed to IndexedDB after
    /// the fact.
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
    // Neither of these decides whether the store opens, so neither is allowed
    // to stop it: `persist` is a request a browser may simply decline, and
    // the OPFS ask is refused wherever the synchronous access handle is not
    // reachable — which the specification says is everywhere but a dedicated
    // worker, and this runs in the window.
    request_persistence().await;
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
    // Bounded, and nothing waits on the answer beyond that. This is one log
    // line: `navigator.storage.estimate()` is a promise the browser is under
    // no obligation to settle, and an account that will not open because a
    // quota-reporting API went quiet is a far worse failure than the one the
    // line is warning about.
    let _ = crate::exec::with_timeout(report_headroom(), HEADROOM_ASK).await;
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

/// Ask the browser not to evict this origin.
///
/// The other half of the same worry as [`report_headroom`], and the half that
/// can actually be acted on: persistent storage is not cleared under pressure
/// without asking. A browser that declines is not an error — it decides on
/// its own criteria — so it is said once and the session goes on.
async fn request_persistence() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let promise = window.navigator().storage().persist();
    let Ok(promise) = promise else {
        return;
    };
    match crate::exec::with_timeout(wasm_bindgen_futures::JsFuture::from(promise), HEADROOM_ASK)
        .await
    {
        Some(Ok(granted)) if granted.is_truthy() => {
            info!("the browser will keep this origin's storage");
        }
        Some(Ok(_)) => info!("the browser may evict this origin's storage under pressure"),
        // A refusal and a promise that never settles are the same thing here:
        // one log line either way, and nothing waits on it.
        Some(Err(_)) | None => {}
    }
}

/// How long the headroom question may take before it is abandoned.
///
/// Generous, because the answer is worth having and the browser is usually
/// instant; bounded, because it is a diagnostic on the path that opens the
/// account.
const HEADROOM_ASK: std::time::Duration = std::time::Duration::from_secs(5);

/// How little room may be left before it is worth saying so.
///
/// An account's database is tens of megabytes and grows with its history, so
/// this is a floor under "there is room for what is coming", not under one
/// write.
const HEADROOM_FLOOR: f64 = 64.0 * 1024.0 * 1024.0;

/// Say how much of this origin's storage is spent.
///
/// The only warning available. A write that the browser refuses for quota is
/// dropped inside the VFS with nobody to hand it to, and the page carries on
/// against a database it is holding in memory — so the account is intact all
/// session and absent on the next load. Asking beforehand does not stop that;
/// it puts a line in front of it that says what happened.
async fn report_headroom() {
    let Some(estimate) = storage_estimate().await else {
        return;
    };
    let (usage, quota) = estimate;
    let left = quota - usage;
    if left < HEADROOM_FLOOR {
        log::warn!(
            "this origin has {:.0} MiB of storage left of {:.0} MiB; \
             writes the browser refuses are not reported, so an account kept \
             here may not survive a reload",
            left / (1024.0 * 1024.0),
            quota / (1024.0 * 1024.0)
        );
    } else {
        info!(
            "this origin is using {:.0} MiB of {:.0} MiB",
            usage / (1024.0 * 1024.0),
            quota / (1024.0 * 1024.0)
        );
    }
}

/// `navigator.storage.estimate()`, as bytes used and bytes allowed.
///
/// `None` wherever the browser will not say, which is not a problem: this is
/// a warning, and one that cannot be produced is one nothing depends on.
async fn storage_estimate() -> Option<(f64, f64)> {
    let manager = web_sys::window()?.navigator().storage();
    let estimate = wasm_bindgen_futures::JsFuture::from(manager.estimate().ok()?)
        .await
        .ok()?;
    // Read by name: `StorageEstimate` is a dictionary in web-sys, so it has
    // the setters a caller building one needs and no getters at all. Both
    // fields are optional in the specification too, which is the same answer
    // this function already gives for a browser that will not say.
    let field = |name: &str| {
        js_sys::Reflect::get(&estimate, &wasm_bindgen::JsValue::from_str(name))
            .ok()
            .and_then(|value| value.as_f64())
    };
    Some((field("usage")?, field("quota")?))
}

/// Delete the local session.
///
/// Awaited to the flush, unlike an ordinary write: this one is followed by
/// pairing a new device, and a delete still sitting in memory when the tab
/// reloads would put the dead account straight back.
pub async fn wipe() -> std::io::Result<()> {
    let commit = STORE.with(|cell| match cell.get() {
        // Nothing installed, so nothing was ever written.
        None => Ok(None),
        // Nothing to await: this backend's delete has reached the disk by the
        // time it answers, which is the whole difference between the two.
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
        // and refusing it would refuse the durability it exists for.
        return whatsapp_rust_sqlite_storage::SqliteStoreConfig::default();
    }
    whatsapp_rust_sqlite_storage::SqliteStoreConfig {
        synchronous: whatsapp_rust_sqlite_storage::Synchronous::Off,
        ..Default::default()
    }
}
