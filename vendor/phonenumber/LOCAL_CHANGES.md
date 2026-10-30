# phonenumber cold-start patch

Source: `phonenumber` 0.3.10+9.0.33 from crates.io, SHA-256
`66d85d3cc5477bfe2f357939e7f02a5a566632508633af1a2c37577d0bef87d9`.
Apache-2.0 license retained. All `src/` files and the bundled
`assets/PhoneNumberMetadata.xml` are unchanged except the four runtime metadata files `database.rs`,
`descriptor.rs`, `format.rs` and `metadata.rs`.
The published manifest is retained without the unavailable upstream integration
test, example and benchmark targets and their unused criterion/proptest
dev-dependencies. Unused assets and external test files are omitted. The original README is retained.
The manifest marks `serde` as required for cargo-machete: `serde_derive`
expands references to it in the retained loader, although source scanning
does not see a direct import.

The default database uses immutable build-generated metadata. `build.rs` now
validates every descriptor, formatting, leading-digit, international-prefix and
national-prefix regex with `regex::RegexBuilder::ignore_whitespace(true)`.
This compiles with the runtime matching engine. The previous check in
`CachedRegexBuilder::build` instead uses `regex-syntax` 0.6 `Parser::new().parse`
without applying the builder whitespace option. They are different validators;
the full bundled XML is tested through both paths, rather than claiming general
validator equivalence.
Only this database uses `CachedRegexBuilder::build_unchecked`, which creates a
lazy cached regex without repeating syntax validation. This is a safe Rust API;
regex compilation and matching remain in the original library. Caller-provided
`Database::from`, `parse` and `load` still validate at runtime.

The patch preserves all regions, calling-code sharing, prefix transforms,
formatting rules and fallback behavior. It does not cache labels or alter JID
handling. The full database is still deserialized and the compiled-regex LRU
still has its original 100-entry capacity. This reduces cold CPU and temporary
allocations, not retained heap or guaranteed RSS.

Maintenance cost: about 1.2 MiB of third-party source and metadata to carry a
small source patch. Keep the source version and metadata together. When updating,
compare the visitor in `build.rs` with every regex construction in
`Database::from_inner`; new regex fields must be validated too. Prefer removing
the patch and this directory when an upstream release provides equivalent
prevalidation. A general update to 0.4 is not part of this change.

See `docs/measurements/phone-prevalidation.md` for the measurement and reproduction.

Immutable descriptor lengths, format lists and leading-digit lists use boxed
slices. Their public getters remain slices, the loader still accepts vectors,
and the regex implementation and 100-entry LRU are unchanged. This removes
unused vector capacity and capacity words from retained metadata. See
`docs/measurements/phone-compaction.md` for paired measurements and limits.

## Update procedure

1. Download the exact new crates.io `.crate`, verify its registry SHA-256 before
   extracting it, and record the version/checksum above. Keep the previous
   pristine archive too. Do not combine this with a general dependency update.
2. Diff the previous pristine `build.rs` and `src/metadata/database.rs` against
   this vendor to recover the local patch, including the tests in `build.rs`.
   Replace retained source, XML, LICENSE, README and the published manifest with
   the new archive. Reapply and review the build visitor and four runtime metadata patches.
3. Remove only manifest targets whose external test/example/bench files are not
   retained, plus dev-dependencies used exclusively by those omitted targets.
   Keep all `src/` modules, internal-test dependencies, README used by `doctest!`,
   LICENSE and the XML used by build/test `include_str!`. No other asset is
   referenced by retained build or source files in this version.
4. Compare every runtime regex construction in `Database::from_inner` with the
   build visitor and its 25 fault-injection positions. Add coverage for any new
   field. The two validators differ, so run the entire new XML through both.
5. Update the targeted phonenumber lock entry. Run
   `cargo test -p oxidezap-core --all-features`, Clippy, fmt, cargo-machete
   and the WASM integration check in the measurement report. The normal workspace/CI tests include the `phone_metadata_build`
   integration target, which compiles this actual `build.rs` as a test binary.
6. Compare base/head outputs for both probe corpora with identical probe source.
   Re-measure cold and warm paths if runtime metadata handling changed. Finally
   compare the new pristine `src/` and XML against the vendor: the XML must be
   byte-identical, and only the four runtime metadata files listed above may differ
   under `src/`. Inspect the separate `build.rs` and manifest differences. Remove
   the vendor instead if upstream now provides equivalent prevalidation.

The fault-injection test places an invalid regex in every descriptor, every
optional regex, both format patterns and each leading-digit entry. It requires
rejection by both the original checked path and the actual build visitor. A
mutation check removing `mobile` from the visitor makes the test fail. The
complete XML must also pass both paths. This guards visitor omissions; it does
not make the hand-maintained field inventory automatically track future schemas.
