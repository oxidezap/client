//! Compiled UI messages. No parsing, allocation or I/O for static text.
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Language {
    #[default]
    English,
    Portuguese,
}

static LANGUAGE: AtomicU8 = AtomicU8::new(Language::English as u8);

impl Language {
    pub const ALL: [Self; 2] = [Self::English, Self::Portuguese];
    pub fn parse(code: &str) -> Self {
        match code
            .split(['-', '_'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "pt" => Self::Portuguese,
            _ => Self::English,
        }
    }
    pub const fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Portuguese => "pt",
        }
    }
    pub const fn name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Portuguese => "Português",
        }
    }
    pub fn current() -> Self {
        match LANGUAGE.load(Ordering::Relaxed) {
            1 => Self::Portuguese,
            _ => Self::English,
        }
    }
    pub fn activate(self) {
        LANGUAGE.store(self as u8, Ordering::Relaxed);
    }
}

macro_rules! catalog {
    ($($key:ident => ($en:literal, $pt:literal);)*) => {
        #[derive(Debug, Clone, Copy)]
        #[allow(dead_code, reason = "the shared catalog also includes web-only messages")]
        pub enum TextKey { $($key,)* }
        impl Language {
            pub const fn text(self, message: TextKey) -> &'static str {
                match self {
                    Self::English => match message { $(TextKey::$key => $en,)* },
                    Self::Portuguese => match message { $(TextKey::$key => $pt,)* },
                }
            }
        }
        #[cfg(test)]
        #[test]
        fn every_message_has_a_translation() {
            for language in Language::ALL {
                for message in [$(TextKey::$key,)*] {
                    assert!(!language.text(message).is_empty(), "{language:?} {message:?}");
                }
            }
        }
    }
}

include!("catalog.rs");

macro_rules! formatted_catalog {
    ($($key:ident($($arg:ident),*) => ($en:literal, $pt:literal);)*) => {
        impl Language {
            $(pub fn $key(self, $($arg: impl std::fmt::Display),*) -> String {
                match self {
                    Self::English => format!($en, $($arg = $arg),*),
                    Self::Portuguese => format!($pt, $($arg = $arg),*),
                }
            })*
        }
        #[cfg(test)]
        #[test]
        fn formatted_messages_preserve_each_named_argument() {
            for language in Language::ALL {
                $(let value = language.$key($(stringify!($arg)),*);
                  $(assert!(value.contains(stringify!($arg)), "{language:?}: {value}");)*
                )*
            }
        }
    }
}

include!("formatted.rs");

#[cfg(test)]
mod tests {
    use super::{Language, TextKey};

    #[test]
    fn language_tags_have_a_deterministic_english_fallback() {
        assert_eq!(Language::default(), Language::English);
        for tag in ["", "en", "EN-us", "fr", "unsupported"] {
            assert_eq!(Language::parse(tag), Language::English, "{tag}");
        }
        for tag in ["pt", "pt-BR", "pt-PT", "PT_br"] {
            assert_eq!(Language::parse(tag), Language::Portuguese, "{tag}");
        }
        for language in Language::ALL {
            assert_eq!(Language::parse(language.code()), language);
        }
    }

    #[test]
    fn attachment_labels_and_refusals_use_the_same_language() {
        for (language, document, photos) in [
            (Language::English, "Document", "Photos and videos"),
            (Language::Portuguese, "Documento", "Fotos e vídeos"),
        ] {
            assert_eq!(language.text(TextKey::Document), document);
            assert_eq!(language.text(TextKey::PhotosAndVideos), photos);
            for refusal in [
                language.unsupported_media("photo.heic"),
                language.unverified_media("photo.jpg"),
            ] {
                assert!(refusal.contains(document), "{refusal}");
                assert!(refusal.contains("photo."), "{refusal}");
            }
        }
    }

    #[test]
    fn formatted_messages_preserve_values_and_do_not_interpret_them() {
        let name = "Foto {name} <Olá>";
        assert_eq!(
            Language::English.on_call_with(name),
            format!("On call with {name}")
        );
        assert_eq!(
            Language::Portuguese.on_call_with(name),
            format!("Em chamada com {name}")
        );
        assert_eq!(Language::English.match_count(2, 5), "2 of 5");
        assert_eq!(Language::Portuguese.match_count(2, 5), "2 de 5");
        assert_eq!(Language::Portuguese.send_files(2), "Enviar 2 arquivos?");
        assert_eq!(Language::Portuguese.files(0), "0 arquivos");
    }
}
