# Native media hydration on demand

The real startup profile identified 11,286,251 transient allocated bytes in
`Frames::apply -> load_media -> fill -> Directory::read -> std::fs::read` while
no conversation was selected. Per-frame deduplication in #280 addresses repeated
references. This change addresses distinct attachments as well: native message
frames retain cache references, and the UI reads a payload only for its visible
owner. It does not claim elimination of the measured laptop peak without a new
real-account measurement. No real profile, database or account data was used.

## Ownership and limits

Only the native directory cache opts in. Web, embedded and follower-tab caches
retain eager frame hydration. Avatar readiness and explicit download responses
also retain their existing paths. There is no wire, daemon, store or dependency
change.

The virtual timeline reports actual child bounds after prepaint. Intersecting
those bounds with the list viewport excludes overdraw and works with GPUI's
bottom-follow layout. The public logical scroll offset alone cannot identify
that initial bottom viewport, because bottom-follow is represented by a sentinel
at the end of the list. The report is deferred until the element releases its
read of the application entity. Status and the fullscreen viewer identify their
one displayed row separately. A selection hidden by Settings or another surface
owns no new cache reads. Visibility means the application's displayed surface;
OS-level window occlusion is not tracked.

The controller holds at most 64 demanded descriptors and one read slot. A
cancelled read retains that slot until its background task completes, so rapid
navigation or account changes cannot accumulate reads in parallel. A cancellation
flag is checked before file I/O. An already-running filesystem read may finish;
its result is discarded after the owner leaves. A later identical owner cannot
revive a cancelled request.

A local read accepts at most 48 MiB, matching the existing web media budget.
Metadata is checked before allocation, then a fixed-size buffer and an extra-byte
probe reject files that grow during the read. The buffer cannot grow past its
admitted length. Oversized or missing files remain references; no upstream
WhatsApp fetch occurs merely to replace an automatic cache read. The byte bound
is for one read, not a new bound on the application's total retained or decoded
heap. Existing retained-media policy still applies after adoption. Explicit
user downloads keep their existing limits and behavior.

Account identity and the existing reset generation travel with each target.
Before adoption the controller checks those identities, current owner, message
content/edit/revoke state and the entire media descriptor, including its key.
A newer explicit download therefore wins. Each recipient of a repeated visible
key adopts its own full/preview metadata while sharing one `Arc<Vec<u8>>`.
Inline previews without a `downloadable` remain usable offline. Failed reads
are suppressed while the row remains in that viewport, and leaving/returning
allows another attempt. This is not a persistent negative cache.

Status waits for the cache-only attempt before resuming its existing full-media
fetch/playback behavior. Automatic channel-preview improvements also wait for
the local attempt. Staging, downloads explicitly initiated by a user and their
cancellation rules are separate from this ownership controller.

## Reproduction and measurement limits

```sh
python3 scripts/measure-demand-media.py 6253f38 WORKTREE > comparison.csv
cargo test -p oxidezap-gui --all-features media_materialization
cargo test -p oxidezap-gui --all-features deferred_frames
cargo test -p oxidezap-gui --all-features deferred_file_reads
```

The performance probe compares #280's production fill helpers with the deferred
helpers on the same Linux x86_64 runner, rustc 1.99.1, `rustc --edition=2024 -O`,
without LTO or debug info. It extracts the helpers verbatim and provides identical
minimal domain/trait shims. Both variants use the same new bounded-file-read
primitive for requested rows; the eager base has already read every row, so it
never needs that second step. Twelve files have distinct fictitious references,
with payload sizes of 1 MiB or 128 bytes. The requested subset is 0, 2 or 12 rows.
File creation and descriptor setup precede timing, and data is page-cache warm.

There are eleven alternating process pairs per scenario and measurement mode,
plus one excluded warmup pair. The entire experiment was repeated; both CSVs
retain 528 observations. Timing and allocation accounting run separately. The
same allocator wrapper remains installed with accounting disabled during timing,
adding an atomic flag check per allocator call. The heap column reports peak
additional requested live bytes relative to descriptor setup, not allocator RSS.

The tiny C parent from #280 forks after exec to avoid Python's inherited RSS
floor. `wait4` reports whole-child user/system CPU and peak RSS, including startup,
assertions, output and teardown. The stopwatch covers fill and requested local
file reads. It excludes the application controller, task dispatch, actual domain
adoption, layout, decoder, IPC, daemon, network, account startup and OS window
visibility. Read counters and retained payload bytes have independent checks;
these are isolated I/O primitive measurements, not application benchmarks.

The no-demand head loop is optimized away because its cache capability is known
in the shim. Its approximately 80 ns raw result is timer overhead and is **not**
a GUI frame or startup time. No timing ratio is claimed for that case. The
meaningful result is zero payload reads and zero payload allocation in this
path. Real production-frame and headless-render tests independently check this
behavior. No CodSpeed, eBPF, root or credentials are used, as authorized.

## Results

First experiment, medians for twelve distinct 1 MiB attachments:

| Requested rows | Payload reads base -> head | Retained payload base -> head | Additional peak requested heap base -> head | Allocation calls base -> head |
| --- | --- | --- | --- | --- |
| 0 | 12 -> 0 | 12 -> 0 MiB | 12,583,508 -> 0 B | 63 -> 0 |
| 2 | 12 -> 2 | 12 -> 2 MiB | 12,583,508 -> 2,097,203 B | 63 -> 8 |
| 12 | 12 -> 12 | 12 -> 12 MiB | 12,583,508 -> 12,582,952 B | 63 -> 48 |

For two requested rows, primitive-loop medians were 5.742 -> 0.711 ms in the
first experiment and 6.374 -> 0.853 ms in the repeat. With all twelve requested,
results were 5.439 -> 5.240 ms and 6.148 -> 6.231 ms. The all-requested distributions
overlap and no speedup is claimed for that case. This change avoids work before
it is requested; it does not make reading all attachments inherently faster.

First-run median whole-child user+system CPU was 6.750 -> 0.876 ms with no demand,
7.094 -> 1.692 ms with two requested rows, and 6.684 -> 6.600 ms with all twelve.
Median peak RSS was 13,344 -> 1,052 KiB, 13,328 -> 3,092 KiB and
13,340 -> 13,332 KiB respectively. These are process metrics for the isolated
probe, with the exclusions above, not whole-GUI savings.

The tiny-file workloads expose constant overhead: two requested rows measured
37.1 -> 7.8 us and all twelve 36.4 -> 22.3 us in the first experiment. These
also exclude scheduling/controller overhead. Keeping every attachment visible
retains the same 1,536 payload bytes, regardless of its loading policy.

## Qualification

Tests exercise actual bottom-aligned GUI layout, scroll to earlier rows, zero
reads without selection, offline previews without download descriptors, full
MIME adoption, same-key sharing, stale account/edited/revoked/replaced results,
a racing explicit download, failure suppression and the read-slot/demand limits.
A separate filesystem test checks exact bytes, byte ceilings, empty files,
missing files and unusable keys. Native production-frame tests preserve keys
without attempting any cache read.

The viewer's album membership currently requires materialized bytes; including
full deferred references in that predicate needs a coordinated follow-up hunk
before this branch is ready for a PR. No viewer code has been changed yet.
Remote platform/web CI and review remain required. #280 is preserved separately.
