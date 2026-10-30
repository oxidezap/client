//! Response deserialization without the outer flattened Content copy.

use serde::{Deserialize, Deserializer};

use super::{RequestId, ResponseEnvelope, ResponseResult};
use crate::{ApiError, DaemonResponse};

impl<'de> Deserialize<'de> for ResponseEnvelope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(
            match Decoded::deserialize(ResponseDeserializer(deserializer))? {
                Decoded::Ok { id, payload } => Self {
                    id,
                    result: ResponseResult::Ok { payload },
                },
                Decoded::Error { id, error } => Self {
                    id,
                    result: ResponseResult::Error { error },
                },
            },
        )
    }
}

// Decode the correlation id inside each status branch. Keeping it beside a
// flattened ResponseResult buffers and clones the entire payload an extra time.
#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Decoded {
    Ok {
        id: Option<RequestId>,
        #[serde(flatten)]
        payload: Box<DaemonResponse>,
    },
    Error {
        id: Option<RequestId>,
        #[serde(flatten)]
        error: ApiError,
    },
}
struct ResponseDeserializer<D>(D);
impl<'de, D: serde::Deserializer<'de>> serde::Deserializer<'de> for ResponseDeserializer<D> {
    type Error = D::Error;
    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
    fn deserialize_any<V: serde::de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_any(ResponseVisitor(visitor))
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier ignored_any
    }
}
struct ResponseVisitor<V>(V);
impl<'de, V: serde::de::Visitor<'de>> serde::de::Visitor<'de> for ResponseVisitor<V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.0.expecting(f)
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, inner: A) -> Result<V::Value, A::Error> {
        self.0.visit_map(ResponseFields {
            inner,
            status: false,
        })
    }
}
struct ResponseFields<A> {
    inner: A,
    status: bool,
}
impl<'de, A: serde::de::MapAccess<'de>> serde::de::MapAccess<'de> for ResponseFields<A> {
    type Error = A::Error;
    fn next_key_seed<K: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, A::Error> {
        self.status = false;
        self.inner.next_key_seed(KeySeed {
            seed,
            status: &mut self.status,
        })
    }
    fn next_value_seed<V: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, A::Error> {
        if self.status {
            self.inner.next_value_seed(StatusSeed(seed))
        } else {
            self.inner.next_value_seed(seed)
        }
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint()
    }
}
// Observe tag names without consuming or converting other key types. Generic
// Serde maps may contain unknown numeric, byte, sequence or map keys; the old
// flattened decoder allowed downstream visitors to ignore those keys.
struct KeySeed<'a, S> {
    seed: S,
    status: &'a mut bool,
}
impl<'de, S: serde::de::DeserializeSeed<'de>> serde::de::DeserializeSeed<'de> for KeySeed<'_, S> {
    type Value = S::Value;
    fn deserialize<D: Deserializer<'de>>(self, inner: D) -> Result<S::Value, D::Error> {
        self.seed.deserialize(KeyDeserializer {
            inner,
            status: self.status,
        })
    }
}
struct KeyDeserializer<'a, D> {
    inner: D,
    status: &'a mut bool,
}
impl<'de, D: Deserializer<'de>> Deserializer<'de> for KeyDeserializer<'_, D> {
    type Error = D::Error;
    fn is_human_readable(&self) -> bool {
        self.inner.is_human_readable()
    }
    fn deserialize_any<V: serde::de::Visitor<'de>>(self, inner: V) -> Result<V::Value, D::Error> {
        self.inner.deserialize_any(KeyVisitor {
            inner,
            status: self.status,
        })
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier ignored_any
    }
}
struct KeyVisitor<'a, V> {
    inner: V,
    status: &'a mut bool,
}
macro_rules! forward_key_values {
    ($($method:ident($ty:ty)),* $(,)?) => {$(
        fn $method<E: serde::de::Error>(self, value: $ty) -> Result<V::Value, E> {
            self.inner.$method(value)
        }
    )*};
}
impl<'de, V: serde::de::Visitor<'de>> serde::de::Visitor<'de> for KeyVisitor<'_, V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.inner.expecting(f)
    }
    forward_key_values! { visit_bool(bool), visit_i8(i8), visit_i16(i16), visit_i32(i32), visit_i64(i64), visit_i128(i128), visit_u8(u8), visit_u16(u16), visit_u32(u32), visit_u64(u64), visit_u128(u128), visit_f32(f32), visit_f64(f64), visit_char(char) }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<V::Value, E> {
        *self.status = value == "status";
        self.inner.visit_str(value)
    }
    fn visit_borrowed_str<E: serde::de::Error>(self, value: &'de str) -> Result<V::Value, E> {
        *self.status = value == "status";
        self.inner.visit_borrowed_str(value)
    }
    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<V::Value, E> {
        *self.status = value == "status";
        self.inner.visit_string(value)
    }
    fn visit_bytes<E: serde::de::Error>(self, value: &[u8]) -> Result<V::Value, E> {
        *self.status = value == b"status";
        self.inner.visit_bytes(value)
    }
    fn visit_borrowed_bytes<E: serde::de::Error>(self, value: &'de [u8]) -> Result<V::Value, E> {
        *self.status = value == b"status";
        self.inner.visit_borrowed_bytes(value)
    }
    fn visit_byte_buf<E: serde::de::Error>(self, value: Vec<u8>) -> Result<V::Value, E> {
        *self.status = value == b"status";
        self.inner.visit_byte_buf(value)
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<V::Value, E> {
        self.inner.visit_none()
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<V::Value, E> {
        self.inner.visit_unit()
    }
    fn visit_some<D: Deserializer<'de>>(self, value: D) -> Result<V::Value, D::Error> {
        self.inner.visit_some(value)
    }
    fn visit_newtype_struct<D: Deserializer<'de>>(self, value: D) -> Result<V::Value, D::Error> {
        self.inner.visit_newtype_struct(value)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, value: A) -> Result<V::Value, A::Error> {
        self.inner.visit_seq(value)
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, value: A) -> Result<V::Value, A::Error> {
        self.inner.visit_map(value)
    }
    fn visit_enum<A: serde::de::EnumAccess<'de>>(self, value: A) -> Result<V::Value, A::Error> {
        self.inner.visit_enum(value)
    }
}
struct StatusSeed<S>(S);
impl<'de, S: serde::de::DeserializeSeed<'de>> serde::de::DeserializeSeed<'de> for StatusSeed<S> {
    type Value = S::Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<S::Value, D::Error> {
        self.0.deserialize(StatusDeserializer(deserializer))
    }
}
// The previous flattened decoder fed this tag through ContentDeserializer,
// which accepts numeric variant identifiers as well as strings. Preserve that
// behavior without materializing the whole response into Content first.
struct StatusDeserializer<D>(D);
impl<'de, D: serde::Deserializer<'de>> serde::Deserializer<'de> for StatusDeserializer<D> {
    type Error = D::Error;
    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
    fn deserialize_any<V: serde::de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_any(visitor)
    }
    fn deserialize_identifier<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0.deserialize_any(visitor)
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct enum ignored_any
    }
}
