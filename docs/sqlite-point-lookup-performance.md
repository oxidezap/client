# Point lookups and SQLite LIMIT specialization

The remaining `sqlite3Reprepare` samples under `ChatStore::contact` and
`reactions_for → chat_key_candidates → counterpart_of` came from Diesel's
`.first()`, which emits a bound `LIMIT ?`. On bundled SQLite 3.53.2, rebinding
that limit to 1 specializes the statement again during `sqlite3_step`.
Prepared-statement caching does not avoid that parser work.

`point_lookup::First` appends the literal `LIMIT 1` to the existing typed
query and calls `get_result().optional()`. It delegates bind collection and
cache-safety checks, and its derived query ID includes the inner query's ID.
Only `contact` and the two directions of `counterpart_of` use it. The fixed
limit preserves limit-aware planning; no CAST or global change is involved.
Message page readers retain the separate #283 implementation.

## Reproduction

Base is main `33108ce`, core `af2e018c`, plus the identical synthetic harness
from this change. Head adds the three lookup changes. Both release test
binaries were built sequentially on the same Linux x86_64 cloud machine,
AMD EPYC 9V74, Rust 1.99.1, using the repository's release settings, including
fat LTO and one codegen unit. No production dependencies or SQLite flags changed.

Build each version, copying the executable Cargo reports aside before the
next build:

```sh
cargo test -p oxidezap-chat-store --release --test point_lookup_profile --no-run
python3 scripts/measure-sqlite-point-lookup.py /tmp/point-base /tmp/point-head \
  --iterations 200 --samples 7 --cpu 0 \
  --output docs/measurements/sqlite-point-lookup.csv
```

Each process seeds 48 synthetic contacts and PN/LID mappings, with competing
rows in account 2. Contact batches read 48 present and 48 absent contacts.
LID batches perform 48 empty `reactions_for` reads that exercise the nested
LID→PN lookup. PN batches are an unchanged control: that path obtains all
LIDs without calling `counterpart_of`. Assertions check the result of every
read. Setup and three warmup batches are outside the measured interval.
The runner alternates base/head order and excludes an extra warmup pair.
All children are pinned to CPU 0. Timing and allocation passes are separate.

## Pooled read results

Seven retained paired samples, each containing 200 batches. Ratios compare
head to base within each pair; ranges describe observed variability, not
confidence intervals. The milliseconds columns are separate sample medians.

| Workload | Wall ms base → head | Median paired wall ratio, range | CPU ms base → head | Median paired CPU ratio, range |
| --- | --- | --- | --- | --- |
| Contact, present + absent | 374.55 → 194.59 | 0.520, 0.446–0.595 | 374.54 → 194.57 | 0.519, 0.446–0.595 |
| LID reaction lookup | 325.95 → 275.26 | 0.794, 0.658–1.000 | 325.89 → 275.22 | 0.793, 0.658–1.002 |
| PN reaction control | 205.53 → 212.13 | 1.037, 0.926–1.179 | 205.52 → 212.13 | 1.008, 0.917–1.131 |

Contact CPU fell about 48% and nested LID reaction CPU about 21% in these
synthetic batches. The unchanged control shows noise and no demonstrated
improvement. This includes pool scheduling, query binding, decoding, auxiliary
queries, assertions and result destruction. It does not isolate parser time.
No application startup or end-to-end percentage follows from these results,
and inclusive profiler sample shares cannot be converted into a guaranteed gain.

## Engine mechanism, initial prepare and C memory

Compile the probe against the **matching release archive** and header from
Cargo's resolved `libsqlite3-sys 0.38.2`, not a system SQLite:

```sh
cc -Wall -Wextra -O2 -I <resolved-sqlite-source> \
  scripts/sqlite-point-reprepare.c <release-libsqlite3.a> -lm -lpthread \
  -o /tmp/point-probe-release
taskset -c 0 /tmp/point-probe-release > sqlite-point-reprepare-time.txt
POINT_C_ALLOC=1 taskset -c 0 /tmp/point-probe-release > sqlite-point-reprepare-memory.txt
```

The engine source ID is recorded in both raw files. The probe uses synthetic
contacts and mapping tables with their production key/index order. It compares
logically equivalent bound/fixed-limit queries, including absent keys and the
newest-LID timestamp tie. Its SQL omits Diesel's identifier quoting. It measures
one initial `sqlite3_prepare_v3` separately, then 20,000 bind/step/reset cycles,
half present and half absent. It alternates variant order, retains seven pairs
and excludes pair zero. No pool or Rust decoding is involved.

Every bound variant recorded **20,000 reprepares** via
`SQLITE_STMTSTATUS_REPREPARE`; every literal variant recorded **zero**.
EXPLAIN QUERY PLAN matched for all pairs: contact/LID autoindex lookups;
PN reverse index plus a temporary B-tree for ordering. Matching plans do not
prove identical internal optimizer estimates. Initial prepare medians were
4–11 µs in either variant; no cold-prepare saving is claimed.

The allocation pass wraps SQLite's own malloc/realloc methods before engine
initialization. Counts and requested bytes exclude initial prepare. Reallocation
bytes count the full requested target size, not a net or retained amount.

| Query, per 20,000 cycles | C malloc/realloc calls base → head | Requested C bytes base → head | SQLite live bytes before = after, both | SQLite peak bytes base → head |
| --- | --- | --- | --- | --- |
| Contact | 1,240,000 → 0 | 144,480,000 → 0 | 96,000 | 102,336 → 96,000 |
| LID→PN | 640,000 → 0 | 85,440,000 → 0 | 93,656 | 97,736 → 93,656 |
| PN→LID | 1,060,000 → 180,000 | 518,560,000 → 401,600,000 | 94,384 | 114,464 → 114,464 |

Those byte counters show transient allocation traffic, not a heap reduction of
hundreds of megabytes. PN ordering still allocates. Retained SQLite memory and
statement memory were unchanged. Direct timing-pass CPU medians for the same
20,000 cycles were 126.43→8.30 ms, 81.09→7.42 ms and 166.38→40.23 ms,
respectively. The pooled measurements above determine the integration benefit.

## Rust allocation and process memory limits

The pooled harness wraps `GlobalAlloc`. It records successful alloc/realloc
calls, full requested bytes, requested live Rust bytes and their peak. Live
accounting runs in both timing binaries, adding the same atomic bookkeeping;
allocation traffic/peak counting is enabled only in separate allocation passes.
This counter cannot see SQLite's C allocations, allocator metadata or physical
realloc overlap. Runtime and pool background allocations can also contribute.

Median requested Rust bytes per 200 batches were 26,420,356→26,265,680 for
contacts and 34,354,820→34,274,392 for LID reactions. Counts were effectively
unchanged. The shorter SQL explains the small buffer-size difference.
Requested live/peak Rust bytes and process RSS varied substantially, including
in the unchanged PN control. They do not demonstrate a pooled retained-heap or
RSS reduction. RSS is a working-set snapshot, not a live heap profile.

All raw pooled samples and both native probe runs are in `docs/measurements/`.
No account data, connected session, network traffic, GUI, file-backed I/O or
startup timing is included. Browser/other-engine performance must be remeasured;
the literal limit and typed queries remain portable, with no production FFI.

## Correctness and validation

The tests compare the wrapper against `.first().optional()` and exercise
repeated binds, present/absent keys, optional contact fields, contact labels,
device-suffixed JIDs, account isolation and PN/LID timestamp/LID tie ordering.
Existing alias/reaction/routing regressions remain covered by the full suite.
Locally: 460 chat-store tests passed, seven opt-in tests ignored; all-targets,
all-features chat-store Clippy passed with warnings denied, and formatting passed.
