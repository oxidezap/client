#![allow(dead_code)]
use oxidezap_wire::{ApiError, DaemonResponse, ResponseEnvelope, ResponseResult};
use serde::{Deserialize, Serialize};

/// The previous derived decoder, kept as an independent compatibility reference.
#[derive(Debug, Serialize, Deserialize)]
pub struct LegacyEnvelope {
    pub id: Option<u64>,
    #[serde(flatten)]
    pub result: ResponseResult,
}
#[inline(never)]
pub fn baseline(frame: &[u8]) -> serde_json::Result<ResponseEnvelope> {
    serde_json::from_slice::<LegacyEnvelope>(frame).map(|v| ResponseEnvelope {
        id: v.id,
        result: v.result,
    })
}
#[inline(never)]
pub fn candidate(frame: &[u8]) -> serde_json::Result<ResponseEnvelope> {
    serde_json::from_slice(frame)
}
pub fn fixtures() -> Vec<ResponseEnvelope> {
    use DaemonResponse::*;
    let responses = vec![
        Ack,
        AccountSelected { local_account: 1 },
        Status(Default::default()),
        Messages {
            messages: vec![Default::default()],
            next_cursor: None,
        },
        Message(Default::default()),
        MessageSent {
            id: "synthetic".into(),
            timestamp_ms: 0,
            enqueued: false,
        },
        Chats {
            chats: vec![Default::default()],
            next_cursor: None,
        },
        Chat(Default::default()),
        MediaDownloaded {
            message_id: "synthetic".into(),
            local_path: "/synthetic/file".into(),
            size_bytes: 1,
        },
        MediaRetryRequested {
            message_id: "synthetic".into(),
        },
        Contacts {
            contacts: vec![Default::default()],
        },
        Contact(Default::default()),
        ContactCheck {
            phone: "synthetic".into(),
            is_registered: false,
            jid: None,
        },
        Groups {
            groups: vec![Default::default()],
        },
        Group(Default::default()),
        GroupInviteLink {
            link: "https://example.invalid".into(),
        },
        GroupCreated {
            jid: "synthetic@example.invalid".into(),
        },
        GroupJoined {
            jid: "synthetic@example.invalid".into(),
            pending_approval: false,
        },
        GroupJoinRequests { requests: vec![] },
        Polls {
            polls: vec![Default::default()],
        },
        Poll(Default::default()),
        Channels {
            channels: vec![Default::default()],
        },
        Channel(Default::default()),
        MediaBackfilled {
            requested: 1,
            downloaded: 1,
        },
        ChatsCleaned { removed: 1 },
        MessagesPurged { purged: 1 },
        Accounts { accounts: vec![] },
        PairCode {
            code: "synthetic".into(),
            expires_at_ms: 0,
        },
        Profile(Default::default()),
        Calls { calls: vec![] },
        HistoryCoverage {
            chat_jid: "synthetic@example.invalid".into(),
            oldest_ts: None,
            newest_ts: None,
            stored_count: 0,
        },
        Storage(Default::default()),
        Doctor(Default::default()),
    ];
    let mut fixtures: Vec<_> = responses
        .into_iter()
        .map(|v| ResponseEnvelope::success(1, v))
        .collect();
    fixtures.push(ResponseEnvelope::error(
        None,
        ApiError::invalid_argument("synthetic é\n\"\\")
            .with_details(serde_json::json!({"synthetic": [null, 1, "é"]})),
    ));
    fixtures
}

pub fn chats(count: usize, escaped: bool) -> Vec<u8> {
    let chats = (0..count)
        .map(|i| oxidezap_wire::ChatDto {
            jid: format!("synthetic-{i}@example.invalid"),
            name: format!("Synthetic conversation {i}"),
            last_message_preview: Some(if escaped {
                "Synthetic é\nwith \"quotes\" and \\ slash".repeat(3)
            } else {
                "Synthetic conversation preview with ordinary text".repeat(3)
            }),
            unread_count: (i % 12) as u32,
            last_message_ts: Some(123456789),
            ..Default::default()
        })
        .collect();
    serde_json::to_vec(&ResponseEnvelope::success(
        1,
        DaemonResponse::Chats {
            chats,
            next_cursor: None,
        },
    ))
    .unwrap()
}
