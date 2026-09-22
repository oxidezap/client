//! Attaching files: choose, send, and draw the bubble for it.
//!
//! The twin of [`super::recording`], and the same three acts in the same
//! order — get the payload, hand it to the session, draw the message before
//! the network has said anything. What differs is where the payload comes
//! from: a recording is made here and a file is chosen, so the failure worth
//! reporting is not "the microphone was refused" but "that file is too big"
//! or "four of the five could be read".
//!
//! Nothing in here knows what a browser is. Choosing is
//! [`crate::platform::picker`], staging is the media cache, and both are one
//! question with two answers behind them.

use oxidezap_core::OutgoingMedia;

use gpui_component::input::{InputEvent, InputState};

use crate::components::PreviewFile;

use super::*;

impl WhatsAppApp {
    pub(crate) fn ensure_file_drop(&mut self, cx: &mut Context<Self>) {
        if self.file_drop_listener.is_none() {
            match crate::platform::drop::install(cx.entity().downgrade(), cx.to_async()) {
                Ok(listener) => self.file_drop_listener = Some(listener),
                Err(error) => warn!("file drops are unavailable: {error}"),
            }
        }
    }

    pub(crate) fn drop_paths(&mut self, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        // Before the read, not after it: while the modal is open the bytes
        // are still on the disk, so refusing here costs nothing and reading
        // first would hold up to a trip's worth of memory merely to discard
        // it. The race — a modal opening between this check and the read
        // finishing — stays answered in `offer_dropped_files`.
        if self.paste_preview.is_some() {
            self.warn_preview_busy(cx);
            return;
        }
        let Some((jid, reply, epoch)) = self.prepare_incoming_files(cx) else {
            return;
        };
        let task = cx
            .background_executor()
            .spawn(async move { crate::platform::drop::read_paths(paths) });
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let chosen = task.await;
            let _ = entity.update(cx, |app, cx| {
                if app.incoming_file_epoch != epoch {
                    return;
                }
                match chosen {
                    Ok(chosen) => app.offer_dropped_files(jid, reply, chosen, cx),
                    Err(error) => app.notify_user(error, notices::Tone::Problem, cx),
                }
            });
        })
        .detach();
    }

    /// Whether an async web file read still belongs to this account.
    #[cfg(any(test, target_family = "wasm"))]
    pub(crate) fn incoming_files_are_current(&self, epoch: u64) -> bool {
        self.incoming_file_epoch == epoch
    }

    /// Finish a drop (or a web paste) that arrived with its files already
    /// read: confirm rather than send.
    ///
    /// The busy branch is the race the pre-read checks cannot close — a
    /// modal opening between the check and the read finishing — so arrivals
    /// normally never reach it. A drop there names itself, while a paste
    /// stays silent: the web's duplicate clipboard read can resolve after
    /// the modal already opened for it.
    pub(crate) fn offer_dropped_files(
        &mut self,
        jid: String,
        reply: Option<ReplyDraft>,
        chosen: crate::platform::picker::Chosen,
        cx: &mut Context<Self>,
    ) {
        if !chosen.files.is_empty() && self.paste_preview.is_some() {
            for refusal in &chosen.refused {
                self.notify_user(refusal.clone(), notices::Tone::Problem, cx);
            }
            self.warn_preview_busy(cx);
            return;
        }
        self.open_confirmation(jid, reply, chosen, cx);
    }

    /// Tell whoever dropped a file onto a busy window to come back: saying
    /// nothing would read as the window swallowing the file.
    pub(crate) fn warn_preview_busy(&mut self, cx: &mut Context<Self>) {
        self.notify_user(
            "Finish or cancel the file preview first, then drop again.",
            notices::Tone::Problem,
            cx,
        );
    }

    /// The destination an incoming file — dropped or pasted — would go to.
    ///
    /// `None` answers "nowhere": no visible conversation, or nothing to
    /// send with while offline. The account epoch travels with the destination
    /// so a read started before an account switch cannot open a modal in the
    /// next account. A hidden composer (Settings, the phone's chat list, or
    /// the media viewer) is not a destination for document-wide web pastes.
    pub(crate) fn prepare_incoming_files(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(String, Option<ReplyDraft>, u64)> {
        if self.destination != Destination::Chats || self.showing_settings(cx) {
            return None;
        }
        let jid = self.selected_chat.clone()?;
        // `selected_chat` persists while the mobile list, fullscreen viewer,
        // or Settings replaces the composer. `visible_chat` is reported by
        // the rendered conversation, and is None for all those surfaces.
        if self.visible_chat.as_deref() != Some(jid.as_str()) {
            return None;
        }
        if !self.is_connected() {
            self.notify_user(
                "Files cannot be sent right now: not connected.",
                notices::Tone::Problem,
                cx,
            );
            return None;
        }
        Some((jid, self.reply_to.clone(), self.incoming_file_epoch))
    }

    /// Offer files for confirmation instead of sending them outright.
    ///
    /// Pastes and drops land here; the file chooser keeps its immediate send
    /// below, where no confirmation was ever promised. Refusals are said out
    /// loud whatever happens to the rest, and an empty arrival — everything
    /// refused, or nothing at all — opens nothing.
    pub(crate) fn open_confirmation(
        &mut self,
        jid: String,
        reply: Option<ReplyDraft>,
        chosen: crate::platform::picker::Chosen,
        cx: &mut Context<Self>,
    ) -> bool {
        for refusal in chosen.refused {
            self.notify_user(refusal, notices::Tone::Problem, cx);
        }
        if chosen.files.is_empty() || self.paste_preview.is_some() {
            return false;
        }
        // The preview image is decoded only where it is drawn — a sole
        // file. A batch near the selection ceiling would otherwise retain a
        // second copy of every image in it until confirmation, which on a
        // page comes out of the bounded linear memory.
        let sole = chosen.files.len() == 1;
        let files = chosen
            .files
            .into_iter()
            .map(|file| PreviewFile::with_preview(file, sole))
            .collect::<Vec<_>>();
        // Built against the retained window: the modal opens from event
        // continuations that hold the app but no window. Where none is left
        // — tests that never set one, a window that closed mid-read — the
        // modal still confirms, it just sends without a caption.
        let caption = self.modal_window.and_then(|window| {
            window
                .update(cx, |_, window, cx| {
                    cx.new(|cx| InputState::new(window, cx).placeholder("Add a caption"))
                })
                .ok()
        });
        if let Some(caption) = &caption {
            let entity = cx.entity().clone();
            cx.subscribe(caption, move |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    entity.update(cx, |app, cx| app.confirm_paste_preview(cx));
                }
            })
            .detach();
        }
        let chat_was_visible = self.visible_chat.as_deref() == Some(jid.as_str());
        self.paste_preview = Some(PendingPastePreview {
            jid,
            reply,
            files,
            caption,
            list_scroll: gpui::ScrollHandle::new(),
            chat_was_visible,
        });
        cx.notify();
        true
    }

    /// Ask for files and send them into the open conversation.
    ///
    /// The choosing is asynchronous on both platforms — a modal on one, a
    /// promise on the other — so everything after it happens in a
    /// continuation, and the conversation it was started from travels with it
    /// rather than being read again at the end: somebody who picks a file and
    /// then opens another chat meant to send it to the first.
    pub(super) fn attach_files(&mut self, cx: &mut Context<Self>) {
        let Some(jid) = self.selected_chat.clone() else {
            return;
        };
        if !self.is_connected() {
            self.notify_user(
                "Files cannot be sent right now: not connected.",
                notices::Tone::Problem,
                cx,
            );
            return;
        }

        // Cloned rather than taken, the way a recording's is: the file
        // chooser can be dismissed, and a draft consumed by a dialog nobody
        // chose anything in is a reply the person still thinks they are
        // composing. It is cleared where it is used.
        let reply = self.reply_to.clone();
        let chosen = crate::platform::picker::choose(cx);
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let chosen = chosen.await;
            let _ = entity.update(cx, |app, cx| app.finish_attaching(&jid, reply, chosen, cx));
        })
        .detach();
    }

    /// Send what was chosen, and say what could not be.
    pub(crate) fn finish_attaching(
        &mut self,
        jid: &str,
        reply: Option<ReplyDraft>,
        chosen: Result<crate::platform::picker::Chosen, String>,
        cx: &mut Context<Self>,
    ) {
        let chosen = match chosen {
            Ok(chosen) => chosen,
            Err(e) => {
                error!("the file chooser failed: {e}");
                self.notify_user(e, notices::Tone::Problem, cx);
                return;
            }
        };
        // Dismissed. Not a failure, and not worth a line on screen.
        if chosen.is_empty() {
            return;
        }

        // Every refusal, and each one names its own file: picking four photos
        // and one film has to send the four and say what happened to the
        // fifth, which one line about "some files" does not.
        for refusal in chosen.refused {
            self.notify_user(refusal, notices::Tone::Problem, cx);
        }

        // The quote goes on the first file only. Attaching four photos to
        // answer one message is one answer, and quoting it four times is what
        // the recipient would see otherwise.
        //
        // And only where there is a first file: a trip that refused everything
        // it was given sent nothing, so taking the draft there would clear the
        // reply bar over a message the person is still composing an answer to.
        let mut quoted = if chosen.files.is_empty() {
            None
        } else {
            self.take_reply_draft(reply, cx)
        };
        let mut drawn = false;
        for file in chosen.files {
            drawn |= self.send_attachment(jid, file, quoted.take(), None, cx);
        }

        // Following the file down is only what the sender expects if they are
        // looking at where it landed — the same rule a voice note follows,
        // and for the same reason: reading a conversation must not be yanked
        // to its newest message by something that finished elsewhere.
        if drawn && self.visible_chat.as_deref() == Some(jid) {
            self.scroll_to_last_message();
        }
    }

    /// Consume the draft this send is answering, if it is still that draft.
    ///
    /// One picked while the chooser was open is answering something else, and
    /// clearing it would take down a reply bar the person is still using.
    pub(super) fn take_reply_draft(
        &mut self,
        reply: Option<ReplyDraft>,
        cx: &mut Context<Self>,
    ) -> Option<QuotedMessage> {
        let draft = reply?;
        if self
            .reply_to
            .as_ref()
            .is_some_and(|current| current.message_id == draft.message_id)
        {
            self.reply_to = None;
            if let Some(input) = &self.input_area {
                input.update(cx, |view, cx| view.set_reply(None, cx));
            }
        }
        Some(QuotedMessage::from(draft))
    }

    pub(crate) fn cancel_paste_preview(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self.paste_preview.take().is_some();
        if cancelled {
            cx.notify();
        }
        cancelled
    }

    pub(crate) fn confirm_paste_preview(&mut self, cx: &mut Context<Self>) {
        // The keyboard path bypasses the rendered Send button's disabled
        // state: leaving `Connected` with the modal open must keep the files
        // waiting, not send through a lingering session or discard them.
        if !self.can_send() {
            return;
        }
        let Some(preview) = self.paste_preview.take() else {
            return;
        };
        let caption = preview
            .caption
            .as_ref()
            .map(|caption| caption.read(cx).value().to_string())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        let super::PendingPastePreview {
            jid,
            reply,
            files,
            caption: _,
            list_scroll: _,
            chat_was_visible,
        } = preview;
        let quoted = self.take_reply_draft(reply, cx);
        // One caption for the trip, and it goes on the first file: quoting
        // once is the same rule the refusals follow, and a caption repeated
        // on every file of four reads as four captions to the recipient.
        let mut quoted = quoted;
        let mut caption = caption;
        let mut drawn = false;
        for file in files {
            drawn |= self.send_attachment(&jid, file.file, quoted.take(), caption.take(), cx);
        }
        let destination_still_open = self.destination == Destination::Chats
            && self.selected_chat.as_deref() == Some(jid.as_str());
        if drawn && chat_was_visible && destination_still_open {
            self.scroll_to_last_message();
        }
        cx.notify();
    }

    /// Hand one file to the session and draw its bubble.
    ///
    /// Answers whether a bubble was added, which is what decides if the
    /// timeline should follow it down. A caption, where one was typed in the
    /// confirmation modal, travels both ways the protocol's own does: into
    /// the upload, and into the echo bubble's text — which is how an
    /// incoming captioned photo arrives, so the sender sees what the
    /// recipient will.
    pub(super) fn send_attachment(
        &mut self,
        jid: &str,
        file: crate::platform::picker::Picked,
        quoted: Option<QuotedMessage>,
        caption: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        #[cfg(test)]
        self.attachment_attempts
            .push((file.clone(), caption.clone()));

        let Some(client) = &self.client else {
            warn!("Cannot send a file: client is unavailable");
            self.notify_user(
                format!("{} could not be sent: not connected.", file.file_name),
                notices::Tone::Problem,
                cx,
            );
            return false;
        };

        // The picker's answer rather than the protocol's: `for_mime` says what
        // an `image/*` is, and the picker says which of those actually reach
        // the recipient as a picture. See `picker::kind_for`.
        let kind = crate::platform::picker::kind_for(&file.mime_type);
        let local_id = Self::next_local_id("local_media");
        // Built before the bytes are handed over, because for a picture it
        // *is* those bytes: the sender sees what they sent rather than a
        // placeholder that resolves into it. That costs a second copy of one
        // photo until the upload finishes, which is the trade — a page has a
        // memory ceiling, and a photo is a fraction of what a video would be
        // if this drew one of those the same way.
        let media = echo_of(&file, kind);

        client.send_media_message(
            jid,
            crate::session::Attachment {
                bytes: file.bytes,
                kind,
                mime_type: file.mime_type,
                file_name: file.file_name,
                caption: caption.clone(),
            },
            local_id.clone(),
            quoted.clone(),
        );

        let mut message =
            ChatMessage::new_outgoing_with_media(local_id, caption.unwrap_or_default(), media);
        // The bubble shows the quote too, or the sender sees a bare photo
        // where the recipient sees a reply.
        message.quoted = quoted;
        self.add_message_to_chat(jid, message, cx)
    }
}

/// What to draw for a file that is on its way.
///
/// A picture is drawn from the bytes in hand, because they are the picture —
/// the sender should see what they sent, not a placeholder that resolves into
/// it a second later. A video and a document have nothing to draw until the
/// store hands the message back: this side holds no decoder it can run here,
/// and inventing a poster frame is not something a composer can do.
fn echo_of(
    file: &crate::platform::picker::Picked,
    kind: OutgoingMedia,
) -> oxidezap_core::MediaContent {
    use oxidezap_core::MediaContent;

    match kind {
        OutgoingMedia::Image => {
            let (width, height) = image_size(&file.bytes);
            MediaContent::image(
                Arc::new(file.bytes.clone()),
                file.mime_type.clone(),
                // These *are* the picture, so nothing is left to fetch.
                false,
            )
            .with_size(width, height)
        }
        // No poster frame, and no duration: both are read from the
        // container by the side that builds the message, and this one is
        // about to hand the bytes over rather than parse them again.
        OutgoingMedia::Video => MediaContent::video(Arc::new(Vec::new()), None),
        OutgoingMedia::Document => {
            MediaContent::document(file.mime_type.clone(), Some(file.file_name.clone()))
        }
    }
}

/// A picture's dimensions, from its header alone.
///
/// The bubble lays the image out before it is decoded, and without these it
/// lays it out as a square: a panorama drawn as a square and then corrected on
/// the next frame is a visible jump. The header is a few dozen bytes, so this
/// is not the decode — it is the part of it that is free.
///
/// `None` for anything this build cannot read, which is the honest answer: a
/// HEIC has dimensions and nothing here can say what they are.
fn image_size(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    match image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
    {
        Some((width, height)) => (Some(width), Some(height)),
        None => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use oxidezap_core::{MediaType, OutgoingMedia};

    use super::echo_of;
    use crate::platform::picker::{Picked, kind_for};

    /// A file of this type, with bytes that are nothing in particular: what is
    /// being asserted is what the *type* decides, and no branch here reads a
    /// byte of a document.
    fn picked(file_name: &str, mime_type: &str) -> Picked {
        Picked {
            file_name: file_name.to_string(),
            mime_type: mime_type.to_string(),
            bytes: vec![0; 4096],
        }
    }

    /// A picture in a format the far end will not draw goes as a document, so
    /// the recipient gets a file they can open instead of a bubble that is
    /// blank on every client — see `picker::kind_for`.
    ///
    /// And the bubble for it holds no bytes. The echo carries a copy of the
    /// payload only where those bytes *are* the picture; drawing a document
    /// from them is not something this side can do, so keeping a second copy
    /// of an SVG until the upload finished bought nothing at all.
    #[test]
    fn a_picture_nothing_draws_is_sent_and_echoed_as_a_document() {
        for undrawable in [
            "image/svg+xml",
            "image/heic",
            "image/heif",
            "image/avif",
            "image/tiff",
            "image/bmp",
        ] {
            let file = picked("desenho", undrawable);
            let kind = kind_for(&file.mime_type);
            assert_eq!(kind, OutgoingMedia::Document, "{undrawable}");

            let echo = echo_of(&file, kind);
            assert_eq!(echo.media_type, MediaType::Document, "{undrawable}");
            assert!(
                echo.data.is_empty(),
                "{undrawable} echoed {} bytes it cannot draw",
                echo.data.len()
            );
            // The name still travels, because a document is drawn as one.
            assert_eq!(echo.file_name.as_deref(), Some("desenho"), "{undrawable}");
        }
    }

    /// And a photo is still a photo, drawn from the bytes in hand: the sender
    /// sees what they sent rather than a placeholder that resolves into it.
    #[test]
    fn a_photo_is_still_echoed_from_its_own_bytes() {
        for photo in ["image/jpeg", "image/png", "image/gif", "image/webp"] {
            let file = picked("praia", photo);
            let kind = kind_for(&file.mime_type);
            assert_eq!(kind, OutgoingMedia::Image, "{photo}");

            let echo = echo_of(&file, kind);
            assert_eq!(echo.media_type, MediaType::Image, "{photo}");
            assert_eq!(echo.data.len(), file.bytes.len(), "{photo}");
        }
    }
}
