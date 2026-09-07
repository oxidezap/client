# Video upgrade follow-up

The September 7 production logs exposed disagreement between negotiated video,
the session's camera owner, and the flags used to admit frames in the GUI.
Raw logs and captures remain private.

## Evidence and fixes

In one outgoing call, a local upgrade timed out and the library released its
video endpoints. The camera kept encoding until a later user action stopped it,
reporting 622 chunks and 527 dropped outputs over that camera lifetime. In a
second call, peer `Disabled` closed the endpoints after an accepted upgrade,
but the camera remained registered and reported 360 chunks with 352 drops.

The local pump now observes endpoint closure even if no next camera frame arrives.
It closes capture admission and schedules cleanup for that camera's identity.
Registry cleanup releases the owner, clears its pending upgrade, and corrects
both direction flags. Endpoint teardown does not send an extra stop stanza;
physical capture failure retains its existing stop announcement. A late event
or callback from camera A cannot retire replacement camera B.

Remote `Enabled` also arrived before call acceptance in the recording. The
reducer discarded it while ringing, then activated the call with remote video
off. The reducer now retains this fact for the exact ringing call and consumes
it on activation. Replacements and call removal clear it.

Upgrade acceptance and simultaneous requests now project the locked library's
resulting enabled state without waiting for an additional peer `Enabled` stanza.
The local camera is not announced as sending while its upgrade is unanswered.
These changes use the existing library state machine, with contract tests against
the actual reducer. They add no negotiation stanzas.

Browser tests reproduced two more resource leaks. A cancelled camera acquisition
could abandon a stream returned later by `getUserMedia`; cancellation during
preview playback could leave an attached element holding the stream. Both now
have owners that stop tracks and detach the preview when the pending operation
is dropped. Tests use real browser canvas tracks and controlled promises, not
hardware or private media.

## Incoming call log

The separate incoming-call recording contains an offer receipt, ringing UI
events, and `accepted_elsewhere` two peer-timestamp seconds after the offer.
It contains no outgoing acceptance, camera acquisition, or media startup.
An offer receipt is not an answer. This recording cannot establish that the web
client answered and then lost the call UI, nor identify which device answered.
GPU resize warnings are present but do not establish a rendering failure.

## Native CPU measurement

No native active-call CPU profile was supplied. Synthetic measurements put MJPEG
decode ahead of H.264 encoding for the tested generated 720p workload, but that
does not establish the bottleneck on a user's camera. A device offering 30 FPS
can satisfy the closest-mode request for 20 FPS by selecting 30. Silently dropping
frames would also change the capture/transport timing contract.

The native camera now logs its requested and negotiated mode and, when camera
debug logging is enabled, reports aggregate stage timings at frame boundaries
after five seconds and at shutdown. Capture time includes blocking sensor wait;
conversion and encoding time also include scheduling delays. These are elapsed
times, not CPU usage. Debug-disabled capture adds no diagnostic clock reads.
No format, FPS, bitrate, or codec-quality setting was changed.

Before a test call, stop the existing daemon and rebuild/restart it with symbols.
Do not restart it during the call or start a second daemon.

```sh
CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_DEBUG=line-tables-only cargo build --release --bin oxidezapd
RUST_LOG=info,oxidezap_video::camera=debug ./target/release/oxidezapd
```

On Linux, from another terminal during the call, record the verified daemon PID
for 30 seconds. Keep the profile on disk, not in `/tmp`.

```bash
mkdir -p .cache/native-video-cpu
pgrep -a -x oxidezapd
pids=( $(pgrep -x oxidezapd) )
if [ "${#pids[@]}" -eq 1 ]; then
    perf record -e cpu-clock:u -F 99 --call-graph dwarf,8192 \
        -p "${pids[0]}" -o .cache/native-video-cpu/native-call.data -- sleep 30 &&
    perf report --stdio --no-children --sort comm,dso,symbol \
        -i .cache/native-video-cpu/native-call.data
fi
```

The sampling tools and permissions were tested against a synthetic process, not
a live call. Perf stacks and existing daemon logs can contain private data.
Do not commit or publish those artifacts unreviewed.

## Still unresolved

Local camera-off still releases remote reception in the locked library and the
client's paired endpoint owner. The UI's independent flags do not repair that
transport limitation. Static captured-WASM evidence distinguishes send-only
camera stop from full downgrade, but the executed oracle currently covers only
the ringing state. [whatsapp-rust#1465](https://github.com/oxidezap/whatsapp-rust/pull/1465)
repairs the oracle host needed to investigate it; it is not a production
directional-stop fix and is not a client dependency update.

Established-call stop/resume, the physical camera LED after hangup, live browser
rendering after the GPUI migration, and sustained native CPU still need real-call
verification. Passing track-lifecycle tests is not a measurement of a hardware
camera or proof of improved call FPS.
