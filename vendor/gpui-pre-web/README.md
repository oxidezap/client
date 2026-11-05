# gpui-pre-web compatibility patch

Source: `gpui-pre-web` 0.3.10 from crates.io, SHA-256 `1acd86af2347f0dd8b63c441a5dbd43b3fcd7dac69b340827c3497da5a13d6fb`.
The original Apache-2.0 license and published manifest are retained.

Only `src/canvas_text.rs` differs from the published source. web-sys 0.3.106
changed `OffscreenCanvasRenderingContext2d::get_image_data` coordinates and sizes
to `i32`. Convert dimensions with checked conversions and pass integer origins.

Remove the root `[patch.crates-io]` entry and this directory once a published
GPUI family includes that fix. Keep the GPUI family on matching versions.
