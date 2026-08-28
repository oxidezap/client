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
    fn read(&self, key: &str) -> Result<Vec<u8>, String>;

    /// Put bytes where the daemon will look for them.
    ///
    /// The one direction that goes the other way: a voice note is recorded by
    /// the front end and sent by the daemon, so the payload is staged under a
    /// key the request then names.
    ///
    /// # Errors
    ///
    /// Where there is no shared place to stage into — a page cannot hand the
    /// daemon a file — which is also why nothing on that platform records.
    fn stage(&self, key: &str, bytes: &[u8]) -> Result<(), String>;

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
    fn read(&self, key: &str) -> Result<Vec<u8>, String> {
        oxidezap_ipc::media_path(key)
            .ok_or_else(|| format!("the daemon named an unusable cache key: {key}"))
            .and_then(|path| std::fs::read(path).map_err(|e| e.to_string()))
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

/// What the bridge has already served, held until the frame that names it has
/// been applied.
///
/// Filled by the reader before it hands a frame on, and emptied by the same
/// pass: nothing here outlives the frame it was fetched for, because the
/// decoded image cache above is what actually remembers media.
#[cfg(target_family = "wasm")]
#[derive(Default)]
pub struct Fetched {
    bytes: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

#[cfg(target_family = "wasm")]
impl Fetched {
    /// Hold bytes for the frame about to be applied.
    pub fn put(&self, key: String, bytes: Vec<u8>) {
        self.bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, bytes);
    }

    /// Forget whatever the last frame did not use.
    pub fn clear(&self) {
        self.bytes.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

#[cfg(target_family = "wasm")]
impl MediaCache for Fetched {
    /// Read without consuming.
    ///
    /// One frame can name the same key on more than one message — media is
    /// content-addressed, so a photo forwarded twice is one payload — and
    /// taking it would leave every message after the first drawing a download
    /// offer for bytes that are already here. `clear` is what bounds the map,
    /// once per frame.
    fn read(&self, key: &str) -> Result<Vec<u8>, String> {
        self.bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
            .ok_or_else(|| format!("media {key} was not fetched with its frame"))
    }

    fn stage(&self, _key: &str, _bytes: &[u8]) -> Result<(), String> {
        // Nothing to stage into: the daemon reads a file and a page has no
        // filesystem to put one in. Recording is unavailable on this platform
        // for the same reason, so nothing reaches here in practice — but a
        // send that somehow did must fail loudly rather than silently go out
        // naming bytes that are not there.
        Err("a page cannot stage a payload for the daemon".to_string())
    }

    fn discard(&self, key: &str) {
        self.bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}
