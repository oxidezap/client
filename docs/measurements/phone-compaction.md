# Immutable phone metadata compaction

Base `4314bf88d584738441c27d71d156f5c437885596`, core `af2e018c`.
Private runtime descriptor lengths, format lists and leading-digit lists use
boxed slices. Public getters, custom database validation, bundled XML, patterns,
regex options and the 100-entry LRU are unchanged. No account/capture data.

The initial compact version `8523c7f` added 1,780 reallocations because collecting
`Result<Box<[_]>, _>` loses exact iterator length, grows a temporary vector, then
shrinks it. The revised constructor reserves the input length once and pushes
checked results. It preserves error propagation without unsafe initialization.
The separate realloc counter confirms 3,020 → 4,800 → 2,892 reallocations for
base, initial compact and revised compact respectively.

Linux x86_64 cloud runner, AMD EPYC 9V45, rustc 1.99.1, unchanged release profile.
Identical probe source per comparison. Each pair alternates base/head order;
one warmup pair is discarded. No builds overlap measurement. Fresh process
means fresh metadata, not flushed OS page caches. CodSpeed was unavailable;
standalone synthetic measurements were authorized.

## Allocation cost

101 fresh-process pairs per mode/case, ordinary CPU affinity:

| Cold format metric | Base | Revised compact |
| --- | ---: | ---: |
| Retained requested heap | 3,153,293 B | 2,700,789 B |
| Peak requested heap delta | 3,225,060 B | 2,772,556 B |
| Allocation + reallocation calls | 61,975 | 61,847 |
| Reallocations within that count | 3,020 | 2,892 |
| Cumulative requested allocation bytes | 13,581,176 B | 13,026,624 B |

Retained and peak heap fall 452,504 B, or 14.35% and 14.03%. Cumulative
allocation traffic falls 554,552 B. Warm allocation counts remain 280,000,
including 40,000 reallocations, per 10,000 calls, with zero additional retention.
Heap counts exclude allocator bookkeeping and fragmentation. CPU pinning reduces
common regex thread-pool storage by 2,304 B in both revisions; the gain is equal.

## CPU and startup limits

Separate probes removed the allocator wrapper entirely and used System directly.
101 fresh pairs for cold-only and 101 pairs for cold + 10,000 warm calls, pinned
to CPU 0. Only the series labelled `timing` in the System CSVs is analyzed here;
`heap` is another uninstrumented series and contains no allocation counters.

| System metric | Base median, IQR | Head median, IQR | Paired median change, 95% bootstrap interval |
| --- | --- | --- | --- |
| Cold call | 4.941 ms, 4.906–5.042 | 4.887 ms, 4.844–4.949 | −1.163%, [−1.491%, −0.884%] |
| Cold-only process CPU | 5.573 ms, 5.524–5.686 | 5.522 ms, 5.463–5.617 | −0.865%, [−1.420%, −0.656%] |
| Cold-only process elapsed | 5.634 ms, 5.581–5.752 | 5.579 ms, 5.519–5.673 | −0.984%, [−1.444%, −0.668%] |
| 10,000 warm calls | 30.663 ms, 30.362–31.029 | 30.630 ms, 30.327–31.077 | +0.101%, [−0.356%, +0.638%] |
| Full process CPU | 36.661 ms, 36.249–37.207 | 36.554 ms, 36.189–37.201 | −0.095%, [−0.479%, +0.562%] |

Bootstrap resamples paired percentage differences 10,000 times, reports percentile
95% intervals for their median, and assumes representative independent pairs.
See `scripts/phone-profile-paired-stats.py`, fixed seed 20261030. No outliers
were removed. Cold-only process elapsed ranges were 5.511–6.391 versus
5.448–8.320 ms; warm ranges 29.710–32.235 versus 29.891–33.507 ms.
Cold-only median RSS was 6,752 → 6,272 KiB. System probe size was
2,782,008 → 2,778,968 B. CPU/elapsed/RSS include process startup and stdout.

The counting-wrapper probe with counters disabled showed a warm slowdown:
+1.478% unpinned, 51 pairs, interval [+0.728%, +1.781%]; +0.597% pinned,
101 pairs, interval [+0.380%, +0.864%]. Its unchanged leave-summary control
slowed about 4%. Pinned A/A, 51 pairs, gave +0.103% warm format difference,
interval [−0.284%, +0.450%]. The warm slowdown did not reproduce with System
directly. This is sensitivity to the probe/binary, not proof of the exact cause.
The System interval still allows a small warm cost up to about 0.64%; overlap
alone is not evidence of equivalence, and no absolute absence of tradeoffs is
claimed. Full application startup, UI responsiveness and real-account savings
remain unmeasured. This PR is a draft for review, not a production acceptance.

## Validation and reproduction

17,982 synthetic calling-code/length cases and 24 representative cases have
byte-identical base/head output. Core all-feature release tests, Clippy,
workspace fmt and wasm audio/core integration check pass. Tests cover invalid
custom regexes, all bundled XML through checked validation, 160 distinct custom
patterns across LRU eviction, order/length preservation and concurrent formatting.

Use the same `phone_profile.rs` at both revisions and preserve each release
executable. Build the C launcher as documented in phone-formatting-performance.md.
Run `scripts/measure-phone-profile.py` with `--samples 101 --cold-only` for
cold-only processes, or without `--cold-only` for warm runs. `taskset -c 0`
pins the driver and all children. CSVs `phone-compaction-cold.csv`,
`phone-compaction.csv`, `phone-compaction-pinned.csv`, `phone-compaction-aa.csv`
and `phone-compaction-system-{cold,warm}.csv` retain all observations.
For the System variant, remove the allocator imports/definitions before
`fn sample`, the counter reads/reset before `let start`, the counter reads and
output after `let elapsed` and the TRACK toggle in main. Keep all fixture and
loop code, both output tags/counts/ns, `use wacore::time::Instant`, and replace
counter output with `println!("{tag} n={n} ns={}", elapsed.as_nanos())`.
Compare corpora using `--equivalence` and `--representative` on both binaries.
