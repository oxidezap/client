//! What a page may hold of an account's media, and where that is decided.

/// How many bytes of media a page may keep resident, per cache.
///
/// One number rather than three, because on the web there is one heap. The
/// daemon's cache, the payloads a frame carries to the front end, and the
/// images the interface has decoded are three budgets in three crates, and
/// each of them was written against "the wasm heap" — which they share.
///
/// **Per cache, and that word is load-bearing.** This is one number rather
/// than one *budget*: `media/web.rs` and `session/web.rs` each allow this
/// much, and [`DECODED_IMAGE_BUDGET_BYTES`] allows a quarter of it again on
/// top. What the heap sees in the worst case is their sum — 48 MiB of the
/// daemon's cache, 12 MiB of decoded images, and a frame's fetch of up to
/// another 48 MiB while it is being applied. Naming them here makes them move
/// together and makes the arithmetic possible; it does not do the arithmetic.
/// Coordinating one allowance across three caches in two crates is a change
/// to all three and wants a measurement of what a page actually holds, which
/// the batch that unified these numbers did not have.
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

/// What one conversation may keep resident in the rows themselves.
///
/// The fourth number, and the one that was missing: the three above bound
/// what is *cached*, and a message holding its own bytes is neither of those
/// caches — it is the interface retaining what a cache already let go of. See
/// `Chat::release_media`, which is the arithmetic, and the front end that
/// calls it, which is the judgement.
///
/// A quarter of [`WEB_MEDIA_BUDGET_BYTES`], the same share as the decoded
/// images and for a related reason: a conversation on screen is on the order
/// of twenty rows and a handful of pictures among them, so twelve megabytes
/// is several full-size photos of headroom past what anybody is looking at.
/// Above it are rows a reader would have to scroll to reach, and reaching one
/// re-fetches — which is what the renderer already does for media the daemon
/// never cached.
///
/// The same number on the desktop, deliberately. A window open for a week
/// growing by every photo it has ever drawn is the same defect as a tab
/// running out of address space, only slower and harder to notice.
pub const RETAINED_MEDIA_BUDGET_BYTES: u64 = WEB_MEDIA_BUDGET_BYTES / 4;

/// What the interface may hold *decoded*, on top of the caches above.
///
/// A quarter of [`WEB_MEDIA_BUDGET_BYTES`] — derived from it so the two move
/// together, not carved out of it; see the note on the sum there.
///
/// A quarter because these are the most expensive bytes of the three: an
/// entry here is the encoded photo *and* whatever the renderer decoded it
/// into, and the decoded half is counted nowhere at all — `gpui` keeps it,
/// keyed by the `Arc<Image>` this cache is what keeps alive. It is also why
/// the entry count did not simply give way to this one: bytes here measure
/// the smaller half.
pub const DECODED_IMAGE_BUDGET_BYTES: u64 = WEB_MEDIA_BUDGET_BYTES / 4;
