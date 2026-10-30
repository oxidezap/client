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
// Borrow ordinary keys; escaped keys may require owned storage.
#[derive(Deserialize)]
struct FieldName<'a>(#[serde(borrow)] std::borrow::Cow<'a, str>);
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
        match self.inner.next_key::<FieldName<'de>>()? {
            None => Ok(None),
            Some(key) => {
                self.status = key.0 == "status";
                match key.0 {
                    std::borrow::Cow::Borrowed(key) => seed.deserialize(
                        serde::de::value::BorrowedStrDeserializer::<A::Error>::new(key),
                    ),
                    std::borrow::Cow::Owned(key) => {
                        seed.deserialize(serde::de::value::StringDeserializer::<A::Error>::new(key))
                    }
                }
                .map(Some)
            }
        }
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
