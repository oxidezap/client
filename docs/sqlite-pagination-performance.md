# Indexed message cursor ranges

The message readers ordered by `(timestamp_ms, id)` but expressed the cursor as
`timestamp_ms < t OR (timestamp_ms = t AND id < seq)`. In the measured bundled
SQLite plan, only the account/chat prefix constrained the index. Older pages
therefore rescanned newer messages; the forward reader had the inverse cost.

The readers now give `idx_messages_chat_time` an explicit timestamp range before
checking the arrival tiebreak. For backward pages this is
`timestamp_ms <= t AND (timestamp_ms < t OR id < seq)`, with the inverse operators
for forward pages. The stored columns are non-null, so the accepted rows and
order are the same, including late backfills. No index, migration, cache, public
API, dependency or write path changes. A row-value alternative was rejected
because it added work in the all-timestamps-equal scenario on SQLite 3.53.2.

## Measurement

Baseline `c50dbda` is main `e6e3020` plus the same workload and extraction of the
forward query helper. Candidate `113d4f6` contains the final scalar bounds.
Both library test executables use the default Cargo cache, repository release
profile with fat LTO, Rust 1.99.1, bundled SQLite 3.53.2 and one Linux x86_64
instance with four CPU quota and 16 GiB memory. No builds run during measurement.

Every fresh process migrates an in-memory database, inserts 200,000 synthetic
messages plus another account's 200,000 rows, warms one query, then times 256
50-row queries using the production SQL and `MessageRow` decoding. Each result
is checked against its expected IDs. Setup is outside the interval. Normal
timestamps have 20 messages per second. `head`, `middle` and `deep` cursors sit
100 rows from the newest end, halfway through, and 100 rows from the oldest
end. `ties` gives every target row one timestamp; `aliases` splits rows across
two chat keys and retains the existing merge/sort cost.

The runner alternates base/head order, excludes one warmup pair per scenario
and retains seven paired samples. Query-loop CPU comes from `/proc/self/stat`,
with 10 ms tick resolution here. No allocation instrumentation, credentials,
external service or privileged profiler is used.

| Cursor | Direction | Base ms, median [min, max] | Candidate ms, median [min, max] | CPU ms, base → candidate |
| --- | --- | ---: | ---: | ---: |
| head | backward | 23.44 [23.09, 24.88] | 22.66 [22.42, 23.59] | 20 → 20 |
| head | forward | 1874.26 [1850.92, 1931.03] | 20.74 [20.64, 21.92] | 1870 → 20 |
| middle | backward | 942.79 [930.53, 973.81] | 22.48 [22.11, 22.82] | 940 → 20 |
| middle | forward | 949.09 [933.95, 968.54] | 20.91 [20.77, 21.10] | 950 → 20 |
| deep | backward | 1863.13 [1834.92, 1894.81] | 22.16 [21.86, 22.92] | 1870 → 20 |
| deep | forward | 21.84 [21.66, 22.22] | 20.88 [20.58, 21.12] | 20 → 20 |
| ties | backward | 2356.36 [2341.29, 2378.81] | 2093.10 [2052.08, 2110.23] | 2350 → 2090 |
| ties | forward | 23.00 [22.45, 24.07] | 22.10 [21.55, 22.91] | 20 → 20 |
| aliases | backward | 1981.05 [1945.97, 2024.13] | 31.10 [30.61, 35.69] | 1980 → 30 |
| aliases | forward | 32.17 [31.93, 32.73] | 31.37 [30.98, 32.52] | 30 → 30 |

Times cover 256 queries, not one page. Timestamp-skipping cases improved
42–90x; their median query-loop CPU fell from 940–1,980 ms to 20–30 ms. The
all-equal backward case improved 11%, with its linear tied-run scan remaining.
Shallow/short-prefix cases changed little and CPU stayed within tick resolution.
[Raw paired samples](measurements/sqlite-pages.csv) retain all 140 observations.


This isolates warm SQL reads and row decoding, with no ANALYZE statistics,
async pool acquisition, alias resolution, logical deduplication, protobuf/quote
hydration, disk I/O or connected account. It does not measure complete UI, CLI,
daemon startup or disk-cache latency. RSS, PSS, live heap and allocator retention
were not measured; no memory reduction is claimed. Large timestamp ties can
still require scanning their tied run.

## Reproduce

At each recorded code ref, run:

```sh
cargo test -p oxidezap-chat-store --lib --all-features --release --no-run \
  --message-format=json
```

Copy the test executable identified by the JSON `executable` field to
`/tmp/sqlite-base` or `/tmp/sqlite-head` before changing refs. Return to the final
PR head to use its measurement runner, then run:

```sh
python3 scripts/measure-sqlite-pages.py /tmp/sqlite-base /tmp/sqlite-head \
  --output /tmp/sqlite-pages.csv
```

The regression tests explain Diesel's generated SQL and require a timestamp
index bound in both directions. Boundary tests cover integer limits, ties,
account scope and one/two chat keys. Existing pagination, alias-copy folding,
unread, quote, edit, search and multi-account tests cover the public readers.
