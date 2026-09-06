use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use wacore::time::Instant;

use oxidezap_core::UiEvent;

const MAX_ITEMS: usize = 256;
const MAX_DATA_ITEMS: usize = 192;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_DATA_AGE: std::time::Duration = std::time::Duration::from_secs(5);

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
    dropped_recoverable: u64,
    dropped_ephemeral: u64,
    dropped_control: u64,
}

struct Inner {
    state: Mutex<State>,
    wake: Notify,
    recovery: Arc<Notify>,
    max_items: usize,
    max_data_items: usize,
    max_bytes: usize,
    max_data_age: std::time::Duration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Stats {
    pub(super) dropped_recoverable: u64,
    pub(super) dropped_ephemeral: u64,
    pub(super) dropped_control: u64,
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

pub(super) fn channel(recovery: Arc<Notify>) -> (Sender, Receiver) {
    channel_with_limits(recovery, MAX_ITEMS, MAX_DATA_ITEMS, MAX_BYTES, MAX_DATA_AGE)
}

fn channel_with_limits(
    recovery: Arc<Notify>,
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
            dropped_recoverable: 0,
            dropped_ephemeral: 0,
            dropped_control: 0,
        }),
        wake: Notify::new(),
        recovery,
        max_items: max_items.max(1),
        max_data_items: max_data_items.min(max_items.max(1)),
        max_bytes: max_bytes.max(1),
        max_data_age,
    });
    (
        Sender {
            inner: Arc::clone(&inner),
        },
        Receiver { inner },
    )
}

impl Sender {
    pub(super) fn send(&self, event: UiEvent) -> Result<(), UiEvent> {
        let class = class_of(&event);
        let bytes = estimated_bytes(&event);
        let mut recover = false;
        let result = {
            let mut state = self.inner.state.lock().expect("UI event queue poisoned");
            if state.closed {
                Err(event)
            } else {
                purge_expired(&mut state, self.inner.max_data_age, &mut recover);
                if let Some(index) = coalesce_index(&state, &event, class) {
                    let old = state.data.remove(index).expect("coalesced entry exists");
                    state.bytes -= old.bytes;
                    state.data.push_back(Entry {
                        event,
                        bytes,
                        queued_at: Instant::now(),
                    });
                    state.bytes += bytes;
                    Ok(())
                } else if class == Class::Control {
                    while state.control.len() + state.data.len() >= self.inner.max_items
                        || state.bytes + bytes > self.inner.max_bytes
                    {
                        if let Some(old) = state.data.pop_front() {
                            state.bytes -= old.bytes;
                            recover |= class_of(&old.event) == Class::Recoverable;
                            state.dropped_recoverable +=
                                u64::from(class_of(&old.event) == Class::Recoverable);
                            state.dropped_ephemeral +=
                                u64::from(class_of(&old.event) == Class::Ephemeral);
                        } else {
                            state.dropped_control += 1;
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
                        recover |= old_class == Class::Recoverable;
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
                            recover = true;
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
                *recover = true;
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
    History,
    ChatPresence(String, String),
    Presence(String),
}

fn coalesce_key(event: &UiEvent) -> Option<CoalesceKey> {
    match event {
        UiEvent::HistoryLoaded { .. } => Some(CoalesceKey::History),
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

fn estimated_bytes(event: &UiEvent) -> usize {
    serde_json::to_vec(event)
        .map(|encoded| encoded.len().max(1))
        .unwrap_or(std::mem::size_of_val(event).max(1))
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
}
