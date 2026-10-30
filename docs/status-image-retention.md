# Image asset retention after browsing status

## Cause and scope

On GPUI 0.3.10, `ImageSource::Image` uses
`AssetLogger<ImageDecoder>` through `Window::use_asset`. `App::loading_assets`
keeps the completed output until `App::remove_asset` is called. Its key is a
type and hash, so dropping the application's `Arc<Image>` does not retire the
decoded output. Removing entries from `WhatsAppApp::decoded_images`, including
its LRU, media-release sweep and account reset, therefore left decoded pixels
in the global asset system. The sticker decoder used the same asset system.

`RetainedImage` now shares ownership between the existing application cache
and the image sources handed to surfaces. Drawing leases its decode entity
through GPUI element state as well. Independently constructed images with
identical bytes share that entity through an application-scoped weak registry,
because GPUI's image IDs and asset keys are content-derived. Releasing one
message cannot retire a decode still owned by another. Element-state leasing
matters because cached
paint can retain atlas coordinates without retaining the image source. Once
all owners and paint leases are gone, deferred retirement removes that one
asset and its atlas entries. Removing a pending asset drops GPUI's cached-load
task; GPUI's weak completion state cannot repopulate the removed entry.

The still decoder calls the same `Image::to_image_data` implementation as
GPUI, including orientation and colour conversion. Stickers keep their
existing compositing decoder, delays and transparency. Cache entry limits,
byte budgets and eviction order are unchanged. A cached image can reopen
without another decode. The change covers status, viewer, message images,
posters and stickers sharing this cache, plus the uncached bubble fallback.
Video player ownership is a separate investigation.

## Synthetic reproduction

Run the ignored diagnostic in separate processes on the same Linux runner:

```sh
OXIDEZAP_IMAGE_PROFILE_MODE=legacy cargo test -p oxidezap-gui \
  synthetic_image_lifecycle_report -- --ignored --nocapture
OXIDEZAP_IMAGE_PROFILE_MODE=owned cargo test -p oxidezap-gui \
  synthetic_image_lifecycle_report -- --ignored --nocapture
```

Both arms generate and decode 80 distinct 1024×1024 PNGs and retain an
application cache of 50 images. They use the same decoder and GPUI asset
system. The legacy arm deliberately omits retirement, reproducing the old
asset lifetime; it is an isolated lifecycle control, not an unmodified full
application binary. Each arm forces 10,000 draws of the last image, then
clears the cache and closes the surface. There is no daemon, account,
network media or user fixture.

Weak references to actual `RenderImage` outputs count live pixel buffers.
The diagnostic reads process RSS/PSS from `/proc/self/smaps_rollup` and CPU
ticks from `/proc/self/stat`. On this runner `CLK_TCK` was 100.

One paired run on 2026-10-30, debug test profile, GPUI TestRenderer:

| Observation | Legacy lifetime | Owned lifetime |
| --- | ---: | ---: |
| Initial RSS, KiB | 31,164 | 30,444 |
| Initial PSS, KiB | 30,691 | 29,971 |
| Live decoded pixels after 80, MiB | 320 | 200 |
| RSS after 80, KiB | 379,684 | 321,248 |
| PSS after 80, KiB | 379,211 | 320,775 |
| Live decoded pixels after cache clear and close, MiB | 320 | 0 |
| RSS after close, KiB | 379,736 | 165,788 |
| PSS after close, KiB | 379,263 | 165,315 |
| Process CPU through close, seconds | 8.50 | 8.44 |
| Process CPU for 10,000 forced draws, seconds | 0.06 | 0.08 |

The 120 MiB reduction while the cache is alive follows its existing
50-entry policy. The stronger result is release of all 320 MiB of abandoned
pixel buffers after all owners disappear. The process still holds about
162 MiB RSS afterward, despite zero live outputs in this fixture. This
measurement does not attribute that residual to a particular allocator pool.

CPU results are coarse observations from one paired run, not a throughput
claim. The forced-draw difference is 20 ms across 10,000 draws, about 2 µs
per draw. Encoding and decoding dominate the complete scenario; there was
no material difference in its total CPU in this sample. The test renderer
does not measure a production WGPU driver, real GPU allocation, frame
latency or interaction responsiveness. Repeat native A/B profiling before
claiming a production CPU or GPU-memory result. Do not expect RSS to fall
to its initial value immediately after close.

## Regression coverage

The ordinary tests exercise:

- eviction while an element still needs the image;
- closing and reopening with a cached decode, checking pointer identity;
- asset and atlas retirement after the last surface releases the image;
- closing a pending decode without late cache repopulation;
- cached paint retaining both pixels and atlas until its pane repaints;
- a shared image surviving another window closing;
- independently constructed identical PNG images and WebP stickers sharing
  retirement ownership, with the first window closing while the other remains;
- existing sticker animation, transparency and layout checks.

The Linux diagnostic is ignored by default because it intentionally retains
320 MiB in its legacy control. The ordinary lifetime tests use synthetic
media and GPUI's deterministic test executor. Real account RSS/PSS, allocator
statistics and WGPU/GPU resources still need the coordinated laptop run.
