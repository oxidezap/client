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
const MAX_UPLOAD_BYTES: u64 = 64 * 1024 * 1024;

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

    if let Some(dir) = path.parent()
        && let Err(e) = tokio::fs::create_dir_all(dir).await
    {
        log::error!("could not make room for {key}: {e}");
        return respond(
            stream.get_mut(),
            500,
            "text/plain",
            origin,
            b"could not stage that payload",
        )
        .await;
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
        tokio::fs::write(&partial, &body).await?;
        tokio::fs::rename(&partial, &path).await
    }
    .await;
    if let Err(e) = staged {
        log::error!("could not stage {key}: {e}");
        let _ = tokio::fs::remove_file(&partial).await;
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
    // Every staging is also an occasion to notice the ones nobody came back
    // for. The cache sweep runs on a threshold of *cache* writes, which a
    // staged upload does not advance, so without this the age rule that
    // reclaims an orphan is only reachable through unrelated traffic.
    if let Some(dir) = oxidezap_ipc::media_dir() {
        super::reclaim_abandoned_writes(&dir);
    }
    respond(stream.get_mut(), 204, "text/plain", origin, b"").await
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
