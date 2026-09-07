use std::collections::VecDeque;

pub(super) struct Output {
    scratch: Box<[f32]>,
    block: js_sys::Float32Array,
}

impl Output {
    pub fn new(len: u32) -> Self {
        Self {
            scratch: vec![0.0; len as usize].into_boxed_slice(),
            block: js_sys::Float32Array::new_with_length(len),
        }
    }

    pub fn fill(&mut self, ring: &mut VecDeque<f32>) {
        for sample in &mut self.scratch {
            *sample = ring.pop_front().unwrap_or(0.0);
        }
    }

    pub fn write(&self, buffer: &web_sys::AudioBuffer) -> Result<(), wasm_bindgen::JsValue> {
        // TypedArray.set accepts a shared source; WebAudio only sees the owned copy.
        self.block.copy_from(&self.scratch);
        buffer.copy_to_channel_with_f32_array(&self.block, 0)
    }
}
