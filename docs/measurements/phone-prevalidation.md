# Phone metadata prevalidation

Measured on 2026-10-30, Linux x86_64, rustc 1.99.1, release profile.
Base is client `33108ce` with core `af2e018c`. Head changes only the phone
metadata validation path, its local source patch, and regression/probe coverage.
The performance CSV records runtime patch `30e3e05`; the follow-up adds tests and
documentation without changing runtime phone parsing or formatting.
No account, session database, captured number or live connection was used.

## Result

The first legitimate `format_phone_jid("12025550123@s.whatsapp.net")` call is
faster because immutable bundled patterns are validated during the build rather
than again when the default database initializes. Regex compilation remains lazy
through the original 100-entry LRU. The library already compiled regexes lazily;
the expensive cold work removed here was syntax validation, not compilation of
all countries' regexes.

The new build check uses `regex::RegexBuilder::ignore_whitespace(true).build()`.
The previous `CachedRegexBuilder::build()` check uses the `regex-syntax` 0.6
`Parser::new().parse()` syntax parser, without applying the builder whitespace
option. These are not literally the same validator. The retained bundled XML
passes both, and the runtime engine used for actual matching is unchanged.

15 fresh processes per revision and mode, alternating order, after discarding
one pair. Timing runs disable allocation counters. Heap runs enable them.
Medians and observed min/max are reported below. The existing probe also runs
10,000 warmed calls in each process. The C launcher records whole-process CPU,
wall time and maximum RSS, so those include the warm loop and process startup.
No builds or other benchmark runs overlapped measurement.

| Measurement | Base | Head |
| --- | ---: | ---: |
| Cold call, counters off | 94.86 ms [88.73, 104.78] | 8.01 ms [7.67, 8.85] |
| Warm 10,000 calls, counters off | 46.48 ms [43.59, 52.72] | 46.51 ms [44.40, 78.27] |
| Process CPU, counters off | 141.45 ms [133.50, 157.56] | 55.96 ms [53.53, 88.42] |
| Process wall, counters off | 141.95 ms [133.62, 157.89] | 56.08 ms [53.63, 88.71] |
| Cold allocation/reallocation calls | 355,830 | 61,975 |
| Cold cumulative allocated bytes | 366,390,824 | 13,581,176 |
| Cold peak live byte delta | 4,142,217 | 3,225,060 |
| Cold retained live byte delta | 3,153,293 | 3,153,293 |
| Warm allocations, 10,000 calls | 280,000 | 280,000 |
| Warm retained bytes | 0 | 0 |
| Process max RSS, counters off | 7,832 KiB [7,740, 7,932] | 6,920 KiB [6,896, 7,048] |

Allocation counts and heap byte values were identical across all 15 samples.
The cold call median fell 91.6%, with 82.6% fewer allocation/reallocation calls.
The warmed case has no demonstrated improvement. Retained heap is unchanged.
The RSS difference is a microprobe observation, not a promised daemon or GUI RSS
saving. This does not measure full application startup or first visible frame.
Inclusive CPU samples in the original real-account capture are not converted
into a whole-application speedup. Raw samples are in
[phone-prevalidation.csv](phone-prevalidation.csv).

The already-optimized `left` notice is a control: cold allocations remain 8,
retained delta 16 bytes, and peak delta 264 bytes on both revisions. This patch
addresses legitimate phone formatting, not the discarded work removed by #279.

## Correctness

The new `phone_profile --equivalence` mode generates 17,982 synthetic cases:
country-code candidates 1 through 999, national lengths 4 through 12, and
all-zero/all-nine national strings. It includes invalid calling codes and
unusual lengths, not just ordinary valid numbers. Every case also asserts that
a device suffix produces the same label. Base and head outputs are byte-identical,
SHA-256 `feeb2545c22b6bf45f7d223a459045c38af3317013c9049dc74a30d7874966d5`.
The identical probe source was used with both library versions. Zero/nine
strings do not exercise every formatting rule or establish exhaustive
international equivalence. The separate `--representative` corpus adds synthetic
fixed/mobile, toll-free, variable-length, Italian leading-zero, shared calling
code and non-geographic cases, alongside PN/LID, device, invalid-code, short and
malformed inputs. These 24 cases also produced byte-identical base/head output,
SHA-256 `b33de771cfc26c144e79bec9894cc95d768aed609adf90a3435319563b0a9476`.
The formatted cases have pinned baseline outputs in core regression tests.
This corpus also remains representative, not exhaustive.

Core regressions cover NANPA and Ofcom fictional-use numbers, multiple calling
codes, invalid-code fallback, resolved/offered labels, LID rejection and PN
aliases. Caller-provided malformed metadata still fails runtime validation.
The repeatable `phone_metadata_build` integration target compiles the actual
vendored `build.rs` as its test source. Its 25 fault-injection positions cover
all 16 descriptors, all three optional regexes, national/international format
patterns and each of two leading-digit entries in both format lists. Every
injected `[` must fail both the original checked runtime path and the build
visitor. A deliberate mutation removing `mobile` from the visitor makes the
test fail with `mobile: build visitor omitted invalid regex`. The whole bundled
XML must pass both paths. This runs in normal workspace tests/CI and can be
repeated with `cargo test -p oxidezap-core --test phone_metadata_build`.
The earlier executable build-script fault injection also failed before emitting
`database.bin`.

The bundled XML is byte-identical to the published 0.3.10+9.0.33 crate. Among
vendored Rust files, only `build.rs` and `src/metadata/database.rs` differ.
This avoids a new telephone parser or region-specific heuristics. The cost is
carrying about 1.2 MiB of third-party source and metadata. The source patch,
provenance, maintenance requirements and removal condition are documented in
[LOCAL_CHANGES.md](../../vendor/phonenumber/LOCAL_CHANGES.md).

## Reproduction

Build head and base with the repository's release profile in the same environment.
Copy each binary before a subsequent build can overwrite it. The base checkout
needs the same updated probe source for both functional-comparison modes.

```sh
git worktree add --detach /tmp/phone-base 33108ce
cp crates/core/examples/phone_profile.rs /tmp/phone-base/crates/core/examples/
cargo build --release --locked -p oxidezap-core --example phone_profile
cp target/release/examples/phone_profile /tmp/phone-head
(cd /tmp/phone-base && cargo build --release --locked -p oxidezap-core --example phone_profile)
cp /tmp/phone-base/target/release/examples/phone_profile /tmp/phone-base-probe
cc -O2 scripts/phone-profile-usage.c -o /tmp/phone-usage
python3 scripts/measure-phone-profile.py /tmp/phone-usage /tmp/phone-base-probe /tmp/phone-head \
  --samples 15 --output /tmp/phone-prevalidation.csv
/tmp/phone-base-probe --equivalence > /tmp/phone-base-labels
/tmp/phone-head --equivalence > /tmp/phone-head-labels
cmp /tmp/phone-base-labels /tmp/phone-head-labels
/tmp/phone-base-probe --representative > /tmp/phone-base-representative
/tmp/phone-head --representative > /tmp/phone-head-representative
cmp /tmp/phone-base-representative /tmp/phone-head-representative
cargo test -p oxidezap-core --test phone_metadata_build
cargo test -p oxidezap-core --all-features
cargo clippy -p oxidezap-core --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo machete
cargo check --locked -p oxidezap-audio --target wasm32-unknown-unknown
```

This investigation used the explicitly authorized local alternative to CodSpeed.
The timings are local wall/CPU measurements, not CodSpeed simulation results.
