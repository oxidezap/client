use std::collections::VecDeque;

use oxidezap_core::{AccountId, CallState, IncomingCall};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AlertKey {
    pub account: AccountId,
    pub call_id: String,
}

impl AlertKey {
    pub fn tag(&self) -> String {
        let mut hash = 14_695_981_039_346_656_037u64;
        for byte in self.call_id.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(1_099_511_628_211);
        }
        format!("oxidezap-call-{}-{hash:016x}", self.account)
    }
}

#[derive(Default)]
pub(super) struct CallAlerts {
    pub target: Option<(AlertKey, IncomingCall)>,
    retired: VecDeque<AlertKey>,
}

impl CallAlerts {
    pub fn follow(&mut self, account: Option<AccountId>, calls: &CallState) -> bool {
        let incoming = calls
            .incoming()
            .cloned()
            .or_else(|| calls.waiting().cloned().map(|waiting| waiting.into_call()));
        let target = account.zip(incoming).and_then(|(account, call)| {
            let key = AlertKey {
                account,
                call_id: call.call_id.clone(),
            };
            (!call.is_offline && !self.retired.contains(&key)).then_some((key, call))
        });
        if self.target.as_ref().map(|(key, _)| key) == target.as_ref().map(|(key, _)| key) {
            return false;
        }
        if let Some((key, _)) = self.target.take() {
            self.retire(key);
        }
        self.target = target;
        true
    }

    pub fn dismiss(&mut self, account: AccountId, call_id: &str) {
        self.retire(AlertKey {
            account,
            call_id: call_id.to_owned(),
        });
    }

    fn retire(&mut self, key: AlertKey) {
        if !self.retired.contains(&key) {
            self.retired.push_back(key);
        }
        while self.retired.len() > 128 {
            self.retired.pop_front();
        }
    }
}

#[cfg(test)]
mod call_alert_tests {
    use super::*;
    use oxidezap_core::OutgoingCall;

    fn offer(id: &str) -> IncomingCall {
        IncomingCall {
            call_id: id.into(),
            caller_name: "Example".into(),
            caller_jid: "999000@s.whatsapp.net".into(),
            is_video: true,
            is_offline: false,
            received_at: wacore::time::now_utc(),
        }
    }

    #[test]
    fn call_alert_ignores_duplicates_offline_offers_and_retired_replays() {
        let account = AccountId::new(1).unwrap();
        let mut alerts = CallAlerts::default();
        let mut calls = CallState::new();
        let mut offline = offer("offline");
        offline.is_offline = true;
        calls.set_incoming(offline);
        assert!(!alerts.follow(Some(account), &calls));
        calls.end_all();
        calls.set_incoming(offer("one"));
        assert!(alerts.follow(Some(account), &calls));
        assert!(!alerts.follow(Some(account), &calls));
        alerts.dismiss(account, "one");
        assert!(alerts.follow(Some(account), &calls));
        assert!(alerts.target.is_none());
        assert!(!alerts.follow(Some(account), &calls));
    }

    #[test]
    fn call_alert_stops_on_accept_end_disconnect_and_scopes_account_identity() {
        let first = AccountId::new(1).unwrap();
        let second = AccountId::new(2).unwrap();
        let mut alerts = CallAlerts::default();
        let mut calls = CallState::new();
        let call = offer("one");
        calls.set_incoming(call.clone());
        assert!(alerts.follow(Some(first), &calls));
        let first_tag = alerts.target.as_ref().unwrap().0.tag();
        calls.connect_accepted(&call);
        assert!(alerts.follow(Some(first), &calls));
        assert!(alerts.target.is_none());
        calls.end_all();
        calls.set_incoming(call);
        assert!(!alerts.follow(Some(first), &calls));
        assert!(alerts.follow(Some(second), &calls));
        assert_ne!(alerts.target.as_ref().unwrap().0.tag(), first_tag);
        assert!(alerts.follow(None, &calls));
        assert!(alerts.target.is_none());
    }

    #[test]
    fn call_alert_waiting_promotion_keeps_the_same_owner() {
        let account = AccountId::new(1).unwrap();
        let mut alerts = CallAlerts::default();
        let mut calls = CallState::new();
        calls.set_outgoing(OutgoingCall::new(
            "active",
            "999000@s.whatsapp.net".into(),
            "Example".into(),
            false,
        ));
        calls.connect(&"active".into());
        calls.set_incoming(offer("waiting"));
        assert!(alerts.follow(Some(account), &calls));
        calls.take();
        assert!(calls.incoming().is_some());
        assert!(!alerts.follow(Some(account), &calls));
        calls.end_all();
        assert!(alerts.follow(Some(account), &calls));
        assert!(alerts.target.is_none());
    }
}
