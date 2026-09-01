//! The relay media channel a browser can open.
//!
//! The native transport dials UDP, handshakes DTLS as the client, runs an
//! SCTP association over it and opens the pre-negotiated `id=0` DataChannel
//! that carries STUN, RTP and RTCP as binary messages. Its own doc comment
//! says that this is what "the synthetic-SDP / wrtc dance reduces to, at this
//! layer" — because the thing being reduced was a WebRTC stack driven from
//! JavaScript.
//!
//! A page cannot do the reduction: there is no UDP socket to open, and no way
//! to get one. What it can do is the dance itself, which is the same stack
//! with the browser assembling it. So this module builds an
//! `RTCPeerConnection`, hands it a synthetic SDP answer describing the relay,
//! and takes the DataChannel out the other side.
//!
//! # The answer is synthetic because the relay does not speak SDP
//!
//! There is no signaling exchange with a WhatsApp relay. It is a UDP endpoint
//! the server names in the call's `<relay>` block, with ICE credentials
//! derived from that block and nothing else. So the answer is *written* here
//! from what the offer already carries — every field in it is either the
//! relay's own or a constant the stack requires:
//!
//! - `a=ice-ufrag` / `a=ice-pwd` come from
//!   [`RelayEndpointParams`](whatsapp_rust::voip::RelayEndpointParams): the
//!   relay token and the relay `<key>`, which is what the relay validates the
//!   browser's connectivity checks against.
//! - `a=ice-lite`, because the relay does not do checks of its own; the
//!   browser is the controlling agent and the relay answers.
//! - `a=setup:passive`, so the browser is the DTLS *client* — the same role
//!   the native transport takes.
//! - `a=sctp-port:5000` and a pre-negotiated `id=0` channel, which is the
//!   shape WA Web opens and the shape the native stack reproduces by hand.
//!
//! The channel is `ordered=false, maxRetransmits=0` for the reason the native
//! transport spells out at length: real-time RTP on a reliable ordered stream
//! head-of-line-blocks on every loss, and the peer hears it.
//!
//! # What is missing, and why it is one constant
//!
//! An SDP answer must carry the fingerprint of the certificate the far end
//! will present, and a browser enforces the match — that is RFC 8122, and it
//! is not negotiable from here. The native transport does not need it: it
//! sets `insecure_skip_verify` and says so, on the grounds that "the SDP
//! fingerprint is fixed and cosmetic at this layer, and media authentication
//! is hop-by-hop SRTP keyed from callKey, not from this handshake".
//!
//! *Fixed* is the operative word, and it is now a value rather than a hope:
//! `super::sdp::RELAY_DTLS_FINGERPRINT` holds the certificate the relays present,
//! observed identical across separate calls placed on WhatsApp Web that
//! reached *different* relay addresses. What varied in those captures was the
//! browser's own certificate, which is what makes it the far end's and not a
//! per-call one.
//!
//! Nothing here is waiting on anything.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use log::{debug, warn};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;
use whatsapp_rust::voip::RelayEndpointParams;
use whatsapp_rust::wacore::voip::demux::{RelayPacketKind, classify_relay_packet};
use whatsapp_rust::wacore::voip::rtp::RTP_PAYLOAD_TYPE_H264;
use whatsapp_rust::wacore::voip::transport::{
    RelayDisconnectReason, RelayTransport, RelayTransportEvent, RelayTransportFactory,
};

use super::synthetic_answer;

/// The DataChannel label WA Web opens, and the one the native stack uses.
const CHANNEL_LABEL: &str = "pre-negotiated";

/// The pre-negotiated channel's stream id. Both ends open it directly; a
/// pre-negotiated channel carries no DCEP handshake, which is why WA Web uses
/// one.
const CHANNEL_ID: u16 = 0;

/// How many inbound packets may wait for the call driver.
///
/// Generous rather than tight, and the reason is that a browser cannot make
/// the trade the native transport makes. There, a full queue parks the
/// delivering task so STUN waits for a slot while media is dropped; here the
/// delivery happens inside a JavaScript callback, which cannot wait for
/// anything without stopping the page. So the queue is sized to make the
/// choice rare, and what it does when it is full is counted rather than
/// silent — see [`Inbound::deliver`].
const INBOUND_DEPTH: usize = 256;

/// How long the peer connection has to reach an open channel.
///
/// The relay is one address and there is no candidate gathering worth the
/// name, so this is the DTLS and SCTP handshake and nothing else. Matches the
/// native transport's own connect ceiling: without one, a relay whose UDP is
/// reachable and whose DTLS wedges parks the caller forever.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

/// How much unsent media the DataChannel may be holding before this side
/// starts dropping rather than adding to it.
///
/// 64 KiB, which at a call's bitrate is a fraction of a second: past that the
/// packets in the buffer are older than anything worth sending, and the whole
/// path from the encoder down is built to drop rather than to queue. Small
/// enough to be a ceiling and not a second jitter buffer.
const OUTBOUND_CEILING: u32 = 64 * 1024;

/// The ceiling above which even audio is dropped.
///
/// Audio is exempt from `OUTBOUND_CEILING` because it can never be the cause
/// of a backlog and is what a call least affords to lose: one Opus stream is
/// 16 kbps against video's 1980, so every byte the ceiling was written for
/// belongs to the other stream. Dropping audio to make room for video is the
/// wrong trade in a call, and the ceiling was making it — a video keyframe
/// filled the buffer and the voice went with it.
///
/// Exempt is not unbounded, though. Past this the channel is not congested,
/// it is wedged, and adding to it only delays noticing.
const OUTBOUND_HARD_CEILING: u32 = 8 * OUTBOUND_CEILING;

/// The platform's answer to "how does media reach the relay".
pub struct BrowserRelay;

#[async_trait(?Send)]
impl whatsapp_rust::wacore::voip::RelayTransportProvider for BrowserRelay {
    async fn factory(
        &self,
        relay: &RelayEndpointParams,
    ) -> Result<std::sync::Arc<dyn RelayTransportFactory>> {
        if !has_peer_connection() {
            bail!("this browser has no RTCPeerConnection, so it cannot carry a call's media");
        }
        Ok(std::sync::Arc::new(BrowserRelayFactory {
            params: relay.clone(),
        }))
    }
}

/// Whether the agent this page runs in defines `RTCPeerConnection`.
///
/// Asked before a factory is handed back rather than at connect time, for the
/// same reason `oxidezap_audio::can_record` is asked before the microphone is
/// offered: a control that is drawn and then always fails is worse than one
/// that says no up front. Here the "control" is the call itself, and the
/// refusal reaches the person as the reason the call was not placed.
fn has_peer_connection() -> bool {
    let global = js_sys::global();
    js_sys::Reflect::get(
        &global,
        &wasm_bindgen::JsValue::from_str("RTCPeerConnection"),
    )
    .is_ok_and(|v| !v.is_undefined() && !v.is_null())
}

/// Dials one relay endpoint through an `RTCPeerConnection`.
struct BrowserRelayFactory {
    params: RelayEndpointParams,
}

#[async_trait(?Send)]
impl RelayTransportFactory for BrowserRelayFactory {
    async fn connect(
        &self,
    ) -> Result<(
        std::sync::Arc<dyn RelayTransport>,
        async_channel::Receiver<RelayTransportEvent>,
    )> {
        let dial = connect_peer_connection(&self.params);
        match crate::exec::with_timeout(dial, CONNECT_TIMEOUT).await {
            Some(result) => result,
            None => Err(anyhow!(
                "relay connect timed out after {CONNECT_TIMEOUT:?} (DTLS/SCTP did not complete) \
                 for {}",
                self.params.addr
            )),
        }
    }
}

/// Everything one live channel keeps alive, released together.
///
/// The closures are held because the browser calls into them: a `Closure`
/// dropped while it is still referenced is a call into freed memory, which
/// takes the tab. The same rule the recorder in `oxidezap-audio` follows, and
/// for the same reason.
struct Wiring {
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_close: Closure<dyn FnMut(web_sys::Event)>,
    _on_error: Closure<dyn FnMut(web_sys::Event)>,
    _on_state: Closure<dyn FnMut(web_sys::Event)>,
}

/// Closes a peer connection that was built but never handed to a channel.
///
/// A `RTCPeerConnection` is not released by dropping the handle: it keeps its
/// ICE agent and DTLS session until `close()` or until the tab's collector
/// reaches it. Every failure in `connect_peer_connection` after construction
/// returns before [`BrowserRelayChannel`] owns it, and against a relay that is
/// unreachable or refuses the answer that is one leaked connection per
/// attempt.
struct ConnectionGuard(Option<web_sys::RtcPeerConnection>);

impl ConnectionGuard {
    fn get(&self) -> &web_sys::RtcPeerConnection {
        self.0.as_ref().expect("held until released")
    }

    /// Setup succeeded; the channel closes it from here.
    fn release(mut self) -> web_sys::RtcPeerConnection {
        self.0.take().expect("a guard is released once")
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if let Some(connection) = self.0.take() {
            connection.close();
        }
    }
}

/// Takes the handlers off a channel that setup never handed to
/// [`BrowserRelayChannel`].
///
/// The peer connection's own guard closes the connection, and `close()` is not
/// synchronous: the channel's `close` and `error` events can still fire, and
/// by then the `Closure` locals in `connect_peer_connection` have been
/// dropped — which is a call into freed memory rather than a missed event.
/// Declared *after* those closures so it drops before them, which is the
/// whole of the ordering this exists for.
struct ChannelGuard(Option<web_sys::RtcDataChannel>);

impl ChannelGuard {
    /// Setup succeeded; the channel keeps its handlers and its wiring.
    fn release(mut self) {
        self.0.take();
    }
}

impl Drop for ChannelGuard {
    fn drop(&mut self) {
        if let Some(channel) = self.0.take() {
            detach(&channel);
        }
    }
}

/// The open media channel, as the call driver sees it.
struct BrowserRelayChannel {
    connection: web_sys::RtcPeerConnection,
    channel: web_sys::RtcDataChannel,
    /// Held for the lifetime of the channel; see [`Wiring`].
    _wiring: Wiring,
    /// So a second `disconnect` — the driver's polite close and then the drop
    /// — does not close a connection twice and log twice.
    closed: std::cell::Cell<bool>,
    /// Whether the last send found the channel over [`OUTBOUND_CEILING`], so
    /// congestion is one line and one line again when it clears.
    congested: std::cell::Cell<bool>,
    /// Counted for that second line: how much media the ceiling has dropped.
    outbound_dropped: std::cell::Cell<u32>,
    /// Where the outbound video stream is between access-unit boundaries.
    ///
    /// The ceiling may only be consulted at a boundary, so the verdict taken
    /// at one has to survive until the next. See [`Outbound`].
    au: std::cell::Cell<Outbound>,
    /// Whether anything has gone out yet; see [`RelayTransport::send`].
    sent_any: std::cell::Cell<bool>,
    /// What has come *in*, kept as two answers rather than one.
    ///
    /// The mirror of `sent_any`, and it exists for the same reason: a call
    /// whose relay opens, sends, and hears nothing back is indistinguishable
    /// — from this side and from every log — from one whose peer simply said
    /// nothing. It has already happened once in production, where the first
    /// call of a session carried not one decoded frame in twenty-one seconds
    /// while the second carried thousands.
    ///
    /// Two answers because one would have got that very call wrong. The relay
    /// answers our STUN allocate whether or not it ever bridges the peer to
    /// us — an unanswered allocate ends the call in ten seconds, and that one
    /// lived twice as long, so control traffic certainly arrived. A flag
    /// raised by any packet would have reported it as having "carried
    /// inbound", which is the opposite of what it is for.
    inbound: std::rc::Rc<InboundSeen>,
}

/// The RTP payload types seen in one direction, in the order they appeared.
///
/// A call carries two streams and they are told apart by nothing else on this
/// path: `RelayPacketKind::Rtp` says "media" and stops there, so a page
/// sending only audio and a page sending audio and video are the same three
/// words in a log. That mattered exactly once and it was expensive — every
/// stage of the outbound video path was proven to work, right up to the media
/// plane, while the peer drew nothing, and the one question left was whether
/// the packets carrying it ever reached the wire. Nothing could answer it.
///
/// The payload type is one byte and stays fixed for a stream, so the *set* is
/// the whole answer: one type outbound is one stream leaving.
#[derive(Default)]
struct PayloadTypes(RefCell<Vec<u8>>);

impl PayloadTypes {
    /// Record this packet's payload type if it is RTP and new.
    ///
    /// Cheap on the hot path by construction: RTP's payload type is the low
    /// seven bits of the second byte, and a stream contributes exactly one
    /// entry however many packets it sends.
    fn note(&self, packet: &[u8]) {
        let Some(pt) = packet.get(1).map(|b| b & 0x7f) else {
            return;
        };
        let mut seen = self.0.borrow_mut();
        if !seen.contains(&pt) {
            seen.push(pt);
        }
    }

    fn describe(&self) -> String {
        let seen = self.0.borrow();
        if seen.is_empty() {
            return "none".to_string();
        }
        seen.iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// What the relay has actually delivered, by kind that matters.
#[derive(Default)]
struct InboundSeen {
    /// Anything at all: the relay is talking to us.
    any: std::cell::Cell<bool>,
    /// RTP: the *peer* is reaching us through it. The one that decides
    /// whether a silent call is their end or the path in between.
    media: std::cell::Cell<bool>,
    /// The peer's RTP streams, by payload type. See [`PayloadTypes`].
    inbound_types: PayloadTypes,
    /// Ours, recorded on the way out for the same reason.
    outbound_types: PayloadTypes,
}

/// Where an inbound packet goes, and what happens when there is no room.
struct Inbound {
    events: async_channel::Sender<RelayTransportEvent>,
    /// Packets dropped since the last one that got through.
    ///
    /// Reported with the next delivery rather than logged, because a silent
    /// drop is indistinguishable from a peer who stopped sending — which is
    /// the ambiguity `RelayTransportEvent::InboundDropped` exists to close.
    dropped: std::cell::Cell<u32>,
    /// Raised on what the relay delivers; see `inbound` on the channel.
    seen: std::rc::Rc<InboundSeen>,
}

impl Inbound {
    fn deliver(&self, packet: Bytes) {
        // Before anything that can drop it: what this answers is whether the
        // relay ever spoke to us, which a full queue does not change.
        if !self.seen.any.replace(true) {
            debug!("voip: the relay channel received its first inbound packet");
        }
        // And separately whether the *peer* did. STUN comes back from the
        // relay itself, so it says only that the path to the relay works;
        // media is the half that says the call has two ends.
        //
        // The flag is read before the classifier runs, not after: this is the
        // callback every inbound packet arrives on, fifty times a second for
        // the length of a call, and the answer stops changing after the first
        // one. Written the other way round it classifies for a question
        // already answered.
        if matches!(classify_relay_packet(&packet), RelayPacketKind::Rtp) {
            if !self.seen.media.replace(true) {
                debug!("voip: the relay channel received the peer's first media packet");
            }
            // Which streams, not just that there were some. The flag above
            // stops changing after the first packet; this does not, because a
            // peer that adds video mid-call adds a payload type mid-call.
            self.seen.inbound_types.note(&packet);
        }
        let pending = self.dropped.get();
        if pending > 0
            && let Ok(()) = self
                .events
                .try_send(RelayTransportEvent::InboundDropped(pending))
        {
            self.dropped.set(0);
        }
        match self
            .events
            .try_send(RelayTransportEvent::PacketReceived(packet))
        {
            Ok(()) => {}
            Err(async_channel::TrySendError::Full(event)) => {
                // The driver is behind. Media is what a call can afford to
                // lose; control traffic is not. STUN keeps the relay binding
                // alive, so a stall that drops it ends the call rather than
                // degrading it — and RTCP is the peer asking for a keyframe
                // after a loss, which on this path is the *only* way they get
                // one: the web encoder is configured with no periodic IDR, so
                // a dropped PLI leaves them frozen after the queue drains
                // rather than for a second. Neither can be *waited* for from
                // inside a JS callback without stopping the page — but the
                // queue can be made room in, which is the same trade the
                // outbound ceiling makes and the opposite answer to the same
                // question.
                if let RelayTransportEvent::PacketReceived(packet) = &event
                    && matches!(
                        classify_relay_packet(packet),
                        RelayPacketKind::Stun | RelayPacketKind::Rtcp
                    )
                {
                    // `force_send` evicts the oldest, which here is the
                    // stalest media in the queue — worth less than the
                    // control packet displacing it. Counted, because a packet
                    // the driver never saw is a packet it is owed an account
                    // of either way.
                    warn!(
                        "the relay channel is behind; evicting media to deliver a control packet"
                    );
                    if self.events.force_send(event).is_ok() {
                        self.dropped.set(self.dropped.get().saturating_add(1));
                        return;
                    }
                    // Only a closed channel gets here, and it is the ending
                    // the other arm treats as nothing to report.
                    return;
                }
                // `self.dropped.get()` and not `pending`: the report above
                // may have just succeeded and zeroed the cell, and adding to
                // the pre-report value would count those losses a second
                // time — and again on every recurrence, so the number the
                // driver is told grows without any packet being lost.
                self.dropped.set(self.dropped.get().saturating_add(1));
            }
            Err(async_channel::TrySendError::Closed(_)) => {}
        }
    }
}

/// What to do with an outbound packet, and with the rest of its access unit.
///
/// `Send`/`Drop` are the verdict for the packet in hand; the *stored* value is
/// the commitment for the unit it belongs to, which is why this is a state and
/// not a boolean answer computed per packet.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outbound {
    /// Not inside a video access unit: the next video packet decides afresh.
    Between,
    /// Inside one that is being sent; finish it whatever the ceiling says.
    Send,
    /// Inside one that was refused at its first packet; refuse the remainder,
    /// since a fragment of an access unit is worth nothing to a decoder and
    /// costs the peer the bytes anyway.
    Drop,
}

impl BrowserRelayChannel {
    /// Whether this packet goes out, holding the verdict across an access unit.
    ///
    /// Control traffic is never dropped: STUN keeps the relay binding alive
    /// and RTCP carries the peer's keyframe requests, both are a handful of
    /// bytes against a frame's thousands, and losing either while the queue is
    /// deep is how a congested call becomes a dead one.
    ///
    /// Audio is decided per packet, because one packet *is* one frame there —
    /// the unit logic below would be wrong for it, since Opus sets the marker
    /// bit by its own rules rather than at frame ends.
    fn au_verdict(&self, data: &[u8]) -> Outbound {
        if matches!(
            classify_relay_packet(data),
            RelayPacketKind::Stun | RelayPacketKind::Rtcp
        ) {
            return Outbound::Send;
        }
        let buffered = self.channel.buffered_amount();
        let over_ceiling = buffered > OUTBOUND_CEILING;
        let wedged = buffered > OUTBOUND_HARD_CEILING;
        // The payload type and the marker bit share RTP's second byte: the top
        // bit is the marker, the low seven are the type. A packet too short to
        // have one is not RTP and is treated as media by the ceiling alone.
        let Some(second) = data.get(1) else {
            return if over_ceiling {
                Outbound::Drop
            } else {
                Outbound::Send
            };
        };
        // Not video, so it is the voice: exempt until the channel is wedged
        // rather than merely behind. See `OUTBOUND_HARD_CEILING`.
        if second & 0x7f != RTP_PAYLOAD_TYPE_H264 {
            return if wedged {
                Outbound::Drop
            } else {
                Outbound::Send
            };
        }
        // The marker ends an access unit, so this packet is the last of one.
        let ends_unit = second & 0x80 != 0;
        let verdict = match self.au.get() {
            // Mid-unit: the commitment already made, whatever the queue has
            // done since.
            committed @ (Outbound::Send | Outbound::Drop) => committed,
            // A boundary, and the only place the ceiling gets a vote.
            Outbound::Between => {
                if over_ceiling {
                    Outbound::Drop
                } else {
                    Outbound::Send
                }
            }
        };
        self.au.set(if ends_unit {
            Outbound::Between
        } else {
            verdict
        });
        verdict
    }
}

#[async_trait(?Send)]
impl RelayTransport for BrowserRelayChannel {
    async fn send(&self, data: Bytes) -> Result<()> {
        // `send` copies into the channel's own buffer and returns: it is not
        // backpressure, and a channel configured
        // `maxRetransmits: 0` still queues locally when SCTP cannot get the
        // bytes out. So a congested path accumulates seconds of RTP that is
        // obsolete by the time it leaves — the exact thing the rest of this
        // path drops for — until the browser's own implementation-defined
        // limit rejects a send and the transport reads as broken.
        //
        // The ceiling is ours instead, and what it drops is media. Control
        // traffic is not media: STUN keeps the binding alive and RTCP carries
        // the reports and the keyframe asks, both are a handful of bytes
        // against a frame's thousands, and losing either while the queue is
        // deep is how a congested call becomes a dead one. The same rule the
        // inbound queue holds, in the other direction.
        // Whole access units, never a piece of one. The ceiling used to be
        // consulted per packet, which is right for audio and catastrophic for
        // video: one Opus packet is one frame, but one H.264 access unit is
        // tens of fragments, and a 720p IDR is large enough to cross the
        // ceiling *while it is being written*. What reached the peer then was
        // a keyframe with a hole in it — undecodable, and so was every frame
        // that referenced it. Worse, this returns `Ok`, so the library
        // believed all of it went out: its own per-unit shedding never ran,
        // no gate closed, and nothing asked the encoder to try again. That is
        // the difference between a call that loses a picture for a moment and
        // one that never shows a picture at all.
        //
        // So the decision is taken once, at an access unit's first packet,
        // and holds for the rest of it. A unit already begun is finished
        // whatever the ceiling now says — the bytes are spent either way, and
        // spending the remainder is what makes them worth anything.
        let verdict = self.au_verdict(&data);
        if verdict == Outbound::Drop {
            self.outbound_dropped
                .set(self.outbound_dropped.get().saturating_add(1));
            // Said once per run of congestion rather than per packet: at a
            // call's frame rate this is otherwise a line per 20ms.
            if !self.congested.replace(true) {
                warn!(
                    "the relay channel is {} bytes behind; dropping outbound media until it \
                     drains",
                    self.channel.buffered_amount()
                );
            }
            return Ok(());
        }
        if self.congested.replace(false) {
            debug!(
                "the relay channel drained; {} outbound packets were dropped while it was behind",
                self.outbound_dropped.replace(0)
            );
        }
        // Recorded here, past the ceiling above: what this has to answer is
        // which streams reached the wire, and a packet the ceiling refused
        // did not. Both directions keep this, since "we sent one stream" and
        // "they sent us two" are different sentences about the same call.
        if matches!(classify_relay_packet(&data), RelayPacketKind::Rtp) {
            self.inbound.outbound_types.note(&data);
        }
        // Asked rather than inferred from the send returning. A channel that
        // is `closing` or `closed` does not throw: the specification has the
        // agent *buffer* the data, so a send onto a transport that will never
        // deliver it comes back `Ok` — which is the one answer this marker
        // may not take at face value, since its whole job is to say whether a
        // packet reached the relay. An explicit error also names the state,
        // where a `DOMException` string names the browser's wording for it.
        //
        // About *this packet* and never about the channel's history: a call
        // that talked for a minute and then lost its transport takes this
        // exit too, and a line here claiming nothing was carried would
        // contradict the release line that correctly says it was.
        if self.channel.ready_state() != web_sys::RtcDataChannelState::Open {
            return Err(anyhow!(
                "the relay channel is not open ({:?}); this packet was not sent",
                self.channel.ready_state()
            ));
        }
        // Copied out of linear memory, not viewed into it — the same rule
        // `net/web.rs` states for the socket, and for the same reason. This
        // module is built with `--shared-memory`, so a `Uint8Array` over the
        // wasm heap is a *shared* view, and `RTCDataChannel.send` refuses
        // those exactly as `WebSocket.send` does. `send_with_u8_array` hands
        // it one.
        //
        // What that cost was worth saying out loud: every relay send threw,
        // the driver treats a failed send as terminal and tears the call down
        // *publishing nothing*, so a call opened its relay and ended a moment
        // later with no error anywhere. Not one browser call has ever carried
        // a packet.
        let bytes = js_sys::Uint8Array::from(&data[..]);
        self.channel
            .send_with_array_buffer(&bytes.buffer())
            .inspect_err(|e| {
                // Said here because nowhere else will: the drive loop's
                // in-flight send arm answers `Err` with `break 'drive` and
                // discards the reason, so a transport that cannot write is
                // otherwise a call that ends for no stated reason.
                warn!("voip: the relay channel refused a packet: {}", describe(e));
            })
            .map_err(|e| anyhow!("relay channel send failed: {}", describe(&e)))?;
        // Marked *here*, and nowhere earlier: the question this answers is
        // whether the driver ever got a packet onto the transport, so a send
        // the browser rejected and a packet this side dropped for congestion
        // both have to leave it unset — either would otherwise let a channel
        // that carried nothing be released claiming it had. The first one,
        // and only the first: at a call's frame rate the rest is a line per
        // 20ms.
        if !self.sent_any.replace(true) {
            debug!("voip: the relay channel carried its first outbound packet");
        }
        Ok(())
    }

    async fn disconnect(&self) {
        if self.closed.replace(true) {
            return;
        }
        // The channel first and the connection second: closing the
        // connection alone leaves the channel's `onclose` to fire against a
        // transport nobody is reading any more.
        self.channel.close();
        self.connection.close();
    }
}

impl Drop for BrowserRelayChannel {
    fn drop(&mut self) {
        // Detached whether or not this is the close that does the work, and
        // *before* `Wiring` drops its closures a line later. `close()`
        // dispatches `onclose` asynchronously, so an ordinary teardown —
        // `disconnect` sets the flag, the driver drops the transport, the
        // browser then fires the event — would call a wasm-bindgen closure
        // that has already been freed, which traps and takes the tab. The
        // early return below is what makes this the only safe place for it:
        // it skips the closes, and it must not skip this.
        detach(&self.channel);
        // Paired with the first-send line: together they say whether the
        // driver ever used this transport. Dropped having sent nothing means
        // the call's driver returned without asking the relay for anything,
        // which is a very different fault from one that sent and then lost
        // the channel — and the two are indistinguishable from a teardown
        // that reports neither.
        // Three answers, because they fail apart. Outbound says whether the
        // driver ever reached the transport; inbound says whether the relay
        // ever answered us at all; media says whether the *peer* got through
        // it. "Sent, heard the relay, never heard the peer" is a bridge that
        // was never made — the shape the first call of a production session
        // took — and it reads identically to a working call unless the last
        // two are asked separately.
        // The payload types alongside them, because "outbound: yes" counts a
        // call's audio and says nothing about its video — which is the exact
        // gap that left a proven-working camera and a peer with no picture
        // with no next question to ask.
        debug!(
            "voip: the relay channel sent RTP stream(s) {} and received {}",
            self.inbound.outbound_types.describe(),
            self.inbound.inbound_types.describe()
        );
        debug!(
            "voip: the relay channel is being released (outbound: {}, inbound: {}, peer media: {})",
            yes_no(self.sent_any.get()),
            yes_no(self.inbound.any.get()),
            yes_no(self.inbound.media.get()),
        );
        if self.closed.replace(true) {
            return;
        }
        // A peer connection is not garbage collected while its transports are
        // live, so a channel dropped without a `disconnect` would hold a UDP
        // socket and a DTLS session open for the life of the tab.
        self.channel.close();
        self.connection.close();
    }
}

/// Take every handler off the channel, so the closures behind them are safe to
/// drop. Idempotent, and cheap enough not to be worth a flag.
fn detach(channel: &web_sys::RtcDataChannel) {
    channel.set_onmessage(None);
    channel.set_onclose(None);
    channel.set_onerror(None);
    channel.set_onopen(None);
}

/// Build the peer connection, feed it the synthetic answer, and wait for the
/// channel to open.
async fn connect_peer_connection(
    params: &RelayEndpointParams,
) -> Result<(
    std::sync::Arc<dyn RelayTransport>,
    async_channel::Receiver<RelayTransportEvent>,
)> {
    // No ICE servers: there is exactly one candidate and it is in the answer.
    // Asking the browser to gather reflexive candidates would add a STUN
    // round trip to a relay that is already the reflexive address.
    let config = web_sys::RtcConfiguration::new();
    config.set_ice_servers(&js_sys::Array::new());
    // Guarded from here: every `?` below — `createOffer`, either description,
    // a channel that never opens, the caller's timeout cancelling this future
    // — returns before `BrowserRelayChannel` owns the connection, and a peer
    // connection nothing closes keeps its ICE and DTLS state alive until the
    // tab's garbage collector gets to it. Against an unreachable relay that is
    // once per attempt.
    let connection = ConnectionGuard(Some(
        web_sys::RtcPeerConnection::new_with_configuration(&config)
            .map_err(|e| anyhow!("RTCPeerConnection: {}", describe(&e)))?,
    ));

    let init = web_sys::RtcDataChannelInit::new();
    init.set_negotiated(true);
    init.set_id(CHANNEL_ID);
    init.set_ordered(false);
    init.set_max_retransmits(0);
    let channel = connection
        .get()
        .create_data_channel_with_data_channel_dict(CHANNEL_LABEL, &init);
    channel.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);

    let (events_tx, events_rx) = async_channel::bounded(INBOUND_DEPTH);
    let seen = std::rc::Rc::new(InboundSeen::default());
    let inbound = Rc::new(Inbound {
        events: events_tx.clone(),
        dropped: std::cell::Cell::new(0),
        seen: std::rc::Rc::clone(&seen),
    });

    // Opened before the SDP exchange, so a channel that opens between the two
    // is not missed. `open` fires once; the receiver is taken by whoever
    // wakes.
    let opened = Rc::new(RefCell::new(None::<futures_channel::oneshot::Sender<()>>));
    let (open_tx, open_rx) = futures_channel::oneshot::channel();
    *opened.borrow_mut() = Some(open_tx);

    let on_message = {
        let inbound = inbound.clone();
        Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
            let data = event.data();
            let Some(buffer) = data.dyn_ref::<js_sys::ArrayBuffer>() else {
                // Binary type is set to arraybuffer, so anything else is the
                // relay sending something this stack does not carry.
                debug!("the relay channel delivered a non-binary message; ignored");
                return;
            };
            let bytes = js_sys::Uint8Array::new(buffer).to_vec();
            inbound.deliver(Bytes::from(bytes));
        }) as Box<dyn FnMut(web_sys::MessageEvent)>)
    };
    channel.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

    let on_close = {
        let events = events_tx.clone();
        let opened = opened.clone();
        Closure::wrap(Box::new(move |_: web_sys::Event| {
            // Setup's waiter, if it is still waiting. A channel that closes
            // before it ever opens — the relay refusing the answer, ICE or
            // DTLS failing outright — otherwise leaves `open_rx` parked on a
            // sender this callback holds, so an attempt that is already over
            // spends the whole connect ceiling before another relay is tried.
            // Dropped rather than sent: the receiver reads a dropped sender
            // as the teardown it is.
            opened.borrow_mut().take();
            // `force_send` rather than `try_send`: this is the one event the
            // driver cannot do without. A packet queue that is full is
            // exactly the state a dying relay leaves behind, so a `try_send`
            // here loses the disconnect precisely when it matters — and the
            // callbacks hold sender clones for the life of the channel, so
            // the receiver never sees a closure either and the call waits on
            // a relay that is already gone. Evicting the oldest packet to say
            // so is the right trade: media is what a call can afford to lose.
            let _ = events.force_send(RelayTransportEvent::Disconnected(
                RelayDisconnectReason::Closed,
            ));
        }) as Box<dyn FnMut(web_sys::Event)>)
    };
    channel.set_onclose(Some(on_close.as_ref().unchecked_ref()));

    let on_error = {
        let events = events_tx.clone();
        let opened = opened.clone();
        Closure::wrap(Box::new(move |event: web_sys::Event| {
            // Terminal before `open`, exactly as in `on_close` above.
            opened.borrow_mut().take();
            // `RTCErrorEvent` carries a reason; a bare `Event` does not, and
            // an empty string in a disconnect reason is worse than a name.
            let reason = event
                .dyn_ref::<web_sys::RtcDataChannelEvent>()
                .map(|_| "the relay channel reported an error".to_string())
                .unwrap_or_else(|| event.type_());
            // Terminal, so `force_send` for the reason `on_close` gives.
            let _ = events.force_send(RelayTransportEvent::Disconnected(
                RelayDisconnectReason::ReadError(reason),
            ));
        }) as Box<dyn FnMut(web_sys::Event)>)
    };
    channel.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    let on_state = {
        let opened = opened.clone();
        Closure::wrap(Box::new(move |_: web_sys::Event| {
            if let Some(tx) = opened.borrow_mut().take() {
                let _ = tx.send(());
            }
        }) as Box<dyn FnMut(web_sys::Event)>)
    };
    channel.set_onopen(Some(on_state.as_ref().unchecked_ref()));
    // Declared here rather than beside the channel, and that is deliberate:
    // locals drop in reverse, so a guard declared after the four closures is
    // one that detaches them before they go. See `ChannelGuard`.
    let wired = ChannelGuard(Some(channel.clone()));

    let offer = js_sys::Reflect::get(
        &wasm_bindgen_futures::JsFuture::from(connection.get().create_offer())
            .await
            .map_err(|e| anyhow!("createOffer: {}", describe(&e)))?,
        &wasm_bindgen::JsValue::from_str("sdp"),
    )
    .ok()
    .and_then(|v| v.as_string())
    .ok_or_else(|| anyhow!("the browser's offer carried no SDP"))?;

    let local = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Offer);
    local.set_sdp(&offer);
    wasm_bindgen_futures::JsFuture::from(connection.get().set_local_description(&local))
        .await
        .map_err(|e| anyhow!("setLocalDescription: {}", describe(&e)))?;

    let answer = synthetic_answer(&offer, params)?;
    let remote = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Answer);
    remote.set_sdp(&answer);
    wasm_bindgen_futures::JsFuture::from(connection.get().set_remote_description(&remote))
        .await
        .map_err(|e| anyhow!("setRemoteDescription: {}", describe(&e)))?;

    // A channel already open by the time the answer is applied resolves the
    // receiver immediately; one that never opens is bounded by the caller's
    // own timeout.
    open_rx
        .await
        .map_err(|_| anyhow!("the relay channel was torn down before it opened"))?;

    debug!("voip: the relay media channel to {} is open", params.addr);
    // Past every `?`: the channel keeps its handlers from here.
    wired.release();
    Ok((
        std::sync::Arc::new(BrowserRelayChannel {
            au: std::cell::Cell::new(Outbound::Between),
            connection: connection.release(),
            channel,
            _wiring: Wiring {
                _on_message: on_message,
                _on_close: on_close,
                _on_error: on_error,
                _on_state: on_state,
            },
            closed: std::cell::Cell::new(false),
            congested: std::cell::Cell::new(false),
            outbound_dropped: std::cell::Cell::new(0),
            sent_any: std::cell::Cell::new(false),
            inbound: seen,
        }),
        events_rx,
    ))
}

/// For a log line that answers three questions in a row.
fn yes_no(answer: bool) -> &'static str {
    if answer { "yes" } else { "no" }
}

/// A `JsValue` as something worth putting in a log line.
///
/// `{:?}` on one prints `JsValue(Object)` for the errors that matter most, so
/// the message is asked for first and the debug form is the fallback.
fn describe(value: &wasm_bindgen::JsValue) -> String {
    value
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| value.as_string())
        .unwrap_or_else(|| format!("{value:?}"))
}
