//! Base64 for the one field that is bytes.
//!
//! A dependency would be the obvious answer and is a poor trade here: the
//! whole of what this is used for is one field of one frame, the standard
//! alphabet with padding has not moved since RFC 4648, and the encoder is
//! shorter than the paragraph justifying it. It is `serde(with = ...)`
//! shaped, so the field that uses it reads like every other field.

use std::borrow::Cow;

use serde::de::{Error as _, Unexpected};
use serde::{Deserialize, Deserializer, Serializer};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const PAD: u8 = b'=';

/// The standard alphabet, with padding.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            PAD as char
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            PAD as char
        });
    }
    out
}

/// The inverse. `None` for anything that is not exactly what [`encode`]
/// produces: a wrong length, a character outside the alphabet, or padding
/// anywhere but the end.
#[must_use]
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, quad) in bytes.chunks(4).enumerate() {
        let last = index == bytes.len() / 4 - 1;
        let mut acc = 0u32;
        let mut kept = 3;
        for (position, &byte) in quad.iter().enumerate() {
            if byte == PAD {
                // Padding is the tail of the final quad and nothing else: two
                // at most, and never before a data character.
                if !last || position < 2 {
                    return None;
                }
                if quad[position..].iter().any(|&b| b != PAD) {
                    return None;
                }
                kept = position - 1;
                acc <<= 6 * (4 - position);
                break;
            }
            acc = (acc << 6) | u32::from(value_of(byte)?);
        }
        out.push((acc >> 16) as u8);
        if kept > 1 {
            out.push((acc >> 8) as u8);
        }
        if kept > 2 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

fn value_of(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&encode(bytes))
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    // `Cow`, not `&str`. Borrowing costs nothing on the common path and is
    // what every reader in this workspace takes, but a deserializer that
    // cannot borrow (`from_reader`) or an input carrying an escape refuses a
    // borrowed string outright, with "invalid type: string, expected a
    // borrowed string", which names neither the field nor the real problem.
    let text = <Cow<'_, str>>::deserialize(deserializer)?;
    decode(&text).ok_or_else(|| D::Error::invalid_value(Unexpected::Str(&text), &"base64"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rfc_4648_vectors_hold() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), encoded, "encoding {plain:?}");
            assert_eq!(
                decode(encoded).as_deref(),
                Some(plain.as_bytes()),
                "decoding {encoded:?}"
            );
        }
    }

    #[test]
    fn every_byte_survives_the_round_trip() {
        let all: Vec<u8> = (0..=255).collect();
        for length in 0..all.len() {
            let slice = &all[..length];
            assert_eq!(decode(&encode(slice)).as_deref(), Some(slice));
        }
    }

    /// A deserializer that cannot hand out a borrow (`from_reader`) refused
    /// the field outright, with "invalid type: string, expected a borrowed
    /// string", a message about serde rather than about the frame.
    #[test]
    fn a_deserializer_that_cannot_borrow_still_reads_the_field() {
        #[derive(serde::Deserialize)]
        struct Frame {
            #[serde(with = "super")]
            bytes: Vec<u8>,
        }

        let json = format!(r#"{{"bytes":"{}"}}"#, encode(b"foobar"));
        let frame: Frame =
            serde_json::from_reader(std::io::Cursor::new(json)).expect("reads without borrowing");
        assert_eq!(frame.bytes, b"foobar");
    }

    #[test]
    fn what_is_not_base64_is_refused_rather_than_guessed() {
        for bad in ["Zg=", "Zg===", "Z===", "Zm9v!", "=Zm8", "Zm 8=", "Zg=Z"] {
            assert!(decode(bad).is_none(), "{bad:?} should not decode");
        }
    }
}
