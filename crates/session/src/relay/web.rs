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
use whatsapp_rust::wacore::voip::rtp::{
    RTP_PAYLOAD_TYPE_H264, is_whatsapp_opus_rtp_payload, parse_whatsapp_media_frame_info,
};
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
    /// Held until the buffer reaches the soft ceiling, even if audio is sent.
    congested: std::cell::Cell<bool>,
    outbound_dropped: std::cell::Cell<u32>,
    ordinal: u64,
    traffic: RefCell<Traffic>,
    last_report: std::cell::Cell<wacore::time::Instant>,
    /// Where the outbound video stream is between access-unit boundaries.
    ///
    /// The ceiling may only be consulted at a boundary, so the verdict taken
    /// at one has to survive until the next. See [`Outbound`].
    au: std::cell::Cell<Outbound>,
    /// Whether a browser send has succeeded.
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Traffic {
    accepted_packets: u64,
    accepted_bytes: u64,
    audio_packets: u64,
    audio_bytes: u64,
    video_packets: u64,
    video_bytes: u64,
    video_markers: u64,
    video_idr_markers: u64,
    audio_drop_packets: u64,
    video_drop_packets: u64,
    other_drop_packets: u64,
    video_attempts: u64,
    // Connecting, open, closing, closed, unknown. Open failures are send throws.
    video_attempts_by_state: [u64; 5],
    video_failures_by_state: [u64; 5],
    send_errors: u64,
    buffer_high_water: u32,
    sample_counter: u8,
    /// Whether the opening video header below was stored.
    first_video_seen: bool,
    /// First twelve bytes of the first admitted video packet. See the note
    /// at the store site.
    first_video: [u8; 12],
}

#[derive(Clone, Copy)]
enum SendOutcome {
    Drop,
    NotOpen(usize),
    SendError,
    Accepted,
}

impl Traffic {
    fn note(&mut self, data: &[u8], buffered: u32, outcome: SendOutcome) -> bool {
        self.buffer_high_water = self.buffer_high_water.max(buffered);
        let rtp = matches!(classify_relay_packet(data), RelayPacketKind::Rtp);
        let pt = data.get(1).copied().unwrap_or(0) & 0x7f;
        let video = rtp && pt == RTP_PAYLOAD_TYPE_H264;
        let marker = video && data[1] & 0x80 != 0;
        if video && !matches!(outcome, SendOutcome::Drop) {
            self.video_attempts = self.video_attempts.saturating_add(1);
            let state = match outcome {
                SendOutcome::NotOpen(state) => state,
                _ => 1,
            };
            self.video_attempts_by_state[state] =
                self.video_attempts_by_state[state].saturating_add(1);
            if !matches!(outcome, SendOutcome::Accepted) {
                self.video_failures_by_state[state] =
                    self.video_failures_by_state[state].saturating_add(1);
            }
        }
        match outcome {
            SendOutcome::Drop => {
                let count = if video {
                    &mut self.video_drop_packets
                } else if rtp && is_whatsapp_opus_rtp_payload(pt) {
                    &mut self.audio_drop_packets
                } else {
                    &mut self.other_drop_packets
                };
                *count = count.saturating_add(1);
            }
            SendOutcome::Accepted => {
                self.accepted_packets = self.accepted_packets.saturating_add(1);
                self.accepted_bytes = self.accepted_bytes.saturating_add(data.len() as u64);
                if rtp && is_whatsapp_opus_rtp_payload(pt) {
                    self.audio_packets = self.audio_packets.saturating_add(1);
                    self.audio_bytes = self.audio_bytes.saturating_add(data.len() as u64);
                }
                if video {
                    self.video_packets = self.video_packets.saturating_add(1);
                    // What the peer's jitter sees first: PT, sequence,
                    // timestamp and SSRC of the opening packet, kept for the
                    // report. Gating on 97 cannot miss our stream: every
                    // video packet here was stamped by our own packetizer
                    // (`next_video_packet`), and the data channel carries no
                    // browser-built RTP to surprise it — the answer is
                    // `m=application` only. A zero SSRC here is a broken
                    // association, not a broken encoder.
                    if self.video_packets == 1 && !self.first_video_seen && data.len() >= 12 {
                        self.first_video.copy_from_slice(&data[..12]);
                        self.first_video_seen = true;
                    }
                    self.video_bytes = self.video_bytes.saturating_add(data.len() as u64);
                    if marker {
                        self.video_markers = self.video_markers.saturating_add(1);
                        if parse_whatsapp_media_frame_info(data).is_some_and(|info| info & 8 != 0) {
                            self.video_idr_markers = self.video_idr_markers.saturating_add(1);
                        }
                    }
                }
            }
            SendOutcome::SendError => self.send_errors = self.send_errors.saturating_add(1),
            SendOutcome::NotOpen(_) => {}
        }
        self.sample_counter = self.sample_counter.wrapping_add(1);
        marker || self.sample_counter == 0
    }
}

/// RTP PTs in first-seen order. Outbound means browser admission, not delivery.
struct PayloadTypes(RefCell<([u8; 128], usize)>);

impl Default for PayloadTypes {
    fn default() -> Self {
        Self(RefCell::new(([0; 128], 0)))
    }
}

impl PayloadTypes {
    /// Record one packet type if it is new. RTCP types pass unmasked (200
    /// SR, 201 RR, 205 RTPFB, 206 PSFB); recording them masked would fold
    /// every one into the RTP dynamic range.
    fn note_pt(&self, pt: u8) {
        let mut seen = self.0.borrow_mut();
        let (types, len) = &mut *seen;
        if *len < types.len() && !types[..*len].contains(&pt) {
            types[*len] = pt;
            *len += 1;
        }
    }

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
        let (types, len) = &mut *seen;
        if !types[..*len].contains(&pt) {
            types[*len] = pt;
            *len += 1;
        }
    }

    fn describe(&self) -> String {
        let seen = self.0.borrow();
        let seen = &seen.0[..seen.1];
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
    /// The peer's RTCP, by leading-subpacket type: 201 is a receiver report,
    /// 205/206 transport/payload feedback (NACK, PLI, REMB). Counted per
    /// datagram rather than per subpacket — SRTCP encrypts past byte 8, so
    /// only the first header is readable here and the rest is answered off
    /// the decrypted path. A peer that never sends any never locked our
    /// stream; one sending feedback sees it but cannot decode it. STUN
    /// answers the relay itself and is not counted here.
    rtcp_count: std::cell::Cell<u32>,
    rtcp_types: PayloadTypes,
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
        match classify_relay_packet(&packet) {
            RelayPacketKind::Rtp => {
                if !self.seen.media.replace(true) {
                    debug!("voip: the relay channel received the peer's first media packet");
                }
                // Which streams, not just that there were some. The flag above
                // stops changing after the first packet; this does not, because a
                // peer that adds video mid-call adds a payload type mid-call.
                self.seen.inbound_types.note(&packet);
            }
            RelayPacketKind::Rtcp => {
                // The leading subpacket only: past byte 8 the wire carries
                // SRTCP ciphertext, so a later header would usually stop the
                // read and occasionally invent a packet type. Which streams
                // feedback names is answered off the decrypted path instead,
                // where the engine has already run `unprotect_srtcp`.
                if let Some(pt) = first_rtcp_type(&packet) {
                    self.seen
                        .rtcp_count
                        .set(self.seen.rtcp_count.get().saturating_add(1));
                    self.seen.rtcp_types.note_pt(pt);
                }
            }
            RelayPacketKind::Stun | RelayPacketKind::Other => {}
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
    fn note_send(&self, data: &[u8], buffered: u32, outcome: SendOutcome) {
        let sample = self.traffic.borrow_mut().note(data, buffered, outcome);
        if sample && log::log_enabled!(log::Level::Debug) {
            let now = wacore::time::Instant::now();
            if now.saturating_duration_since(self.last_report.get())
                >= std::time::Duration::from_secs(5)
            {
                self.last_report.set(now);
                self.report("activity");
            }
        }
    }

    fn report(&self, phase: &str) {
        debug!("{}", self.report_line(phase));
    }

    fn report_line(&self, phase: &str) -> String {
        format!(
            "voip: relay transport={} {} datachannel_admission_only=true peer_receipt=unknown \
             marker_scope=admitted_packets_not_complete_aus \
             state_order=connecting,open,closing,closed,unknown counters={:?} \
             admitted_rtp_pts=[{}] inbound_rtp_pts=[{}] inbound_rtcp_pts=[{}] rtcp_count={} first_video={} inbound={} inbound_media={}",
            self.ordinal,
            phase,
            self.traffic.borrow(),
            self.inbound.outbound_types.describe(),
            self.inbound.inbound_types.describe(),
            self.inbound.rtcp_types.describe(),
            self.inbound.rtcp_count.get(),
            describe_first_video(
                self.traffic.borrow().first_video_seen,
                self.traffic.borrow().first_video
            ),
            yes_no(self.inbound.any.get()),
            yes_no(self.inbound.media.get()),
        )
    }

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
    fn au_verdict(&self, data: &[u8], buffered: u32) -> Outbound {
        if matches!(
            classify_relay_packet(data),
            RelayPacketKind::Stun | RelayPacketKind::Rtcp
        ) {
            return Outbound::Send;
        }
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
        let buffered = self.channel.buffered_amount();
        let verdict = self.au_verdict(&data, buffered);
        if verdict == Outbound::Drop {
            self.note_send(&data, buffered, SendOutcome::Drop);
            self.outbound_dropped
                .set(self.outbound_dropped.get().saturating_add(1));
            if !self.congested.replace(true) {
                warn!(
                    "voip: relay transport={} is {} bytes behind; dropping outbound media until it drains",
                    self.ordinal, buffered,
                );
            }
            return Ok(());
        }
        // Audio is exempt up to the hard ceiling. An accepted audio packet
        // must not announce a drain or re-arm the warning while video is shed.
        if buffered <= OUTBOUND_CEILING && self.congested.replace(false) {
            debug!(
                "voip: relay transport={} drained; {} outbound packets were dropped while it was behind",
                self.ordinal,
                self.outbound_dropped.replace(0),
            );
        }
        // Check this packet's state, not whether the channel ever opened or
        // sent anything. A channel that talked for a minute can still close;
        // naming the current state distinguishes that from a send exception.
        let state = self.channel.ready_state();
        if state != web_sys::RtcDataChannelState::Open {
            let index = match state {
                web_sys::RtcDataChannelState::Connecting => 0,
                web_sys::RtcDataChannelState::Closing => 2,
                web_sys::RtcDataChannelState::Closed => 3,
                _ => 4,
            };
            self.note_send(&data, buffered, SendOutcome::NotOpen(index));
            return Err(anyhow!(
                "the relay channel is not open ({:?}); this packet was not sent",
                state
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
                self.note_send(&data, buffered, SendOutcome::SendError);
                // The drive loop answers a failed send with `break 'drive`
                // and discards the reason. Keep it visible without debug logs.
                warn!(
                    "voip: relay transport={} refused a packet: {}",
                    self.ordinal,
                    describe(e),
                );
            })
            .map_err(|e| anyhow!("relay channel send failed: {}", describe(&e)))?;
        if matches!(classify_relay_packet(&data), RelayPacketKind::Rtp) {
            self.inbound.outbound_types.note(&data);
        }
        self.note_send(
            &data,
            buffered.max(self.channel.buffered_amount()),
            SendOutcome::Accepted,
        );
        if !self.sent_any.replace(true) {
            debug!(
                "voip: relay transport={} first outbound packet admitted to browser buffer",
                self.ordinal,
            );
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
        self.report("final");
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
            ordinal: next_transport_ordinal(),
            traffic: RefCell::new(Traffic::default()),
            last_report: std::cell::Cell::new(wacore::time::Instant::now()),
            sent_any: std::cell::Cell::new(false),
            inbound: seen,
        }),
        events_rx,
    ))
}

fn next_transport_ordinal() -> u64 {
    static NEXT: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, portable_atomic::Ordering::Relaxed)
}

/// For a log line that answers three questions in a row.
fn yes_no(answer: bool) -> &'static str {
    if answer { "yes" } else { "no" }
}

/// A `JsValue` as something worth putting in a log line.
///
/// `{:?}` on one prints `JsValue(Object)` for the errors that matter most, so
/// the message is asked for first and the debug form is the fallback.
/// The opening outbound video header for the report, or `none` before any
/// video was admitted. PT is masked to seven bits the way RTP defines it.
/// The leading RTCP subpacket's type, or nothing when the datagram does not
/// open with one. SRTCP encrypts past byte 8 (`RTCP_HEADER_LEN` in the
/// library's `e2e_srtp`, which decrypts from there), so the first header is
/// the only one readable before decryption: walking on would read ciphertext
/// as headers, usually stopping and occasionally inventing a packet type.
/// Anything past the lead — which streams feedback names, what the reports
/// say — is collected off the decrypted path instead.
fn first_rtcp_type(data: &[u8]) -> Option<u8> {
    let header = data.get(..4)?;
    if header[0] >> 6 != 2 || !(192..=223).contains(&header[1]) {
        return None;
    }
    Some(header[1])
}

fn describe_first_video(seen: bool, header: [u8; 12]) -> String {
    if !seen {
        return "none".to_string();
    }
    let seq = u16::from_be_bytes([header[2], header[3]]);
    let ts = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let ssrc = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    format!(
        "[pt={} seq={seq} ts={ts} ssrc={ssrc:#010x}]",
        header[1] & 0x7f
    )
}

fn describe(value: &wasm_bindgen::JsValue) -> String {
    value
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| value.as_string())
        .unwrap_or_else(|| format!("{value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen::prelude::wasm_bindgen;
    use wasm_bindgen_test::wasm_bindgen_test;

    std::thread_local! {
        static LOGS: RefCell<Vec<(log::Level, String)>> = const { RefCell::new(Vec::new()) };
    }

    struct TestLogger;

    impl log::Log for TestLogger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            LOGS.with_borrow_mut(|logs| logs.push((record.level(), record.args().to_string())));
        }

        fn flush(&self) {}
    }

    struct RestoreLogLevel(log::LevelFilter);

    impl Drop for RestoreLogLevel {
        fn drop(&mut self) {
            log::set_max_level(self.0);
            LOGS.with_borrow_mut(Vec::clear);
        }
    }

    fn log_count(level: log::Level, text: &str) -> usize {
        LOGS.with_borrow(|logs| {
            logs.iter()
                .filter(|(actual, message)| *actual == level && message.contains(text))
                .count()
        })
    }

    fn video(marker: bool, idr: bool) -> Bytes {
        let mut packet = [0u8; 28];
        packet[0] = 0x90;
        packet[1] = RTP_PAYLOAD_TYPE_H264 | if marker { 0x80 } else { 0 };
        packet[12..18].copy_from_slice(&[0xde, 0xbe, 0, 3, 0x30, if idr { 8 } else { 0 }]);
        Bytes::copy_from_slice(&packet)
    }

    #[wasm_bindgen_test]
    fn counters_distinguish_admission_from_drops_and_failures() {
        let mut traffic = Traffic::default();
        let packet = video(true, true);
        for outcome in [
            SendOutcome::Drop,
            SendOutcome::NotOpen(0),
            SendOutcome::NotOpen(2),
            SendOutcome::NotOpen(3),
            SendOutcome::NotOpen(4),
            SendOutcome::SendError,
        ] {
            traffic.note(&packet, 900_000, outcome);
        }
        assert_eq!(traffic.accepted_packets, 0);
        assert_eq!(traffic.accepted_bytes, 0);
        assert_eq!(traffic.video_markers, 0);
        assert_eq!(traffic.video_idr_markers, 0);
        assert_eq!(traffic.video_drop_packets, 1);
        assert_eq!(traffic.video_attempts, 5);
        assert_eq!(traffic.video_failures_by_state, [1; 5]);
        traffic.note(&video(false, true), 0, SendOutcome::Accepted);
        traffic.note(&packet, 10, SendOutcome::Accepted);
        traffic.note(&video(true, false), 0, SendOutcome::Accepted);
        assert_eq!(traffic.accepted_packets, 3);
        assert_eq!(traffic.accepted_bytes, 84);
        assert_eq!(traffic.video_packets, 3);
        assert_eq!(traffic.video_bytes, 84);
        assert_eq!(traffic.video_markers, 2);
        assert_eq!(traffic.video_idr_markers, 1);
        assert_eq!(traffic.video_attempts_by_state, [1, 4, 1, 1, 1]);
        assert_eq!(traffic.buffer_high_water, 900_000);
        let mut audio = [0; 12];
        audio[0] = 0x80;
        audio[1] = 120;
        traffic.note(&audio, 0, SendOutcome::Drop);
        traffic.note(&[], 0, SendOutcome::Drop);
        assert_eq!(traffic.audio_drop_packets, 1);
        assert_eq!(traffic.other_drop_packets, 1);
        assert_eq!(traffic.accepted_packets, 3);
        wasm_bindgen_test::console_log!("admission sequence {traffic:?}");
    }

    #[wasm_bindgen_test]
    fn sampling_and_payload_types_are_bounded() {
        let mut traffic = Traffic::default();
        for _ in 0..255 {
            assert!(!traffic.note(&[], 0, SendOutcome::Accepted));
        }
        assert!(traffic.note(&[], 0, SendOutcome::Accepted));
        assert!(traffic.note(&video(true, false), 0, SendOutcome::Drop));
        assert!(!traffic.note(&video(false, true), 0, SendOutcome::Accepted));
        let types = PayloadTypes::default();
        for _ in 0..2 {
            for pt in 0..128 {
                types.note(&[0x80, pt]);
            }
        }
        assert_eq!(types.0.borrow().1, 128);
        assert_eq!(types.0.borrow().0[127], 127);
        traffic.accepted_packets = u64::MAX;
        traffic.note(&[], 0, SendOutcome::Accepted);
        assert_eq!(traffic.accepted_packets, u64::MAX);
        wasm_bindgen_test::console_log!(
            "fixed diagnostic storage traffic={} bytes payload_types={} bytes",
            std::mem::size_of::<Traffic>(),
            std::mem::size_of::<PayloadTypes>(),
        );
    }

    fn rtcp(pt: u8) -> Bytes {
        let mut packet = [0u8; 12];
        packet[0] = 0x81;
        packet[1] = pt;
        Bytes::copy_from_slice(&packet)
    }

    #[wasm_bindgen_test]
    fn inbound_rtcp_is_counted_by_packet_type() {
        let inbound = Inbound {
            events: async_channel::unbounded::<RelayTransportEvent>().0,
            dropped: std::cell::Cell::new(0),
            seen: std::rc::Rc::new(InboundSeen::default()),
        };
        let mut audio = [0u8; 12];
        audio[0] = 0x80;
        audio[1] = 120;
        inbound.deliver(Bytes::copy_from_slice(&audio));
        inbound.deliver(rtcp(201));
        inbound.deliver(rtcp(206));
        inbound.deliver(rtcp(205));
        inbound.deliver(Bytes::copy_from_slice(&[0x00, 0x01, 0x00, 0x00]));
        assert!(inbound.seen.media.get());
        assert_eq!(inbound.seen.inbound_types.describe(), "120");
        assert_eq!(inbound.seen.rtcp_count.get(), 3);
        assert_eq!(inbound.seen.rtcp_types.describe(), "201, 206, 205");
        // A compound datagram counts once, for its leading subpacket: past
        // byte 8 the wire carries SRTCP ciphertext, not a second header.
        let mut compound = [0u8; 20];
        compound[0..8].copy_from_slice(&[0x80, 201, 0, 1, 0, 0, 0, 0]);
        compound[8..20].copy_from_slice(&[0x81, 206, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0]);
        inbound.deliver(Bytes::copy_from_slice(&compound));
        assert_eq!(inbound.seen.rtcp_count.get(), 4);
        assert_eq!(inbound.seen.rtcp_types.describe(), "201, 206, 205");
        // A leading header that overclaims its length still counts: the
        // eight clear octets are there even when the body is not.
        // Eight bytes so the classifier admits it as RTCP first: shorter
        // than that never reaches the reader at all.
        inbound.deliver(Bytes::copy_from_slice(&[0x80, 201, 0, 9, 0, 0, 0, 0]));
        let mut ragged = [0u8; 12];
        ragged[0..8].copy_from_slice(&[0x80, 201, 0, 1, 0, 0, 0, 0]);
        inbound.deliver(Bytes::copy_from_slice(&ragged));
        inbound.deliver(Bytes::copy_from_slice(&[0x00, 201, 0, 0]));
        assert_eq!(inbound.seen.rtcp_count.get(), 6);
        assert_eq!(inbound.seen.rtcp_types.describe(), "201, 206, 205");
    }

    #[wasm_bindgen_test]
    fn first_video_header_is_kept_for_the_report() {
        assert_eq!(describe_first_video(false, [0u8; 12]), "none");
        let mut traffic = Traffic::default();
        let mut first = [0u8; 28];
        first[0] = 0x90;
        first[1] = RTP_PAYLOAD_TYPE_H264 | 0x80;
        first[2..4].copy_from_slice(&[0x12, 0x34]);
        first[4..8].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        first[8..12].copy_from_slice(&[0xab, 0xcd, 0xef, 0x01]);
        traffic.note(&first, 0, SendOutcome::Accepted);
        assert_eq!(
            describe_first_video(traffic.first_video_seen, traffic.first_video),
            "[pt=97 seq=4660 ts=287454020 ssrc=0xabcdef01]"
        );
        let mut second = first;
        second[8..12].copy_from_slice(&[0x00, 0x00, 0x00, 0x02]);
        traffic.note(&second, 0, SendOutcome::Accepted);
        assert_eq!(
            describe_first_video(traffic.first_video_seen, traffic.first_video),
            "[pt=97 seq=4660 ts=287454020 ssrc=0xabcdef01]"
        );
        let short = Traffic::default();
        assert!(!short.first_video_seen);
    }

    #[wasm_bindgen(inline_js = "
        export function patchChannel(channel, state, buffered, throws) {
            Object.defineProperty(channel, 'readyState', {configurable: true, value: state});
            Object.defineProperty(channel, 'bufferedAmount', {configurable: true, value: buffered});
            channel.send = function(buffer) {
                if (!(buffer instanceof ArrayBuffer)) throw new Error('not an ArrayBuffer');
                this.testCalls = (this.testCalls || 0) + 1;
                if (throws) throw new Error('synthetic send failure');
                this.testBytes = (this.testBytes || 0) + buffer.byteLength;
            };
        }
        export function sentCalls(channel) { return channel.testCalls || 0; }
        export function sentBytes(channel) { return channel.testBytes || 0; }
    ")]
    extern "C" {
        #[wasm_bindgen(js_name = patchChannel)]
        fn patch_channel(
            channel: &web_sys::RtcDataChannel,
            state: &str,
            buffered: u32,
            throws: bool,
        );
        #[wasm_bindgen(js_name = sentCalls)]
        fn sent_calls(channel: &web_sys::RtcDataChannel) -> u32;
        #[wasm_bindgen(js_name = sentBytes)]
        fn sent_bytes(channel: &web_sys::RtcDataChannel) -> u32;
    }

    #[wasm_bindgen_test]
    async fn production_send_accounts_only_successful_browser_calls() {
        let memory = wasm_bindgen::memory().unchecked_into::<js_sys::WebAssembly::Memory>();
        let shared = memory
            .buffer()
            .is_instance_of::<js_sys::SharedArrayBuffer>();
        assert_eq!(shared, cfg!(target_feature = "atomics"));
        wasm_bindgen_test::console_log!(
            "WASM memory.buffer instanceof SharedArrayBuffer = {shared}"
        );
        log::set_logger(&TestLogger).unwrap();
        let _restore = RestoreLogLevel(log::max_level());
        log::set_max_level(log::LevelFilter::Info);
        assert!(!log::log_enabled!(log::Level::Debug));
        let connection = web_sys::RtcPeerConnection::new().unwrap();
        let channel = connection.create_data_channel("test");
        let relay = BrowserRelayChannel {
            connection,
            channel,
            _wiring: Wiring {
                _on_message: Closure::wrap(Box::new(|_: web_sys::MessageEvent| {})),
                _on_close: Closure::wrap(Box::new(|_: web_sys::Event| {})),
                _on_error: Closure::wrap(Box::new(|_: web_sys::Event| {})),
                _on_state: Closure::wrap(Box::new(|_: web_sys::Event| {})),
            },
            closed: std::cell::Cell::new(false),
            congested: std::cell::Cell::new(false),
            outbound_dropped: std::cell::Cell::new(0),
            ordinal: next_transport_ordinal(),
            traffic: RefCell::new(Traffic::default()),
            last_report: std::cell::Cell::new(wacore::time::Instant::now()),
            au: std::cell::Cell::new(Outbound::Between),
            sent_any: std::cell::Cell::new(false),
            inbound: Rc::new(InboundSeen::default()),
        };
        patch_channel(&relay.channel, "open", OUTBOUND_CEILING + 1, false);
        assert!(relay.send(video(false, true)).await.is_ok());
        patch_channel(&relay.channel, "open", 0, false);
        assert!(relay.send(video(true, true)).await.is_ok());
        assert_eq!(sent_calls(&relay.channel), 0);
        for state in ["connecting", "closing", "closed"] {
            patch_channel(&relay.channel, state, 0, false);
            assert!(relay.send(video(true, true)).await.is_err());
        }
        assert_eq!(sent_calls(&relay.channel), 0);
        patch_channel(&relay.channel, "open", 0, true);
        assert!(relay.send(video(true, true)).await.is_err());
        assert_eq!(log_count(log::Level::Warn, "synthetic send failure"), 1);
        assert_eq!(log_count(log::Level::Debug, ""), 0);
        LOGS.with_borrow(|logs| wasm_bindgen_test::console_log!("info-level records {logs:?}"));
        assert_eq!(relay.inbound.outbound_types.describe(), "none");
        assert!(!relay.sent_any.get());
        assert_eq!(relay.traffic.borrow().accepted_packets, 0);
        log::set_max_level(log::LevelFilter::Debug);
        patch_channel(&relay.channel, "open", 0, false);
        assert!(relay.send(video(false, true)).await.is_ok());
        patch_channel(&relay.channel, "open", OUTBOUND_HARD_CEILING + 1, false);
        assert!(relay.send(video(true, true)).await.is_ok());
        let mut audio = [0; 12];
        audio[0] = 0x80;
        audio[1] = 120;
        assert!(relay.send(Bytes::copy_from_slice(&audio)).await.is_ok());
        assert_eq!(relay.inbound.outbound_types.describe(), "97");
        patch_channel(&relay.channel, "open", OUTBOUND_CEILING + 1, false);
        assert!(relay.send(Bytes::copy_from_slice(&audio)).await.is_ok());
        assert_eq!(relay.inbound.outbound_types.describe(), "97, 120");
        let traffic = *relay.traffic.borrow();
        assert_eq!(sent_calls(&relay.channel), 4);
        assert_eq!(sent_bytes(&relay.channel), 68);
        assert_eq!(traffic.accepted_packets, 3);
        assert_eq!(traffic.accepted_bytes, 68);
        assert_eq!(traffic.audio_packets, 1);
        assert_eq!(traffic.audio_bytes, 12);
        assert_eq!(traffic.video_packets, 2);
        assert_eq!(traffic.video_markers, 1);
        assert_eq!(traffic.video_idr_markers, 1);
        assert_eq!(traffic.video_drop_packets, 2);
        assert_eq!(traffic.audio_drop_packets, 1);
        assert_eq!(traffic.video_attempts_by_state, [1, 3, 1, 1, 0]);
        assert_eq!(traffic.video_failures_by_state, [1, 1, 1, 1, 0]);
        assert_eq!(traffic.send_errors, 1);
        assert_eq!(traffic.buffer_high_water, OUTBOUND_HARD_CEILING + 1);
        wasm_bindgen_test::console_log!("production send calls=4 accepted_bytes=68 {traffic:?}");
        assert_eq!(
            log_count(
                log::Level::Debug,
                "first outbound packet admitted to browser buffer"
            ),
            1
        );

        patch_channel(&relay.channel, "open", 0, false);
        assert!(relay.send(Bytes::copy_from_slice(&audio)).await.is_ok());
        LOGS.with_borrow_mut(Vec::clear);
        let calls_before_congestion = sent_calls(&relay.channel);
        for _ in 0..8 {
            patch_channel(&relay.channel, "open", OUTBOUND_CEILING + 1, false);
            assert!(relay.send(video(true, false)).await.is_ok());
            assert!(relay.send(Bytes::copy_from_slice(&audio)).await.is_ok());
        }
        assert_eq!(sent_calls(&relay.channel), calls_before_congestion + 8);
        assert_eq!(log_count(log::Level::Warn, "dropping outbound media"), 1);
        assert_eq!(log_count(log::Level::Debug, "drained"), 0);
        patch_channel(&relay.channel, "open", OUTBOUND_CEILING, false);
        assert!(relay.send(Bytes::copy_from_slice(&audio)).await.is_ok());
        assert_eq!(
            log_count(log::Level::Debug, "8 outbound packets were dropped"),
            1
        );
        assert!(relay.send(Bytes::copy_from_slice(&audio)).await.is_ok());
        assert_eq!(log_count(log::Level::Debug, "drained"), 1);
        patch_channel(&relay.channel, "open", OUTBOUND_CEILING + 1, false);
        assert!(relay.send(video(true, false)).await.is_ok());
        assert_eq!(log_count(log::Level::Warn, "dropping outbound media"), 2);
        LOGS.with_borrow(|logs| wasm_bindgen_test::console_log!("congestion records {logs:?}"));
        relay.disconnect().await;
    }
}
