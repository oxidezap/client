//! Native H.264 preview decoder.
use super::{Arc, RenderImage};

#[derive(Default)]
pub(super) struct PreviewDecoder {
    decoder: Option<openh264::decoder::Decoder>,
    started: bool,
}
impl PreviewDecoder {
    pub(super) fn decode(
        &mut self,
        frame: &oxidezap_ipc::CameraPreviewFrame,
    ) -> Result<Option<Arc<RenderImage>>, String> {
        use openh264::formats::YUVSource as _;
        if !self.started && !frame.keyframe {
            return Ok(None);
        }
        if self.decoder.is_none() {
            self.decoder = Some(openh264::decoder::Decoder::new().map_err(|e| e.to_string())?);
        }
        let decoder = self
            .decoder
            .as_mut()
            .ok_or("Preview decoder is unavailable")?;
        let Some(yuv) = decoder.decode(&frame.data).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        if width == 0 || height == 0 || width.saturating_mul(height) > 1280 * 720 {
            return Err("Preview dimensions exceed the supported limit".into());
        }
        self.started = true;
        let mut bytes = vec![0; width * height * 4];
        yuv.write_rgba8(&mut bytes);
        for pixel in bytes.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        let image = image::RgbaImage::from_raw(width as u32, height as u32, bytes)
            .ok_or("Invalid preview image")?;
        Ok(Some(Arc::new(RenderImage::new(
            smallvec::SmallVec::from_elem(image::Frame::new(image), 1),
        ))))
    }
}
#[cfg(test)]
mod preview_tests {
    use super::*;
    use oxidezap_core::CameraPreviewId;
    #[test]
    fn preview_decodes_synthetic_video_without_a_call_identity() {
        use openh264::encoder::{Encoder, EncoderConfig};
        use openh264::formats::{RgbSliceU8, YUVBuffer};
        let mut encoder =
            Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new())
                .unwrap();
        let pixels = vec![80; 32 * 32 * 3];
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&pixels, (32, 32)));
        let idr = encoder.encode(&yuv).unwrap().to_vec();
        let delta = encoder.encode(&yuv).unwrap().to_vec();
        let id = CameraPreviewId(31);
        let mut decoder = PreviewDecoder::default();
        assert!(
            decoder
                .decode(&oxidezap_ipc::CameraPreviewFrame {
                    preview_id: id,
                    data: delta.clone(),
                    keyframe: false
                })
                .unwrap()
                .is_none()
        );
        let mut images = usize::from(
            decoder
                .decode(&oxidezap_ipc::CameraPreviewFrame {
                    preview_id: id,
                    data: idr,
                    keyframe: true,
                })
                .unwrap()
                .is_some(),
        );
        images += usize::from(
            decoder
                .decode(&oxidezap_ipc::CameraPreviewFrame {
                    preview_id: id,
                    data: delta,
                    keyframe: false,
                })
                .unwrap()
                .is_some(),
        );
        assert!(
            images > 0,
            "synthetic preview must produce a renderable image"
        );
    }
}
