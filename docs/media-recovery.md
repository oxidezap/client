# Media recovery and readback follow-up

The PR 139 preview recording showed shorter main-thread tasks, but still contained
11 remote decoder failures and repeated audio underruns. It began during a call
and ended before hangup, so it did not verify camera shutdown. Raw logs and
profiling data remain private.

## H.264 admission

The upstream transport's keyframe flag can describe SPS/PPS-only access units.
Those units configure a decoder but cannot restart its picture reference chain.
Native and web call decoders now inspect the bitstream rather than treating that
flag as proof of recovery.

The shared helper retains bounded parameter sets by ID and resolves each IDR
slice's PPS-to-SPS references. A WebCodecs Annex-B key chunk contains its required
sets, including when they arrived separately. Valid parameter-only updates do
not reset an active decoder. Their declarations precede the next picture, and
sets announced for later delta pictures are preserved. Ordinary deltas and
complete in-band IDRs can borrow their input rather than rebuilding it.

An invalid admission header or unresolved IDR reference returns to recovery.
The helper checks leading headers, parameter references, and the pixel budget;
it is not a full H.264 validator. Decoders still handle malformed picture data.
Tests compare direct decoding with prepared streams using OpenH264 and real
browser codecs, including repeated parameters and a later delta selecting a
different PPS. No captured media is used.

Recovery diagnostics report bounded request attempts, queue admission, IDR
admission, and first output. Queue admission is not an acknowledgment from the
peer, and first decoder output is not proof of presentation on screen.

## Browser readback

Unrotated pictures use BGRA output when a cached one-pixel probe verifies both
support and channel order. Browsers that reject or silently ignore the format
option use RGBA. A real-frame BGRA copy refusal retries RGBA while the frame is
still open. Rotation keeps the existing fused RGBA-to-BGRA rotation path.

The asynchronous destination remains JS-owned. The active/pending bounds,
generation checks, dimensions, frame rate, and encoded quality are unchanged.

Four interleaved synthetic Chrome runs measured 6.4-10.5% lower mean time from
output callback to publication with BGRA. The tests used optimized WASM, identical
640x360 and 1280x720 RGBA/I420 inputs, 50 warmup pictures, and 1,000 measured
pictures per path. The removed Rust swizzle accounts for the benefit; `copyTo`
itself did not consistently get faster. This is not a live-call FPS or CPU result.
The [browser test README](../crates/gui/tests/webcodecs/README.md) records the
benchmark commands, results, fallbacks, and limitations.

## Audio callbacks

The browser playout callback now reuses a JS-owned sample buffer and Rust scratch
storage. It does not log or format write errors. A cancellable platform wait
reports starvation and scheduling counters at five-second intervals, with one
final total on teardown. The first speaker-write error is retained for reporting
even when statistics are disabled.

An alternating 1,000-block test previously made 500 warning decisions. Reporting
the same events at a synthetic 20 ms cadence produces three interval summaries
and one final total. Sample-buffer backing allocations fall from 1,000 to one
at setup, with no further sample-buffer allocations during the test. Copies and
temporary binding views still exist. Startup priming, buffering ceilings, codec
negotiation, and capture behavior are unchanged.

Real-browser tests verify sample values, zero filling, reuse after WASM memory
growth, rejection of shared views by WebAudio, and cancellation of reporting.
They run locally in ordinary and shared-memory builds. CI runs the ordinary
browser configuration and native statistics tests. See the
[playout test README](../crates/audio/src/web/call_device/tests/README.md).

Successful active-frame and SID messages from MLow now use `trace` rather than
`debug`, through merged [whatsapp-rust#1466](https://github.com/oxidezap/whatsapp-rust/pull/1466).
Warnings, rejected operating points, and failure diagnostics remain unchanged.
Its tests compare decoded PCM and frame reports across logging levels.

## Remaining work

These fixes do not establish complete WhatsApp receive-policy parity or eliminate
network loss. The upstream receive queue requests recovery after overflow but
does not propagate that loss to the next delivered access unit. Its depacketizer
also needs separate whole-access-unit loss coverage. Correcting those contracts
requires an upstream change; this patch does not infer loss from timestamps or
add another PLI trigger.

Audio still uses ScriptProcessorNode, not AudioWorklet. Video still crosses CPU
memory before texture upload. Those architectural changes need separate runtime
and performance evidence. No live post-fix call or native daemon CPU profile was
recorded for this iteration, and the local-camera-off/remote-reception limitation
documented in [video lifecycle](video-lifecycle.md) remains.
