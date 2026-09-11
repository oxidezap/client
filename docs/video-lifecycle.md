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

Re-enabling our video into a call where the peer's direction is still active
cannot complete as an upgrade: the reducer ignores an upgrade request against
an already-active direction, so no accept ever comes, the send-gated plane
admits nothing (the relay counters freeze), and the library's five-second
timeout cancels with `UpgradeCancelByTimeout` — which the peer applies by
tearing its own direction down too, collapsing the whole call. The registry
therefore watches each upgrade it initiates and withdraws it at four seconds
when unanswered: a `Stopped` the peer applies without touching its direction,
clearing the library's pending request so its timeout finds nothing to cancel
and a later retry can begin. Fenced on the armed camera and settled through
the newest intent, so an answer or a newer attempt disarms it.

Browser tests reproduced two more resource leaks. A cancelled camera acquisition
could abandon a stream returned later by `getUserMedia`; cancellation during
preview playback could leave an attached element holding the stream. Both now
have owners that stop tracks and detach the preview when the pending operation
is dropped. Tests use real browser canvas tracks and controlled promises, not
hardware or private media.

## Outgoing acceptance

The compared incoming and outgoing calls used the same preview bundle. In the
outgoing failure, 166 remote access units reached the session and were published
toward the GUI, but no remote decoder started. Acceptance only announced the
local camera, leaving remote frame admission off without a later `Enabled`.
The working incoming-call path explicitly announced both directions.

Outgoing acceptance waits for the startups already in progress, then reads the
registered `CallHandle`. It does not cache accepts under arbitrary call IDs.
Registration, failure, cancellation and task drop wake the waiters. Each waiter
retains startup stamps, so a redial cannot extend an old event's wait. There is
no timer that silently discards a valid early accept.

The library resolves the original PN target while placing the call. The client
reads the handle's immutable `initial_peer_jid()` afterward, without a second
cache lookup that could fail after the offer is sent. Cache eviction cannot
terminate the placement or substitute a different target. Accepted media must name that target user, the
handle's selected device and its creator. All direct-call video states and
upgrade tokens come from `CallEvent::PeerVideoStateChanged` on the handle's
ordered queue. Global `IncomingCall::VideoState` events do not update video.
The legacy `VideoStateChanged` companion is ignored, including when it carries
a token. There is no fallback that invents a sender from the selected handle.

Before activation, the watcher retains one complete source-bearing operation
within the registered call and waits. The bounded handle queue holds subsequent
operations. Acceptance replays the retained operation only for the winner and
shares the watcher's per-handle processing lock, then releases the watcher.
This preserves `UpgradeAccept` followed by `Stopped`, and request followed by
`Stopped`, without coalescing away a token or negotiation outcome. A losing
sibling's `Stopped` cannot turn off the winner's picture. Group participant
events remain roster-owned and are not fed into the 1:1 negotiation reducer.
Orientation remains on the library's media-plane path.

A raw-node lease preserves the accept's video-child presence, including accepts
with missing or invalid orientation. Raw accepts and parsed call events share a
lane. Advertisement metadata is retained only for a registered outgoing call
and consumed by the corresponding parsed accept. Audio-only acceptance does
not imply remote-on, and frame arrival does not grant video permission.

Native tests use the upstream `CallFixture` to complete Noise XX and login,
block the real offer send, and obtain real dormant handles from the public call
builder. Injected stanzas pass through the library parser and handlers. The
tests then exercise the client handler and daemon reducer through serialized
`CallsChanged` messages. They do not set the winning device or fabricate readiness.
Separate session-free GUI tests exercise `Frames` with generated H.264, including
an IDR rejected before acceptance and a subsequent recovery request. Native GUI
tests do not depend on the daemon or session. These are producer and consumer
contract tests, not one cross-crate fixture inside the front end.
Ordering tests hold the watcher while the global lane drains, reverse that
scheduling before activation, and fill the real bounded queue before injecting
the final transitions. They require remote-off and no pending peer request
after draining. A fault-injection case discards the source-bearing operation
and leaves only its real legacy companion; the watcher ignores it and exits
when the queue closes. Legacy-only custom producers cannot drive video state
and must migrate to the source-bearing event. This does not promise recovery
of an operation evicted from the bounded queue.

Session's opt-in `test-support` feature forwards to the upstream fixture feature.
The fixture is native-only, remains outside default production builds, and needs
no standalone wrapper workspace. Web tests separately exercise raw advertisement
parsing, source-event/token extraction, lane identity and registration waits;
shared-memory WebCodecs tests exercise the real browser codecs.

Stanza injection bypasses inbound Noise framing. No media relay or Android
decoder runs in this fixture. Upstream still permits an unrung first winner
and lets a later sibling reject end the call unless it is per-device
(`busy`, or `enc` for a device that could not decrypt the offer) — and our
own event arm matches that rule rather than hanging up on one stale
device. Matching application metadata does not repair those policies. Neither a standalone post-accept
video announcement nor a caller-side upgrade re-request goes out on a video-from-start accept:
production retests kept failing with the announce in place, and the re-request API only retries an
outstanding local upgrade, which such a call never has open. The fixture asserts zero of either on
live-camera accepts. Android reception of this client's outbound video still needs a live retest,
reading the direction matrix, the PLI signature, and Android's `state="1"` (transaction-ids, `dec`,
repeats) there.

Browser relay summaries now count successful `RTCDataChannel.send` calls
separately from congestion drops, non-open channels and send exceptions.
Outbound payload types are recorded only after successful browser admission.
Video byte counts, admitted marker packets and IDR-marked marker packets help
locate the remaining outgoing failure; they do not prove complete access units,
relay delivery or Android receipt. Each transport uses a local ordinal rather
than logging keys, account identifiers or media contents.

Counters are fixed-size and cumulative. Debug summaries occur at most once per
five seconds of sampled activity and once at teardown. Existing congestion and
send-error warnings remain visible without debug logging. Production send-path
tests verify accounting with a patched browser channel in ordinary and shared
WASM memory, not a real network connection.

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

The subsequent Android-initiated production retest confirmed correct front/rear
rotation and camera LED shutdown after hangup on that setup. Established-call
stop/resume, outbound video presentation on Android and sustained native CPU
still need further verification. Passing track-lifecycle tests does not establish
hardware behavior on every device or improved call FPS.
