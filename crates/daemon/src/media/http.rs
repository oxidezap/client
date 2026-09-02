//! Cached media, over HTTP, for the front end that shares no filesystem.
//!
//! Natively a front end reads `media_path(key)` itself: it is the same
//! machine and the same user, so the daemon hands over a name and nothing
//! else moves. A page has no filesystem, so the same three verbs — read one
//! payload, stage one to send, drop one that will not be sent — are answered
//! over the web bridge's port instead.
//!
//! It lives here rather than under `listener/` because none of it is a
//! transport: what a key means, which keys a caller may write, and what a
//! staged payload costs are all facts about media, and the module beside this
//! one is where the rest of them are. What the bridge lends it is the socket
//! and the handful of HTTP headers in [`crate::listener::web::http`]; the
//! admission check happens there, before any of this is reached.

use anyhow::Result;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;

use crate::listener::web::http::{percent_decode, respond};

/// The most a front end may stage in one payload.
///
/// This one *is* read into the daemon's memory, unlike a served file, because
/// the write has to be a single act: a partly staged payload under a key a
/// send is about to name is worse than a refused one. So the ceiling is what
/// keeps that from being unbounded, and it is sized for what actually goes
/// through here — a voice note or a photo, never a film.
///
/// The number is the protocol's rather than this module's, because the front
/// end has to know it before it reads a file: `oxidezap_ipc` is where the two
/// ends meet, and a client that learned this from a `413` would have paid for
/// the whole read to be told it.
const MAX_UPLOAD_BYTES: u64 = oxidezap_ipc::MAX_STAGED_BYTES;

/// How long a staged payload has to arrive once its head has been read.
///
/// The head has its own deadline in the listener; the body needs one too, and
/// for a sharper reason: this read holds an upload permit *and* a buffer of
/// the declared size while it waits. A client killed mid-upload — no attacker
/// required — otherwise parks both until the process ends, and enough of them
/// leave the daemon refusing every new connection including a front end's.
const BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How many payloads may be in memory at once.
///
/// The per-payload ceiling bounds one upload and the listener's own pending
/// bound counts the connections, so without this the product is what the
/// process holding the account can be made to hold. [`serve`] streams for
/// exactly this reason and staging cannot, so it gets a bound instead.
const MAX_CONCURRENT_UPLOADS: usize = 4;

/// The permits [`MAX_CONCURRENT_UPLOADS`] hands out.
static UPLOAD_SLOTS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_CONCURRENT_UPLOADS);

/// Distinguishes one in-flight staging write from another.
static STAGING_SEQUENCE: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// Hand over one cached payload.
///
/// The same bytes `media_path` names for a front end that shares this
/// filesystem — a page does not, so it reads them over HTTP instead. The key
/// is validated by `media_path` itself, which is what keeps an echoed key
/// from naming a file outside the cache.
pub(crate) async fn serve(stream: &mut TcpStream, key: &str, origin: Option<&str>) -> Result<()> {
    let key = percent_decode(key);
    let Some(path) = oxidezap_ipc::media_path(&key) else {
        return respond(
            stream,
            400,
            "text/plain",
            origin,
            b"that is not a cache key",
        )
        .await;
    };
    // Opened rather than read. A video is tens of megabytes and this process
    // is also the one holding the WhatsApp session — reading each request's
    // payload whole would let a handful of tabs fetching attachments at once
    // put several films on the daemon's heap and take the account down with
    // them. The length comes from the metadata, so the head is still exact.
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) => {
            log::debug!("media {key} is not cached: {e}");
            return respond(stream, 404, "text/plain", origin, b"not cached").await;
        }
    };
    let length = match file.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(e) => {
            log::debug!("media {key} could not be measured: {e}");
            return respond(stream, 404, "text/plain", origin, b"not cached").await;
        }
    };

    // The daemon does not record what a payload was, and the front end
    // already knows: every one of these is named by a message that carried
    // its MIME type.
    let mut head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Length: {length}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n"
    );
    if let Some(origin) = origin {
        head.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\n\
             Access-Control-Allow-Methods: GET, PUT, DELETE, OPTIONS\r\n\
             Vary: Origin\r\n"
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;

    let mut file = tokio::io::BufReader::new(file);
    tokio::io::copy(&mut file, stream).await?;
    stream.flush().await?;
    Ok(())
}

/// Take a payload a front end staged for a send it is about to ask for.
///
/// The mirror of [`serve`], and deliberately not its equal. A page has
/// no filesystem, so a voice note it recorded exists only in its own memory
/// until the daemon can read it — and the daemon reads payloads from disk,
/// because `SendAudio` names a key rather than carrying bytes. This is the
/// only way those two facts meet.
///
/// Three things narrow it, because a write endpoint on the process holding
/// the account deserves more than a read one:
///
/// * Only `u-` keys. `f-` and `d-` are the daemon's own cache of things it
///   fetched and can fetch again; letting a caller write those would let one
///   replace the bytes of a photo already on screen. `u-` is a payload whose
///   only copy is the one being sent, and nothing else writes there.
/// * A ceiling, checked against the declared length *before* anything is
///   read, so an oversized upload costs a header rather than a disk.
/// * The token, which the caller has already passed to get here.
pub(crate) async fn receive(
    stream: &mut BufReader<TcpStream>,
    key: &str,
    origin: Option<&str>,
    length: Option<u64>,
) -> Result<()> {
    let key = percent_decode(key);
    if let Some((status, reason)) = staging_refusal(&key, length) {
        log::warn!("refusing an upload to {key}: {reason}");
        return respond(
            stream.get_mut(),
            status,
            "text/plain",
            origin,
            reason.as_bytes(),
        )
        .await;
    }
    let Some(path) = oxidezap_ipc::media_path(&key) else {
        return respond(
            stream.get_mut(),
            400,
            "text/plain",
            origin,
            b"that is not a cache key",
        )
        .await;
    };
    // Checked above; named here because the read below needs the number.
    let length = length.unwrap_or(0);

    // One at a time, up to the bound: this is the read that holds a payload in
    // the daemon's memory, and the permit is taken before the buffer exists.
    let Ok(_slot) = UPLOAD_SLOTS.try_acquire() else {
        log::warn!("refusing an upload to {key}: too many already in flight");
        return respond(
            stream.get_mut(),
            503,
            "text/plain",
            origin,
            b"too many uploads in flight",
        )
        .await;
    };

    // Exactly the declared length, so a client that promises less than it
    // sends leaves the surplus in the socket rather than in the file, and one
    // that promises more is cut off by the read rather than trusted.
    let mut body = vec![0u8; usize::try_from(length).unwrap_or(0)];
    let read = tokio::time::timeout(BODY_TIMEOUT, stream.read_exact(&mut body)).await;
    if let Err(e) = read.map_err(std::io::Error::from).and_then(|inner| inner) {
        log::warn!("an upload to {key} ended early: {e}");
        return respond(
            stream.get_mut(),
            400,
            "text/plain",
            origin,
            b"the payload ended early",
        )
        .await;
    }

    if let Err(e) = stage_to_disk(&path, &key, &body).await {
        log::error!("could not stage {key}: {e}");
        return respond(
            stream.get_mut(),
            500,
            "text/plain",
            origin,
            b"could not stage that payload",
        )
        .await;
    }
    log::debug!("staged {} bytes under {key}", body.len());
    respond(stream.get_mut(), 204, "text/plain", origin, b"").await
}

/// Put `body` under `path`, directory and all.
///
/// Split out of [`receive`] because it is the whole of what staging does to
/// the filesystem, and both halves of it are worth asserting without a
/// socket in front of them.
///
/// The directory is prepared rather than merely created. `create_dir_all`
/// leaves it at the umask's mode, and this used to be the only path that made
/// it on an account which stages uploads and never caches a download — the
/// repair lives in `put`, so nothing tightened it and nothing swept it. What
/// that costs is in `platform::prepare_dir`: every name here is derived from
/// content the account has published, so a file planted under one while the
/// directory was open is served back as the account's own media.
///
/// Preparing is *only* the privacy check, and no longer walks anything. It
/// used to, and a staging request paid for it: the budget sweep runs on a
/// threshold of *cache* writes and a staged payload advances none of them, so
/// the orphan age rule was reached from here — a `read_dir` plus a `stat` per
/// cached file, once per staged upload, on the thread holding the session.
/// It now runs on a schedule of its own, which also covers the front end that
/// stages by writing the file itself and never reaches this endpoint at all.
/// See `media::reclaim_abandoned_writes_periodically`.
///
/// Still off the reactor, because `prepare_dir` is `std::fs` throughout and
/// this is the thread holding the WhatsApp session — the check is two
/// syscalls now rather than a walk, but blocking is blocking.
async fn stage_to_disk(path: &std::path::Path, key: &str, body: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || super::platform::prepare_dir(&dir)).await??;
    }

    // Written beside the target and renamed onto it. Reading the whole body
    // first stops a short *client* from leaving a partial file; it does not
    // stop a crash or a full disk part way through the write, and what that
    // leaves is a valid-looking key holding a truncated voice note, which the
    // daemon then opens when it handles the send. A rename within one
    // directory is atomic, so the key holds the whole payload or nothing.
    // Named so that no key can address it: `media_path` refuses a leading
    // dot, so this file is not something a caller can read half-written or
    // delete out from under the rename. The counter keeps two uploads of one
    // key from writing over each other's temporary file.
    let partial = path.with_file_name(format!(
        "{}{}-{key}",
        super::STAGING_PARTIAL_PREFIX,
        STAGING_SEQUENCE.fetch_add(1, portable_atomic::Ordering::Relaxed)
    ));
    let staged = async {
        write_new(&partial, body).await?;
        tokio::fs::rename(&partial, path).await
    }
    .await;
    if let Err(e) = staged {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(e.into());
    }
    Ok(())
}

/// Write `body` to a name nothing is already using, refusing what is there.
///
/// `tokio::fs::write` opens whatever the path resolves to and truncates it,
/// so through a symlink it fills somebody else's file as the user holding the
/// session. The name is nothing like a secret: the key is the caller's own and
/// the sequence in front of it starts at zero every run, so a directory that
/// was open long enough for one link to be planted is the whole of what this
/// takes.
///
/// The honest way to meet the name is the leftover of a run that died between
/// the write and the rename — one daemon per user holds the startup lock, so
/// there is no live writer it could belong to. The entry is unlinked once —
/// the link, never its target — and the create tried again.
/// `media::native::write_new` is the same discipline for a download in
/// flight, where the argument for it was made first.
async fn write_new(path: &std::path::Path, body: &[u8]) -> std::io::Result<()> {
    let create = async || {
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
    };
    let mut file = match create().await {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            tokio::fs::remove_file(path).await?;
            create().await?
        }
        other => other?,
    };
    file.write_all(body).await?;
    file.flush().await
}

/// Drop a staged payload whose send is not going to happen.
///
/// Only `u-` keys, for the reason the write takes only those: the daemon's
/// own cache is not a caller's to delete either. Silent about a key that is
/// not there, because the caller is discarding rather than asking.
pub(crate) async fn discard(stream: &mut TcpStream, key: &str, origin: Option<&str>) -> Result<()> {
    let key = percent_decode(key);
    if !is_staged(&key) {
        log::warn!("refusing to discard {key}: only staged payloads may be removed");
        return respond(
            stream,
            403,
            "text/plain",
            origin,
            b"only staged payloads may be removed",
        )
        .await;
    }
    if let Some(path) = oxidezap_ipc::media_path(&key) {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => log::debug!("discarded the staged payload {key}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("could not discard {key}: {e}"),
        }
    }
    respond(stream, 204, "text/plain", origin, b"").await
}

/// Whether this key names a payload a caller staged, and may therefore write
/// or remove.
///
/// `f-` and `d-` are the daemon's own cache of what it fetched and can fetch
/// again; those are not a caller's to replace or delete.
fn is_staged(key: &str) -> bool {
    oxidezap_ipc::is_staged_key(key)
}

/// Why this staging request may not proceed, if it may not.
///
/// Pure, and separate from the handler, because these three are the whole
/// authorization story for the one route on this endpoint that *writes*: the
/// prefix is what keeps a caller out of the daemon's own cache, and the
/// length is what keeps an upload from being unbounded. A guard worth having
/// is a guard worth testing without a socket.
fn staging_refusal(key: &str, length: Option<u64>) -> Option<(u16, &'static str)> {
    // `f-` and `d-` are the daemon's cache of what it fetched and can fetch
    // again; writing those would let a caller replace the bytes behind a
    // photo already on screen. `u-` is a payload whose only copy is the one
    // being sent, and nothing else writes there.
    if !is_staged(key) {
        return Some((403, "only staged payloads may be written"));
    }
    let Some(length) = length else {
        return Some((411, "a staged payload must declare its length"));
    };
    // Before a byte is read, so an oversized upload costs a header rather
    // than a disk.
    if length > MAX_UPLOAD_BYTES {
        return Some((413, "that payload is too large to stage"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon's own cache is not writable through the bridge. Without
    /// this a caller holding the token could replace the bytes behind a photo
    /// already drawn, which is a different power from staging one to send.
    #[test]
    fn only_staged_keys_may_be_written() {
        for key in ["f-abc", "d-abc", "abc", "approvals", ".."] {
            assert_eq!(
                staging_refusal(key, Some(16)).map(|(status, _)| status),
                Some(403),
                "{key} should not be writable"
            );
        }
        assert_eq!(staging_refusal("u-abc", Some(16)), None);
    }

    /// The length decides how much is read, so a request without one has no
    /// bound to be held to.
    #[test]
    fn a_staged_payload_declares_its_length() {
        assert_eq!(
            staging_refusal("u-abc", None).map(|(status, _)| status),
            Some(411)
        );
    }

    /// Refused from the header rather than discovered by accepting it: this
    /// payload is read into the process holding the account.
    #[test]
    fn an_oversized_payload_is_refused_before_it_is_read() {
        assert_eq!(
            staging_refusal("u-abc", Some(MAX_UPLOAD_BYTES + 1)).map(|(status, _)| status),
            Some(413)
        );
        assert_eq!(staging_refusal("u-abc", Some(MAX_UPLOAD_BYTES)), None);
    }

    /// What the mode assertions below need and Windows has not got: there is
    /// no mode to read there, and `private_dir::prepare` answers for a
    /// directory that is already inside the user's own profile. See
    /// docs/roadmap.md on the half of this that is still open there.
    #[cfg(unix)]
    mod modes {
        use std::os::unix::fs::PermissionsExt as _;

        fn scratch(name: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "oxidezap-media-http-{}-{:?}-{name}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            dir
        }

        /// The gap this module's staging used to leave open. An account that
        /// stages a voice note and never caches a download never reaches
        /// `put`, which is where the repair lives — so the directory kept
        /// whatever mode the umask gave it, under names another local account
        /// can predict, with nothing ever sweeping what they left there.
        #[tokio::test]
        async fn staging_makes_the_cache_ours_without_a_put() {
            let dir = scratch("stage");
            std::fs::create_dir_all(&dir).unwrap();
            // As `create_dir_all` under a shared-group umask leaves it.
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();

            let path = dir.join("u-audio_4242_1764000000000_0");
            super::super::stage_to_disk(&path, "u-audio_4242_1764000000000_0", b"a voice note")
                .await
                .expect("the payload is staged");

            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700,
                "the cache was left reachable by other accounts on this machine"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"a voice note");
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// And a directory that was open is one whose contents are suspect:
        /// the staging partial's name is the sequence and the key, both of
        /// which a caller can work out, so a link planted under one turned
        /// this write into the daemon truncating somebody else's file as the
        /// user holding the session.
        #[tokio::test]
        async fn a_staging_write_does_not_follow_a_planted_link() {
            let dir = scratch("link");
            std::fs::create_dir_all(&dir).unwrap();
            let victim = dir.join("victim");
            std::fs::write(&victim, b"somebody else's file").unwrap();
            let partial = dir.join(".staging-0-u-audio_4242_1764000000000_0");
            std::os::unix::fs::symlink(&victim, &partial).unwrap();

            super::super::write_new(&partial, b"a voice note")
                .await
                .expect("a leftover name is not a failure");

            assert_eq!(
                std::fs::read(&victim).unwrap(),
                b"somebody else's file",
                "the payload was written through the link"
            );
            assert_eq!(
                std::fs::read(&partial).unwrap(),
                b"a voice note",
                "and the write went somewhere"
            );
            assert!(
                !std::fs::symlink_metadata(&partial)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "the link was kept and written through"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A discard is narrowed the same way a write is: the daemon's own cache
    /// is not a caller's to delete either.
    #[test]
    fn only_staged_keys_may_be_discarded() {
        for key in ["f-abc", "d-abc", "abc", "approvals"] {
            assert!(!is_staged(key), "{key} should not be removable");
        }
        assert!(is_staged("u-abc"));
    }
}
