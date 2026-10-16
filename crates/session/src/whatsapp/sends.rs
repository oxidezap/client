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
    ///
    /// `announce` exists for the one flow whose answer is the id *before* the
    /// send resolves: an enqueued send tells a client what to watch for, and
    /// the id it is told has to be the one the store records and WhatsApp is
    /// handed. It is sent after the outgoing record is admitted, then the send runs on.
    pub fn send_text_wire(
        &self,
        to: String,
        text: String,
        reply_to: Option<String>,
        mentions: Vec<String>,
        announce: Option<tokio::sync::oneshot::Sender<String>>,
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
                {
                    let mut constructed: wa::Message = Default::default();
                    constructed.extended_text_message = whatsapp_rust::buffa::MessageField::some({
                        let mut constructed: wa::message::ExtendedTextMessage = Default::default();
                        constructed.text = Some(text.clone());
                        constructed.context_info =
                            whatsapp_rust::buffa::MessageField::some(context);
                        constructed
                    });
                    constructed
                }
            } else {
                {
                    let mut constructed: wa::Message = Default::default();
                    constructed.conversation = Some(text.clone());
                    constructed
                }
            };

            let client = &live.client;
            let msg_id = client.generate_message_id();
            admit_announced_outgoing(
                &live.chat_store,
                &chat,
                &msg_id,
                &message,
                &live.resolve_chat_names,
                announce,
            )?;
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
            } else {
                oxidezap_core::OutgoingMedia::for_mime(&mime)
            };
            let file = super::OutgoingFile {
                data,
                kind,
                mime_type: mime,
                file_name,
                caption,
            };

            let (shape, mut file) =
                match crate::exec::unblock(move || super::outgoing::prepare_for_send(file)).await {
                    Ok(Ok(prepared)) => prepared,
                    Ok(Err(reason)) => return Err(reason),
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
            super::record_outgoing(
                &live.chat_store,
                &chat,
                &msg_id,
                &message,
                &live.resolve_chat_names,
            )?;
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
            let message = {
                let mut constructed: wa::Message = Default::default();
                constructed.audio_message = whatsapp_rust::buffa::MessageField::some({
                    let mut constructed: wa::message::AudioMessage = Default::default();
                    constructed.url = Some(upload.url);
                    constructed.direct_path = Some(upload.direct_path);
                    constructed.media_key = Some(upload.media_key.to_vec());
                    constructed.file_sha256 = Some(upload.file_sha256.to_vec());
                    constructed.file_enc_sha256 = Some(upload.file_enc_sha256.to_vec());
                    constructed.file_length = Some(upload.file_length);
                    constructed.mimetype = Some("audio/ogg; codecs=opus".to_string());
                    constructed.seconds = Some(0);
                    constructed.ptt = Some(ptt);
                    constructed
                });
                constructed
            };
            let msg_id = client.generate_message_id();
            super::record_outgoing(
                &live.chat_store,
                &chat,
                &msg_id,
                &message,
                &live.resolve_chat_names,
            )?;
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

    /// Send a WebP sticker read from a daemon-local path.
    pub fn send_sticker_from_path(
        &self,
        to: String,
        file_path: String,
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
                    whatsapp_rust::wacore::download::MediaType::Sticker,
                    Default::default(),
                )
                .await
            {
                Ok(response) => response,
                Err(e) => return Err(e.to_string()),
            };
            let message = {
                let mut constructed: wa::Message = Default::default();
                constructed.sticker_message = whatsapp_rust::buffa::MessageField::some({
                    let mut constructed: wa::message::StickerMessage = Default::default();
                    constructed.url = Some(upload.url);
                    constructed.direct_path = Some(upload.direct_path);
                    constructed.media_key = Some(upload.media_key.to_vec());
                    constructed.file_sha256 = Some(upload.file_sha256.to_vec());
                    constructed.file_enc_sha256 = Some(upload.file_enc_sha256.to_vec());
                    constructed.file_length = Some(upload.file_length);
                    constructed.mimetype = Some("image/webp".to_string());
                    constructed
                });
                constructed
            };
            let msg_id = client.generate_message_id();
            super::record_outgoing(
                &live.chat_store,
                &chat,
                &msg_id,
                &message,
                &live.resolve_chat_names,
            )?;
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

/// The wire's enqueue acknowledgement is valid only after local admission.
fn admit_announced_outgoing(
    store: &oxidezap_chat_store::ChatStore,
    chat: &Jid,
    message_id: &str,
    message: &wa::Message,
    names: &super::chat_names::ChatNameResolveSignal,
    announce: Option<tokio::sync::oneshot::Sender<String>>,
) -> Result<(), String> {
    super::record_outgoing(store, chat, message_id, message, names)?;
    if let Some(announce) = announce {
        let _ = announce.send(message_id.to_owned());
    }
    Ok(())
}

/// The quote behind a reply id, read from the store.
pub(super) async fn quoted_for_reply(
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
        "jpg" | "jpeg" | "jfif" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
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
        assert_eq!(guess_mime("clipe.m4v"), "video/mp4");
        assert_eq!(guess_mime("clipe.mkv"), "video/x-matroska");
        assert_eq!(guess_mime("foto.HEIC"), "image/heic");
        assert_eq!(guess_mime("foto.avif"), "image/avif");
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

#[cfg(all(test, not(target_family = "wasm")))]
mod announcement_tests {
    use super::*;
    use oxidezap_chat_store::ChatStore;
    use whatsapp_rust::store::SqliteStore;

    #[tokio::test(flavor = "current_thread")]
    async fn wire_id_announcement_requires_successful_outgoing_admission() {
        for mode in ["accepted", "full", "stopped"] {
            let backend = SqliteStore::new(&format!(
                "file:wire-announcement-{mode}-{}?mode=memory&cache=shared",
                std::process::id()
            ))
            .await
            .unwrap();
            backend.create_new_device().await.unwrap();
            let store = ChatStore::new(&backend).await.unwrap();
            let chat: Jid = "559900000001@s.whatsapp.net".parse().unwrap();
            let names = super::super::chat_names::ChatNameResolveSignal::new();
            let message = {
                let mut constructed: wa::Message = Default::default();
                constructed.conversation = Some("synthetic queued send".into());
                constructed
            };
            if mode == "full" {
                for index in 0..1024 {
                    store
                        .record_outgoing(
                            &chat,
                            format!("FILL-{index}"),
                            &message,
                            whatsapp_rust::wacore::time::now_utc(),
                        )
                        .unwrap();
                }
            } else if mode == "stopped" {
                store.close().await.unwrap();
            }
            let (announce, mut announced) = tokio::sync::oneshot::channel();
            let result = admit_announced_outgoing(
                &store,
                &chat,
                "WIRE-ID",
                &message,
                &names,
                Some(announce),
            );
            if mode == "accepted" {
                result.unwrap();
                assert_eq!(announced.try_recv().unwrap(), "WIRE-ID");
                store.flush().await.unwrap();
                assert!(store.own_message(&chat, "WIRE-ID").await.unwrap().is_some());
                store.close().await.unwrap();
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.contains(if mode == "full" {
                        "writer queue full"
                    } else {
                        "writer stopped"
                    }),
                    "{error}"
                );
                assert!(
                    matches!(
                        announced.try_recv(),
                        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
                    ),
                    "{mode}: refused sends must not announce an enqueued ID"
                );
                if mode == "full" {
                    assert!(store.flush().await.is_err());
                    store.flush().await.unwrap();
                    assert!(store.own_message(&chat, "WIRE-ID").await.unwrap().is_none());
                    assert!(
                        store
                            .own_message(&chat, "FILL-1023")
                            .await
                            .unwrap()
                            .is_some()
                    );
                    store.close().await.unwrap();
                }
            }
        }
    }
}
