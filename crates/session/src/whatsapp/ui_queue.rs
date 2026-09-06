use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use portable_atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;
use wacore::time::Instant;

use oxidezap_core::UiEvent;

const MAX_ITEMS: usize = 256;
const MAX_DATA_ITEMS: usize = 192;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_DATA_AGE: std::time::Duration = std::time::Duration::from_secs(5);
const MIN_HISTORY_CHAT_LIMIT: usize = 1;
const MIN_HISTORY_MESSAGE_LIMIT: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Control,
    Recoverable,
    Ephemeral,
}

struct Entry {
    event: UiEvent,
    bytes: usize,
    queued_at: Instant,
}

struct State {
    control: VecDeque<Entry>,
    data: VecDeque<Entry>,
    bytes: usize,
    closed: bool,
    control_overflow: bool,
    dropped_recoverable: u64,
    dropped_ephemeral: u64,
    dropped_control: u64,
    control_faults: u64,
}

struct Inner {
    state: Mutex<State>,
    wake: Notify,
    recovery: Arc<Notify>,
    max_items: usize,
    max_data_items: usize,
    max_bytes: usize,
    max_data_age: std::time::Duration,
    history_budget: Arc<HistoryBudget>,
}

pub(super) struct HistoryBudget {
    chat_limit: AtomicUsize,
    message_limit: AtomicUsize,
    exhausted: AtomicBool,
}

impl HistoryBudget {
    pub(super) fn new() -> Self {
        Self {
            chat_limit: AtomicUsize::new(100),
            message_limit: AtomicUsize::new(50),
            exhausted: AtomicBool::new(false),
        }
    }

    pub(super) fn limits(&self) -> (usize, usize) {
        (
            self.chat_limit.load(Ordering::Relaxed),
            self.message_limit.load(Ordering::Relaxed),
        )
    }

    pub(super) fn reduce(&self) -> bool {
        loop {
            let chats = self.chat_limit.load(Ordering::Relaxed);
            let messages = self.message_limit.load(Ordering::Relaxed);
            let next_chats = (chats / 2).max(MIN_HISTORY_CHAT_LIMIT);
            let next_messages = (messages / 2).max(MIN_HISTORY_MESSAGE_LIMIT);
            if next_chats == chats && next_messages == messages {
                self.exhausted.store(true, Ordering::Relaxed);
                return false;
            }
            if self
                .chat_limit
                .compare_exchange(chats, next_chats, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                self.message_limit.store(next_messages, Ordering::Relaxed);
                return true;
            }
        }
    }

    pub(super) fn exhausted(&self) -> bool {
        self.exhausted.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Stats {
    pub(super) dropped_recoverable: u64,
    pub(super) dropped_ephemeral: u64,
    pub(super) dropped_control: u64,
    pub(super) control_faults: u64,
}

#[derive(Clone)]
pub(super) struct Sender {
    inner: Arc<Inner>,
}

pub struct Receiver {
    inner: Arc<Inner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Closed,
}

pub(super) fn channel(
    recovery: Arc<Notify>,
    history_budget: Arc<HistoryBudget>,
) -> (Sender, Receiver) {
    channel_with_limits(
        recovery,
        history_budget,
        MAX_ITEMS,
        MAX_DATA_ITEMS,
        MAX_BYTES,
        MAX_DATA_AGE,
    )
}

fn channel_with_limits(
    recovery: Arc<Notify>,
    history_budget: Arc<HistoryBudget>,
    max_items: usize,
    max_data_items: usize,
    max_bytes: usize,
    max_data_age: std::time::Duration,
) -> (Sender, Receiver) {
    let inner = Arc::new(Inner {
        state: Mutex::new(State {
            control: VecDeque::new(),
            data: VecDeque::new(),
            bytes: 0,
            closed: false,
            control_overflow: false,
            dropped_recoverable: 0,
            dropped_ephemeral: 0,
            dropped_control: 0,
            control_faults: 0,
        }),
        wake: Notify::new(),
        recovery,
        max_items: max_items.max(1),
        max_data_items: max_data_items.min(max_items.max(1)),
        max_bytes: max_bytes.max(1),
        max_data_age,
        history_budget,
    });
    (
        Sender {
            inner: Arc::clone(&inner),
        },
        Receiver { inner },
    )
}

impl Sender {
    pub(super) fn signal_control_overflow(&self) {
        let mut state = self.inner.state.lock().expect("UI event queue poisoned");
        state.control_overflow = true;
        state.control_faults += 1;
        drop(state);
        self.inner.wake.notify_one();
    }

    pub(super) fn send(&self, event: UiEvent) -> Result<(), UiEvent> {
        let class = class_of(&event);
        let bytes = estimated_bytes(&event);
        let mut recover = false;
        if class == Class::Recoverable
            && matches!(event, UiEvent::HistoryLoaded { .. })
            && bytes > self.inner.max_bytes
        {
            let should_retry = self.inner.history_budget.reduce();
            let mut state = self.inner.state.lock().expect("UI event queue poisoned");
            state.dropped_recoverable += 1;
            drop(state);
            if should_retry {
                self.inner.recovery.notify_one();
            } else if self.inner.history_budget.exhausted() {
                log::error!(
                    "history payload still exceeds UI event budget at minimum page size; stopping recovery retries"
                );
            }
            self.inner.wake.notify_one();
            return Err(event);
        }
        let result = {
            let mut state = self.inner.state.lock().expect("UI event queue poisoned");
            if state.closed {
                Err(event)
            } else {
                purge_expired(&mut state, self.inner.max_data_age, &mut recover);
                if let Some(index) = coalesce_index(&state, &event, class) {
                    let old_bytes = state.data[index].bytes;
                    if state.bytes - old_bytes + bytes <= self.inner.max_bytes {
                        let old = state.data.remove(index).expect("coalesced entry exists");
                        state.bytes -= old.bytes;
                        state.data.push_back(Entry {
                            event,
                            bytes,
                            queued_at: Instant::now(),
                        });
                        state.bytes += bytes;
                        Ok(())
                    } else {
                        if class == Class::Recoverable {
                            state.dropped_recoverable += 1;
                            recover = needs_recovery(&event);
                        } else {
                            state.dropped_ephemeral += 1;
                        }
                        Err(event)
                    }
                } else if class == Class::Control {
                    while state.control.len() + state.data.len() >= self.inner.max_items
                        || state.bytes + bytes > self.inner.max_bytes
                    {
                        if let Some(old) = state.data.pop_front() {
                            state.bytes -= old.bytes;
                            recover |= needs_recovery(&old.event);
                            state.dropped_recoverable +=
                                u64::from(class_of(&old.event) == Class::Recoverable);
                            state.dropped_ephemeral +=
                                u64::from(class_of(&old.event) == Class::Ephemeral);
                        } else {
                            state.dropped_control += 1;
                            state.control_overflow = true;
                            state.control_faults += 1;
                            break;
                        }
                    }
                    if state.control.len() + state.data.len() < self.inner.max_items
                        && state.bytes + bytes <= self.inner.max_bytes
                    {
                        state.bytes += bytes;
                        state.control.push_back(Entry {
                            event,
                            bytes,
                            queued_at: Instant::now(),
                        });
                        Ok(())
                    } else {
                        log::warn!("dropping control UI event from full mailbox");
                        Err(event)
                    }
                } else {
                    while state.data.len() >= self.inner.max_data_items
                        || state.control.len() + state.data.len() >= self.inner.max_items
                        || state.bytes + bytes > self.inner.max_bytes
                    {
                        let Some(old) = state.data.pop_front() else {
                            break;
                        };
                        state.bytes -= old.bytes;
                        let old_class = class_of(&old.event);
                        recover |= needs_recovery(&old.event);
                        state.dropped_recoverable += u64::from(old_class == Class::Recoverable);
                        state.dropped_ephemeral += u64::from(old_class == Class::Ephemeral);
                    }
                    if state.data.len() < self.inner.max_data_items
                        && state.control.len() + state.data.len() < self.inner.max_items
                        && state.bytes + bytes <= self.inner.max_bytes
                    {
                        state.bytes += bytes;
                        state.data.push_back(Entry {
                            event,
                            bytes,
                            queued_at: Instant::now(),
                        });
                        Ok(())
                    } else {
                        let old_class = class;
                        if old_class == Class::Recoverable {
                            state.dropped_recoverable += 1;
                            recover = needs_recovery(&event);
                        } else {
                            state.dropped_ephemeral += 1;
                        }
                        if old_class == Class::Control {
                            log::warn!("dropping control UI event from full mailbox");
                        }
                        Err(event)
                    }
                }
            }
        };
        if recover {
            self.inner.recovery.notify_one();
        }
        self.inner.wake.notify_one();
        result
    }

    pub(super) fn stats(&self) -> Stats {
        let state = self.inner.state.lock().expect("UI event queue poisoned");
        Stats {
            dropped_recoverable: state.dropped_recoverable,
            dropped_ephemeral: state.dropped_ephemeral,
            dropped_control: state.dropped_control,
            control_faults: state.control_faults,
        }
    }
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<UiEvent> {
        loop {
            if let Ok(event) = self.try_recv() {
                return Some(event);
            }
            let notified = self.inner.wake.notified();
            if self
                .inner
                .state
                .lock()
                .expect("UI event queue poisoned")
                .closed
            {
                return None;
            }
            notified.await;
        }
    }

    pub fn try_recv(&mut self) -> Result<UiEvent, TryRecvError> {
        let mut recover = false;
        let mut state = self.inner.state.lock().expect("UI event queue poisoned");
        purge_expired(&mut state, self.inner.max_data_age, &mut recover);
        if state.control_overflow {
            state.control_overflow = false;
            drop(state);
            if recover {
                self.inner.recovery.notify_one();
            }
            return Ok(UiEvent::Error(
                "The session event queue overflowed; reconnecting to resynchronize.".to_string(),
            ));
        }
        let entry = state.control.pop_front().or_else(|| state.data.pop_front());
        let result = match entry {
            Some(entry) => {
                state.bytes -= entry.bytes;
                Ok(entry.event)
            }
            None if state.closed => Err(TryRecvError::Closed),
            None => Err(TryRecvError::Empty),
        };
        drop(state);
        if recover {
            self.inner.recovery.notify_one();
        }
        result
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().expect("UI event queue poisoned");
        state.closed = true;
        self.inner.wake.notify_waiters();
    }
}

fn purge_expired(state: &mut State, max_age: std::time::Duration, recover: &mut bool) {
    let now = Instant::now();
    while state
        .data
        .front()
        .is_some_and(|entry| now.saturating_duration_since(entry.queued_at) > max_age)
    {
        let old = state.data.pop_front().expect("expired entry exists");
        state.bytes -= old.bytes;
        match class_of(&old.event) {
            Class::Recoverable => {
                state.dropped_recoverable += 1;
                *recover |= needs_recovery(&old.event);
            }
            Class::Ephemeral => state.dropped_ephemeral += 1,
            Class::Control => state.dropped_control += 1,
        }
    }
}

fn coalesce_index(state: &State, event: &UiEvent, class: Class) -> Option<usize> {
    if class == Class::Control {
        return None;
    }
    let key = coalesce_key(event)?;
    state
        .data
        .iter()
        .position(|entry| coalesce_key(&entry.event) == Some(key.clone()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CoalesceKey {
    ChatPresence(String, String),
    Presence(String),
}

fn coalesce_key(event: &UiEvent) -> Option<CoalesceKey> {
    match event {
        UiEvent::ChatPresence {
            chat_jid,
            sender_jid,
            ..
        } => Some(CoalesceKey::ChatPresence(
            chat_jid.clone(),
            sender_jid.clone(),
        )),
        UiEvent::PresenceUpdated { jid, .. } => Some(CoalesceKey::Presence(jid.clone())),
        _ => None,
    }
}

fn class_of(event: &UiEvent) -> Class {
    match event {
        UiEvent::MessageReceived { .. }
        | UiEvent::HistoryLoaded { .. }
        | UiEvent::ReceiptReceived { .. }
        | UiEvent::ReactionReceived { .. } => Class::Recoverable,
        UiEvent::ChatPresence { .. } | UiEvent::PresenceUpdated { .. } => Class::Ephemeral,
        _ => Class::Control,
    }
}

fn needs_recovery(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::MessageReceived { .. }
            | UiEvent::ReactionReceived { .. }
            | UiEvent::HistoryLoaded { .. }
    )
}

fn estimated_bytes(event: &UiEvent) -> usize {
    match event {
        UiEvent::HistoryLoaded { chats, .. } => {
            std::mem::size_of_val(event) + chats.iter().map(chat_bytes).sum::<usize>()
        }
        UiEvent::MessageReceived {
            chat_jid,
            message,
            sender_name,
        } => {
            std::mem::size_of_val(event)
                + string_bytes(chat_jid)
                + message_bytes(message)
                + sender_name.as_deref().map(string_bytes).unwrap_or_default()
        }
        _ => std::mem::size_of_val(event) + event_strings(event),
    }
}

fn string_bytes(value: &str) -> usize {
    value.len().saturating_add(16)
}

fn chat_bytes(chat: &oxidezap_core::Chat) -> usize {
    std::mem::size_of_val(chat)
        + string_bytes(&chat.jid)
        + string_bytes(&chat.name)
        + chat
            .last_message
            .as_deref()
            .map(string_bytes)
            .unwrap_or_default()
        + chat
            .participants
            .iter()
            .map(|(jid, name)| string_bytes(jid) + string_bytes(name))
            .sum::<usize>()
        + chat.messages.iter().map(message_bytes).sum::<usize>()
}

fn message_bytes(message: &oxidezap_core::ChatMessage) -> usize {
    let media = message.media.as_ref().map_or(0, |media| {
        media.data.len()
            + media
                .waveform
                .as_ref()
                .map(|waveform| waveform.len())
                .unwrap_or_default()
            + media
                .cache_key
                .as_deref()
                .map(string_bytes)
                .unwrap_or_default()
            + string_bytes(&media.mime_type)
            + media
                .caption
                .as_deref()
                .map(string_bytes)
                .unwrap_or_default()
            + media
                .file_name
                .as_deref()
                .map(string_bytes)
                .unwrap_or_default()
            + media.downloadable.as_ref().map_or(0, |download| {
                string_bytes(&download.direct_path)
                    + download.media_key.len()
                    + download.file_enc_sha256.len()
                    + string_bytes(&download.mime_type)
            })
    });
    std::mem::size_of_val(message)
        + string_bytes(&message.id)
        + string_bytes(&message.sender)
        + message
            .sender_name
            .as_deref()
            .map(string_bytes)
            .unwrap_or_default()
        + string_bytes(&message.content)
        + media
        + message
            .reactions
            .iter()
            .map(|(emoji, senders)| {
                string_bytes(emoji)
                    + senders
                        .iter()
                        .map(|sender| string_bytes(sender))
                        .sum::<usize>()
            })
            .sum::<usize>()
        + message.quoted.as_ref().map_or(0, |quoted| {
            string_bytes(&quoted.message_id)
                + string_bytes(&quoted.sender)
                + string_bytes(&quoted.sender_name)
                + string_bytes(&quoted.preview)
        })
}

fn event_strings(event: &UiEvent) -> usize {
    match event {
        UiEvent::QrCode { code, .. } | UiEvent::PairCode { code, .. } => string_bytes(code),
        UiEvent::Disconnected(reason) | UiEvent::LoggedOut(reason) | UiEvent::Error(reason) => {
            string_bytes(reason)
        }
        UiEvent::MessageIdAssigned {
            chat_jid,
            local_id,
            message_id,
        } => string_bytes(chat_jid) + string_bytes(local_id) + string_bytes(message_id),
        UiEvent::SendFailed {
            chat_jid,
            message_id,
            reason,
        } => string_bytes(chat_jid) + string_bytes(message_id) + string_bytes(reason),
        UiEvent::ChatPresence {
            chat_jid,
            sender_jid,
            sender_name,
            ..
        } => {
            string_bytes(chat_jid)
                + string_bytes(sender_jid)
                + sender_name.as_deref().map(string_bytes).unwrap_or_default()
        }
        UiEvent::PresenceUpdated { jid, .. } => string_bytes(jid),
        UiEvent::IncomingCall(call) => {
            string_bytes(&call.call_id)
                + string_bytes(&call.caller_name)
                + string_bytes(&call.caller_jid)
        }
        UiEvent::OutgoingCallStarted {
            call_id,
            recipient_jid,
            placeholder_id,
            ..
        } => string_bytes(call_id) + string_bytes(recipient_jid) + string_bytes(placeholder_id),
        UiEvent::OutgoingCallFailed {
            recipient_jid,
            error,
        } => string_bytes(recipient_jid) + string_bytes(error),
        UiEvent::CallAccepted(call_id)
        | UiEvent::CallEnded(call_id)
        | UiEvent::CallEndedElsewhere(call_id)
        | UiEvent::CallUnrecorded(call_id) => string_bytes(call_id),
        UiEvent::CallAnswered { call_id, .. }
        | UiEvent::CallVideoRequested { call_id, .. }
        | UiEvent::CallMuteChanged { call_id, .. }
        | UiEvent::CallVideoChanged { call_id, .. } => string_bytes(call_id),
        UiEvent::CallMediaFailed { call_id, reason }
        | UiEvent::CallVideoUnavailable { call_id, reason } => {
            string_bytes(call_id) + string_bytes(reason)
        }
        UiEvent::AccountUpdated { name, jid, lid } => {
            name.as_deref().map(string_bytes).unwrap_or_default()
                + jid.as_deref().map(string_bytes).unwrap_or_default()
                + lid.as_deref().map(string_bytes).unwrap_or_default()
        }
        UiEvent::SystemNotice {
            chat_jid,
            notice_id,
            ..
        } => string_bytes(chat_jid) + string_bytes(notice_id),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_channel(
        max_items: usize,
        max_data_items: usize,
        max_bytes: usize,
        max_data_age: std::time::Duration,
    ) -> (Sender, Receiver, Arc<Notify>) {
        let recovery = Arc::new(Notify::new());
        let (sender, receiver) = channel_with_limits(
            recovery.clone(),
            Arc::new(HistoryBudget::new()),
            max_items,
            max_data_items,
            max_bytes,
            max_data_age,
        );
        (sender, receiver, recovery)
    }

    #[test]
    fn control_survives_data_overflow() {
        let (sender, mut receiver, _) = test_channel(2, 1, 4096, MAX_DATA_AGE);
        sender
            .send(UiEvent::PresenceUpdated {
                jid: "1@s.whatsapp.net".into(),
                availability: oxidezap_core::Availability::Online,
            })
            .unwrap();
        sender.send(UiEvent::Connected).unwrap();
        sender.send(UiEvent::LoggedOut("done".into())).unwrap();
        assert!(matches!(receiver.try_recv(), Ok(UiEvent::Connected)));
        assert!(matches!(receiver.try_recv(), Ok(UiEvent::LoggedOut(_))));
        assert_eq!(sender.stats().dropped_ephemeral, 1);
    }

    #[test]
    fn control_overflow_emits_a_bounded_resynchronization_fault() {
        let (sender, mut receiver, _) = test_channel(2, 0, 4096, MAX_DATA_AGE);
        sender.send(UiEvent::Connected).unwrap();
        sender.send(UiEvent::PairSuccess).unwrap();
        assert!(sender.send(UiEvent::LoggedOut("done".into())).is_err());
        assert!(matches!(receiver.try_recv(), Ok(UiEvent::Error(_))));
        assert_eq!(sender.stats().control_faults, 1);
    }

    #[test]
    fn a_control_flood_enters_failover_instead_of_silently_staling_calls() {
        let (sender, mut receiver, _) = test_channel(64, 0, 64 * 1024, MAX_DATA_AGE);
        for index in 0..65 {
            let _ = sender.send(UiEvent::CallEnded(format!("call-{index}")));
        }
        assert!(matches!(receiver.try_recv(), Ok(UiEvent::Error(_))));
        assert_eq!(sender.stats().control_faults, 1);
    }

    #[test]
    fn recoverable_overflow_requests_reload_and_is_observable() {
        let (sender, _receiver, recovery) = test_channel(1, 1, 4096, MAX_DATA_AGE);
        let recovered = recovery.notified();
        sender
            .send(UiEvent::MessageReceived {
                chat_jid: "1@s.whatsapp.net".into(),
                message: Box::new(oxidezap_core::ChatMessage::new_incoming(
                    "m1".into(),
                    "1@s.whatsapp.net".into(),
                    "body".into(),
                )),
                sender_name: None,
            })
            .unwrap();
        sender
            .send(UiEvent::MessageReceived {
                chat_jid: "1@s.whatsapp.net".into(),
                message: Box::new(oxidezap_core::ChatMessage::new_incoming(
                    "m2".into(),
                    "1@s.whatsapp.net".into(),
                    "body".into(),
                )),
                sender_name: None,
            })
            .unwrap();
        assert_eq!(sender.stats().dropped_recoverable, 1);
        futures_lite::future::block_on(recovered);
    }

    #[test]
    fn age_bound_evicts_data_without_sleeping() {
        let (sender, mut receiver, _) = test_channel(2, 2, 4096, std::time::Duration::ZERO);
        sender
            .send(UiEvent::HistoryLoaded {
                chats: Vec::new(),
                complete: true,
                next: None,
            })
            .unwrap();
        sender.send(UiEvent::Connected).unwrap();
        assert!(matches!(receiver.try_recv(), Ok(UiEvent::Connected)));
        assert_eq!(sender.stats().dropped_recoverable, 1);
    }

    #[test]
    fn oversized_history_is_rejected_by_byte_budget() {
        let (sender, _receiver, _) = test_channel(4, 4, 128, MAX_DATA_AGE);
        sender
            .send(UiEvent::HistoryLoaded {
                chats: vec![oxidezap_core::Chat::from_store(
                    "1@s.whatsapp.net".into(),
                    "x".repeat(512),
                    1,
                )],
                complete: true,
                next: None,
            })
            .unwrap_err();
        assert_eq!(sender.stats().dropped_recoverable, 1);
    }

    #[test]
    fn an_oversized_coalescing_replacement_keeps_the_previous_state() {
        let (sender, mut receiver, _) = test_channel(4, 4, 512, MAX_DATA_AGE);
        sender
            .send(UiEvent::ChatPresence {
                chat_jid: "1@s.whatsapp.net".into(),
                sender_jid: "2@s.whatsapp.net".into(),
                sender_name: Some("small".into()),
                composing: Some(oxidezap_core::ComposingKind::Text),
            })
            .unwrap();
        assert!(
            sender
                .send(UiEvent::ChatPresence {
                    chat_jid: "1@s.whatsapp.net".into(),
                    sender_jid: "2@s.whatsapp.net".into(),
                    sender_name: Some("x".repeat(1024)),
                    composing: None,
                })
                .is_err()
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(UiEvent::ChatPresence {
                composing: Some(oxidezap_core::ComposingKind::Text),
                ..
            })
        ));
    }

    #[test]
    fn history_completeness_and_cursor_events_are_not_coalesced() {
        let (sender, mut receiver, _) = test_channel(4, 4, 4096, MAX_DATA_AGE);
        sender
            .send(UiEvent::HistoryLoaded {
                chats: Vec::new(),
                complete: true,
                next: None,
            })
            .unwrap();
        sender
            .send(UiEvent::HistoryLoaded {
                chats: Vec::new(),
                complete: false,
                next: Some("next-page".into()),
            })
            .unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(UiEvent::HistoryLoaded {
                complete: true,
                next: None,
                ..
            })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(UiEvent::HistoryLoaded {
                complete: false,
                next: Some(next),
                ..
            }) if next == "next-page"
        ));
    }

    #[tokio::test]
    async fn producer_and_consumer_progress_after_history_budget_reduction() {
        let recovery = Arc::new(Notify::new());
        let budget = Arc::new(HistoryBudget::new());
        let (sender, mut receiver) =
            channel_with_limits(recovery, budget.clone(), 4, 4, 512, MAX_DATA_AGE);
        let producer_sender = sender.clone();
        let producer = tokio::spawn(async move {
            for _ in 0..8 {
                let _ = producer_sender.send(UiEvent::HistoryLoaded {
                    chats: vec![oxidezap_core::Chat::from_store(
                        "1@s.whatsapp.net".into(),
                        "x".repeat(512),
                        1,
                    )],
                    complete: true,
                    next: None,
                });
            }
        });
        producer.await.unwrap();
        assert_eq!(budget.limits(), (1, 1));
        assert!(budget.exhausted());
        sender
            .send(UiEvent::HistoryLoaded {
                chats: Vec::new(),
                complete: false,
                next: None,
            })
            .unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(UiEvent::HistoryLoaded { .. })
        ));
    }
}
