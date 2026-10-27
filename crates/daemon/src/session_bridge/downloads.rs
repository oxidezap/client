//! One download per account content key and cache epoch, with bounded waiters.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oxidezap_ipc::ProtocolError;
use tokio::sync::oneshot;

type Answer = Result<String, ProtocolError>;
type Key = (String, usize);

#[derive(Default)]
struct Pending {
    entries: HashMap<Key, Vec<oneshot::Sender<Answer>>>,
    waiting: usize,
}

#[derive(Clone, Default)]
pub(super) struct Downloads(Arc<Mutex<Pending>>);

pub(super) struct Flight {
    pending: Downloads,
    key: Key,
    finished: bool,
}

impl Downloads {
    pub(super) fn subscribe(
        &self,
        key: String,
        epoch: usize,
    ) -> Option<(oneshot::Receiver<Answer>, Option<Flight>)> {
        let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if pending.waiting >= super::MAX_IN_FLIGHT {
            return None;
        }
        let key = (key, epoch);
        let first = !pending.entries.contains_key(&key);
        let (send, receive) = oneshot::channel();
        pending.entries.entry(key.clone()).or_default().push(send);
        pending.waiting += 1;
        Some((
            receive,
            first.then(|| Flight {
                pending: self.clone(),
                key,
                finished: false,
            }),
        ))
    }
}

impl Flight {
    pub(super) fn finish(mut self, answer: Answer) {
        self.finished = true;
        self.publish(answer);
    }

    fn publish(&self, answer: Answer) {
        let waiters = {
            let mut pending = self.pending.0.lock().unwrap_or_else(|e| e.into_inner());
            let waiters = pending.entries.remove(&self.key).unwrap_or_default();
            pending.waiting = pending.waiting.saturating_sub(waiters.len());
            waiters
        };
        for waiter in waiters {
            let _ = waiter.send(answer.clone());
        }
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        if !self.finished {
            self.publish(Err(ProtocolError::NoSession {
                detail: "the shared download stopped before completion".into(),
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_account_hash_coalesces_and_other_accounts_and_epochs_do_not() {
        let downloads = Downloads::default();
        let (first, owner) = downloads.subscribe("a1-d-hash".into(), 0).unwrap();
        let (alias, duplicate) = downloads.subscribe("a1-d-hash".into(), 0).unwrap();
        assert!(duplicate.is_none());
        let (other, other_owner) = downloads.subscribe("a2-d-hash".into(), 0).unwrap();
        let (cleared, next_owner) = downloads.subscribe("a1-d-hash".into(), 1).unwrap();
        assert!(other_owner.is_some());
        assert!(next_owner.is_some());
        owner.unwrap().finish(Ok("a1-d-hash".into()));
        assert_eq!(first.await.unwrap().unwrap(), "a1-d-hash");
        assert_eq!(alias.await.unwrap().unwrap(), "a1-d-hash");
        drop(other_owner);
        drop(next_owner);
        assert!(matches!(
            other.await.unwrap(),
            Err(ProtocolError::NoSession { .. })
        ));
        assert!(matches!(
            cleared.await.unwrap(),
            Err(ProtocolError::NoSession { .. })
        ));
        assert_eq!(downloads.0.lock().unwrap().waiting, 0);
    }

    #[tokio::test]
    async fn waiters_are_bounded_and_cancellation_releases_the_content_key() {
        let downloads = Downloads::default();
        let (_, owner) = downloads.subscribe("a1-d-hash".into(), 0).unwrap();
        for _ in 1..super::super::MAX_IN_FLIGHT {
            assert!(
                downloads
                    .subscribe("a1-d-hash".into(), 0)
                    .unwrap()
                    .1
                    .is_none()
            );
        }
        assert!(downloads.subscribe("a1-d-hash".into(), 0).is_none());
        drop(owner);
        assert!(
            downloads
                .subscribe("a1-d-hash".into(), 0)
                .unwrap()
                .1
                .is_some()
        );
    }
}
