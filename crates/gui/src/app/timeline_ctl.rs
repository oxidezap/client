//! Acting on a message: replying, retrying, reacting, jumping to a quote.

use super::*;

/// [`Resend`] with the message let go of.
///
/// The answer borrows the chat and sending needs `self` mutably, so this is
/// the same two cases holding what they name.
enum Retry {
    Text(String),
    /// Boxed: a `MediaContent` is ten times the size of a `String`, and this
    /// value exists for one statement.
    VoiceNote(Box<MediaContent>),
}

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
            kind: message
                .media
                .as_ref()
                .and_then(|media| oxidezap_core::QuotedKind::of(&media.media_type)),
        };

        self.reply_to = Some(draft.clone());
        if let Some(input) = &self.input_area {
            input.update(cx, |view, cx| view.set_reply(Some(draft), cx));
        }
        // Replying is a composing gesture: put the caret where the user is
        // about to type rather than making them click into the field.
        self.focus_composer(window, cx);
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
        // Same rule as the composer: in the offline state the history is
        // readable and nothing else. The timeline is still drawn there, so
        // the retry under a failed bubble was a live send out of a window
        // that says it is not connected.
        if !self.can_send() {
            warn!("Cannot send again: this window is offline");
            return;
        }
        let Some(chat) = self.selected_chat_data() else {
            return;
        };
        let Some(message) = chat.messages.iter().find(|m| m.id == message_id) else {
            return;
        };
        // What the bubble asked before drawing the button, asked again here
        // so the two cannot disagree about what a retry can do.
        let Some(again) = message.resend() else {
            return;
        };
        // A reply that failed is retried as a reply. The draft that produced
        // it was consumed by the first send, so the quote has to come from
        // the message itself.
        let quoted = message.quoted.clone();
        // Lifted off the chat before sending, which needs `self` mutably.
        let again = match again {
            Resend::Text(text) => Retry::Text(text.to_owned()),
            Resend::VoiceNote(media) => Retry::VoiceNote(Box::new(media.clone())),
        };
        let jid = chat.jid.clone();
        let _ = window;

        match again {
            Retry::Text(content) => self.send_quoted(&content, quoted, cx),
            Retry::VoiceNote(media) => {
                self.send_voice_note(
                    cx,
                    &jid,
                    (*media.data).clone(),
                    media.waveform.as_deref().cloned().unwrap_or_default(),
                    media.duration_secs.unwrap_or(0),
                    quoted,
                );
                cx.notify();
            }
        }
    }

    /// The emojis the quick-react strip offers, in the order it offers them.
    /// Vote on a poll option.
    ///
    /// One tap, one option: multi-select polls exist on the wire and the
    /// vote carries a list for them, but the bubble offers one option per
    /// tap rather than a ballot to assemble. Offline the daemon would
    /// refuse, so the tap is refused here where the window can say why.
    /// The tap is drawn at once: the vote travels fire-and-forget and no
    /// event answers it, so waiting would leave the chosen option looking
    /// untapped until the next reload.
    pub fn vote_poll(
        &mut self,
        chat_jid: &str,
        message_id: &str,
        option_index: u32,
        cx: &mut Context<Self>,
    ) {
        if !self.can_send() {
            warn!("Cannot vote: this window is offline");
            return;
        }
        let Some(client) = self.client.as_ref() else {
            return;
        };
        client.vote_poll(chat_jid, message_id, vec![option_index]);
        self.my_poll_votes
            .insert((chat_jid.to_string(), message_id.to_string()), option_index);
        self.invalidate_message_cache(chat_jid, cx);
        cx.notify();
    }

    /// The option this window voted for on a poll, if it tapped one.
    pub fn my_poll_vote(&self, chat_jid: &str, message_id: &str) -> Option<u32> {
        self.my_poll_votes
            .get(&(chat_jid.to_string(), message_id.to_string()))
            .copied()
    }

    ///
    /// Fixed rather than the full emoji table: this is a reaction control,
    /// not an emoji picker, and anything typed beyond these travels the
    /// composer's own path. Kept here rather than in the bubble so the strip
    /// and the context menu cannot disagree about what a tap sends.
    pub const QUICK_REACTIONS: [&'static str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

    /// Open the quick-react strip under one message, or close it when it is
    /// already that message's.
    pub fn toggle_reaction_picker(&mut self, message_id: &str, cx: &mut Context<Self>) {
        let open = self.reaction_picker_for.as_deref() == Some(message_id);
        self.reaction_picker_for = (!open).then(|| message_id.to_string());
        // The strip moves every row below it, so the cached measurements the
        // list laid out against are laid out against a timeline without it.
        if let Some(jid) = self.selected_chat_jid() {
            self.invalidate_message_cache(&jid, cx);
        }
        cx.notify();
    }

    /// Send `emoji` as our reaction to `message_id`, or take ours back when
    /// it names the reaction already drawn as ours.
    ///
    /// Painted optimistically: the row is updated before the daemon answers,
    /// and the network echo arriving as `ReactionReceived` confirms it. Our
    /// own sender is the LID first, which is the canonical form the echo
    /// carries — stamping the phone number beside it would draw one person
    /// as two until the next history load.
    pub fn toggle_reaction(
        &mut self,
        message_id: &str,
        emoji: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = window;
        // Same rule as the composer: in the offline state the history is
        // readable and nothing else, and a tap that silently went nowhere
        // would leave the optimistic row as a lie.
        if !self.can_send() {
            warn!("Cannot react: this window is offline");
            return;
        }
        let Some(own) = self
            .account_lid
            .clone()
            .or_else(|| self.account_jid.clone())
        else {
            warn!("Cannot react: the account identity is not known yet");
            return;
        };
        let Some(chat) = self.selected_chat_data() else {
            return;
        };
        let Some(message) = chat.messages.iter().find(|m| m.id == message_id) else {
            return;
        };
        // Lifted before sending, which needs `self` mutably.
        let chat_jid = chat.jid.clone();
        let already_ours = message
            .reactions
            .get(emoji)
            .is_some_and(|senders| senders.contains(&own));
        let send = if already_ours {
            String::new()
        } else {
            emoji.to_string()
        };

        // The account connection, not the control plane: a reaction touches
        // the account, and the daemon refuses account requests anywhere
        // else. `control()` is the process-wide connection beside it, which
        // is why this sent nothing on a page until it moved here.
        if let Some(session) = &self.client {
            session.send_reaction(&chat_jid, message_id, &send);
        }
        // Optimistic, in the same form the echo will confirm: an empty send
        // removes rather than adds, which is the one spelling
        // `add_reaction` already gives a removal.
        if let Some(chat) = self.find_chat_mut(&chat_jid) {
            chat.add_reaction(message_id, send, own);
            self.invalidate_message_cache(&chat_jid, cx);
        }
        self.reaction_picker_for = None;
        cx.notify();
    }

    /// Entry point reserved for the emoji and sticker picker.
    #[expect(dead_code, reason = "the picker UI is not built yet")]
    pub fn open_emoji_picker(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        debug!("emoji and sticker picker is not implemented yet");
    }
}
