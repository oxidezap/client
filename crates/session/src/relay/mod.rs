//! How a call's media reaches the relay.
//!
//! The same arrangement as [`crate::net`] and [`crate::exec`]: one name the
//! session calls, two implementations behind it, and no `cfg` in the
//! session's own logic above.
//!
//! On a desktop there is nothing to choose. The library's own
//! `voip-relay-native` opens a UDP socket per call and runs DTLS, SCTP and a
//! pre-negotiated DataChannel over it, and installing anything here would
//! only replace a dialler that already works.
//!
//! A page has no UDP socket and cannot have one. What it has is the same
//! stack with a different door on it: an `RTCPeerConnection` is DTLS, SCTP
//! and a DataChannel, assembled by the browser rather than by hand. So the
//! browser half is not a second protocol — it is the one the native
//! transport's own comment calls "the synthetic-SDP / wrtc dance", performed
//! by the thing the dance was written for.

#[cfg(target_family = "wasm")]
pub mod web;

/// Give the client this platform's way onto the media wire.
///
/// Called once, where the client is built, so nothing else in the session
/// names a platform. On a desktop it is deliberately nothing: the library's
/// native dialler is the default and is what should stay in place.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn install(_client: &std::sync::Arc<whatsapp_rust::Client>) {}

/// See the desktop half. Here the provider is the whole reason a call can be
/// placed at all.
#[cfg(target_family = "wasm")]
pub(crate) fn install(client: &std::sync::Arc<whatsapp_rust::Client>) {
    client.set_relay_transport_provider(std::sync::Arc::new(web::BrowserRelay));
}

/// The synthetic SDP answer, and what a build still has to know to write one.
///
/// Portable rather than shut inside the browser half, because it is string
/// arithmetic over a `RelayEndpointParams` and nothing else -- so it is
/// testable where `cargo test` already runs, which a `wasm32`-only module is
/// not. The same reason `net::abort_requested` lives where it does.
#[cfg(any(target_family = "wasm", test))]
mod sdp {
    use whatsapp_rust::anyhow::{Result, bail};
    use whatsapp_rust::voip::RelayEndpointParams;

    /// The SHA-256 fingerprint of the certificate a WhatsApp relay presents,
    /// formatted as SDP expects it (`AA:BB:...`, 32 upper-case hex pairs).
    ///
    /// Empty, deliberately, and the one thing this module is waiting on. See the
    /// module docs: the value is a constant in WhatsApp Web's bundle rather than
    /// anything derivable from the `<relay>` block, and guessing it produces a
    /// DTLS failure that reads as a network fault.
    ///
    /// A build that has it fills this in and nothing else changes.
    pub(crate) const RELAY_DTLS_FINGERPRINT: &str = "";

    /// The SCTP-over-DTLS WebRTC port. Fixed by the stack, not by the relay.
    const SCTP_PORT: u16 = 5000;

    /// The `a=mid` the offer used, which the answer has to repeat.
    ///
    /// Read rather than assumed: it is `0` in every browser this runs in today,
    /// and an answer whose mid does not match the offer's is rejected outright —
    /// which is a failure with no useful message on it.
    fn offer_mid(offer: &str) -> &str {
        offer
            .lines()
            .find_map(|line| line.strip_prefix("a=mid:"))
            .map(str::trim)
            .unwrap_or("0")
    }

    /// Write the answer the relay would have sent, if a relay spoke SDP.
    ///
    /// Every field is either the relay's own, taken from the call's `<relay>`
    /// block, or a constant the stack fixes. Nothing here is negotiated, because
    /// there is nobody on the other end to negotiate with.
    pub(crate) fn synthetic_answer(offer: &str, params: &RelayEndpointParams) -> Result<String> {
        let ip = match params.addr {
            std::net::SocketAddr::V4(v4) => v4.ip().to_string(),
            // The relay walk picks an IPv4 endpoint, so this is a call that
            // should not have reached here; said rather than silently written
            // into an `IN IP4` line that would then not resolve.
            std::net::SocketAddr::V6(_) => {
                bail!("the relay endpoint is IPv6, which the synthetic answer does not describe")
            }
        };
        let port = params.addr.port();
        let mid = offer_mid(offer);
        Ok(format!(
            "v=0\r\n\
             o=- 0 0 IN IP4 {ip}\r\n\
             s=-\r\n\
             t=0 0\r\n\
             a=group:BUNDLE {mid}\r\n\
             m=application {port} UDP/DTLS/SCTP webrtc-datachannel\r\n\
             c=IN IP4 {ip}\r\n\
             a=mid:{mid}\r\n\
             a=ice-lite\r\n\
             a=ice-ufrag:{ufrag}\r\n\
             a=ice-pwd:{pwd}\r\n\
             a=fingerprint:sha-256 {fingerprint}\r\n\
             a=setup:passive\r\n\
             a=sctp-port:{SCTP_PORT}\r\n\
             a=max-message-size:262144\r\n\
             a=candidate:1 1 udp 2130706431 {ip} {port} typ host\r\n\
             a=end-of-candidates\r\n",
            ufrag = params.ice_ufrag,
            pwd = params.ice_pwd,
            fingerprint = RELAY_DTLS_FINGERPRINT,
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use whatsapp_rust::voip::RelayEndpointParams;

        fn params() -> RelayEndpointParams {
            RelayEndpointParams {
                addr: "203.0.113.7:3480".parse().expect("addr"),
                ice_ufrag: "UFRAG".to_string(),
                ice_pwd: "PWD".to_string(),
            }
        }

        /// The mid travels from the offer, because an answer that renames it is
        /// rejected with nothing useful on the error.
        #[test]
        fn the_answer_repeats_the_offer_mid() {
            let answer = synthetic_answer("v=0\r\na=mid:7\r\n", &params()).expect("answer");
            assert!(answer.contains("a=mid:7\r\n"), "{answer}");
            assert!(answer.contains("a=group:BUNDLE 7\r\n"), "{answer}");
        }

        /// An offer with no mid at all still produces a usable answer rather than
        /// an empty attribute: every browser writes one, and a missing one is not
        /// worth failing a call over.
        #[test]
        fn a_missing_mid_falls_back_rather_than_failing() {
            let answer = synthetic_answer("v=0\r\n", &params()).expect("answer");
            assert!(answer.contains("a=mid:0\r\n"), "{answer}");
        }

        /// The relay's own address and credentials are what the answer describes.
        #[test]
        fn the_answer_describes_the_relay() {
            let answer = synthetic_answer("a=mid:0", &params()).expect("answer");
            assert!(
                answer.contains("m=application 3480 UDP/DTLS/SCTP webrtc-datachannel\r\n"),
                "{answer}"
            );
            assert!(answer.contains("c=IN IP4 203.0.113.7\r\n"), "{answer}");
            assert!(answer.contains("a=ice-ufrag:UFRAG\r\n"), "{answer}");
            assert!(answer.contains("a=ice-pwd:PWD\r\n"), "{answer}");
            assert!(
                answer.contains("a=candidate:1 1 udp 2130706431 203.0.113.7 3480 typ host\r\n"),
                "{answer}"
            );
        }

        /// `passive` and not `active`: the browser is the DTLS client, which is
        /// the role the native transport takes. Getting it the other way round is
        /// a handshake where both ends wait.
        #[test]
        fn the_browser_is_the_dtls_client() {
            let answer = synthetic_answer("a=mid:0", &params()).expect("answer");
            assert!(answer.contains("a=setup:passive\r\n"), "{answer}");
        }

        /// An IPv6 relay is refused rather than written into an `IN IP4` line
        /// that would not resolve.
        #[test]
        fn an_ipv6_relay_is_refused_by_name() {
            let mut p = params();
            p.addr = "[2001:db8::1]:3480".parse().expect("addr");
            let error = synthetic_answer("a=mid:0", &p).expect_err("IPv6 must be refused");
            assert!(error.to_string().contains("IPv6"), "{error}");
        }
    }
}

#[cfg(target_family = "wasm")]
pub(crate) use sdp::{RELAY_DTLS_FINGERPRINT, synthetic_answer};
