//! Pairing a companion device, the one flow that exists before there is a
//! session to speak of.
//!
//! The daemon owns the connection, so a front end that wants a phone-number
//! code asks for it here rather than calling the library itself. What comes
//! back is the code and the deadline the server gave it, which the daemon
//! republishes as a `PairCode` event and answers the request with.

use super::WhatsAppClient;
use crate::exec::Task;

/// A pairing code and the instant it stops being valid.
#[derive(Debug, Clone)]
pub struct PairCodeView {
    pub code: String,
    pub expires_at_ms: i64,
}

impl WhatsAppClient {
    /// Ask the primary device for a phone-number pairing code.
    ///
    /// The phone number is normalized by the library, which drops every
    /// non-digit and refuses one that is empty, too short or starts with a
    /// zero. A failure comes back as a string, like every other session call a
    /// front end can ask for.
    pub fn request_pair_code(&self, phone: String) -> Task<Result<PairCodeView, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let options = whatsapp_rust::pair_code::PairCodeOptions {
                phone_number: phone,
                ..Default::default()
            };
            let code = live
                .client
                .pair_with_code(options)
                .await
                .map_err(|e| e.to_string())?;
            let expires_at_ms = wacore::time::now_millis()
                + wacore::pair_code::PairCodeUtils::code_validity()
                    .as_millis()
                    .min(i64::MAX as u128) as i64;
            Ok(PairCodeView {
                code,
                expires_at_ms,
            })
        })
    }
}
