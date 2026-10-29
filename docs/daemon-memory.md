# Daemon memory measurements

The harness uses fresh Linux processes, the existing release profile, `/proc`
and `wait4`. No root, eBPF, CodSpeed, credentials or application dependencies are
needed. Build immutable binaries on the baseline revision, then the candidate,
using the same toolchain, runner and profile:

```sh
python3 scripts/build-memory-workload.py --instrument /tmp/base-memory
python3 scripts/build-memory-workload.py /tmp/base-timing
# Switch to the candidate revision, keeping the same harness.
python3 scripts/build-memory-workload.py --instrument /tmp/head-memory
python3 scripts/build-memory-workload.py /tmp/head-timing
python3 scripts/measure-daemon-memory.py --mode memory --base /tmp/base-memory --head /tmp/head-memory --output /tmp/memory.json
python3 scripts/measure-daemon-memory.py --mode timing --base /tmp/base-timing --head /tmp/head-timing --output /tmp/timing.json
```

The driver alternates base/head order, excludes one warmup per scenario and
reports seven isolated samples with median/min/max and raw observations. Each
binary has a recorded SHA-256; build metadata records the compiler and revision.
The OS file cache is warm, not forcibly cleared. Do not compare different hosts.

The bridge workloads replay 75,703 fixed-size synthetic text messages over 1,032
chats, after a 100-chat attach with eight read rows per chat. Cases cover unread
traffic with/without a subscriber, already-read messages, and groups with eight
alternating authors. The attached subscriber deliberately lags until the publisher
drains. Assertions cover duplicate delivery, badges, receipt queries and the
publication tail. `account_scale_paging` separately checks 1,653 contacts and
75,703 SQLite rows; it is not part of these bridge memory numbers.

The opt-in `memory-profiling` feature installs a counting allocator in native
test binaries only. It measures requested Rust heap bytes, live and peak, and
allocation traffic across all threads. It excludes allocator metadata, C
allocations and alignment slack. Checkpoints separate initialization, attach,
queued sync, drained publication, detach, receipt queries and released state.
Sampling and formatting allocate small amounts themselves. `realloc` traffic
counts its requested new size, not the unknown amount copied by the allocator.

RSS/PSS, anonymous/file/shared resident pages and the kernel's RSS high-water mark
are reported separately. A fall in live heap without an equal RSS fall can include
allocator retention, stacks and untracked allocations; it does not establish an
exact allocator-retained byte count. File-backed pages are not live Rust heap.
Timing binaries have no counting allocator or memory probes; CPU/wall values
include process/test startup and are reported as distributions, not universal
latency guarantees.

These workloads isolate bridge bookkeeping and publication. They do not represent
a whole authenticated daemon, cryptographic state, SQLite residency, media or
calls. Whole-daemon startup/idle claims require a separate scenario. Production
binaries keep their existing allocator even when the profiling feature is enabled.

## Read-tracker compaction (2026-10-29)

Compared harness baseline `1ccaaf3` with production change `9086fcc`, using
Rust 1.99.1, the repository release profile, Linux 6.18.55, AMD EPYC 9V74 and
the same five-CPU affinity. Immutable IDs and sender strings now use `Box<str>`
instead of retaining unused `String` capacity fields. Cache limits, receipt
semantics and public types are unchanged. Draining receipts transfers the
buffers back to strings without copying. No runtime dependency was added.

Seven paired samples measured heap; fifteen paired uninstrumented samples
measured peak RSS and CPU/wall time. Each excluded one warmup per variant and
scenario. Equal-length executable paths kept startup allocations identical.

| Scenario | Live heap after drain, base → head (MiB) | Peak RSS, base → head (KiB; median [min–max]) |
| --- | --- | --- |
| headless | 17.30 → 14.18 | 32340 [32156–32532] → 28828 [28792–29064] |
| attached | 17.80 → 14.68 | 35204 [34916–35328] → 31468 [29880–31852] |
| groups | 17.28 → 14.16 | 33068 [31504–33132] → 29500 [29260–29636] |
| read | 6.92 → 5.82 | 18844 [17304–19200] → 17552 [16016–17864] |

Unread scenarios save exactly 3,269,376 requested live bytes (3.12 MiB) in
every sample: about 18% after drain and 24% after receipt queries. Already-read
traffic saves 1,155,840 bytes (16%). Startup and released live bytes match the
baseline. Anonymous PSS after drain falls by approximately 3.1 MiB for unread
traffic and 1.1 MiB for read traffic; resident pages can remain after objects
are freed, so these are not exact allocator-retention measurements.

| Scenario | CPU seconds, base → head (median [min–max]) | Wall seconds, base → head (median [min–max]) |
| --- | --- | --- |
| headless | 0.365 [0.347–0.453] → 0.358 [0.334–0.403] | 0.366 [0.347–0.451] → 0.358 [0.335–0.403] |
| attached | 0.929 [0.881–1.266] → 0.918 [0.875–1.237] | 0.751 [0.712–0.987] → 0.744 [0.704–0.963] |
| groups | 0.394 [0.367–0.454] → 0.386 [0.354–0.537] | 0.394 [0.367–0.454] → 0.386 [0.354–0.537] |
| read | 0.313 [0.292–0.347] → 0.308 [0.293–0.339] | 0.313 [0.292–0.347] → 0.308 [0.293–0.339] |

The distributions overlap. A preceding independent paired run also reduced
RSS, but CPU/wall medians varied slightly in both directions. This establishes
a memory reduction in these workloads, not a universal latency improvement or
a measured reduction for a complete connected account.
