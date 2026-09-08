# Video call performance investigation

Investigation of the September 7, 2026 production browser recording. Raw traces
and logs are private and are not included here.

## Recorded behavior

The active renderer recording covers 19.818 seconds. Older trace events without
renderer activity are excluded. Comparing its first and last five seconds:

| Metric | First five seconds | Last five seconds |
| --- | ---: | ---: |
| Main-thread task wall time | 3.796 s | 4.566 s |
| CPU time within those tasks | 2.554 s | 2.717 s |
| Sample-weighted `VideoFrame.copyTo` time | 1.689 s | 2.332 s |
| Animation callback p95 duration | 20.37 ms | 42.68 ms |
| Tasks longer than 50 ms | 0 | 6 |

`copyTo` accounts for 42.5% of the main-thread sampled timeline overall.
Sample-weighted elapsed time includes waiting and is not per-function CPU time.
The recording has substantial profiler overhead, including 25 profiler threads
with 17.220 cumulative CPU seconds. Animation callbacks are not video
presentation FPS.

The JS heap starts at 5.90 MiB and ends at 5.86 MiB. That does not measure WASM
memory, GPU textures, or process RSS. The recording alone does not prove a leak.

The separate log contains 292 audio-underrun warnings, with the affected-block
counter reaching 1,742, and one remote video decoding failure. An underrun means
the playout ring lacked samples, not necessarily network loss. The log has no
timestamps and cannot be aligned to individual trace stalls. The camera reports
1280x720 at 20 FPS.

## Corrections

- Retire video textures with GPUI element state. Dropping `Arc<RenderImage>` alone
  does not remove the atlas entry. Cached paint can still reference a tile after
  the producer replaces its picture, so retirement must follow scene ownership.
  The same helper covers calls and attachment playback on both platforms.
- Cache the screen independently of call pictures. State, theme, bounds, and
  input changes still invalidate it. Focus transitions run after drawing so GPUI
  can update focus paths and emit blur events.
- Use the post-release GPUI Kit 0.6.0 revision recorded in `Cargo.lock`,
  containing the input notification fix merged in
  [upstream PR #2988](https://github.com/longbridge/gpui-kit/pull/2988),
  rather than the `v0.6.0` tag.
  Unchanged input scroll offsets and paint geometry no longer notify every draw.
- Give decoded-frame readiness a capacity-one lane, separate from ordinary
  events. After at most 16 ordinary deliveries, pending video gets a turn.
- Reject video outside authoritative call/camera state. Retire each direction
  independently, including its delayed callbacks, when its camera stops.
- Request a keyframe after a GUI decoder loses its compressed reference chain.
  Requests carry the original call and direction, use the originating connection,
  and are limited to one attempt per second per direction. Superseding a decoded
  picture does not request recovery. Daemon refresh requests both directions.
- Bound live WebCodecs output to one active readback and one replaceable pending
  frame. Reuse the JS destination after the promise settles, reject obsolete
  output before Rust materialization, and swap unrotated pixels in place.
  Configuration failure closes the decoder before releasing its callbacks.

At 720p, one packed pixel buffer is 3.516 MiB. Removing one Rust allocation and
reusing one JS allocation at 20 FPS each avoids 70.31 MiB/s of allocation demand
per stream at stable resolution. These are arithmetic reductions, not measured
CPU, bandwidth, or FPS improvements. A readback still costs work; overloaded
output now skips obsolete decoded pictures before starting more readbacks.

The client also consumes the merged
[WhatsApp rotation fix #1463](https://github.com/oxidezap/whatsapp-rust/pull/1463).
Its real-WASM oracle establishes upright frame-info bytes `0x00` for delta and
`0x08` for IDR, rather than `0x01` and `0x09`. The incorrect rotation metadata
crossed RTP to Android but was absent from the local encoded preview.

## Regression evidence

- Atlas tests inspect actual GPUI test-atlas membership across 120 replacements,
  removal, cached paint, shared views, and shared windows.
- Body tests assert zero rebuilds for 60 steady-state call notifications with
  chat, settings, and status screens, plus actual input blur/focus handoff.
- Lifecycle tests use valid H.264 IDR and delta frames to verify that turning one
  camera off preserves the opposite decoder's reference chain.
- Queue tests cover continuously replenished ordinary traffic and a full
  ordinary queue with a final pending video notification.
- Nine [browser readback tests](../crates/gui/tests/webcodecs/README.md) import
  production code and exercise real `VideoFrame.copyTo` with controlled promise
  completion. CI runs them separately from GPUI's shared-memory build.

## Remaining measurements

No post-fix live call or native GPU-memory profile was recorded. Browser tests
do not exercise hardware decoding, GPUI painting, or shared-memory integration.
Linux tests and native/web compilation do not replace Windows/macOS runtime
testing. Decoder-error recovery without subsequent input is not autonomous.

For a before/after call, use the same camera, dimensions, peer, browser, and
window layout. Compare a low-overhead timing recording against a sampled one;
use the [symbol-preserving web build](building.md) for the latter. Record a long
call and hangup, including memory, frame age, audio underruns, and input latency.
Check all four video directions visually after the RTP fix.

Further work needs separate reproduction and measurement. Candidates include
moving browser audio callbacks to AudioWorklet, avoiding CPU pixel readback,
preserving capture timestamps across encoder drops, and isolating slow IPC media
prefetch or transport writes from live video. This patch does not claim to solve
those paths or to establish a production FPS gain.
