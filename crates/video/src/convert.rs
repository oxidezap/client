//! Camera pixels into the one layout an H.264 encoder takes.
//!
//! Every webcam hands out one of a handful of formats and the encoder wants
//! I420, so this is where the difference is absorbed. The two that matter are
//! done by hand — YUYV and NV12 are already YUV, and going through RGB to
//! come back would be two conversions and a loss for nothing — and anything
//! else is decoded to RGB by the capture backend and converted by openh264's
//! own (SIMD) path.
//!
//! Chroma is averaged down the row pair rather than dropped, because a call
//! is mostly faces and a dropped row is visibly noisier at the same cost of
//! one add.

use anyhow::{Result, bail};
use openh264::formats::{YUVBuffer, YUVSlices};

/// A planar I420 frame, kept as one allocation and reused every frame.
pub struct I420Buffer {
    data: Vec<u8>,
    width: usize,
    height: usize,
}

impl I420Buffer {
    /// Both dimensions must be even, which is what "4:2:0" means.
    pub fn new(width: usize, height: usize) -> Result<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("I420 needs positive even dimensions, got {width}x{height}");
        }
        Ok(Self {
            data: vec![0; width * height * 3 / 2],
            width,
            height,
        })
    }

    /// The three planes, as the encoder reads them.
    pub fn as_source(&self) -> YUVSlices<'_> {
        let luma = self.width * self.height;
        let chroma = luma / 4;
        let (y, rest) = self.data.split_at(luma);
        let (u, v) = rest.split_at(chroma);
        YUVSlices::new(
            (y, u, v),
            (self.width, self.height),
            (self.width, self.width / 2, self.width / 2),
        )
    }

    fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        let luma = self.width * self.height;
        let chroma = luma / 4;
        let (y, rest) = self.data.split_at_mut(luma);
        let (u, v) = rest.split_at_mut(chroma);
        (y, u, v)
    }

    /// Packed 4:2:2, two luma samples per `Y0 Cb Y1 Cr` quad.
    pub fn read_yuyv(&mut self, src: &[u8]) -> Result<()> {
        let (width, height) = (self.width, self.height);
        let stride = width * 2;
        if src.len() < stride * height {
            bail!(
                "YUYV frame is {} bytes, {}x{} needs {}",
                src.len(),
                width,
                height,
                stride * height
            );
        }
        let (y_plane, u_plane, v_plane) = self.planes_mut();
        for row in 0..height {
            let src_row = &src[row * stride..row * stride + stride];
            let dst_row = &mut y_plane[row * width..row * width + width];
            for (pair, quad) in src_row.chunks_exact(4).enumerate() {
                dst_row[pair * 2] = quad[0];
                dst_row[pair * 2 + 1] = quad[2];
            }
        }
        for row in 0..height / 2 {
            let top = &src[row * 2 * stride..row * 2 * stride + stride];
            let bottom = &src[(row * 2 + 1) * stride..(row * 2 + 1) * stride + stride];
            for column in 0..width / 2 {
                let at = column * 4;
                u_plane[row * (width / 2) + column] = mean(top[at + 1], bottom[at + 1]);
                v_plane[row * (width / 2) + column] = mean(top[at + 3], bottom[at + 3]);
            }
        }
        Ok(())
    }

    /// Planar luma followed by interleaved chroma at half resolution.
    pub fn read_nv12(&mut self, src: &[u8]) -> Result<()> {
        let (width, height) = (self.width, self.height);
        let luma = width * height;
        if src.len() < luma * 3 / 2 {
            bail!(
                "NV12 frame is {} bytes, {}x{} needs {}",
                src.len(),
                width,
                height,
                luma * 3 / 2
            );
        }
        let (y_plane, u_plane, v_plane) = self.planes_mut();
        y_plane.copy_from_slice(&src[..luma]);
        for (index, pair) in src[luma..luma * 3 / 2].chunks_exact(2).enumerate() {
            u_plane[index] = pair[0];
            v_plane[index] = pair[1];
        }
        Ok(())
    }

    /// Luma only, from a camera that has no colour to give (infrared, some
    /// industrial sensors). The chroma planes are the neutral point, which is
    /// what makes the result grey rather than green.
    pub fn read_gray(&mut self, src: &[u8]) -> Result<()> {
        let (width, height) = (self.width, self.height);
        let luma = width * height;
        if src.len() < luma {
            bail!(
                "GRAY frame is {} bytes, {width}x{height} needs {luma}",
                src.len()
            );
        }
        let (y_plane, u_plane, v_plane) = self.planes_mut();
        y_plane.copy_from_slice(&src[..luma]);
        u_plane.fill(128);
        v_plane.fill(128);
        Ok(())
    }
}

/// Rounded mean of two samples. `+1` before the shift so a pair straddling a
/// value does not always fall short of it.
fn mean(a: u8, b: u8) -> u8 {
    (u16::from(a) + u16::from(b)).div_ceil(2) as u8
}

/// What a frame is converted *through*.
///
/// One arm per shape rather than a trait: the format is settled when the
/// camera opens and never changes, and the RGB arm has to own a second buffer
/// the others do not need.
pub enum Frames {
    /// Already YUV: converted in place into one reusable plane set.
    Planar(I420Buffer),
    /// Anything else, decoded to RGB by the capture backend first. openh264
    /// owns the conversion (and its SIMD paths); the RGB scratch is kept so
    /// the decode has somewhere to land that is not a fresh allocation.
    Rgb { rgb: Vec<u8>, yuv: YUVBuffer },
}

impl Frames {
    pub fn planar(width: usize, height: usize) -> Result<Self> {
        Ok(Self::Planar(I420Buffer::new(width, height)?))
    }

    pub fn rgb(width: usize, height: usize) -> Result<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("video needs positive even dimensions, got {width}x{height}");
        }
        Ok(Self::Rgb {
            rgb: vec![0; width * height * 3],
            yuv: YUVBuffer::new(width, height),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::formats::YUVSource as _;

    #[test]
    fn odd_dimensions_are_refused_rather_than_rounded() {
        assert!(I420Buffer::new(641, 480).is_err());
        assert!(I420Buffer::new(640, 0).is_err());
        assert!(I420Buffer::new(640, 480).is_ok());
    }

    #[test]
    fn yuyv_keeps_every_luma_sample_and_averages_the_chroma() {
        let mut buffer = I420Buffer::new(4, 2).expect("even");
        // Two rows of two `Y0 Cb Y1 Cr` quads. The chroma differs between the
        // rows so the average is visible in the result.
        let src: Vec<u8> = vec![
            10, 100, 11, 200, 12, 100, 13, 200, // row 0
            20, 140, 21, 240, 22, 140, 23, 240, // row 1
        ];
        buffer.read_yuyv(&src).expect("sized");
        let source = buffer.as_source();
        assert_eq!(source.y(), &[10, 11, 12, 13, 20, 21, 22, 23]);
        assert_eq!(source.u(), &[120, 120]);
        assert_eq!(source.v(), &[220, 220]);
    }

    #[test]
    fn a_short_frame_is_an_error_rather_than_a_panic() {
        let mut buffer = I420Buffer::new(4, 2).expect("even");
        assert!(buffer.read_yuyv(&[0; 8]).is_err());
        assert!(buffer.read_nv12(&[0; 8]).is_err());
        assert!(buffer.read_gray(&[0; 4]).is_err());
    }

    #[test]
    fn nv12_deinterleaves_its_chroma() {
        let mut buffer = I420Buffer::new(2, 2).expect("even");
        buffer.read_nv12(&[1, 2, 3, 4, 90, 200]).expect("sized");
        let source = buffer.as_source();
        assert_eq!(source.y(), &[1, 2, 3, 4]);
        assert_eq!(source.u(), &[90]);
        assert_eq!(source.v(), &[200]);
    }

    #[test]
    fn a_camera_with_no_colour_produces_grey_rather_than_green() {
        let mut buffer = I420Buffer::new(2, 2).expect("even");
        buffer.read_gray(&[5, 6, 7, 8]).expect("sized");
        let source = buffer.as_source();
        assert_eq!(source.y(), &[5, 6, 7, 8]);
        assert_eq!(source.u(), &[128]);
        assert_eq!(source.v(), &[128]);
    }
}
