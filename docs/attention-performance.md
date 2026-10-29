# Incremental daemon attention

Every state publication used to scan all chats for the tray count. Resetting the
mute timer scanned them twice more. Loading a list therefore did quadratic work
under the state lock. `StateStore` now keeps an unsaturated total and a `BTreeMap`
of mute deadlines, with chat counts and unread contributions per deadline. Updates
adjust the old and new contributions; advancing or reversing time visits only
crossed deadlines. Timer queries can use an older captured time without changing
the cached total. Past deadlines remain until their chats change or leave, so the
index has at most one bucket per chat with a mute timestamp, including expired
mutes. Equal deadlines share a bucket. No JIDs or payloads are copied into it.

Regression coverage compares with a full scan through updates, removals, time
changes, manual unread, status exclusion, u32 saturation and indefinite mutes.
Existing hub tests cover published tray state and finite-mute refresh.

## Measurement

Base `fce85ae` is main `e6e3020` plus the identical workload. Candidate `c6e08ce`
changes only the state implementation and adds the runner. Both full daemon test
executables use the repository's release profile, including fat LTO, and the same
lockfile, Rust 1.99.1, Linux x86_64 environment, four CPU quota and 16 GiB memory
limit. No allocation instrumentation, external services, credentials or root. A tiny C
launcher forks the measured child from a small process, avoiding Python's
pre-exec RSS floor. The runner compiles it once with the system C compiler.

Each fresh process inserts N chats and updates each once. Every event checks its
published count, refreshes attention and queries the next mute deadline, as the
bridge's timer reset does. The mute cases use distinct finite deadlines to stress
the new index. There is no socket, JSON, SQLite, media or connected session in
this workload. It measures the state path, not complete startup or daemon speed.
The runner alternates base/head order, excludes one warmup pair per scenario and
retains nine paired samples, with no builds running.

| Chats | Muted | Base ms, median [min, max] | Candidate ms, median [min, max] |
| ---: | ---: | ---: | ---: |
| 100 | none | 0.135 [0.130, 0.220] | 0.101 [0.090, 0.112] |
| 100 | half | 0.131 [0.125, 0.172] | 0.108 [0.101, 0.159] |
| 100 | all | 0.127 [0.119, 0.133] | 0.115 [0.100, 0.130] |
| 1,000 | none | 6.033 [5.986, 6.518] | 0.889 [0.876, 2.661] |
| 1,000 | half | 6.381 [6.317, 7.766] | 1.001 [0.975, 1.022] |
| 1,000 | all | 5.723 [5.618, 5.943] | 1.028 [1.014, 1.131] |
| 10,000 | none | 814.569 [786.749, 857.336] | 9.199 [8.889, 10.409] |
| 10,000 | half | 877.634 [861.339, 892.765] | 10.332 [10.178, 10.652] |
| 10,000 | all | 697.102 [666.408, 713.071] | 10.930 [10.543, 12.116] |

At 10,000 chats, workload time falls 64–89x. Median process CPU falls from
819/882/701 ms to 10.9/11.3/12.4 ms for none/half/all muted. At 100 chats,
process startup dominates CPU and the distributions overlap.

For 10,000 chats, median post-workload PSS changes by +24/+400/+808 KiB and peak
RSS by +68/+200/+504 KiB for none/half/all muted. The all-muted case uses a
different deadline for every chat, so it also exercises the largest index.
[Raw paired samples](measurements/attention.csv) retain the complete distributions.


RSS/PSS are sampled after the workload; peak RSS and CPU come from Linux wait4.
These complete-process figures include runtime, code and allocator retention.
Live heap was not measured separately. The new index trades memory for CPU.

## Reproduce

Use the same checkout and default Cargo target cache for both commits. At each
ref run `cargo test -p oxidezap-daemon --lib --all-features --release --no-run
--message-format=json`, and copy the daemon test executable named by the JSON
`executable` field to `/tmp/attention-base` or `/tmp/attention-head` before changing
refs. Then run:

```sh
python3 scripts/measure-attention.py /tmp/attention-base /tmp/attention-head \
  --output /tmp/attention-results.json
```

Raw JSON retains all samples and metrics. Ordinary tests ignore the workload.

## Other candidates inspected

- Native/web GUI timeline invalidation rebuilds all message text, IDs and message
  clones. Incremental reuse needs selection, edited-message and pagination tests.
- The native/web publisher's unbounded media queue can retain a backlog. Bounding
  it requires an explicit delivery/backpressure design, beyond this change.
- CLI attach receives the shared state snapshot. Reducing snapshot cloning or
  serialization needs a separate protocol-compatible measurement.

Those candidates were not changed. The shared state improvement applies to both
daemon implementations; no frontend rendering or CLI startup gain was measured.
