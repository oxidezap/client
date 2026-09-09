//! H.264 for the wire.
//!
//! WhatsApp's video plane carries Constrained Baseline (`avc1.42E01F`) with
//! repeated parameter sets, up to 1280x720 at 20 fps — so the encoder is
//! configured to produce exactly that and nothing a phone would refuse: no
//! B-frames (baseline has none), a real-time rate control that would rather
//! drop a frame than overshoot, and a keyframe every few seconds so a peer
//! that joins the stream late or loses a reference recovers without asking.

use anyhow::{Context as _, Result};

use crate::{KEYFRAME_SECONDS, MIN_REQUESTED_KEYFRAME_SECONDS};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Level, Profile,
    RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::YUVSource;

use crate::VideoQuality;

use crate::EncodedFrame;

/// The encoder, plus the one piece of state it does not keep itself: whether
/// the next frame has been asked to be a keyframe.
pub struct H264Encoder {
    encoder: Encoder,
    force_keyframe: bool,
    /// Frames since the last forced IDR, and how many must pass before
    /// another request is honoured.
    ///
    /// The periodic intra period above hides the cost here — a forced IDR is
    /// only ever *extra* — but the requests arrive from four uncoordinated
    /// places, all of them caused by congestion, so answering each one
    /// individually spends the bitrate budget on the largest frames the
    /// encoder can make at the worst possible moment. See
    /// `MIN_REQUESTED_KEYFRAME_SECONDS`; the browser backend rate-limits the
    /// same requests the same way, and this is the number they share.
    since_forced: u32,
    min_between_forced: u32,
}

impl H264Encoder {
    pub fn new(quality: VideoQuality) -> Result<Self> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .profile(Profile::Baseline)
            .level(Level::Level_3_1)
            .bitrate(BitRate::from_bps(quality.bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(quality.fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            // Off, though a live stream is exactly what it is for. What
            // carries an access unit is an RTP clock advanced by one fixed
            // stride per unit — `VideoSource::rtp_timestamp_stride`, which is
            // a constant — so a frame the rate control declines to encode is
            // not a frame skipped but a frame the clock never accounts for:
            // the video timeline falls one stride behind wall time and stays
            // there, and enough of them under load drift the picture away
            // from the voice. Overshooting the bitrate for a moment is
            // recovered from; a clock that has lost time is not.
            .skip_frames(false)
            // Repeated SPS/PPS under one id, which is what a WhatsApp peer
            // expects to see in front of every IDR.
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .intra_frame_period(IntraFramePeriod::from_num_frames(
                quality.fps.saturating_mul(KEYFRAME_SECONDS).max(1),
            ))
            // Left to the encoder: one thread per core on a machine also
            // running a UI and an audio device is not a bargain, and openh264
            // picks by frame size.
            .num_threads(0);
        let encoder = Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
            .context("initializing the H.264 encoder")?;
        Ok(Self {
            encoder,
            force_keyframe: false,
            since_forced: 0,
            min_between_forced: quality
                .fps
                .saturating_mul(MIN_REQUESTED_KEYFRAME_SECONDS)
                .max(1),
        })
    }

    /// Make the next frame an IDR.
    ///
    /// Asked for when the peer says it lost the stream (an RTCP PLI or FIR)
    /// and when we ourselves dropped an access unit: every frame after a gap
    /// references one the peer does not have, so without this the picture
    /// stays broken until the periodic keyframe comes round.
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Encode one frame, or `None` when the rate control chose to skip it.
    pub fn encode<S: YUVSource>(
        &mut self,
        source: &S,
        at: openh264::Timestamp,
    ) -> Result<Option<EncodedFrame>> {
        self.since_forced = self.since_forced.saturating_add(1);
        // Honoured, but not sooner than the last forced IDR — and the ask is
        // *kept* when it is too soon, so the last request of a burst is
        // never the one lost.
        let forcing = self.force_keyframe && self.since_forced >= self.min_between_forced;
        if forcing {
            self.encoder.force_intra_frame();
        }
        let bitstream = self
            .encoder
            .encode_at(source, at)
            .context("encoding a video frame")?;
        let frame_type = bitstream.frame_type();
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let mut data = Vec::new();
        bitstream.write_vec(&mut data);
        if data.is_empty() {
            return Ok(None);
        }
        let keyframe = matches!(frame_type, FrameType::IDR | FrameType::I);
        // Spent on the IDR rather than on asking for one. Rate control may
        // skip the very frame the force was set on, and clearing the ask there
        // would drop it while starting a cooldown against a keyframe that was
        // never made — the peer waits the interval out for nothing, which is
        // the failure the interval exists to prevent, arrived at from the
        // other side.
        if forcing && keyframe {
            self.force_keyframe = false;
            self.since_forced = 0;
        }
        Ok(Some(EncodedFrame { data, keyframe }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::I420Buffer;

    /// The encoder is the half of this crate a machine with no camera can
    /// still exercise, and what it produces is what the peer has to decode:
    /// an Annex-B unit that starts with a parameter set.
    #[test]
    fn the_first_frame_is_a_keyframe_with_its_parameter_sets_in_front() {
        let quality = VideoQuality::default();
        let mut encoder = H264Encoder::new(quality).expect("the encoder is built in");
        let mut frame = I420Buffer::new(64, 48).expect("even");
        frame.read_gray(&vec![128; 64 * 48]).expect("sized");

        let encoded = encoder
            .encode(&frame.as_source(), openh264::Timestamp::ZERO)
            .expect("encodes")
            .expect("a first frame is never skipped");

        assert!(encoded.keyframe);
        assert!(
            encoded.data.starts_with(&[0, 0, 0, 1]),
            "an access unit begins with a start code"
        );
        // 7 = SPS. A peer that gets a keyframe without one cannot start.
        assert_eq!(encoded.data[4] & 0x1f, 7);
    }

    /// What the peer's decoder gets must decode: packetize one real IDR
    /// access unit the way the send path does, reassemble it the way the
    /// receive path does, and decode the result with an independent
    /// decoder. A failure here is a packetization bug, not a network one —
    /// and the self-view passing is no cover, since it reads the unit
    /// before it is ever fragmented.
    #[test]
    fn packetized_idr_reassembles_into_a_decodable_access_unit() {
        use openh264::decoder::Decoder;
        use wacore::voip::h264::{H264Depacketizer, PacketizedAu, packetize_au};

        let quality = VideoQuality::default();
        let mut encoder = H264Encoder::new(quality).expect("the encoder is built in");
        // Noise defeats compression, so the IDR overshoots the single-NAL
        // ceiling and the fragments under test are real FU-A ones.
        let mut frame = I420Buffer::new(320, 240).expect("even");
        let noise: Vec<u8> = (0..320 * 240)
            .map(|i: u32| (i.wrapping_mul(2654435761) >> 24) as u8)
            .collect();
        frame.read_gray(&noise).expect("sized");
        let encoded = encoder
            .encode(&frame.as_source(), openh264::Timestamp::ZERO)
            .expect("encodes")
            .expect("a first frame is never skipped");
        assert!(encoded.keyframe);

        // Control: the unpacketized unit decodes, on a decoder instance of
        // its own — sharing one would prime it with the original's
        // parameter sets and let a reassembled unit missing them decode
        // anyway, passing for exactly the failure this test claims to
        // catch.
        let mut control = Decoder::new().expect("decoder builds");
        assert!(
            control
                .decode(&encoded.data)
                .expect("control decodes")
                .is_some(),
            "the encoder's own output is a picture"
        );

        // The send path, then the receive path: one timestamp, marker on
        // the last payload, sequences from zero, like the stream's first
        // unit on the wire.
        let mut packetized = PacketizedAu::default();
        packetize_au(&encoded.data, &mut packetized);
        assert!(
            packetized.iter().any(|payload| payload[0] & 0x1f == 28),
            "a noisy IDR fragments into FU-A, not just single NALs"
        );
        let mut depacketizer = H264Depacketizer::default();
        let last = packetized.len() - 1;
        let mut reassembled = None;
        for (i, payload) in packetized.iter().enumerate() {
            if let Some(ready) = depacketizer.push(i as u16, 0, payload, i == last) {
                reassembled = Some(ready);
            }
        }
        let (_, au) = reassembled.expect("the fragments reassemble into one unit");
        // A fresh instance: whatever parameter sets the reassembled unit
        // needs must arrive inside it, not linger from the control.
        let mut decoder = Decoder::new().expect("decoder builds");
        assert!(
            decoder
                .decode(&au)
                .expect("reassembled output decodes")
                .is_some(),
            "the reassembled IDR yields a picture"
        );
    }
}
