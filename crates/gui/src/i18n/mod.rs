//! English-default UI localization and projections of domain facts.
mod language;
pub use language::{Language, TextKey};
mod presentation;

#[macro_export]
macro_rules! tr {
    ($key:ident, $($arg:expr),* $(,)?) => { $crate::i18n::Language::current().$key($(&$arg),*) };
    ($key:ident) => {
        $crate::i18n::Language::current().text($crate::i18n::TextKey::$key)
    };
}
