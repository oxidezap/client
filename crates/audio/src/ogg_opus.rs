//! Opus packets, in the OGG stream WhatsApp expects.
//!
//! The container is not the codec, and only the codec was ever the problem
//! here: libopus is C and does not build for `wasm32-unknown-unknown`, while
//! the `ogg` crate is plain Rust and builds anywhere. So the packaging lives
//! on its own, takes packets from whichever encoder produced them — libopus
//! on a desktop, the browser's own `AudioEncoder` in a page — and writes the
//! identical stream either way.
//!
//! That identity is the point. A voice note is read by the recipient's
//! WhatsApp, not by us, so two encoders producing two slightly different
//! containers would be two things to get right and one of them would only
//! ever be tested by strangers.

use std::io::Cursor;

use ogg::writing::PacketWriteEndInfo;

/// What a voice note is encoded at, and what the header must therefore say.
///
/// Not a choice made here: WhatsApp's own voice notes are 16 kHz mono Opus,
/// and a note that differs plays at the wrong speed or not at all.
pub const SAMPLE_RATE: u32 = 16_000;
/// Opus always reports granule positions at 48 kHz, whatever it encoded at.
const GRANULE_RATE: u32 = 48_000;
/// The frame length everything here is cut to.
pub const FRAME_SIZE_MS: usize = 20;
/// How many input samples one frame is.
pub const FRAME_SIZE_SAMPLES: usize = (SAMPLE_RATE as usize * FRAME_SIZE_MS) / 1000;
/// Granules one frame advances by.
const GRANULE_PER_FRAME: u64 = (GRANULE_RATE as u64 * FRAME_SIZE_MS as u64) / 1000;
/// Samples a decoder discards at the head of the stream.
pub const PRE_SKIP: u16 = 312;

/// The end-of-stream granule for a capture of `sample_count` samples.
///
/// Exposed because it is what a decoder trims to, which makes it the one
/// number worth asserting about a produced stream.
#[must_use]
pub fn eos_granule(sample_count: usize) -> u64 {
    u64::from(PRE_SKIP) + sample_count as u64 * u64::from(GRANULE_RATE / SAMPLE_RATE)
}

/// Wrap Opus packets in an OGG stream.
///
/// `sample_count` is how many input samples were captured, which is what the
/// end-of-stream granule is computed from: decoders trim to it, so it has to
/// describe the recording rather than the packets.
///
/// # Errors
///
/// The OGG writer refused a packet, which on an in-memory cursor means the
/// stream itself was malformed rather than that any I/O failed.
pub fn package(packets: Vec<Vec<u8>>, sample_count: usize) -> Result<Vec<u8>, String> {
    let mut ogg_buffer = Vec::new();
    let serial = rand_serial();

    // The EOS granule reflects the real capture length plus pre-skip:
    // decoders discard `PRE_SKIP` samples up front, so trimming to the
    // granule must land on the capture's end rather than that many samples
    // before it.
    let eos_granule = eos_granule(sample_count);

    {
        let cursor = Cursor::new(&mut ogg_buffer);
        let mut writer = ogg::PacketWriter::new(cursor);

        writer
            .write_packet(id_header(), serial, PacketWriteEndInfo::EndPage, 0)
            .map_err(|e| e.to_string())?;
        writer
            .write_packet(comment_header(), serial, PacketWriteEndInfo::EndPage, 0)
            .map_err(|e| e.to_string())?;

        let total = packets.len();
        let mut granule: u64 = 0;
        for (i, packet) in packets.into_iter().enumerate() {
            granule += GRANULE_PER_FRAME;
            let (end, at) = if i + 1 == total {
                (PacketWriteEndInfo::EndStream, eos_granule)
            } else {
                (PacketWriteEndInfo::NormalPacket, granule)
            };
            writer
                .write_packet(packet, serial, end, at)
                .map_err(|e| e.to_string())?;
        }
    }

    Ok(ogg_buffer)
}

/// Whether one more silent frame is needed to cover the logical duration.
///
/// When the final frame's zero-padding cannot absorb the pre-skip — and an
/// exact frame multiple has no padding at all — the packet stream stops short
/// of the granule the header promises, and a decoder trims into real audio.
#[must_use]
pub fn needs_trailing_silence(packet_count: usize, sample_count: usize) -> bool {
    eos_granule(sample_count) > packet_count as u64 * GRANULE_PER_FRAME
}

fn id_header() -> Vec<u8> {
    let mut header = Vec::with_capacity(19);
    header.extend_from_slice(b"OpusHead");
    header.push(1); // Version
    header.push(1); // Channels (mono)
    header.extend_from_slice(&PRE_SKIP.to_le_bytes());
    header.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    header.extend_from_slice(&0u16.to_le_bytes()); // Output gain (0 dB)
    header.push(0); // Channel mapping family
    header
}

fn comment_header() -> Vec<u8> {
    let mut header = Vec::new();
    header.extend_from_slice(b"OpusTags");
    let vendor = b"whatsapp-rust";
    header.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    header.extend_from_slice(vendor);
    header.extend_from_slice(&0u32.to_le_bytes()); // No comments
    header
}

fn rand_serial() -> u32 {
    let seed = wacore::time::now_millis() as u32;
    seed.wrapping_mul(1_103_515_245).wrapping_add(12345)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identification header is 16 kHz mono with the pre-skip a decoder
    /// trims by; a recipient reads these bytes, so they are worth pinning.
    #[test]
    fn the_identification_header_says_what_was_encoded() {
        let header = id_header();
        assert_eq!(&header[0..8], b"OpusHead");
        assert_eq!(header[8], 1, "version");
        assert_eq!(header[9], 1, "mono");
        assert_eq!(
            u16::from_le_bytes([header[10], header[11]]),
            PRE_SKIP,
            "pre-skip"
        );
        assert_eq!(
            u32::from_le_bytes([header[12], header[13], header[14], header[15]]),
            SAMPLE_RATE,
        );
    }

    #[test]
    fn the_comment_header_is_a_well_formed_opus_tags() {
        let header = comment_header();
        assert_eq!(&header[0..8], b"OpusTags");
    }

    /// The stream a recipient's WhatsApp reads has to start with the two
    /// header pages, whichever encoder produced the packets behind them.
    #[test]
    fn the_stream_opens_with_the_opus_headers() {
        let ogg = package(vec![vec![0xFC; 8]], FRAME_SIZE_SAMPLES).expect("packaged");
        let head = ogg
            .windows(8)
            .position(|w| w == b"OpusHead")
            .expect("an identification header");
        let tags = ogg
            .windows(8)
            .position(|w| w == b"OpusTags")
            .expect("a comment header");
        assert!(head < tags, "the identification header comes first");
    }

    /// An exact frame multiple has no zero-padding to absorb the pre-skip, so
    /// the packets stop short of the granule the header promises and a
    /// decoder would trim into real audio.
    #[test]
    fn an_exact_frame_multiple_needs_one_more_frame() {
        assert!(needs_trailing_silence(1, FRAME_SIZE_SAMPLES));
        assert!(needs_trailing_silence(2, FRAME_SIZE_SAMPLES * 2));
    }

    /// A recording whose last frame is mostly padding already covers it.
    #[test]
    fn a_short_final_frame_covers_the_pre_skip() {
        assert!(!needs_trailing_silence(2, FRAME_SIZE_SAMPLES + 1));
    }

    /// Empty in, empty out rather than a stream with no packets: the caller
    /// refuses an empty recording before it gets here, and this says so if
    /// one ever does.
    #[test]
    fn no_packets_still_writes_the_headers() {
        let ogg = package(Vec::new(), 0).expect("packaged");
        assert!(ogg.windows(8).any(|w| w == b"OpusHead"));
    }
}
