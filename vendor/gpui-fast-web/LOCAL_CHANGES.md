# Canvas readback compatibility patch

Source: `gpui-fast-web` 0.1.13 from crates.io, SHA-256
`f5194d398f801160d6be5660055a14b175aed9b2dbca348d56065d742b11dbd9`.
The Apache-2.0 license and published manifest are retained. The crate-local
lockfile is omitted because the root lockfile resolves this dependency.

The only source change is in `src/canvas_text.rs`. With
`web_sys_unstable_apis`, web-sys 0.3.106 takes integer coordinates and sizes
for `OffscreenCanvasRenderingContext2d::get_image_data`. The published backend
passes floats and fails to compile for our web target. Convert the unsigned
sizes with checked `try_into` and pass integer origins. This carries the
existing gpui-pre-web patch onto the gpui-fast backend.

Remove this directory and its root patch when the published backend supports
that binding. When updating, compare the published source with this directory;
only the readback call should differ. Validate through the Pages WASM build,
which compiles the GUI with `web_sys_unstable_apis` and shared memory. The
standalone WebCodecs test crate does not compile this backend.
