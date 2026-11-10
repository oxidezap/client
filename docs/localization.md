# UI languages

English is the default, independently of the operating system or browser language.
Settings → Appearance → Language offers English and Portuguese. The choice applies
immediately. Save persists it, Revert restores the language that was active when
Settings opened, and Reload reads the saved choice again.

The existing `theme.json` preferences document stores `"language": "en"` or
`"language": "pt"`. Desktop uses the configuration directory already shown in
Settings. Web uses the existing `oxidezap.theme` localStorage document. No second
file, storage key, daemon request or migration is needed. Old documents and
unsupported language tags use English. Portuguese regional tags also resolve to
Portuguese. Storage failures use the existing visible Save error reporting.

## Translation code

`crates/gui/src/i18n/catalog.rs` pairs English and Portuguese static messages.
Each key becomes a Rust enum variant. `tr!(Save)` returns a static string through
a relaxed atomic locale read and an enum match. It does not allocate, parse a
catalog, acquire a lock or perform I/O.

`formatted.rs` holds complete sentences with named placeholders, for example
`on_call_with(name)`. `tr!(on_call_with, name)` borrows the value and formats the
selected sentence once. Rust checks the placeholders in both translations.
Translators can reorder placeholders. Keep sentences whole instead of joining
translated fragments. The supported English and Portuguese count forms are
selected explicitly where the product needs singular/plural wording.

`presentation.rs` formats domain facts such as typing, call outcomes, membership
and quoted media. The stored data and wire types have no language preference.
Contact names, captions, message bodies, plugin-provided text and filenames are
never translation keys. New media replies retain an empty caption when the source
has none; the view derives the media label from its kind, rather than putting a
translated label into the outgoing quote. Self-authored replies retain their
identity separately from their display role: the composer and quote choose
“You” or “Você” when rendered, while outgoing metadata keeps the account name.

A chat-row cache includes the locale in its key. Timeline measurement keys also
include it because a translation can wrap differently. Retained composer and
search inputs refresh their placeholders without replacing their values. GPUI
Kit's own controls receive `en` or `pt-BR`, and the web document's `lang` attribute
follows the selection.

Technical details from the operating system, daemon, plugins or network remain
verbatim. Native file chooser chrome follows the operating system. Historical
free-form notices that were already stored as sentences cannot be reconstructed
in another language; structured group events and calls can.

## Adding messages or languages

- Add a semantic key and both translations to `catalog.rs`, then use `tr!(Key)`.
  Missing or misspelled keys fail compilation.
- For interpolation, add a named-argument entry to `formatted.rs`. Both formats
  must use every declared argument. Add a regression for grammatical choices.
- For another locale, extend `Language`, `ALL`, tag parsing and the persisted code
  in `language.rs`. Extend the two catalog macros with the new locale column and
  supply that column for each entry. Rust's exhaustive matches identify missing
  cases. Set the corresponding GPUI Kit locale in `theme::install_resolved`, and
  add language-specific grammar to `presentation.rs`. Unsupported structured
  grammar currently falls back to the domain's English rendering.
- Add assertions for the locale's default/fallback, interpolation, plural forms
  and data preservation. UI translations must stay in the GUI, not core or IPC.

## Choice of implementation

[Fluent](https://projectfluent.org/) provides a richer translation language with
plural categories and grammar. [rust-i18n](https://docs.rs/rust-i18n/) supports
external YAML/JSON/TOML catalogs and is already used internally by GPUI Kit.
For this two-language interface, typed Rust catalogs keep the application's
static labels as static strings and validate formatting with the compiler.
They add no dependency or runtime catalog loader. Revisit Fluent when the product
needs translator-managed grammar or languages with additional plural categories.

## Validation

The catalog and interpolation regressions have no GUI or protocol dependencies:

```sh
rustc --edition=2024 --test crates/gui/src/i18n/language.rs -o /tmp/oxidezap-i18n-tests
/tmp/oxidezap-i18n-tests
```

`cargo test -p oxidezap-gui --all-features` also runs those tests, domain rendering
regressions and preference round trips. Follow the repository CI flags for
formatting, Clippy and web type-checks. For a manual check, switch languages with
a draft and an active search, inspect the attachment and delete menus, Save and
restart, then Revert and Reload. Verify longer labels at narrow window sizes.
