# Phone formatting in leave summaries

The production change moves the existing self-leave identity check ahead of
participant label construction in `GroupMembership::summary`. A named actor
leaving produces `Alex left`; previously an unnamed target participant's phone
was formatted and discarded. Output, alias normalization, international metadata
and unknown-contact fallback stay unchanged. Only core summary ordering changes.

The real-profile observation was a cold global metadata initialization. It was
not a leak or the full cost of every contact. No account data or laptop profile
was used for this synthetic reproduction.

## Scope and method

Base production revision: `e6e302043a216419058c87ec5e2ca7985eba51ca`.
Measured production change: `e6342c29ab00985d776b7464f7f042a81e221b3c`.
Both revisions used the same `crates/core/examples/phone_profile.rs` harness.
The subsequent supporting-files commit changes no production code.

Linux x86_64 cloud runner, AMD EPYC 9V74, rustc 1.99.1. Both probes use the
repository's unchanged release profile, opt-level 3, fat LTO, one codegen unit,
panic abort, frame pointers and stripping. The ordinary Cargo cache and target
directory were reused; copies of executables preserve the two revisions.

Each new process performs one cold call followed by 10,000 warm calls. The
fixture is constructed before the measured intervals and uses reserved fictional
NANP number +1 202-555-0123. `left` checks `Alex left` on every iteration. The
`format` control calls the unchanged international formatter directly. It is a
control for actual number display, not an end-to-end chat workload.

Nine paired samples per case and mode alternate base/head process order. One
warmup pair per case and mode is excluded. All 72 retained observations appear
in `measurements/phone-profile.csv`. No outlier removal, pinning or frequency
control. Each cold observation is a fresh process, even after warmup pairs.

Heap mode counts successful allocation/reallocation calls, cumulative requested
bytes, requested live bytes and peak requested live bytes. It uses a forwarding
System allocator wrapper. It does not count allocator bookkeeping, fragmentation,
physical pages or RSS. The clock provider's first initialization adds 16 retained
bytes in both revisions. Cumulative allocation bytes are churn, not peak memory.

Timing mode disables allocation accounting before constructing the fixture.
The allocator wrapper and an atomic flag check remain, so this is not a bare
System allocator timing. No calibration against a wrapper-free binary was made.
Heap-mode timings are retained only for transparency and are not used below.
`cold_ns` and `warm_ns` cover only their loops, including correctness assertions.
The small C launcher measures child user+system CPU with wait4 and total elapsed
time with CLOCK_MONOTONIC. Child max RSS avoids a Python parent's RSS floor.
CPU, process elapsed time and RSS include startup, fixtures, both loops and stdout.
They are not cold-call-only metrics.

## Results

Medians from counter-disabled timing mode:

| Metric | Left base | Left head | Display control base | Display control head |
| --- | ---: | ---: | ---: | ---: |
| First call | 89.742 ms | 4.016 us | 87.276 ms | 88.380 ms |
| 10,000 warm calls | 47.577 ms | 2.002 ms | 44.631 ms | 44.578 ms |
| Complete process CPU | 138.820 ms | 2.895 ms | 132.404 ms | 133.834 ms |
| Complete process elapsed | 138.945 ms | 2.949 ms | 132.530 ms | 133.981 ms |
| Complete process peak RSS | 7,800 KiB | 1,816 KiB | 7,768 KiB | 7,788 KiB |

The nine left cold samples ranged 87.268–92.722 ms in base and 3.325–6.379 us in
head. Warm left samples ranged 46.505–50.775 ms versus 1.950–2.196 ms. Median
paired warm reduction is 95.77%. This is the discarded-label case alone. No
startup or account-wide percentage is inferred. The unchanged display control
has overlapping timing ranges and about 1% median process variation, with no
speedup claimed.

Heap-mode medians for the first call:

| Metric | Left base | Left head | Display control, both |
| --- | ---: | ---: | ---: |
| Allocation/reallocation calls | 355,837 | 8 | 355,830 |
| Cumulative requested bytes | 366,391,096 | 288 | 366,390,824 |
| Additional peak requested live bytes | 4,142,221 | 264 | 4,142,217 |
| Additional requested live bytes after call | 3,153,293 | 16 | 3,153,293 |

The metadata portion of retained requested heap is about 3.15 MB on this runner.
The 16 bytes left in head belong to the common clock provider, not phone metadata.
Warm left calls allocate 35 versus 7 times per call, request 1,509 versus 272
bytes per call and retain no additional heap after their results are dropped.
The display control allocates 28 times and requests 1,237 bytes per warm call in
both revisions. These allocator counts do not depend on timing noise.

This patch does not reduce the complete international database. The first
legitimate phone display still pays its global initialization cost and retains
its metadata. Memoization would not remove that cost. Reducing the metadata set
would need separate correctness work for international numbers and shared
calling codes; no digit grouping or regional subset was introduced. Other eager
projections, including duplicate store events and participant hydration, remain
possible followups requiring coordination with their owners.

## Reproduce

Use a disposable, clean checkout of this PR branch. The harness is identical
across both builds; only the production source is replaced for baseline.
No account, network session, root, eBPF or CodSpeed is required. Raw alternatives
were authorized for this investigation. No production dependency was added.

```sh
cc -O2 -Wall -Wextra -Werror scripts/phone-profile-usage.c -o /tmp/phone-profile-usage
git show e6e302043a216419058c87ec5e2ca7985eba51ca:crates/core/src/group_membership.rs > crates/core/src/group_membership.rs
cargo build --release -p oxidezap-core --example phone_profile --locked
cp target/release/examples/phone_profile /tmp/phone-profile-base
git restore -- crates/core/src/group_membership.rs
cargo build --release -p oxidezap-core --example phone_profile --locked
cp target/release/examples/phone_profile /tmp/phone-profile-head
python3 scripts/measure-phone-profile.py /tmp/phone-profile-usage /tmp/phone-profile-base /tmp/phone-profile-head --output /tmp/phone-profile.csv
```

Timing CSV rows intentionally report zero allocation fields because counting is
disabled. Read allocation fields only from heap rows. The initial instrumented
exploratory run was superseded by this separated measurement and is not pooled.

Validation: core all-feature release library tests, 249 passed and 2 unrelated
profiling tests ignored; all-target/all-feature core Clippy with warnings denied;
workspace formatting; diff checks; C launcher compiled with warnings denied;
all fresh measurement processes exited successfully. Existing core tests cover
leave notices through authoritative PN/LID aliases, self notices, action grammar,
serialization, device suffix normalization and invalid telephone identities.
