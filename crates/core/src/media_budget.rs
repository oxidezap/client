//! What a page may hold of an account's media, as one number.

/// How many bytes of media a page may keep resident.
///
/// One number rather than three, because on the web there is one heap. The
/// daemon's cache, the payloads a frame carries to the front end, and the
/// images the interface has decoded are three budgets in three crates, and
/// each of them was written against "the wasm heap" — which they share. Three
/// independent ceilings on one resource is not a ceiling; it is their sum, and
/// nobody was adding them up.
///
/// Two orders of magnitude under the daemon's disk budget, and for a different
/// reason. Disk is cheap and outlives the process; this is a linear memory
/// with a one-gigabyte maximum that is also holding everything being drawn,
/// and it never shrinks. A cache that spent it would not be a slow page — it
/// would be an allocation failure with no way back.
///
/// What is left out is not lost: media the renderer does not have is drawn as
/// an offer to download, which is what it already does for media the daemon
/// never cached.
pub const WEB_MEDIA_BUDGET_BYTES: u64 = 48 * 1024 * 1024;

/// The share of [`WEB_MEDIA_BUDGET_BYTES`] the interface may hold *decoded*.
///
/// A quarter, because these are the most expensive bytes of the three: an
/// entry here is the encoded photo *and* whatever the renderer decoded it
/// into, and the decoded half is not counted anywhere — `gpui` keeps it,
/// keyed by the `Arc<Image>` this cache is what keeps alive.
pub const DECODED_IMAGE_BUDGET_BYTES: u64 = WEB_MEDIA_BUDGET_BYTES / 4;
