//! Acting on a message: replying, retrying, reacting, jumping to a quote.

use super::*;

impl WhatsAppApp {
    /// Focus target for the call card, so its actions are reachable from the
    /// keyboard while it floats.
    pub fn call_focus(&self) -> &FocusHandle {
        &self.call_focus
    }

    /// Scroll the timeline to a message, for the jump out of a quote.
    ///
    /// A quote is a snapshot, so the original may not be loaded — it can be
    /// older than the window, or deleted. Saying nothing is the honest
    /// outcome there; the quote still shows what was said.
    pub fn jump_to_message(&mut self, message_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat_data() else {
            return;
        };
        let Some(position) = chat.messages.iter().position(|m| m.id == message_id) else {
            debug!("quoted message {message_id} is outside the loaded window");
            return;
        };
        // Timeline coordinates, not message coordinates: dividers and the
        // typing row are items too, so the two indices diverge.
        let cache = self.message_list_cache.borrow();
        let item_ix = cache.get(&chat.jid).and_then(|cache| {
            cache.items.iter().position(
                |item| matches!(item, TimelineItem::Message { ix, .. } if *ix == position),
            )
        });
        drop(cache);

        if let Some(item_ix) = item_ix {
            self.message_list.scroll_to_reveal_item(item_ix);
            cx.notify();
        }
    }

    /// Start composing a reply to `message_id`.
    pub fn begin_reply(&mut self, message_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat_data() else {
            return;
        };
        let Some(message) = chat.messages.iter().find(|m| m.id == message_id) else {
            return;
        };

        let sender_name = if message.is_from_me {
            "You".to_string()
        } else {
            chat.author_name(message)
                .map(str::to_owned)
                .unwrap_or_else(|| chat.name.clone())
        };
        let preview = if message.content.is_empty() {
            message
                .media
                .as_ref()
                .map(|m| {
                    crate::app::chat_row::PreviewGlyph::of(&m.media_type)
                        .label()
                        .to_string()
                })
                .unwrap_or_default()
        } else {
            message.content.clone()
        };

        // `sender` is sent as the quote's participant, so it has to be a JID.
        // A message this window composed carries the literal `"Me"` — that is
        // what `ChatMessage::new_outgoing` writes, and the optimistic row
        // survives the rename its id gets — so replying to something you had
        // only just sent quoted an author no group has.
        let sender = if message.is_from_me {
            self.account_jid
                .clone()
                .or_else(|| self.account_lid.clone())
                .unwrap_or_else(|| message.sender.clone())
        } else {
            message.sender.clone()
        };

        let draft = ReplyDraft {
            message_id: message_id.to_string(),
            sender,
            sender_name,
            preview,
        };

        self.reply_to = Some(draft.clone());
        if let Some(input) = &self.input_area {
            input.update(cx, |view, cx| view.set_reply(Some(draft), cx));
            // Replying is a composing gesture: put the caret where the user
            // is about to type rather than making them click into the field.
            let handle = input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    /// Drop the reply being composed.
    pub fn cancel_reply(&mut self, cx: &mut Context<Self>) {
        self.reply_to = None;
        if let Some(input) = &self.input_area {
            input.update(cx, |view, cx| view.set_reply(None, cx));
        }
        cx.notify();
    }

    /// Send a failed message again.
    ///
    /// A retry is a fresh send, not a resurrection: the original keeps its
    /// failed state and its place in the timeline, because that is what
    /// happened. Re-sending the text is the recovery the user asked for.
    pub fn retry_send(&mut self, message_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat_data() else {
            return;
        };
        let Some(message) = chat.messages.iter().find(|m| m.id == message_id) else {
            return;
        };
        if !message.is_failed() {
            return;
        }
        // A reply that failed is retried as a reply. The draft that produced
        // it was consumed by the first send, so the quote has to come from
        // the message itself.
        let quoted = message.quoted.clone();
        let content = message.content.clone();
        // A voice note has no text and is not therefore beyond recovery: the
        // failed bubble still holds the encoded opus, its length and its
        // waveform, which is everything the send needs. Refusing here made
        // the retry button under one a control that answered a click with
        // nothing.
        let voice = message
            .media
            .clone()
            .filter(|media| media.media_type == MediaType::Audio && !media.data.is_empty());
        let jid = chat.jid.clone();
        let _ = window;

        if !content.is_empty() {
            self.send_quoted(&content, quoted, cx);
        } else if let Some(media) = voice {
            self.send_voice_note(
                &jid,
                (*media.data).clone(),
                media.waveform.as_deref().cloned().unwrap_or_default(),
                media.duration_secs.unwrap_or(0),
                quoted,
            );
            cx.notify();
        }
    }

    /// Offer the emoji picker for a message.
    ///
    /// Not built yet: the reaction path exists inbound only, and a picker that
    /// cannot send is worse than one that says so.
    pub fn open_reaction_picker(
        &mut self,
        message_id: &str,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        debug!("reaction picker for {message_id} is not implemented yet");
    }
}
