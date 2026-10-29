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
