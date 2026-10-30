# Sharing media within a GUI frame

A real-laptop startup profile attributed 11,286,251 transient allocated bytes to
`Frames::apply -> load_media -> fill -> Directory::read -> std::fs::read`, with
no conversation selected. That profile identified a path, not a measured patch
gain. No account data, database or raw profile was copied into this experiment.

The native `MediaCache::read` returns a new `Arc<Vec<u8>>` around each file read.
Consequently, repeating a content-addressed key in a history load used to read
and retain a new copy for every message. The web cache already shares those
bytes. `fill_many` now holds successful reads while filling one history,
message page or chat page, sharing them across matching references. Missing
reads are retried on later references. The map is dropped before publication;
there is no persistent cache, account namespace change, eviction or asynchronous
work. Existing per-message full/preview MIME adoption remains in `fill_with`.

This does **not** defer distinct media until conversation selection. A distinct
file still loads eagerly. Removing those reads safely needs selection-aware
hydration for open conversations, Status, offline reading and previews. Merely
removing `load_media` would regress those paths. The sanitized profile does not
establish how many references were duplicates, so these results cannot explain
or claim elimination of the laptop's entire 11.3 MB peak.

## Synthetic reproduction

From the repository root, with Rust and a C compiler on PATH:

```sh
python3 scripts/measure-frame-media.py e6e3020 WORKTREE > comparison.csv
```

The script extracts the production `fill` helpers verbatim from each ref and
compiles them using `rustc --edition=2024 -O`. Both variants use identical
minimal domain/trait shims and real `std::fs::read`. The shims implement the
members used by the helpers; they do **not** exercise the actual core adoption
method, deserialization, GUI state machine, rendering, decoding, network,
service worker, scheduling or the connected daemon. Actual GUI tests separately
check the adoption method. This isolated measurement is not an application
startup benchmark. No Cargo manifests, locks or productive dependencies change.

Each operation fills 12 empty synthetic descriptors with references to 1, 3 or
12 distinct files, each 1 MiB. A separate 12-distinct 128-byte case exposes
bookkeeping overhead. Setup and file creation are outside the timed section;
file contents are already in the OS page cache. There is one excluded warmup
pair and nine retained process pairs per scenario/mode, alternating order.
The entire experiment was repeated. The two CSV files retain all 288 measured
observations. The initial baseline-versus-itself run preceded application edits.

Timing and heap accounting run in separate fresh processes. A test allocator
wrapper remains installed during timing but its counters are disabled, leaving
an atomic flag check per allocation/deallocation/reallocation. Heap runs count
allocation/reallocation calls and additional peak **requested** live bytes,
relative to descriptor setup. Deallocations of setup objects reduce that live
baseline. This is not allocator-resident memory. The retained-payload column
counts distinct `Arc` identities in the result, outside the interval.

A tiny C launcher forks after exec to avoid Python's inherited pre-exec RSS
floor. `wait4` reports Rust-process peak RSS and user/system CPU, including
startup, assertions, output and destruction. CPU values for this small workload
are coarse and sometimes charge almost everything to either user or system
time. They are not the isolated fill-loop CPU time. No CPU percentage or
application-wide RSS gain is claimed. No eBPF, CodSpeed, root, account or
credentials are used. The user explicitly authorized alternate measurement.

Runner: Linux x86_64, rustc 1.99.1. Isolated executable uses release optimization
`-O`, no debug info or LTO. These flags are identical for base/head, but differ
from the laptop's fat-LTO/frame-pointer profile build.

## Results and costs

First experiment medians, timing with heap counters disabled:

| Distinct files / size | Loop base -> head | Bytes read base -> head | Retained payload base -> head | Additional peak requested heap base -> head | Allocation calls base -> head |
| --- | --- | --- | --- | --- | --- |
| 12 / 1 MiB | 5.002 -> 4.906 ms | 12 -> 12 MiB | 12 -> 12 MiB | 12,582,950 -> 12,583,506 B | 48 -> 63 |
| 1 / 1 MiB | 3.690 -> 0.303 ms | 12 -> 1 MiB | 12 -> 1 MiB | 12,582,951 -> 1,048,765 B | 48 -> 6 |
| 3 / 1 MiB | 3.784 -> 0.921 ms | 12 -> 3 MiB | 12 -> 3 MiB | 12,582,951 -> 3,145,926 B | 48 -> 16 |
| 12 / 128 B | 18.3 -> 28.6 us | 1,536 -> 1,536 B | 1,536 -> 1,536 B | 1,574 -> 2,130 B | 48 -> 63 |

Repeat medians were 4.375 -> 4.242 ms, 3.597 -> 0.282 ms,
3.416 -> 0.943 ms, and 17.1 -> 27.4 us respectively. No speedup is claimed
for unique files. Small unique payloads pay about 10 us per 12-row operation,
15 additional allocation calls and 556 additional peak requested bytes.
The successful-read table has one entry per distinct key for the duration of
the frame. It retains the same payloads the output already owns and adds key,
map and reference-count overhead. The tradeoff is deliberate: large repeated
attachments avoid megabytes of duplicate file reads and retained bytes.

First-run median complete-process peak RSS was 13,344 -> 13,320 KiB for unique
1 MiB files, 13,328 -> 2,056 KiB for one repeated file, 13,324 -> 4,112 KiB
for three repeated files, and 912 -> 1,032 KiB for tiny unique files.
Median process user+system CPU, calculated per observation before taking the
median, and full ranges remain derivable from the raw CSV. The tiny-case CPU
distributions overlap; these process metrics include the exclusions above.

## Correctness and outstanding work

Focused tests cover shared pointer identity, preview MIME/flags, no reuse in a
later frame, changing cache contents and retry after a failed read. The existing
wire-preview test exercises full adoption and original-MIME restoration.
Pending downloads and discarded requests keep their existing checks before
`fill_many`; explicit downloads, staging, cancellation and persistent caches
are unchanged. A frame never mixes accounts, and no table survives the frame.

Native GUI tests passed, 589 passed and 7 measurement tests ignored. Scoped
all-target/all-feature GUI Clippy and workspace formatting also passed.
Web/platform CI and Codex review must be checked on the final head. No real
account interaction or UI/browser end-to-end performance is measured here.
