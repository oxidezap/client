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

use super::*;

impl WhatsAppApp {
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
    fn finish_attaching(
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
        let mut quoted = self.take_reply_draft(reply, cx);
        let mut drawn = false;
        for file in chosen.files {
            drawn |= self.send_attachment(jid, file, quoted.take(), cx);
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
    fn take_reply_draft(
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

    /// Hand one file to the session and draw its bubble.
    ///
    /// Answers whether a bubble was added, which is what decides if the
    /// timeline should follow it down.
    fn send_attachment(
        &mut self,
        jid: &str,
        file: crate::platform::picker::Picked,
        quoted: Option<QuotedMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(client) = &self.client else {
            warn!("Cannot send a file: client is unavailable");
            self.notify_user(
                format!("{} could not be sent: not connected.", file.file_name),
                notices::Tone::Problem,
                cx,
            );
            return false;
        };

        let kind = OutgoingMedia::for_mime(&file.mime_type);
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
                // Nothing types a caption yet: the composer's own text is a
                // message of its own until there is a step between choosing a
                // file and sending it for the caption to be typed in. The
                // protocol carries one so that step is a front end change and
                // not a protocol change.
                caption: None,
            },
            local_id.clone(),
            quoted.clone(),
        );

        let mut message = ChatMessage::new_outgoing_with_media(local_id, String::new(), media);
        // The bubble shows the quote too, or the sender sees a bare photo
        // where the recipient sees a reply.
        message.quoted = quoted;
        self.add_message_to_chat(jid, message)
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
