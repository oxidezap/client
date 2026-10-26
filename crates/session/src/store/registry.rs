//! Account-scoped handles for the shared WhatsApp database.
//!
//! This is deliberately the only session-layer entry point that turns an
//! [`AccountId`] into a SQLite store, and the only one that lists, creates,
//! resets or removes one. The lifecycle primitives it calls
//! belong to `SqliteDatabase`; this registry never touches the private
//! `device` table itself.
//!
//! **Caller obligation, inherited from WR-1 as-is.** Neither `reset_device` nor
//! `remove_device` invalidates a live handle for that account: a session still
//! running against the old `device_id` can still write through it and
//! repopulate what was just purged. The caller here is
//! `crate::store::StoreRegistry`'s caller — the daemon's account runtime
//! lifecycle — and it must fully stop that account's session (disconnect,
//! flush, drop every handle) *before* calling [`StoreRegistry::reset_account`]
//! or [`StoreRegistry::remove_account`]. This module has no way to enforce
//! that; it only names it.

use std::sync::Arc;

use oxidezap_core::AccountId;
use tokio::sync::OnceCell;
use wacore::store::error::StoreError;

#[cfg(not(target_family = "wasm"))]
type Store = whatsapp_rust::store::SqliteStore;
#[cfg(not(target_family = "wasm"))]
type Database = whatsapp_rust::store::SqliteDatabase;
#[cfg(target_family = "wasm")]
type Store = whatsapp_rust_sqlite_storage::SqliteStore;
#[cfg(target_family = "wasm")]
type Database = whatsapp_rust_sqlite_storage::SqliteDatabase;

/// One account as [`StoreRegistry::accounts`] reports it, with no `device`
/// vocabulary leaking past this module: [`Self::id`] is the same opaque
/// [`AccountId`] the rest of the client uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccount {
    pub id: AccountId,
    pub jid: Option<String>,
    pub lid: Option<String>,
    pub push_name: String,
    pub linked: bool,
}

/// The one physical database namespace used by all local accounts.
#[derive(Clone)]
pub struct StoreRegistry {
    database_path: String,
    chat_schema: Arc<OnceCell<()>>,
    /// Opened once. Every account store shares this database's connection
    /// pools and write permits, so concurrent account writers cannot deadlock
    /// by upgrading independent read transactions on the same physical file.
    base: Arc<OnceCell<Database>>,
}

impl std::fmt::Debug for StoreRegistry {
    // The database handle does not implement `Debug`, so name it rather
    // than trying to print its connection pools.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreRegistry")
            .field("database_path", &self.database_path)
            .finish_non_exhaustive()
    }
}

impl StoreRegistry {
    /// Create a registry for the already-prepared shared database.
    #[must_use]
    pub fn new(database_path: impl Into<String>) -> Self {
        Self {
            database_path: database_path.into(),
            chat_schema: Arc::new(OnceCell::new()),
            base: Arc::new(OnceCell::new()),
        }
    }

    /// The database URL/path passed to each account-scoped store.
    #[must_use]
    pub fn database_path(&self) -> &str {
        &self.database_path
    }

    /// Open the store bound to exactly `account_id`.
    ///
    /// This is a sibling of the one shared pool ([`Self::base`]), not a new
    /// connection: opening a sibling store changes only the logical
    /// `device_id` a handle is bound to, and the physical database and its
    /// write-permit semaphore stay the registry's one shared instance. See
    /// the field doc on [`Self::base`] for why that sharing matters.
    pub async fn store(&self, account_id: AccountId) -> Result<Store, StoreError> {
        let base = self.base().await?;
        Ok(base.store(account_id.as_i32()))
    }

    /// The one open pool for this database, opened on first use.
    ///
    /// Every account-scoped handle ([`Self::store`]) and every lifecycle
    /// operation below is a sibling of this same handle, bound to whichever
    /// id it needs via `SqliteDatabase::store`.
    async fn base(&self) -> Result<&Database, StoreError> {
        self.base
            .get_or_try_init(|| async {
                crate::store::open_for_account(&self.database_path, AccountId::LEGACY)
                    .await
                    .map(|store| store.database())
            })
            .await
    }

    /// Every account in this database file, oldest allocation first.
    pub async fn accounts(&self) -> Result<Vec<StoredAccount>, StoreError> {
        let base = self.base().await?;
        let devices = base.list_devices().await?;
        let mut accounts = Vec::with_capacity(devices.len());
        for device in devices {
            match AccountId::new(device.id) {
                Ok(id) => accounts.push(StoredAccount {
                    id,
                    jid: device.pn.map(|jid| jid.to_string()),
                    lid: device.lid.map(|jid| jid.to_string()),
                    push_name: device.push_name,
                    linked: device.linked,
                }),
                // `device.id` is an AUTOINCREMENT primary key, so this is not
                // reachable in practice. Skipping rather than panicking: a
                // corrupt row must not take the whole listing down with it.
                Err(_) => log::error!(
                    "the shared database named a device with a non-positive id ({}); skipping it",
                    device.id
                ),
            }
        }
        Ok(accounts)
    }

    /// Allocate a new local account in this database, race-free.
    ///
    /// The id comes from SQLite's own `AUTOINCREMENT`, inside the transaction
    /// [`whatsapp_rust::store::SqliteDatabase::create_device`] runs, so two
    /// concurrent calls can never return the same id and a removed id is never
    /// reissued.
    pub async fn create_account(&self) -> Result<AccountId, StoreError> {
        let base = self.base().await?;
        let id = base.create_device().await?.device_id();
        AccountId::new(id).map_err(|_| {
            StoreError::Validation(format!(
                "the shared database allocated a non-positive account id ({id})"
            ))
        })
    }

    /// Allocate one separate pairing account. The daemon serializes this operation.
    pub async fn pairing_successor(
        &self,
        id: AccountId,
    ) -> Result<AccountId, oxidezap_chat_store::ChatStoreError> {
        let store = self.store(id).await?;
        match oxidezap_chat_store::ChatStore::pairing_successor(&store).await? {
            Some(id) => Ok(AccountId::new(id)
                .map_err(|_| StoreError::Validation("invalid pairing account".into()))?),
            None => {
                let next = self.create_account().await?;
                oxidezap_chat_store::ChatStore::record_pairing_successor(&store, next.as_i32())
                    .await?;
                Ok(next)
            }
        }
    }

    /// Wipe one account's WhatsApp state and start it over under the same id.
    ///
    /// # Caller obligation
    ///
    /// The caller must have already stopped every live session/writer bound to
    /// `id` before calling this. See the module-level note.
    pub async fn reset_account(&self, id: AccountId) -> Result<(), StoreError> {
        let base = self.base().await?;
        base.reset_device(id.as_i32()).await?;
        Ok(())
    }

    /// Permanently delete one account's WhatsApp state and its `device` row.
    ///
    /// The id is retired for good: SQLite's `AUTOINCREMENT` never reissues it,
    /// so a stale `AccountId` held anywhere in the client can never resolve to
    /// a different person later.
    ///
    /// # Caller obligation
    ///
    /// The caller must have already stopped every live session/writer bound to
    /// `id` before calling this. See the module-level note.
    pub async fn remove_account(&self, id: AccountId) -> Result<(), StoreError> {
        let base = self.base().await?;
        base.remove_device(id.as_i32()).await?;
        Ok(())
    }

    /// Prepare the account-independent chat schema once for this database.
    ///
    /// The first runtime supplies an already-open account-scoped backend; the
    /// migration runner still operates on the shared SQLite handle. Later
    /// runtimes await the same cell and open only their own writer, so N
    /// account startups never race the migration set.
    pub async fn prepare_chat_schema(
        &self,
        store: &Store,
    ) -> Result<(), oxidezap_chat_store::ChatStoreError> {
        self.chat_schema
            .get_or_try_init(|| async {
                oxidezap_chat_store::ChatStore::prepare(store).await?;
                Ok::<(), oxidezap_chat_store::ChatStoreError>(())
            })
            .await
            .map(|_| ())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    /// Two account runtimes starting at once must not run the migration set
    /// twice against the same physical database: `prepare_chat_schema` is
    /// what a real daemon startup calls once per `AccountRuntime`, and this
    /// is the seam the registry step exists to serialize.
    #[tokio::test]
    async fn concurrent_prepare_runs_the_migration_set_once() {
        let database = format!(
            "file:memdb_store_registry_prepare_once_{}?mode=memory&cache=shared",
            std::process::id()
        );

        // Seed both device rows through the public storage API, exactly as
        // the cascade isolation test does: the registry never touches the
        // private `device` table itself.
        let account_a = AccountId::LEGACY;
        let account_b = AccountId::new(2).expect("positive account id");
        let seed_a = whatsapp_rust::store::SqliteDatabase::open(&database, Default::default())
            .await
            .expect("open device A")
            .store(account_a.as_i32());
        seed_a
            .database()
            .provision_device(seed_a.device_id())
            .await
            .expect("create device A");
        let seed_b = whatsapp_rust::store::SqliteDatabase::open(&database, Default::default())
            .await
            .expect("open device B")
            .store(account_b.as_i32());
        seed_b
            .database()
            .provision_device(seed_b.device_id())
            .await
            .expect("create device B");

        let registry = StoreRegistry::new(database);
        let store_a = registry.store(account_a).await.expect("open store A");
        let store_b = registry.store(account_b).await.expect("open store B");

        // Both runtimes ask to prepare the schema at once; only one of them
        // may actually run the migration set, and the other must simply wait
        // for it rather than racing a second runner over the same file.
        let (first, second) = tokio::join!(
            registry.prepare_chat_schema(&store_a),
            registry.prepare_chat_schema(&store_b),
        );
        first.expect("first preparation succeeds");
        second.expect("second preparation observes the same completed schema");

        // Both account-scoped writers can now open without repreparing.
        oxidezap_chat_store::ChatStore::new_prepared(&store_a)
            .await
            .expect("writer A opens against the prepared schema");
        oxidezap_chat_store::ChatStore::new_prepared(&store_b)
            .await
            .expect("writer B opens against the prepared schema");
    }

    fn database_url(name: &str) -> String {
        format!(
            "file:memdb_store_registry_{name}_{}?mode=memory&cache=shared",
            std::process::id()
        )
    }

    /// A real temporary file, cleaned up on drop (including `-wal`/`-shm`).
    ///
    /// The shared-cache in-memory URL [`database_url`] builds keeps a database
    /// alive across independent connections, but SQLite's shared cache uses
    /// table-level locking that `busy_timeout` cannot absorb
    /// (`SQLITE_LOCKED_SHAREDCACHE`): two accounts' writers, each its own
    /// `SqliteStore` pool, hit "database table is locked" against each other
    /// under it. A real file uses ordinary WAL/file locking instead, which is
    /// what production's on-disk `whatsapp.db` does, and what upstream's own
    /// lifecycle tests use for the same reason.
    struct TempDb(std::path::PathBuf);

    impl TempDb {
        fn new(tag: &str) -> Self {
            use portable_atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "oxidezap_store_registry_{tag}_{}_{id}.db",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }

        fn url(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = self.0.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// A brand-new database has no accounts, and creating one allocates a
    /// positive id that then shows up in the listing.
    #[tokio::test]
    async fn create_account_allocates_and_is_listed() {
        let registry = StoreRegistry::new(database_url("create"));

        assert_eq!(registry.accounts().await.expect("list"), Vec::new());

        let id = registry.create_account().await.expect("create");
        assert!(id.get() > 0);

        let listed = registry.accounts().await.expect("list again");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert!(!listed[0].linked, "a freshly created account is unpaired");
    }

    /// Two accounts created in the same file always get distinct ids, and a
    /// removed id is never handed back to a later creation.
    #[tokio::test]
    async fn ids_are_distinct_and_never_reused_after_removal() {
        let registry = StoreRegistry::new(database_url("no_reuse"));

        let a = registry.create_account().await.expect("create A");
        let b = registry.create_account().await.expect("create B");
        assert_ne!(a, b);

        registry.remove_account(a).await.expect("remove A");
        let c = registry.create_account().await.expect("create C");
        assert_ne!(c, a, "a removed id must never be reissued");
        assert_ne!(c, b);

        let listed: Vec<AccountId> = registry
            .accounts()
            .await
            .expect("list")
            .into_iter()
            .map(|account| account.id)
            .collect();
        assert_eq!(listed, vec![b, c]);
    }

    #[tokio::test]
    async fn relinking_retains_history_and_reuses_an_isolated_account_after_reload() {
        let db = TempDb::new("relink");
        let registry = StoreRegistry::new(db.url());
        let old = registry.create_account().await.unwrap();
        let store = registry.store(old).await.unwrap();
        registry.prepare_chat_schema(&store).await.unwrap();
        let history = oxidezap_chat_store::ChatStore::new_prepared(&store)
            .await
            .unwrap();
        let peer: whatsapp_rust::wacore_binary::Jid =
            "999000000001@s.whatsapp.net".parse().unwrap();
        history
            .record_outgoing(
                &peer,
                "RETAINED",
                &whatsapp_rust::waproto::whatsapp::Message::default(),
                wacore::time::now_utc(),
            )
            .unwrap();
        history.flush().await.unwrap();
        history
            .record_logout("Synthetic unlink".into())
            .await
            .unwrap();
        let next = registry.pairing_successor(old).await.unwrap();
        assert_ne!(old, next);
        assert_eq!(registry.pairing_successor(old).await.unwrap(), next);
        assert!(history.message(&peer, "RETAINED").await.unwrap().is_some());
        let new_store = registry.store(next).await.unwrap();
        let new_history = oxidezap_chat_store::ChatStore::new_prepared(&new_store)
            .await
            .unwrap();
        assert!(
            new_history
                .message(&peer, "RETAINED")
                .await
                .unwrap()
                .is_none()
        );
        assert!(new_history.logout_message().await.unwrap().is_none());
        new_history
            .record_outgoing(
                &peer,
                "NEW-ACCOUNT",
                &whatsapp_rust::waproto::whatsapp::Message::default(),
                wacore::time::now_utc(),
            )
            .unwrap();
        new_history.flush().await.unwrap();
        assert!(
            history
                .message(&peer, "NEW-ACCOUNT")
                .await
                .unwrap()
                .is_none()
        );
        history.close().await.unwrap();
        new_history.close().await.unwrap();
        let reopened = StoreRegistry::new(db.url());
        assert_eq!(reopened.pairing_successor(old).await.unwrap(), next);
        assert_eq!(reopened.accounts().await.unwrap().len(), 2);
        let retained_store = reopened.store(old).await.unwrap();
        let retained = oxidezap_chat_store::ChatStore::new_prepared(&retained_store)
            .await
            .unwrap();
        assert_eq!(
            retained.logout_message().await.unwrap().as_deref(),
            Some("Synthetic unlink")
        );
        assert!(retained.message(&peer, "RETAINED").await.unwrap().is_some());
        retained.close().await.unwrap();
    }

    /// Resetting an account keeps its id but clears everything the account
    /// wrote through the chat store, and never touches another account's rows
    /// in the same physical file.
    #[tokio::test]
    async fn reset_keeps_the_id_and_does_not_touch_other_accounts() {
        let db = TempDb::new("reset");
        let registry = StoreRegistry::new(db.url());

        let a = registry.create_account().await.expect("create A");
        let b = registry.create_account().await.expect("create B");

        let store_a = registry.store(a).await.expect("open A");
        let store_b = registry.store(b).await.expect("open B");
        registry
            .prepare_chat_schema(&store_a)
            .await
            .expect("prepare schema");
        let chat_a = oxidezap_chat_store::ChatStore::new_prepared(&store_a)
            .await
            .expect("chat store A");
        let chat_b = oxidezap_chat_store::ChatStore::new_prepared(&store_b)
            .await
            .expect("chat store B");

        let peer: whatsapp_rust::wacore_binary::Jid =
            "559900000001@s.whatsapp.net".parse().expect("valid jid");
        chat_a
            .record_outgoing(
                &peer,
                "A-MSG",
                &whatsapp_rust::waproto::whatsapp::Message::default(),
                wacore::time::now_utc(),
            )
            .expect("record A's message");
        chat_b
            .record_outgoing(
                &peer,
                "B-MSG",
                &whatsapp_rust::waproto::whatsapp::Message::default(),
                wacore::time::now_utc(),
            )
            .expect("record B's message");
        chat_a.flush().await.expect("flush A");
        chat_b.flush().await.expect("flush B");

        assert!(chat_a.message(&peer, "A-MSG").await.unwrap().is_some());
        assert!(chat_b.message(&peer, "B-MSG").await.unwrap().is_some());

        // The caller obligation the module documents: every handle bound to
        // the account being reset is dropped before the reset call.
        chat_a.close().await.expect("close A's writer");
        drop(store_a);

        registry.reset_account(a).await.expect("reset A");

        let listed = registry.accounts().await.expect("list after reset");
        assert!(
            listed.iter().any(|account| account.id == a),
            "reset keeps the account's id"
        );
        assert!(
            listed.iter().any(|account| account.id == b),
            "reset must not remove the other account"
        );

        // B's own history is untouched by A's reset.
        assert!(chat_b.message(&peer, "B-MSG").await.unwrap().is_some());
        chat_b.close().await.expect("close B's writer");
    }

    /// Removing an account retires its id and takes its chat history with it,
    /// while a sibling account in the same file is left exactly as it was.
    #[tokio::test]
    async fn remove_retires_the_id_and_spares_other_accounts() {
        let registry = StoreRegistry::new(database_url("remove"));

        let a = registry.create_account().await.expect("create A");
        let b = registry.create_account().await.expect("create B");

        let store_a = registry.store(a).await.expect("open A");
        let store_b = registry.store(b).await.expect("open B");
        registry
            .prepare_chat_schema(&store_a)
            .await
            .expect("prepare schema");
        let chat_a = oxidezap_chat_store::ChatStore::new_prepared(&store_a)
            .await
            .expect("chat store A");
        let chat_b = oxidezap_chat_store::ChatStore::new_prepared(&store_b)
            .await
            .expect("chat store B");

        let peer: whatsapp_rust::wacore_binary::Jid =
            "559900000001@s.whatsapp.net".parse().expect("valid jid");
        chat_b
            .record_outgoing(
                &peer,
                "B-KEEP",
                &whatsapp_rust::waproto::whatsapp::Message::default(),
                wacore::time::now_utc(),
            )
            .expect("record B's message");
        chat_b.flush().await.expect("flush B");

        chat_a.close().await.expect("close A's writer");
        drop(store_a);

        registry.remove_account(a).await.expect("remove A");

        let listed: Vec<AccountId> = registry
            .accounts()
            .await
            .expect("list after remove")
            .into_iter()
            .map(|account| account.id)
            .collect();
        assert_eq!(listed, vec![b]);

        assert!(chat_b.message(&peer, "B-KEEP").await.unwrap().is_some());
        chat_b.close().await.expect("close B's writer");
    }
}
