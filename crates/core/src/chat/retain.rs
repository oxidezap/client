//! What a conversation keeps in memory, and what it lets go of.
//!
//! A message holds its media as bytes for as long as the row is loaded, and
//! nothing used to take them back: the daemon's disk cache and the page's
//! media map bound what is *cached*, never what the interface is *retaining*,
//! so a sweep could drop a cache entry whose bytes were still alive through a
//! message that named them. On a desktop that is a window growing for as long
//! as it is open; in a tab it is a linear memory with a one-gigabyte ceiling,
//! so the web feels it first.
//!
//! # Who decides a row is far off screen
//!
//! Not this crate. `oxidezap-core` is domain types with no viewport, no
//! frame and no idea which conversation is on screen, and putting one here
//! would be a front end hiding inside the wire format. What lives here is the
//! *arithmetic*: given an order and a budget, which rows have to let go. The
//! front end supplies the judgement — it names the rows it is holding open
//! and the allowance the rest share — because it is the only thing that knows
//! what is being looked at.
//!
//! The order is the conversation's own: newest first. A row's bytes are kept
//! while the budget lasts and released after it, so what survives is the end
//! of the timeline, which is where a reader is unless they have gone looking.
//!
//! # What may be let go of
//!
//! Only bytes that can be got back — a payload with a `downloadable` beside
//! it. Everything else stays, whatever the budget says: a voice note recorded
//! here and not yet sent, a poster frame that is the only picture the row
//! will ever have, a document composed locally. Releasing one of those would
//! not be eviction, it would be deletion.
//!
//! A released row is left in exactly the state a row whose media never
//! arrived is already in — no bytes, and a `downloadable` saying where they
//! are — which is what makes the re-fetch free: every front end already draws
//! that as an offer to download, and the press that accepts it is the same
//! press it has always been. **This is also why the policy costs no protocol
//! change**: `data` is `#[serde(skip)]` and never crossed the wire, and no
//! field was added to say a row had been released, because the state is one
//! the type could already hold and every reader already answers.

use super::Chat;
use super::media::MediaContent;
use super::message::ChatMessage;
use std::sync::Arc;

/// What one sweep let go of.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReleasedMedia {
    /// The ids of the rows whose bytes were released.
    ///
    /// Returned rather than counted, because a front end that caches anything
    /// *derived* from those bytes has to drop it in the same breath — the
    /// window keys decoded images by message id, and an entry left behind
    /// holds the picture this just released the encoded copy of.
    pub rows: Vec<String>,
    /// How many bytes those rows were holding.
    ///
    /// What the rows *named*, not what the allocator got back: one payload is
    /// shared by every row that names it, so releasing one handle of two
    /// frees nothing until the second goes. Sharing the `Arc` is what made
    /// this cheap in the first place and is not worth undoing to make the
    /// number exact.
    pub bytes: usize,
}

impl ReleasedMedia {
    /// Whether anything was let go of at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl MediaContent {
    /// Whether these bytes can be let go of and got back again.
    ///
    /// Three conditions, and each of them is a row this must not silently
    /// empty: there have to *be* bytes, they have to be the media rather than
    /// a preview standing in for it — a thumbnail is a few kilobytes and the
    /// only picture the row can draw before a fetch — and there has to be
    /// somewhere to fetch them from.
    #[must_use]
    pub fn is_releasable(&self) -> bool {
        !self.data.is_empty() && !self.data_is_preview && self.can_download()
    }

    /// Let go of bytes that can be fetched again, and answer how many.
    ///
    /// The inverse of [`adopt_full_bytes`](Self::adopt_full_bytes), and
    /// deliberately not its exact inverse: the MIME type is left describing
    /// the media that will come back, because that is still what the
    /// `downloadable` will deliver. Nothing is left saying the row was ever
    /// materialized, which is the point — see the module note.
    pub fn release(&mut self) -> usize {
        if !self.is_releasable() {
            return 0;
        }
        let released = self.data.len();
        self.data = Arc::new(Vec::new());
        released
    }
}

impl Chat {
    /// Let go of the media this conversation is holding beyond `budget_bytes`.
    ///
    /// Rows are considered newest first, and `keep` is asked about every one
    /// of them: a row the caller is holding open — playing, in a viewer, mid
    /// download — keeps its bytes whatever the budget says, and still counts
    /// against it, because it is resident either way and pretending otherwise
    /// would push the allowance past what the heap actually holds.
    ///
    /// A row that cannot be re-fetched is kept for the same reason and with
    /// the same accounting; see [`MediaContent::is_releasable`].
    ///
    /// `budget_bytes` of zero is the honest way to say "this conversation is
    /// not being looked at": everything re-fetchable goes.
    pub fn release_media(
        &mut self,
        budget_bytes: usize,
        keep: impl Fn(&ChatMessage) -> bool,
    ) -> ReleasedMedia {
        let mut released = ReleasedMedia::default();
        let mut held = 0usize;

        for message in self.messages.iter_mut().rev() {
            // Asked before the media is borrowed mutably, and asked about
            // every row rather than only the ones over the budget: a caller's
            // predicate is about the row, not about how much came before it.
            let pinned = keep(message);
            let Some(media) = message.media.as_mut() else {
                continue;
            };
            let weight = media.data.len();
            if weight == 0 {
                continue;
            }

            if !pinned && held.saturating_add(weight) > budget_bytes {
                let let_go = media.release();
                if let_go > 0 {
                    released.bytes = released.bytes.saturating_add(let_go);
                    released.rows.push(message.id.clone());
                    continue;
                }
                // Nothing to fetch it back from, so it stays and is counted:
                // it is resident either way, and an allowance that ignored it
                // would be a number about a heap that does not exist.
            }

            held = held.saturating_add(weight);
        }

        released
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::media::{DownloadableMedia, MediaType};
    use crate::chat::message::make_message;
    use wacore::download::MediaType as DownloadMediaType;

    /// A photo of `bytes` bytes that the row could fetch again.
    fn fetchable_photo(bytes: usize) -> MediaContent {
        MediaContent::image(Arc::new(vec![7; bytes]), "image/jpeg".to_string(), false)
            .with_download(Some(DownloadableMedia {
                direct_path: "/v/t62".to_string(),
                media_key: vec![1],
                file_enc_sha256: vec![2],
                file_length: bytes as u64,
                mime_type: "image/jpeg".to_string(),
                duration_secs: None,
                download_type: DownloadMediaType::Image,
            }))
    }

    /// A conversation of `count` photos, oldest first, each `bytes` long.
    fn chat_of_photos(count: usize, bytes: usize) -> Chat {
        let mut chat = Chat::new("a@s.whatsapp.net".to_string());
        for index in 0..count {
            let mut message = make_message(&format!("m{index}"), index as i64);
            message.media = Some(fetchable_photo(bytes));
            chat.messages.push(message);
        }
        chat
    }

    /// How many bytes the conversation is holding through its rows.
    fn resident(chat: &Chat) -> usize {
        chat.messages
            .iter()
            .filter_map(|message| message.media.as_ref())
            .map(|media| media.data.len())
            .sum()
    }

    /// The defect: a conversation's media had no ceiling at all, so what the
    /// interface retained grew with the history rather than with the budget.
    /// A hundred photos is an ordinary album and half a gigabyte of tab.
    #[test]
    fn a_conversation_holds_no_more_media_than_its_budget() {
        let mut chat = chat_of_photos(100, 1024 * 1024);
        assert_eq!(resident(&chat), 100 * 1024 * 1024);

        let released = chat.release_media(8 * 1024 * 1024, |_| false);

        assert!(
            resident(&chat) <= 8 * 1024 * 1024,
            "{} bytes still resident",
            resident(&chat)
        );
        assert_eq!(released.rows.len(), 92);
        assert_eq!(released.bytes, 92 * 1024 * 1024);
    }

    /// What survives is the end of the conversation, which is where a reader
    /// is unless they have gone looking — and the caller says which rows they
    /// have gone looking at.
    #[test]
    fn the_newest_rows_are_the_ones_kept() {
        let mut chat = chat_of_photos(6, 10);

        chat.release_media(30, |_| false);

        let kept: Vec<&str> = chat
            .messages
            .iter()
            .filter(|message| {
                message
                    .media
                    .as_ref()
                    .is_some_and(|media| !media.data.is_empty())
            })
            .map(|message| message.id.as_str())
            .collect();
        assert_eq!(kept, ["m3", "m4", "m5"]);
    }

    /// A released row is left in the state a row whose media never arrived is
    /// already in: no bytes, and a `downloadable` saying where they are. That
    /// is what makes it re-fetch rather than show nothing — every front end
    /// already draws exactly this as an offer to download.
    #[test]
    fn a_released_row_asks_for_its_bytes_again_rather_than_showing_nothing() {
        let mut chat = chat_of_photos(2, 4096);

        let released = chat.release_media(0, |_| false);
        assert_eq!(released.rows, ["m1", "m0"]);

        for message in &chat.messages {
            let media = message.media.as_ref().expect("the row still has media");
            assert!(!media.has_data(), "the bytes are gone");
            assert!(!media.has_still_image(), "and nothing decodes them");
            assert!(media.can_download(), "but the row knows where they were");
            assert!(media.can_play());
        }

        // And the bytes coming back put the row where it was.
        let media = chat.messages[0]
            .media
            .as_mut()
            .expect("the row still has media");
        media.adopt_full_bytes(Arc::new(vec![7; 4096]));
        assert!(media.has_still_image());
    }

    /// Bytes with nowhere to fetch them from are not evictable, they are the
    /// only copy: a voice note recorded here and not yet sent, a poster frame
    /// that is the row's only picture. Releasing one is deletion.
    #[test]
    fn media_that_cannot_be_fetched_again_is_never_let_go_of() {
        let mut chat = Chat::new("a@s.whatsapp.net".to_string());

        let mut recorded = make_message("recorded", 1);
        recorded.media = Some(MediaContent::audio(
            Arc::new(vec![1; 4096]),
            "audio/ogg".to_string(),
            Some(3),
            None,
        ));
        chat.messages.push(recorded);

        let mut poster = make_message("poster", 2);
        poster.media = Some(MediaContent::video(Arc::new(vec![2; 4096]), Some(15)));
        chat.messages.push(poster);

        assert!(chat.release_media(0, |_| false).is_empty());
        assert_eq!(resident(&chat), 8192);
    }

    /// What the interface is holding open stays, whatever the budget says —
    /// and still counts against it, because it is resident either way.
    #[test]
    fn a_row_the_interface_is_holding_open_keeps_its_bytes() {
        let mut chat = chat_of_photos(4, 1000);

        let released = chat.release_media(0, |message| message.id == "m0");

        assert_eq!(released.rows, ["m3", "m2", "m1"]);
        assert_eq!(
            chat.messages[0]
                .media
                .as_ref()
                .map(|media| media.data.len()),
            Some(1000),
            "the row somebody is looking at is not evicted out from under them"
        );
    }

    /// A preview is a thumbnail: kilobytes, and the only thing the row can
    /// draw before a fetch. Releasing one buys nothing and costs the picture.
    #[test]
    fn a_preview_is_not_worth_letting_go_of() {
        let mut chat = Chat::new("a@s.whatsapp.net".to_string());
        let mut message = make_message("m0", 1);
        message.media = Some(
            MediaContent::image(Arc::new(vec![3; 512]), "image/jpeg".to_string(), true)
                .with_download(Some(DownloadableMedia {
                    direct_path: "/v/t62".to_string(),
                    media_key: vec![1],
                    file_enc_sha256: vec![2],
                    file_length: 4096,
                    mime_type: "image/jpeg".to_string(),
                    duration_secs: None,
                    download_type: DownloadMediaType::Image,
                })),
        );
        chat.messages.push(message);

        assert!(chat.release_media(0, |_| false).is_empty());
        assert_eq!(resident(&chat), 512);
    }

    /// The sweep is about media, and a conversation is mostly text: a chat of
    /// rows with nothing hanging off them is not something to walk twice.
    #[test]
    fn a_conversation_of_text_releases_nothing() {
        let mut chat = Chat::new("a@s.whatsapp.net".to_string());
        for index in 0..8 {
            chat.messages
                .push(make_message(&format!("m{index}"), index));
        }
        assert!(chat.release_media(0, |_| false).is_empty());
    }

    /// The media type does not decide it — a video's own file is the largest
    /// thing a conversation holds, and the one most worth letting go of.
    #[test]
    fn a_fetched_video_is_the_biggest_thing_worth_releasing() {
        let mut chat = Chat::new("a@s.whatsapp.net".to_string());
        let mut message = make_message("m0", 1);
        let mut media = MediaContent::video(Arc::new(vec![9; 128]), Some(15)).with_download(Some(
            DownloadableMedia {
                direct_path: "/v/t62".to_string(),
                media_key: vec![1],
                file_enc_sha256: vec![2],
                file_length: 4096,
                mime_type: "video/mp4".to_string(),
                duration_secs: Some(15),
                download_type: DownloadMediaType::Video,
            },
        ));
        media.adopt_full_bytes(Arc::new(vec![9; 4096]));
        assert_eq!(media.media_type, MediaType::Video);
        message.media = Some(media);
        chat.messages.push(message);

        let released = chat.release_media(0, |_| false);
        assert_eq!(released.bytes, 4096);
        assert_eq!(resident(&chat), 0);
    }
}
