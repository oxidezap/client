//! Event order, kept per subject and nowhere else.
//!
//! The stream reaches this side already ordered. Handling each event on its
//! own task threw that away; handling them all on one gave the order back at
//! the price of a pairing code queueing behind a history sync. So an event is
//! keyed by what it is about and a key always reaches the same lane, which is
//! what [`EventLanes`] is.

use std::sync::Arc;

use tokio::sync::mpsc;
use whatsapp_rust::client::Client;
use whatsapp_rust::wacore::types::events::Event;
use whatsapp_rust::wacore_binary::jid::Jid;

use super::normalize_chat_jid;

/// How many lanes events about a subject are spread across.
///
/// Fixed rather than one per subject: a lane is a task and a queue, and a
/// lane per chat is one of each for every conversation an account has ever
/// had. Subjects share a lane by hash, so two busy chats can queue behind
/// each other — which costs latency, where the alternative costs order.
const EVENT_LANES: usize = 8;

/// Events about one subject, handled in the order they arrived.
///
/// The event stream reaches this side already ordered, and handling each
/// event on its own task threw that away: a `CallEndedElsewhere` could run
/// before the `IncomingCall` it ends, leaving a card ringing for a call that
/// is over, and a receipt could run before the message it answers. Ordering
/// only matters between events about the same thing, so events are keyed by
/// their call or their chat and a key always reaches the same lane. Anything
/// naming neither is session-wide and gets a lane of its own, so a pairing
/// code never waits behind a conversation.
pub(super) struct EventLanes {
    lanes: Vec<mpsc::UnboundedSender<Arc<Event>>>,
}

impl EventLanes {
    pub(super) fn new<F, Fut>(handle: F, stopping: tokio::sync::watch::Receiver<()>) -> Self
    where
        F: Fn(Arc<Event>) -> Fut + Clone + crate::exec::MaybeSend + 'static,
        Fut: Future<Output = ()> + crate::exec::MaybeSend + 'static,
    {
        let lanes = (0..=EVENT_LANES)
            .map(|_| {
                let (tx, mut rx) = mpsc::unbounded_channel::<Arc<Event>>();
                let handle = handle.clone();
                let mut stopping = stopping.clone();
                crate::exec::spawn_owned(async move {
                    loop {
                        let event = tokio::select! {
                            event = rx.recv() => match event {
                                Some(event) => event,
                                None => return,
                            },
                            // Dropping the senders is not enough on its own:
                            // a receiver hands out everything already queued
                            // before it answers `None`, so a lane would work
                            // through a backlog belonging to an account this
                            // session no longer speaks for. On a page that
                            // matters twice over, where nothing cancels a
                            // spawned task and the backlog keeps the old
                            // client and its store alive.
                            _ = stopping.changed() => return,
                        };
                        handle(event).await;
                    }
                });
                tx
            })
            .collect();
        Self { lanes }
    }

    pub(super) async fn dispatch(&mut self, client: &Client, event: Arc<Event>) {
        // A batch may span chats, and a lane is one chat's order: sent whole
        // on the first message's lane, a receipt for a later chat in it runs
        // on that chat's own lane and can overtake the message it answers.
        // Split, each chat's messages keep their order against everything
        // else about that chat, and two chats in one batch were never ordered
        // against each other.
        for event in split_by_subject(&event) {
            let lane = lane_for(client, &event).await;
            let _ = self.lanes[lane].send(event);
        }
    }
}

/// One event per subject it is about, which for everything but a batch of
/// messages is the event itself.
pub(super) fn split_by_subject(event: &Arc<Event>) -> Vec<Arc<Event>> {
    let Event::Messages(batch) = &**event else {
        return vec![Arc::clone(event)];
    };
    let mut chats: Vec<String> = Vec::new();
    for inbound in batch.iter() {
        let chat = inbound.info.source.chat.to_string();
        if !chats.contains(&chat) {
            chats.push(chat);
        }
    }
    if chats.len() <= 1 {
        return vec![Arc::clone(event)];
    }
    chats
        .into_iter()
        .map(|chat| {
            let messages: Arc<[whatsapp_rust::wacore::types::events::InboundMessage]> = batch
                .iter()
                .filter(|inbound| inbound.info.source.chat.to_string() == chat)
                .cloned()
                .collect();
            // The origin travels with every part: it says how the batch was
            // delivered, which is as true of one chat's share of it as of the
            // whole, and it is what decides whether media is fetched eagerly.
            Arc::new(Event::Messages(
                whatsapp_rust::wacore::types::events::MessageBatch::builder()
                    .messages(messages)
                    .origin(batch.origin)
                    .build(),
            ))
        })
        .collect()
}

/// Which lane an event is handled on. Same subject, same lane.
///
/// The address is canonicalized first, which is the whole reason this is not
/// a pure function of the event. The wire names one peer two ways and the two
/// hash to different lanes, so a message under a phone number and its receipt
/// under the LID were handled concurrently: the receipt could overtake the
/// message it answers -- most easily while that message waits on an eager
/// media fetch -- and a front end drops a receipt naming a row it has not
/// been given yet. The library keeps the pairing in memory in front of its
/// store, so this is a map read for a peer already seen and not asked at all
/// of a LID.
async fn lane_for(client: &Client, event: &Event) -> usize {
    let subject = match event_subject(event) {
        Some(Subject::Call(id)) => Some(id),
        Some(Subject::Chat(jid)) => Some(normalize_chat_jid(client, &jid.to_string()).await),
        None => None,
    };
    lane_of(subject.as_deref())
}

/// The lane a subject hashes to, or the session-wide one for no subject.
pub(super) fn lane_of(subject: Option<&str>) -> usize {
    match subject {
        Some(subject) => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&subject, &mut hasher);
            (std::hash::Hasher::finish(&hasher) as usize) % EVENT_LANES
        }
        None => EVENT_LANES,
    }
}

/// What an event is about, for the lane that keeps its order.
///
/// A call or a chat. `None` is a session-wide event, which is about the
/// account rather than about anything in it.
pub(super) enum Subject {
    Call(String),
    Chat(Jid),
}

pub(super) fn event_subject(event: &Event) -> Option<Subject> {
    match event {
        Event::IncomingCall(call) => Some(Subject::Call(call.action.call_id().to_string())),
        Event::MissedCall(missed) => Some(Subject::Call(missed.call_id.clone())),
        Event::CallEndedElsewhere(ended) => Some(Subject::Call(ended.call_id.clone())),
        Event::Messages(batch) => batch
            .iter()
            .next()
            .map(|inbound| Subject::Chat(inbound.info.source.chat.clone())),
        Event::Receipt(receipt) => Some(Subject::Chat(receipt.source.chat.clone())),
        Event::ChatPresence(update) => Some(Subject::Chat(update.source.chat.clone())),
        // Both name somebody, and both handlers go to the store for a name or
        // an identity. On the session-wide lane a burst of either delayed
        // `Connected`, `PairingQrCode` and `LoggedOut` behind it, which are
        // the events a window is waiting on to draw anything at all.
        Event::Presence(update) => Some(Subject::Chat(update.from.clone())),
        Event::GroupUpdate(update) => Some(Subject::Chat(update.group_jid.clone())),
        _ => None,
    }
}

impl Subject {
    /// The address, before canonicalization. For tests and for logging: a
    /// lane is chosen from the canonical form, which needs the client.
    #[cfg(test)]
    pub(super) fn as_written(&self) -> String {
        match self {
            Self::Call(id) => id.clone(),
            Self::Chat(jid) => jid.to_string(),
        }
    }
}
