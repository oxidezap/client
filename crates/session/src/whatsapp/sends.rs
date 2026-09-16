//! Sends a scriptable front end asks for: text with quotes and mentions,
//! media and voice notes read from a filesystem path.
//!
//! The GUI stages payloads through the daemon's media cache because a frame
//! is capped at one megabyte; the CLI names a path the daemon can read
//! itself, so there is nothing to stage. What happens after the bytes exist
//! is the same upload-record-send as [`WhatsAppClient::send_media_message`],
//! and what comes back — rather than a bubble rename — is the id the server
//! assigned, which is what a script keys on.

use whatsapp_rust::wacore_binary::jid::Jid;
use whatsapp_rust::waproto::whatsapp as wa;

use super::WhatsAppClient;
use super::convert::quote_context;
use crate::exec::Task;

/// The server-assigned id and the send's timestamp, for a script to key on.
pub type SentReceipt = (String, i64);

impl WhatsAppClient {
    /// Send text, optionally as a reply and mentioning peers.
    ///
    /// The quote is resolved from the store rather than trusted from the
    /// caller: the wire carries a message id, and the sender, preview and
    /// kind behind it are facts this side holds. An unknown id is refused
    /// rather than sent as a hollow quote.
    pub fn send_text_wire(
        &self,
        to: String,
        text: String,
        reply_to: Option<String>,
        mentions: Vec<String>,
    ) -> Task<Result<SentReceipt, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = to.parse().map_err(|_| "not a chat address".to_string())?;
            if text.is_empty() {
                return Err("message text must not be empty".to_string());
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };

            let quoted = match reply_to {
                Some(id) => Some(quoted_for_reply(&live.chat_store, &chat, &id).await?),
                None => None,
            };
            let mentioned: Vec<String> =
                mentions.into_iter().filter(|jid| !jid.is_empty()).collect();
            let message = if quoted.is_some() || !mentioned.is_empty() {
                let mut context = quoted.as_ref().map(quote_context).unwrap_or_default();
                if !mentioned.is_empty() {
                    context.mentioned_jid = mentioned;
                }
                wa::Message {
                    extended_text_message: whatsapp_rust::buffa::MessageField::some(
                        wa::message::ExtendedTextMessage {
                            text: Some(text.clone()),
                            context_info: whatsapp_rust::buffa::MessageField::some(context),
                            ..Default::default()
                        },
                    ),
                    ..Default::default()
                }
            } else {
                wa::Message {
                    conversation: Some(text.clone()),
                    ..Default::default()
                }
            };

            let client = &live.client;
            let msg_id = client.generate_message_id();
            super::record_outgoing(&live.chat_store, &chat, &msg_id, &message);
            let options = whatsapp_rust::SendOptions::default().with_message_id(msg_id.clone());
            match client
                .send_message_with_options(chat.clone(), message, options)
                .await
            {
                Ok(_) => Ok((msg_id, whatsapp_rust::wacore::time::now_millis())),
                Err(e) => {
                    super::mark_send_failed(&live.chat_store, &chat, &msg_id);
                    Err(e.to_string())
                }
            }
        })
    }

    /// Send a file read from a daemon-local path.
    ///
    /// The kind comes from the mime — images and video inline, everything
    /// else as a document unless forced — because the wire carries a path,
    /// not a picker choice.
    pub fn send_media_from_path(
        &self,
        to: String,
        file_path: String,
        caption: Option<String>,
        mime_type: Option<String>,
        as_document: bool,
    ) -> Task<Result<SentReceipt, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = to.parse().map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let path = std::path::PathBuf::from(&file_path);
            let data = crate::exec::unblock(move || std::fs::read(&path))
                .await
                .map_err(|e| format!("the send could not start: {e}"))?
                .map_err(|e| format!("could not read {file_path}: {e}"))?;
            if data.is_empty() {
                return Err(format!("{file_path} is empty"));
            }
            let file_name = file_path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(&file_path)
                .to_string();
            let mime = mime_type.unwrap_or_else(|| guess_mime(&file_name));
            let kind = if as_document {
                oxidezap_core::OutgoingMedia::Document
            } else if mime.starts_with("image/") {
                oxidezap_core::OutgoingMedia::Image
            } else if mime.starts_with("video/") {
                oxidezap_core::OutgoingMedia::Video
            } else {
                oxidezap_core::OutgoingMedia::Document
            };
            let file = super::OutgoingFile {
                data,
                kind,
                mime_type: mime,
                file_name,
                caption,
            };

            let (shape, mut file) =
                match crate::exec::unblock(move || super::outgoing::prepare(file)).await {
                    Ok(prepared) => prepared,
                    Err(e) => return Err(format!("that file could not be prepared: {e}")),
                };
            let data = std::mem::take(&mut file.data);
            let client = &live.client;
            let (media_type, options) = file.upload_options();
            let uploaded = match client.upload(data, media_type, options).await {
                Ok(response) => super::outgoing::Uploaded::from(response),
                Err(e) => return Err(e.to_string()),
            };
            let message = super::outgoing::message(&file, shape, uploaded, None);
            let msg_id = client.generate_message_id();
            super::record_outgoing(&live.chat_store, &chat, &msg_id, &message);
            let options = whatsapp_rust::SendOptions::default().with_message_id(msg_id.clone());
            match client
                .send_message_with_options(chat.clone(), message, options)
                .await
            {
                Ok(_) => Ok((msg_id, whatsapp_rust::wacore::time::now_millis())),
                Err(e) => {
                    super::mark_send_failed(&live.chat_store, &chat, &msg_id);
                    Err(e.to_string())
                }
            }
        })
    }

    /// Send a voice note read from a daemon-local path.
    ///
    /// Duration and waveform are not derived: reading them means decoding
    /// the file, and a zeroed header sends where a refused one does not.
    pub fn send_audio_from_path(
        &self,
        to: String,
        file_path: String,
        ptt: bool,
    ) -> Task<Result<SentReceipt, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = to.parse().map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let path = std::path::PathBuf::from(&file_path);
            let data = crate::exec::unblock(move || std::fs::read(&path))
                .await
                .map_err(|e| format!("the send could not start: {e}"))?
                .map_err(|e| format!("could not read {file_path}: {e}"))?;
            if data.is_empty() {
                return Err(format!("{file_path} is empty"));
            }
            let client = &live.client;
            let upload = match client
                .upload(
                    data,
                    whatsapp_rust::wacore::download::MediaType::Audio,
                    Default::default(),
                )
                .await
            {
                Ok(response) => response,
                Err(e) => return Err(e.to_string()),
            };
            let message = wa::Message {
                audio_message: whatsapp_rust::buffa::MessageField::some(
                    wa::message::AudioMessage {
                        url: Some(upload.url),
                        direct_path: Some(upload.direct_path),
                        media_key: Some(upload.media_key.to_vec()),
                        file_sha256: Some(upload.file_sha256.to_vec()),
                        file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                        file_length: Some(upload.file_length),
                        mimetype: Some("audio/ogg; codecs=opus".to_string()),
                        seconds: Some(0),
                        ptt: Some(ptt),
                        ..Default::default()
                    },
                ),
                ..Default::default()
            };
            let msg_id = client.generate_message_id();
            super::record_outgoing(&live.chat_store, &chat, &msg_id, &message);
            let options = whatsapp_rust::SendOptions::default().with_message_id(msg_id.clone());
            match client
                .send_message_with_options(chat.clone(), message, options)
                .await
            {
                Ok(_) => Ok((msg_id, whatsapp_rust::wacore::time::now_millis())),
                Err(e) => {
                    super::mark_send_failed(&live.chat_store, &chat, &msg_id);
                    Err(e.to_string())
                }
            }
        })
    }
}

/// The quote behind a reply id, read from the store.
async fn quoted_for_reply(
    store: &oxidezap_chat_store::ChatStore,
    chat: &Jid,
    message_id: &str,
) -> Result<oxidezap_core::QuotedMessage, String> {
    let stored = store
        .message(chat, message_id)
        .await
        .map_err(|e| format!("database query failed: {e}"))?
        .ok_or_else(|| format!("no message {message_id} to reply to"))?;
    Ok(oxidezap_core::QuotedMessage {
        message_id: stored.id,
        sender: stored.sender_jid.to_string(),
        sender_name: String::new(),
        preview: stored.text.unwrap_or_default(),
        kind: quoted_kind_of(&stored.kind),
    })
}

/// What the quote bar says about a non-text original.
fn quoted_kind_of(kind: &oxidezap_chat_store::MessageKind) -> Option<oxidezap_core::QuotedKind> {
    use oxidezap_chat_store::MessageKind as Stored;
    match kind {
        Stored::Image => Some(oxidezap_core::QuotedKind::Image),
        Stored::Video | Stored::VideoNote => Some(oxidezap_core::QuotedKind::Video),
        Stored::Audio | Stored::VoiceNote => Some(oxidezap_core::QuotedKind::Audio),
        Stored::Document => Some(oxidezap_core::QuotedKind::Document),
        Stored::Sticker => Some(oxidezap_core::QuotedKind::Sticker),
        _ => None,
    }
}

/// A mime from the file extension. Unknown extensions are documents, which
/// promise nothing about what is inside — the same rule the picker keeps.
fn guess_mime(file_name: &str) -> String {
    let ext = file_name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "3gp" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg; codecs=opus",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kind decides inline versus document, so a photo with an odd
    /// extension still sends — as a document, which promises nothing.
    #[test]
    fn common_extensions_guess_their_mime() {
        assert_eq!(guess_mime("foto.jpg"), "image/jpeg");
        assert_eq!(guess_mime("foto.PNG"), "image/png");
        assert_eq!(guess_mime("clipe.mp4"), "video/mp4");
        assert_eq!(guess_mime("nota.ogg"), "audio/ogg; codecs=opus");
        assert_eq!(guess_mime("doc.pdf"), "application/pdf");
    }

    #[test]
    fn an_unknown_extension_is_a_document() {
        assert_eq!(guess_mime("arquivo.xyz"), "application/octet-stream");
        assert_eq!(guess_mime("sem-extensao"), "application/octet-stream");
    }

    #[test]
    fn quote_bars_name_the_original_kind() {
        use oxidezap_chat_store::MessageKind as Stored;
        assert_eq!(
            quoted_kind_of(&Stored::Image),
            Some(oxidezap_core::QuotedKind::Image)
        );
        assert_eq!(
            quoted_kind_of(&Stored::VoiceNote),
            Some(oxidezap_core::QuotedKind::Audio)
        );
        assert_eq!(quoted_kind_of(&Stored::Text), None);
        assert_eq!(quoted_kind_of(&Stored::Poll), None);
    }
}
