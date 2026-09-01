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
    /// caller. Where staging is synchronous this runs `then` before returning
    /// and the ordering is exactly what it always was; where it is not, the
    /// caller's frame is sent from the upload's own completion.
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
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, bytes).map_err(|e| e.to_string())
    }

    fn discard(&self, key: &str) {
        if let Some(path) = oxidezap_ipc::media_path(key) {
            let _ = std::fs::remove_file(path);
        }
    }
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
