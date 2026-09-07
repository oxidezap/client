//! Compile the production readback code without GPUI's platform backend.

#![allow(dead_code, private_interfaces)]

extern crate self as gpui;

use std::cell::Cell;
use std::rc::Rc;

#[path = "../../src/video/geometry.rs"]
mod geometry;
#[path = "../../src/video/sps.rs"]
mod sps;
#[path = "../../src/video/webcodecs.rs"]
mod webcodecs;

pub use webcodecs::{Decoder, Picture};

thread_local! {
    static IMAGES: Cell<usize> = const { Cell::new(0) };
}

pub struct RenderImage(pub smallvec::SmallVec<[image::Frame; 1]>);

impl RenderImage {
    pub fn new(frames: smallvec::SmallVec<[image::Frame; 1]>) -> Self {
        IMAGES.with(|count| count.set(count.get() + 1));
        Self(frames)
    }
}

pub fn images_created() -> usize {
    IMAGES.with(Cell::get)
}

pub fn decoder(sink: Option<Rc<dyn Fn(Picture)>>) -> Result<Decoder, String> {
    // Synthetic baseline SPS for one 16x16 macroblock. No captured media.
    let sps = [0, 0, 0, 1, 0x67, 0x42, 0, 0x1e, 0xf4, 0xf2];
    assert_eq!(sps::coded_size(&sps), sps::Geometry::Size(16, 16));
    Decoder::with_budget(&sps, geometry::Rotation::None, 1280 * 720, sink)
}

pub fn feed(decoder: &Decoder, timestamp: i32, device_orientation: u8) {
    decoder.set_rotation(geometry::Rotation::to_upright(device_orientation));
    decoder.decode(&[], timestamp, false);
}
