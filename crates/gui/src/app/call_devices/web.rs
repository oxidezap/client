//! Browser preview display capability.
use super::{Arc, RenderImage};

#[derive(Default)]
pub(super) struct PreviewDecoder {}
impl PreviewDecoder {
    pub(super) fn decode(
        &mut self,
        _frame: &oxidezap_ipc::CameraPreviewFrame,
    ) -> Result<Option<Arc<RenderImage>>, String> {
        Err("Camera preview display is unavailable from this browser connection".into())
    }
}
