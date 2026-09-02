//! The media sideband, from the front end's side.
//!
//! A photo is megabytes and the protocol is newline-delimited JSON, so media
//! never travels as a frame: the daemon writes the bytes somewhere and names
//! the key, and the front end reads them back. *Where* is the only part that
//! differs between a front end that shares a filesystem with the daemon and
//! one that shares nothing with it at all.
//!
//! So this is one trait with two implementations, and everything above it —
//! `fill`, the download answer, a staged recording — is written once. The
//! native one is the media cache directory; the web one is what the bridge
//! served over HTTP, already fetched (see [`super::web`]), because a frame is
//! applied synchronously and a fetch is not.

use std::sync::Arc;

/// What to do once a payload is staged, or once staging has failed.
///
/// Boxed because it is handed across a trait whose implementations finish at
/// different times, and `Send` because the native cache is written from
/// whichever thread made the request.
pub type StageThen = Box<dyn FnOnce(Result<(), String>) + Send + 'static>;

/// Where a front end finds the bytes a frame only named.
///
/// `Send + Sync` because the native reader thread holds one while the UI
/// thread holds another; the web implementation is only ever touched from the
/// one thread a page has, and satisfies the bound because what it holds is an
/// ordinary map rather than anything from JS.
pub trait MediaCache: Send + Sync {
    /// The bytes under this key, or why they are not available.
    ///
    /// Never blocking: a frame is applied without awaiting anything, so an
    /// implementation whose bytes arrive over the network has to have them
    /// before the frame reaches it.
    /// Shared, not copied. One frame can name the same payload on many
    /// messages — media is content-addressed, so a photo forwarded into a
    /// hundred chats is one payload — and every reader here puts what it gets
    /// straight into an `Arc`. Handing back a `Vec` meant a hundred rows
    /// allocated a hundred copies of it, so a 10 MiB photo could cost a
    /// gigabyte against a budget that had counted it once.
    fn read(&self, key: &str) -> Result<Arc<Vec<u8>>, String>;

    /// The bytes answering a request somebody is waiting on.
    ///
    /// Separate from [`read`](Self::read) because it also *releases the
    /// claim*: a requested download is held against the cache's own sweep
    /// until it has been handed over, and this is the handing over. What it
    /// does not do is destroy it. Two messages carrying the same content are
    /// one payload under one content-addressed key, so a delivery that
    /// removed it would answer the first request and tell the second that
    /// bytes WhatsApp had already delivered were missing — and a save that
    /// failed on the browser's expired activation would re-download instead
    /// of finding what it just fetched.
    ///
    /// So what is left behind is an ordinary cache entry: reclaimable by the
    /// budget like any other, and there for the retry that is about to want
    /// it.
    ///
    /// Defaults to [`read`](Self::read), which is right wherever there is no
    /// claim to release — a directory the daemon also owns, or a per-frame
    /// map that is cleared wholesale.
    fn read_once(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        self.read(key)
    }

    /// Put bytes where the daemon will look for them.
    ///
    /// The one direction that goes the other way: a voice note is recorded by
    /// the front end and sent by the daemon, so the payload is staged under a
    /// key the request then names.
    ///
    /// # Errors
    ///
    /// Nowhere to write, or the write failed.
    fn stage(&self, key: &str, bytes: &[u8]) -> Result<(), String>;

    /// Stage, and only then run `then`.
    ///
    /// Staging is a local write where the daemon shares a filesystem and a
    /// round trip where it does not, and the request naming the key may not go
    /// out before the bytes have landed — the daemon reads the payload when it
    /// handles the request, so a frame that overtakes its own upload names a
    /// file that is not there yet.
    ///
    /// So the continuation belongs to the implementation rather than to the
    /// caller: the caller's frame is sent from the staging's own completion,
    /// wherever and whenever that happens. The default runs `then` before
    /// returning, which is right for an implementation that has nowhere else
    /// to do the work — every one in this tree has somewhere, and each says
    /// where.
    fn stage_then(&self, key: &str, bytes: Vec<u8>, then: StageThen) {
        then(self.stage(key, &bytes));
    }

    /// Drop a staged payload whose request is never going to run.
    ///
    /// Best effort and silent: the daemon may have taken it already.
    fn discard(&self, key: &str);
}

/// The media cache as a process that shares the daemon's filesystem sees it:
/// a directory both of them can open.
#[cfg(not(target_family = "wasm"))]
pub struct Directory;

#[cfg(not(target_family = "wasm"))]
impl MediaCache for Directory {
    fn read(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        oxidezap_ipc::media_path(key)
            .ok_or_else(|| format!("the daemon named an unusable cache key: {key}"))
            .and_then(|path| std::fs::read(path).map_err(|e| e.to_string()))
            .map(Arc::new)
    }

    fn stage(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        let path = oxidezap_ipc::media_path(key)
            .ok_or_else(|| "no media cache to stage the recording".to_string())?;
        if let Some(dir) = path.parent() {
            make_private(dir).map_err(|e| e.to_string())?;
        }
        write_new(&path, bytes).map_err(|e| e.to_string())
    }

    /// The write, off whichever thread asked for it.
    ///
    /// The default runs [`stage`](Self::stage) inline, and the thread that
    /// asks for one here is the window's own: a send is started from the frame
    /// the person pressed. A staged payload is up to
    /// [`oxidezap_ipc::MAX_STAGED_BYTES`], so that default is sixty four
    /// megabytes of `write` — plus a `create_dir_all` and whatever the disk is
    /// doing — inside a frame, and the window draws nothing at all while it
    /// runs. A page never had this problem: staging there is a `fetch` that
    /// could not be awaited inline anyway, which is why only this half needed
    /// saying.
    ///
    /// Not through `oxidezap_platform::spawn`, which is where the tree's rule
    /// points and is the wrong tool exactly here: its desktop half is
    /// `tokio::spawn`, and the window deliberately owns no Tokio runtime to
    /// spawn onto — see the note beside `smol` in this crate's manifest. What
    /// this needs is not a task but somewhere blocking is allowed, which on a
    /// page is nowhere and on a desktop is a thread.
    ///
    /// One thread rather than one per send, and rather than a pool, so that
    /// what one thread asks for happens in the order it asked: a
    /// [`discard`](Self::discard) posted after a stage lands after it, and two
    /// attachments are written one after the other rather than both at once
    /// through one disk. What that does
    /// *not* settle is two threads racing over one key — the reader thread
    /// discards an abandoned send while the window thread stages it — which is
    /// the race the web half keeps a map of in-flight uploads for and this
    /// half has never had an answer to. It is no worse than it was: those two
    /// calls raced on the same key before this, on their own threads.
    ///
    /// If there is nowhere to hand the work, the write happens here: a stalled
    /// frame is what this cost before, and it is better than a send that never
    /// goes out.
    ///
    /// The cost of the hand-off is the window closing on a queued write: the
    /// worker is not drained at shutdown, so a send begun in the last moments
    /// of a session can end with nothing staged and no frame delivered, where
    /// the inline write had finished before the call returned. It is a
    /// message the daemon never hears about rather than a broken one, and the
    /// alternative was every attachment stalling the frame it was sent from.
    fn stage_then(&self, key: &str, bytes: Vec<u8>, then: StageThen) {
        let key = key.to_string();
        writes::off_thread(Box::new(move || then(Directory.stage(&key, &bytes))));
    }

    /// Ordered behind whatever is already queued, and off the caller's thread
    /// for the reason [`stage_then`](Self::stage_then) gives — the removal is
    /// smaller than the write, but it is the same disk and the same frame.
    fn discard(&self, key: &str) {
        let key = key.to_string();
        writes::off_thread(Box::new(move || {
            if let Some(path) = oxidezap_ipc::media_path(&key) {
                let _ = std::fs::remove_file(path);
            }
        }));
    }
}

/// Somewhere blocking is allowed, for the one process that has one.
///
/// A single worker rather than a pool, because what it is for is ordering as
/// much as it is for latency: the media cache is a directory, two of these
/// naming the same key are a write and a delete, and the daemon reads whatever
/// is there when it handles the frame.
#[cfg(not(target_family = "wasm"))]
mod writes {
    use std::sync::OnceLock;
    use std::sync::mpsc::{Sender, channel};

    /// One filesystem errand, made somewhere it may take its time.
    type Errand = Box<dyn FnOnce() + Send + 'static>;

    /// The worker's queue, or `None` where the thread could not be started.
    ///
    /// Started on the first errand rather than with the process: a front end
    /// that never attaches anything never starts it, and every one of these
    /// arrives on the thread that owns the window.
    static WORKER: OnceLock<Option<Sender<Errand>>> = OnceLock::new();

    /// Hand the errand over, or run it here if there is nobody to hand it to.
    ///
    /// Unbounded, and bounded in practice by what a person can ask for: one
    /// errand per attachment, per voice note and per abandoned send. A bounded
    /// queue would put the wait back on the thread this exists to keep free.
    pub(super) fn off_thread(errand: Errand) {
        let Some(worker) = WORKER.get_or_init(start) else {
            errand();
            return;
        };
        if let Err(unsent) = worker.send(errand) {
            // The worker is gone — it only ends when this sender does, so this
            // is a panic in an errand. Whatever this one is, it still has to
            // happen: the continuation of a staged send is what releases that
            // send's place in the outbox.
            log::error!("the media cache's worker is gone; writing on the caller's thread");
            (unsent.0)();
        }
    }

    fn start() -> Option<Sender<Errand>> {
        let (tx, rx) = channel::<Errand>();
        std::thread::Builder::new()
            .name("oxidezap-media".to_string())
            .spawn(move || {
                for errand in rx {
                    errand();
                }
            })
            .map_err(|e| log::error!("no thread to write the media cache on: {e}"))
            .ok()?;
        Some(tx)
    }
}

/// Make the cache directory, private from the moment it exists.
///
/// `create_dir_all` makes it at the umask's mode, and under a shared-group
/// umask that is a directory another local account can write — where no name
/// is a secret: a staged key is `u-` plus a local id built from this process's
/// id, the millisecond and a counter, and the daemon's own keys are derived
/// from content the account has already published. The daemon writes
/// attachments into the same directory and serves them back as this account's
/// media, so which of the two processes happened to create it must not decide
/// whether it is private.
///
/// What this deliberately does *not* do is repair a directory that is already
/// there. Deciding an existing one is ours — owner, mode, and then sweeping
/// what somebody else left inside — is `oxidezap-daemon`'s `private_dir`, and
/// the front end cannot call it: `gui` never depends on the daemon on a
/// platform where there is a daemon process to depend on, which is the rule
/// its manifest states. It is also the daemon's answer to give: the daemon
/// prepares this directory at startup, before a front end has anything to
/// stage, so the only case left here is the one where nothing has made it
/// yet, and this makes that one `0700` instead of the umask's.
#[cfg(not(target_family = "wasm"))]
fn make_private(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        // The mode applies to what this creates and to nothing that is
        // already there, which is the half above that belongs to the daemon.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        // Nothing to set: the directory is under the profile's own ACL. See
        // docs/roadmap.md.
        std::fs::create_dir_all(dir)
    }
}

/// Write `bytes` to a name nothing is already using, refusing what is there.
///
/// `std::fs::write` opens whatever the path resolves to and truncates it, and
/// a staged key is nothing like a secret — `u-` plus a local id made of this
/// process's id, the millisecond and a counter. Through a symlink planted
/// under one, that is this front end filling somebody else's file as the user
/// whose session it is. The daemon's own writers take the same care from
/// their side of the filesystem the two share; `media::native::write_new` is
/// where the argument for it was made.
///
/// The honest way to meet the name is a leftover from an earlier run whose
/// send never happened, so the entry — the link, never its target — is
/// unlinked once and the create tried again.
#[cfg(not(target_family = "wasm"))]
fn write_new(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let create = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    };
    let mut file = match create() {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(path)?;
            create()?
        }
        other => other?,
    };
    file.write_all(bytes)
}

/// What a page has already fetched, held until the frame that names it has
/// been applied.
///
/// Filled by the reader before it hands a frame on, and emptied by the same
/// pass: nothing here outlives the frame it was fetched for, because the
/// decoded image cache above is what actually remembers media.
///
/// Both page transports need exactly this map and fill it exactly this way —
/// the errand differs (an HTTP request to the bridge, a message to the tab
/// holding the account) and what it lands in does not. What they do *not*
/// share is staging, which is why there are still two caches around this one
/// map rather than one cache: over HTTP a `DELETE` can overtake its own
/// `PUT`, and on a channel that preserves its own order it cannot.
#[cfg(target_family = "wasm")]
#[derive(Default)]
pub struct Held {
    bytes: std::sync::Mutex<std::collections::HashMap<String, Arc<Vec<u8>>>>,
}

#[cfg(target_family = "wasm")]
impl Held {
    /// Hold bytes for the frame about to be applied.
    pub fn put(&self, key: String, bytes: Vec<u8>) {
        self.bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, Arc::new(bytes));
    }

    /// Forget whatever the last frame did not use.
    pub fn clear(&self) {
        self.bytes.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Forget one key, whether or not the frame is done with it.
    pub fn forget(&self, key: &str) {
        self.bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    /// Read without consuming.
    ///
    /// One frame can name the same key on more than one message — media is
    /// content-addressed, so a photo forwarded twice is one payload — and
    /// taking it would leave every message after the first drawing a download
    /// offer for bytes that are already here. [`clear`](Self::clear) is what
    /// bounds the map, once per frame.
    pub fn read(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        self.bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .map(Arc::clone)
            .ok_or_else(|| format!("media {key} was not fetched with its frame"))
    }

    /// Moved out, not copied.
    ///
    /// This map is the page's only copy, and a document can be hundreds of
    /// megabytes: cloning it would hold two in a linear memory that has a
    /// ceiling, for as long as it takes the next frame to clear the map.
    /// Nothing else is going to ask for this key — a download answers one
    /// request — so there is nothing to leave behind.
    pub fn read_once(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        self.bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key)
            .ok_or_else(|| format!("media {key} was not fetched with its frame"))
    }
}

/// What the bridge has already served, and the uploads going the other way.
#[cfg(target_family = "wasm")]
#[derive(Default)]
pub struct Fetched {
    /// The frame's own media, fetched over HTTP before it is applied.
    pub held: Held,
    /// Keys whose upload is in flight, and whether the send was abandoned
    /// while it was.
    ///
    /// A `DELETE` issued while the `PUT` is still going is a race the daemon
    /// cannot settle: the two are separate requests and the write can land
    /// after the removal, leaving the payload staged with nothing that will
    /// ever read it. So a discard during an upload is *recorded* rather than
    /// sent, and the upload's own completion is what removes it, one place
    /// deciding, after the write it is undoing.
    ///
    /// Shared rather than borrowed: the completion that reads it runs in a
    /// spawned task, which cannot borrow this cache.
    uploading: Arc<std::sync::Mutex<std::collections::HashMap<String, bool>>>,
}

#[cfg(target_family = "wasm")]
impl MediaCache for Fetched {
    fn read(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        self.held.read(key)
    }

    fn read_once(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        self.held.read_once(key)
    }

    /// Refused, because staging from a page is not synchronous.
    ///
    /// The bytes go to the daemon over HTTP, which cannot be awaited from
    /// here. [`stage_then`](MediaCache::stage_then) is the one that works, and
    /// this stays as the loud failure for anything that has not been moved
    /// onto it — silently going out naming a payload that is not there is the
    /// outcome worth refusing.
    fn stage(&self, _key: &str, _bytes: &[u8]) -> Result<(), String> {
        Err("a page stages over HTTP, which cannot be awaited here".to_string())
    }

    /// Upload, then continue.
    ///
    /// The page's own copy is dropped when this finishes either way: on
    /// success the daemon holds it, and on failure the send is not going to
    /// run. A tab's memory has a ceiling and a voice note is the one payload
    /// this side allocates whole.
    fn stage_then(&self, key: &str, bytes: Vec<u8>, then: StageThen) {
        let key = key.to_string();
        let base = oxidezap_ipc::web::media_base_url();
        self.uploading
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), false);
        let uploading = Arc::clone(&self.uploading);
        wasm_bindgen_futures::spawn_local(async move {
            let staged = oxidezap_ipc::web::upload_media(&base, &key, &bytes).await;
            let abandoned = uploading
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key)
                .unwrap_or(false);
            if abandoned {
                // Answered before the cleanup, not after it. `then` is what
                // releases this send's place in the outbox, and the discard
                // below carries a deadline: a daemon that stops answering the
                // `DELETE` would otherwise hold every frame queued behind an
                // abandoned voice note for the whole of it. Nothing in the
                // cleanup changes the answer.
                then(Err("that send was abandoned".to_string()));
                // The payload is really there now, so this is the removal the
                // discard could not safely make at the time.
                if staged.is_ok() {
                    wasm_bindgen_futures::spawn_local(async move {
                        oxidezap_ipc::web::discard_media(&base, &key).await;
                    });
                }
                return;
            }
            then(staged);
        });
    }

    /// Both copies: the page's, and the daemon's if one was staged.
    ///
    /// A staged payload is the one thing this side writes to the *other* end,
    /// so forgetting it locally is only half the job — the daemon spares
    /// staged uploads from its cache sweep, so a send abandoned after the
    /// upload landed would leave that file until the account is wiped.
    fn discard(&self, key: &str) {
        self.held.forget(key);
        if !oxidezap_ipc::is_staged_key(key) {
            return;
        }
        {
            // Still on its way: marked instead of removed, because a `DELETE`
            // that overtakes its own `PUT` leaves the payload staged for ever.
            let mut uploading = self.uploading.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(abandoned) = uploading.get_mut(key) {
                *abandoned = true;
                return;
            }
        }
        let key = key.to_string();
        let base = oxidezap_ipc::web::media_base_url();
        wasm_bindgen_futures::spawn_local(async move {
            oxidezap_ipc::web::discard_media(&base, &key).await;
        });
    }
}

/// Two separate claims about staging: that the write does not happen on the
/// thread drawing the window, and that it does not go through whatever is at
/// the name. The second is unix-only — what it asserts is a mode, and Windows
/// has none; docs/roadmap.md carries that half.
#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    /// A staged payload is up to `MAX_STAGED_BYTES` and the thread that asks
    /// for one is the thread drawing the window, so the write must not happen
    /// under it — a frame that stalls for sixty four megabytes of `write` is a
    /// window that draws nothing for as long as the disk takes.
    ///
    /// Asserted on *where* the continuation runs rather than on how long
    /// anything took: the claim is that a frame is not the place for this, not
    /// that a write is fast, and a timing assertion would be a different and
    /// much worse test of it.
    ///
    /// The key is deliberately one `media_path` refuses, so this touches no
    /// media directory belonging to whoever is running the suite. The answer
    /// travels the same way whichever it is.
    #[test]
    fn a_staged_payload_is_written_off_the_calling_thread() {
        let (tx, rx) = channel();
        let here = std::thread::current().id();
        Directory.stage_then(
            "not a usable key/",
            vec![1, 2, 3],
            Box::new(move |staged| {
                let _ = tx.send((std::thread::current().id(), staged));
            }),
        );
        let (there, staged) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the continuation of a staged send has to run");
        assert_ne!(
            here, there,
            "the payload was written on the thread that asked for it"
        );
        // And the continuation still carries the answer, which is what the
        // send's place in the outbox is released on.
        assert!(
            staged.is_err(),
            "a key the cache refuses cannot be staged: {staged:?}"
        );
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oxidezap-gui-stage-{}-{:?}-{name}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The front end can be the one that creates the cache — it stages a
    /// recording before the account has ever cached a download — and
    /// `create_dir_all` made it at the umask's mode. Nothing on this side
    /// ever tightened it afterwards.
    #[cfg(unix)]
    #[test]
    fn a_cache_this_front_end_creates_is_ours_alone() {
        let dir = scratch("create").join("media");
        make_private(&dir).expect("the cache is created");

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "the cache was left reachable by other accounts on this machine"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// And the payload does not go through whatever is at the name. A staged
    /// key is `u-` plus a local id — a process id, a millisecond and a
    /// counter, none of them secret — so a link planted under one made this
    /// write truncate and fill somebody else's file.
    #[cfg(unix)]
    #[test]
    fn staging_does_not_follow_a_planted_link() {
        let dir = scratch("link");
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, b"somebody else's file").unwrap();
        let staged = dir.join("u-audio_4242_1764000000000_0");
        std::os::unix::fs::symlink(&victim, &staged).unwrap();

        write_new(&staged, b"a voice note").expect("a leftover name is not a failure");

        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"somebody else's file",
            "the recording was written through the link"
        );
        assert_eq!(std::fs::read(&staged).unwrap(), b"a voice note");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
