# phonenumber cold-start patch

Source: `phonenumber` 0.3.10+9.0.33 from crates.io, SHA-256
`66d85d3cc5477bfe2f357939e7f02a5a566632508633af1a2c37577d0bef87d9`.
Apache-2.0 license retained. All `src/` files and the bundled
`assets/PhoneNumberMetadata.xml` are unchanged except `src/metadata/database.rs`.
The published manifest is retained without the unavailable upstream integration
test, example and benchmark targets. Unused assets and external test files are
omitted. The original README is retained.

The default database uses immutable build-generated metadata. `build.rs` now
validates every descriptor, formatting, leading-digit, international-prefix and
national-prefix regex with the same runtime regex engine and whitespace option.
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
