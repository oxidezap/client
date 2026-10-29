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

## Reproduction

The base is `a5c7c3e9d968c455bbba572c0cea2ccda3adfafd` from #278 plus the
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
