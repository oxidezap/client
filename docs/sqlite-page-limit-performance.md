# Message page limits and SQLite specialization

Bundled SQLite 3.53.2 recompiles `SELECT ... LIMIT ?` during `sqlite3_step`
after the limit is bound. Reusing the statement and rebinding the same integer
still pays for specialization. The page readers issue these statements while
filling an attach page, fetching a timeline and refilling after alias copies
collapse. A statement cache alone does not remove this work.

`PageLimit` appends `LIMIT CAST(? AS INTEGER)` to the existing Diesel query.
The value remains a `BigInt` bind carrying the original `i64`; the SQL text
does not vary with the limit. The wrapper delegates the inner query's bind
collection and cache-safety checks. It has no static query ID, matching the
boxed page queries it wraps. The two call sites are `fill_unique` and
`fill_unique_after`. Their filters, cursor bounds, ordering, identity folding
and quote hydration are the implementation inherited from #278.

CAST adds runtime work and prevents limit-sensitive planning. Equality of the
chosen plan does not prove equality of the optimizer's internal estimates.
This tradeoff is restricted to the timestamp-indexed message pages, with
measurement required when the engine or these queries change. The regular
tests compare the wrapper against Diesel's integer limit across negative,
zero, positive, `i32` boundary and `i64` boundary values, in both directions.
Application entry points continue to normalize negative budgets to zero.

## Original reproduction

The initial study predates the merge of #278. Its base is
`a5c7c3e9d968c455bbba572c0cea2ccda3adfafd` from #278 plus the
same profiling harness from `92ecea4` in both binaries. The implementation
change is `1569e95`; `92ecea4` also selects portable profiling counters.
Build both sequentially on the same runner using the ordinary Cargo cache:

```sh
cargo test -p oxidezap-chat-store --release --all-features \
  --test page_limit_profile --no-run
```

Copy the test executable Cargo reports aside after each build, then run:

```sh
python3 scripts/measure-sqlite-page-limit.py /tmp/page-limit-base \
  /tmp/page-limit-head --iterations 32 --samples 7 \
  --output docs/measurements/sqlite-page-limit.csv
```

`scripts/sqlite-limit-reprepare.c` independently checks the engine mechanism.
Compile it against the header and **matching release archive** from the
resolved `libsqlite3-sys` build, rather than the system SQLite:

```sh
cc -O2 -I <resolved-sqlite-source> scripts/sqlite-limit-reprepare.c \
  <matching-release-libsqlite3.a> -lm -lpthread -o /tmp/limit-reprepare
/tmp/limit-reprepare
```

On SQLite 3.53.2 it prints five reprepare counts of `1` for the bare bind and
five counts of `0` for CAST. This is a counter probe on an empty synthetic
table, with no timing claim.

## What the pooled workload measures

Each process seeds 24 chats with 80 messages each, tied timestamps, a quoted
reply every tenth message and one reaction every tenth message. It then adds
proven PN/LID mappings and legacy alias copies after writer reconciliation,
plus competing rows in a second account. Reads assert logical page sizes,
quote restoration, reaction counts and exclusion of the other account.
Three warmup batches and all setup are outside the measured interval.

The cases cover an attach page of eight messages, backward and forward pages
of fifty, a batched history load of fifty followed by reaction reads, and
reaction reads alone as an unchanged control. Each case uses fresh processes
for base and head, alternating their order. The runner excludes an additional
warmup pair and retains seven paired samples for each measurement mode.

Wall time uses the monotonic clock. CPU uses Linux process CPU time across
the runtime and pool threads. Allocation passes count Rust allocation and
reallocation requests and requested bytes with `GlobalAlloc`; SQLite's C
allocations are outside that counter. Bytes sum full request sizes, including
reallocation target sizes, and do not represent retained heap. These passes
are separate from timing.
The disabled allocator wrapper remains in both timed binaries, adding one
branch per Rust allocation. Assertions, checksums and result destruction are
included equally in both versions.

RSS is sampled from `/proc/self/status` before and after the interval. It is
a process working-set snapshot influenced by setup and allocator retention,
not a live-heap profile. The workload uses the real shared in-memory pool,
Diesel decoding, alias folding, quote hydration and reaction API. It excludes
file I/O, a connected session, the GUI, and daemon startup. Results therefore
describe these synthetic read batches rather than application startup gains.

## Allocation and RSS observations

The 32-iteration allocation passes show a small Rust-side change. For an
attach batch, median allocation/reallocation requests per batch were
25,585 in the base and 25,537 in the head. Requested bytes increased by
816 bytes per batch, about 0.035%. The CAST SQL suffix makes query buffers
slightly larger. Reaction-only Rust allocation counts and bytes matched.
These counters do not measure the SQLite parser's C allocations, retained
heap or peak live memory. No overall heap or RSS reduction is claimed.

Accounting-enabled runs also record elapsed time and CPU for transparency.
Their medians relative to accounting-disabled runs ranged from roughly
0.94 to 1.08 across cases and binaries, including scheduling noise. The
performance conclusions use accounting-disabled data. The constant cost of
the disabled allocator wrapper is not isolated.

## Paired release results

The runner was Linux x86_64 on AMD EPYC 9V74, with a four-CPU quota,
Rust 1.99.1 and bundled SQLite 3.53.2. Both binaries used the workspace
release profile, fat LTO, one codegen unit and identical flags.

The initial 32-iteration run retained 140 observations covering every case
and both modes. Its paired timing medians improved in the changed readers,
but individual pairs and the unchanged control varied substantially. A
confirmation run increased batches to 256 iterations and pinned both
binaries to the same allowed CPU, retaining another 42 timing observations.
These runs are reported separately; affinity and duration differ.

Confirmation timings below are milliseconds for 256 batches. Ratios use
head/base within each pair; the table reports their median and the full
CPU ratio range. Independent time medians need not have the same ratio as
the median of paired ratios.

| Case | Base wall ms | Head wall ms | Paired wall ratio | Paired CPU ratio | CPU ratio range |
| --- | ---: | ---: | ---: | ---: | ---: |
| attach | 2213.9 | 1907.6 | 0.860 | 0.860 | 0.848–0.900 |
| history | 4778.0 | 4455.1 | 0.930 | 0.930 | 0.922–1.002 |
| reactions | 358.2 | 353.3 | 0.982 | 0.982 | 0.958–1.033 |

Attach's observed paired CPU median fell about 14%; history with reactions
fell about 7%. The unchanged reaction control moved about 2%, with no
attributable algorithmic change. Pair ranges remain part of the evidence;
these observations are not a universal gain or a daemon startup measurement.
The first run's backward and forward paired CPU medians were 0.920 and
0.897, respectively, with wider uncertainty than the longer confirmation.

Raw data: [initial run](measurements/sqlite-page-limit.csv),
[confirmation](measurements/sqlite-page-limit-confirmation.csv), and
[provenance](measurements/sqlite-page-limit-provenance.json).

## Revalidation after the pagination dependency landed

After #278 was squash-merged, `main` at
`cbacb4ee0b0aa72e778045e1f25c06e38e2adc7d` was incorporated by merge commit
`7ca608a5b3242b9ca97753b78846cabee61e9cf4`. Main's `queries.rs` is byte-identical
to the previously measured #278 head. The resolved `queries.rs` is byte-identical
to the reviewed CAST implementation at `10ecd51`. This preserves both cursor
predicates and LIMIT wrappers, with no rewriting of other merged commits.

The integrated all-feature chat-store suite passed 457 tests; six profiling
and documentation tests were ignored in ordinary runs. All-target/all-feature
chat-store Clippy passed. Both release binaries were rebuilt with the same
harness and flags against the integrated sources, using main's query file for
the base and the CAST query file for the head. Setup, dataset and result checks
are the same as above. No compiler ran during measurement.

The new timing run again used seven alternating pairs, 256 batches per process,
allocation accounting disabled, and CPU affinity 0. It retained 42 observations.
Times below are milliseconds for 256 batches; ratios and ranges are paired.

| Case | Base wall ms | Head wall ms | Paired wall ratio | Paired CPU ratio | CPU ratio range |
| --- | ---: | ---: | ---: | ---: | ---: |
| attach | 2302.1 | 1960.6 | 0.864 | 0.863 | 0.817–0.893 |
| history | 5282.9 | 4762.4 | 0.917 | 0.917 | 0.868–0.977 |
| reactions | 366.3 | 354.8 | 0.989 | 0.990 | 0.940–1.026 |

These observed CPU medians correspond to about 13.7% less CPU for attach and
8.3% for history with reactions. The unchanged reaction control moved about 1%.
They describe these synthetic pooled reads on the pinned runner, with the
ranges shown, rather than whole-application startup or connected behavior.

A separate 32-iteration allocation run retained 42 observations. Attach Rust
allocation requests per batch were 25,585.03 in the base and 25,537.03 in the head.
Requested bytes increased by 816.00 bytes per batch, about 0.035%.
Reaction-only Rust requests and bytes matched. As in the initial study, the
SQLite C allocator is outside this counter; no total heap or RSS gain is claimed.

Timing reproduction with the rebuilt binaries:

```sh
python3 scripts/measure-sqlite-page-limit.py /tmp/page-limit-post-main-base \
  /tmp/page-limit-post-main-head --iterations 256 --samples 7 \
  --cases attach history reactions --modes 0 --cpu 0 \
  --output docs/measurements/sqlite-page-limit-post-main.csv
```

Data: [post-main timing](measurements/sqlite-page-limit-post-main.csv),
[post-main Rust allocation traffic](measurements/sqlite-page-limit-post-main-alloc.csv),
and [post-main provenance](measurements/sqlite-page-limit-post-main-provenance.json).
Historical CSV line endings were normalized to LF without changing any values.
