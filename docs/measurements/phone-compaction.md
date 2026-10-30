# Immutable phone metadata compaction

Base `4314bf88d584738441c27d71d156f5c437885596`, core `af2e018c`.
Only private runtime descriptor lengths, format lists and leading-digit lists
change from `Vec<T>` to `Box<[T]>`. Public getters, loaded/custom database
validation, bundled XML, pattern strings, regex options and the 100-entry LRU
are unchanged. No account or capture data was used.

Linux x86_64 cloud runner, AMD EPYC 9V45, rustc 1.99.1, unchanged repository
release profile. The existing `phone_profile` allocator probe and C wait4
launcher were identical in both builds. 25 alternating pairs per case/mode,
one discarded warmup pair, fresh processes, no builds during measurement.
CodSpeed CLI was unavailable; standalone measurements were authorized for this
investigation. Raw observations are in `phone-compaction.csv`.

| Format probe metric, median | Base | Head |
| --- | ---: | ---: |
| Cold retained requested heap | 3,153,293 B | 2,700,789 B |
| Cold peak requested heap delta | 3,225,060 B | 2,772,556 B |
| Cold allocation/reallocation calls | 61,975 | 63,755 |
| Cold cumulative requested bytes | 13,581,176 B | 13,739,552 B |
| Warm allocations, 10,000 calls | 280,000 | 280,000 |
| Warm retained heap | 0 B | 0 B |
| Cold call, counters disabled | 5.229 ms | 5.246 ms |
| Warm 10,000 calls, counters disabled | 31.110 ms | 31.195 ms |
| Process CPU, counters disabled | 37.222 ms | 37.254 ms |
| Process elapsed, counters disabled | 37.300 ms | 37.359 ms |
| Process max RSS, counters disabled | 6,792 KiB | 6,340 KiB |
| Stripped probe executable | 2,822,552 B | 2,820,840 B |

Retained and peak requested heap each fall 452,504 B, or 14.35% and 14.03%
respectively. Exact-sized lists require 1,780 extra cold reallocations and
158,376 B more cumulative allocation traffic. This is initialization work,
not extra regex recompilation. Warm allocation behavior is identical.

Timing ranges overlap: cold 5.077–5.668 versus 5.011–6.049 ms; warm
30.543–32.799 versus 30.697–32.472 ms. Median process CPU increases 0.09%.
No CPU or startup improvement is claimed, and these samples cannot exclude a
small regression. The `left` control initializes no phone metadata and retains
16 B in both revisions. RSS includes allocator effects; requested heap excludes
allocator bookkeeping and fragmentation. CPU/RSS include process startup and
both loops. This is a formatter probe, not a UI responsiveness measurement or
an estimate of total client savings. Full regex caches still dominate after
more regions are used.

Validation: 17,982 synthetic calling-code/length cases and 24 representative
cases produce byte-identical base/head output. Core all-feature release tests
pass, including all bundled XML through checked/custom validation and invalid
regex fault injection. Added tests exercise 160 distinct custom patterns across
LRU eviction, descriptor lengths, format ordering and concurrent multiregional
formatting. No LRU reduction or regional subset is involved.

Reproduce with the unchanged harness on both revisions:

```sh
cargo build --release --locked -p oxidezap-core --example phone_profile
# Preserve each executable as /tmp/phone-base and /tmp/phone-head.
cc -O2 -Wall -Wextra -Werror scripts/phone-profile-usage.c -o /tmp/phone-usage
python3 scripts/measure-phone-profile.py /tmp/phone-usage /tmp/phone-base /tmp/phone-head --samples 25 --output /tmp/phone-compaction.csv
/tmp/phone-base --equivalence > /tmp/base-corpus
/tmp/phone-head --equivalence > /tmp/head-corpus
cmp /tmp/base-corpus /tmp/head-corpus
```
