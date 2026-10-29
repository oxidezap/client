# Daemon memory profiling

The PR currently adds regression coverage and workloads, not a measured memory
optimization. `account_scale_paging` checks a synthetic SQLite store with 1,032
chats, 1,653 contacts and 75,703 messages.

The daemon's `session_bridge::memory_scenario` replays fixed-size synthetic text
messages through the real bridge, read tracker and publisher, after an initial
attach with 100 chats and eight read rows per chat. Its ordinary test
checks badges, duplicate delivery, read receipts and the publication tail with
and without a subscriber. Two ignored workloads each replay 75,703 messages
across 1,032 chats, then join the publisher before finishing. The attached case
deliberately leaves its subscriber behind during the burst to exercise backlog
and broadcast resynchronization. It is a slow-consumer stress case, not a typical
frontend session. No network, account database or media files are used.

Compile the release test binary without executing profiling workloads:

```sh
cargo test -p oxidezap-daemon --release --lib --no-run
```

Use the executable path Cargo reports, and run each workload separately through
CodSpeed so their peaks are not combined:

```sh
codspeed exec -m memory --repository oxidezap/client --name bridge-headless -- \
  <daemon-test-binary> --exact session_bridge::memory_scenario::account_scale_memory_headless --ignored --test-threads=1
codspeed exec -m memory --repository oxidezap/client --name bridge-attached -- \
  <daemon-test-binary> --exact session_bridge::memory_scenario::account_scale_memory_attached --ignored --test-threads=1
```

Compare the same release build configuration on base and head, including walltime
runs through CodSpeed. These workloads isolate the bridge; they do not measure
the session's Tokio runtime, SQLite, cryptographic state, calls, media or full
daemon RSS/PSS. A complete daemon scenario is still needed before attributing
real-account startup or idle memory to this path.

Execution currently requires network access to the CodSpeed API, an authenticated
profile authorized for this project, and the memory executor's eBPF capabilities
on a supported runner. The existing CI runs correctness checks but has no
CodSpeed profiling integration. Do not substitute virtual address size for RAM
or report workload results as a whole-daemon measurement.
