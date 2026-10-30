#[path = "support/response_cases.rs"]
mod cases;

use oxidezap_wire::{ResponseEnvelope, ResponseResult};
use serde_json::{Value, json};

fn compare(frame: &[u8]) {
    match (cases::baseline(frame), cases::candidate(frame)) {
        (Ok(base), Ok(head)) => {
            assert_eq!(base, head, "{}", String::from_utf8_lossy(frame));
            assert_eq!(
                serde_json::to_vec(&base).unwrap(),
                serde_json::to_vec(&head).unwrap(),
                "re-encoding differs"
            );
        }
        (Err(_), Err(_)) => {}
        (base, head) => panic!(
            "acceptance differs for {}: base={base:?}, head={head:?}",
            String::from_utf8_lossy(frame)
        ),
    }
}
fn encoded(value: &Value) -> String {
    serde_json::to_string(value).unwrap()
}
fn permutations(fields: &mut [(String, Value)], start: usize) {
    if start == fields.len() {
        let text = format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| format!(
                    "{}:{}",
                    serde_json::to_string(key).unwrap(),
                    encoded(value)
                ))
                .collect::<Vec<_>>()
                .join(",")
        );
        compare(text.as_bytes());
        return;
    }
    for i in start..fields.len() {
        fields.swap(start, i);
        permutations(fields, start + 1);
        fields.swap(start, i);
    }
}

#[test]
fn response_values_and_serialization_match_the_legacy_envelope() {
    let fixtures = cases::fixtures();
    assert_eq!(fixtures.len(), 34);
    for response in fixtures {
        let bytes = serde_json::to_vec(&response).unwrap();
        let legacy = cases::LegacyEnvelope {
            id: response.id,
            result: response.result.clone(),
        };
        assert_eq!(bytes, serde_json::to_vec(&legacy).unwrap());
        assert_eq!(cases::candidate(&bytes).unwrap(), response);
    }
}

#[test]
fn field_orders_duplicates_unknowns_types_and_depth_match_the_legacy_decoder() {
    let values = [
        json!(null),
        json!(false),
        json!(true),
        json!(0),
        json!(1),
        json!(2),
        json!(-1),
        json!(-0.0),
        json!(1.0),
        json!(u64::MAX),
        json!(""),
        json!("é\n\"\\"),
        json!([]),
        json!({}),
        json!(["nested"]),
        json!({"unknown":[null]}),
    ];
    for fixture in cases::fixtures() {
        let value = serde_json::to_value(&fixture).unwrap();
        let object = value.as_object().unwrap();
        let mut fields: Vec<_> = object.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        permutations(&mut fields, 0);
        for key in object.keys() {
            for replacement in &values {
                let mut changed = value.clone();
                changed[key] = replacement.clone();
                compare(encoded(&changed).as_bytes());
                let mut duplicate = encoded(&value);
                duplicate.pop();
                duplicate.push_str(&format!(
                    ",{}:{}}}",
                    serde_json::to_string(key).unwrap(),
                    encoded(replacement)
                ));
                compare(duplicate.as_bytes());
            }
            let mut removed = value.clone();
            removed.as_object_mut().unwrap().remove(key);
            compare(encoded(&removed).as_bytes());
        }
        for extra in &values {
            let mut changed = value.clone();
            changed["unknown"] = extra.clone();
            compare(encoded(&changed).as_bytes());
        }
        for depth in [1, 64, 120, 126, 127, 128, 129, 130, 140] {
            let mut prefix = encoded(&value);
            prefix.pop();
            let frame = format!(
                "{prefix},\"unknown\":{}null{}}}",
                "[".repeat(depth),
                "]".repeat(depth)
            );
            compare(frame.as_bytes());
        }
        let text = encoded(&value);
        compare(&text.as_bytes()[..text.len() - 1]);
        compare(format!("{text} trailing").as_bytes());
        compare(text.replace("\"status\"", "\"sta\\u0074us\"").as_bytes());
        compare(text.replace("\"id\"", "\"\\u0069d\"").as_bytes());
        compare(
            text.replace("\"status\":\"ok\"", "\"status\":18446744073709551616")
                .as_bytes(),
        );
    }
    for value in values {
        compare(encoded(&value).as_bytes());
    }
}

#[test]
fn numeric_status_tags_and_structured_error_values_remain_compatible() {
    let ack: ResponseEnvelope =
        serde_json::from_str(r#"{"status":0,"type":"ack","id":18446744073709551615}"#).unwrap();
    assert_eq!(ack.id, Some(u64::MAX));
    assert!(matches!(ack.result, ResponseResult::Ok { .. }));
    let response: ResponseEnvelope = serde_json::from_str(r#"{"status":1,"code":"synthetic","message":"é\n\"\\","details":{"unknown":[null,1]},"id":null}"#).unwrap();
    match response.result {
        ResponseResult::Error { error } => {
            assert_eq!(error.code, "synthetic");
            assert_eq!(error.message, "é\n\"\\");
            assert_eq!(error.details, Some(json!({"unknown":[null,1]})));
        }
        ResponseResult::Ok { .. } => panic!("an error became a success"),
    }
    for frame in [
        b"\xff".as_slice(),
        b"{\"status\":\"ok\",\"type\":\"ack\",\"unknown\":\"\xff\"}",
        b"{}",
        b"{\"status\":\"unknown\"}",
    ] {
        assert!(cases::baseline(frame).is_err());
        assert!(cases::candidate(frame).is_err());
    }
}

#[test]
fn structured_error_details_keep_the_reference_map_order() {
    compare(br#"{"id":null,"status":"error","message":"synthetic","code":"synthetic","details":{"z":1,"a":2,"nested":{"y":1,"b":2}}}"#);
    for frame in [
        br#"{"sta\uD800tus":"ok","type":"ack"}"#.as_slice(),
        br#"{"status":"ok","type":"ack","id":-0}"#,
        br#"{"status":"ok","type":"ack","id":1e0}"#,
    ] {
        compare(frame);
    }
}

// A self-describing generic key deserializer. serde_json::Value's own
// deserialize_identifier deliberately requires text, which would hide the
// non-string-key behavior this regression test needs to exercise.
#[derive(Clone)]
struct GenericKey(Value);
impl<'de> serde::Deserializer<'de> for GenericKey {
    type Error = serde_json::Error;
    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        serde::Deserializer::deserialize_any(self.0, visitor)
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier ignored_any
    }
}
impl<'de> serde::de::IntoDeserializer<'de, serde_json::Error> for GenericKey {
    type Deserializer = Self;
    fn into_deserializer(self) -> Self {
        self
    }
}
#[test]
fn generic_map_keys_match_the_previous_flattened_decoder() {
    use serde::Deserialize;
    use serde::de::value::{MapAccessDeserializer, MapDeserializer};
    let keys = [
        json!(0),
        json!(1),
        json!(2),
        json!(u64::MAX),
        json!(-1),
        json!(1.5),
        json!(7),
        json!(true),
        json!(null),
        json!([7]),
        json!({"synthetic":7}),
    ];
    for key in keys {
        let entries = vec![
            (GenericKey(key.clone()), json!(null)),
            (GenericKey(json!("status")), json!("ok")),
            (GenericKey(json!("type")), json!("ack")),
        ];
        let legacy = cases::LegacyEnvelope::deserialize(MapAccessDeserializer::new(
            MapDeserializer::new(entries.clone().into_iter()),
        ))
        .map(|v| ResponseEnvelope {
            id: v.id,
            result: v.result,
        });
        let current = ResponseEnvelope::deserialize(MapAccessDeserializer::new(
            MapDeserializer::new(entries.into_iter()),
        ));
        if key == json!(7) {
            assert!(
                legacy.is_ok(),
                "the reference accepts an unknown numeric key"
            );
        }
        match (legacy, current) {
            (Ok(base), Ok(head)) => assert_eq!(base, head),
            (Err(_), Err(_)) => {}
            (base, head) => panic!("generic key {key}: base={base:?}, head={head:?}"),
        }
    }
}

#[test]
fn generic_byte_keys_and_numeric_status_keep_the_legacy_behavior() {
    use serde::Deserialize;
    use serde::de::value::{BorrowedBytesDeserializer, MapAccessDeserializer, MapDeserializer};
    for unknown in [b"unknown".as_slice(), b"\xff"] {
        let entries = vec![
            (
                BorrowedBytesDeserializer::<serde_json::Error>::new(unknown),
                json!(null),
            ),
            (BorrowedBytesDeserializer::new(b"status"), json!(0)),
            (BorrowedBytesDeserializer::new(b"type"), json!("ack")),
            (BorrowedBytesDeserializer::new(b"id"), json!(u64::MAX)),
        ];
        let legacy = cases::LegacyEnvelope::deserialize(MapAccessDeserializer::new(
            MapDeserializer::new(entries.clone().into_iter()),
        ))
        .unwrap();
        let current = ResponseEnvelope::deserialize(MapAccessDeserializer::new(
            MapDeserializer::new(entries.into_iter()),
        ))
        .unwrap();
        assert_eq!(legacy.id, Some(u64::MAX));
        assert_eq!(current.id, legacy.id);
        assert_eq!(current.result, legacy.result);
    }
}

struct MapOnly<D>(D);
impl<'de, D: serde::Deserializer<'de>> serde::Deserializer<'de> for MapOnly<D> {
    type Error = D::Error;
    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        _visitor: V,
    ) -> Result<V::Value, D::Error> {
        Err(serde::de::Error::custom(
            "root deserialize_any is unavailable",
        ))
    }
    fn deserialize_map<V: serde::de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_map(visitor)
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct struct enum identifier ignored_any
    }
}
#[test]
fn root_deserializers_that_support_maps_keep_working_without_deserialize_any() {
    use serde::Deserialize;
    let frame = r#"{"id":1,"status":"ok","type":"ack"}"#;
    let mut base = serde_json::Deserializer::from_str(frame);
    let base = cases::LegacyEnvelope::deserialize(MapOnly(&mut base)).unwrap();
    let mut head = serde_json::Deserializer::from_str(frame);
    let head = ResponseEnvelope::deserialize(MapOnly(&mut head)).unwrap();
    assert_eq!(base.id, head.id);
    assert_eq!(base.result, head.result);
}

#[derive(Clone)]
enum GenericValue {
    Json(Value),
    Bytes(&'static [u8]),
}
impl<'de> serde::Deserializer<'de> for GenericValue {
    type Error = serde_json::Error;
    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self {
            Self::Json(value) => serde::Deserializer::deserialize_any(value, visitor),
            Self::Bytes(value) => visitor.visit_borrowed_bytes(value),
        }
    }
    fn deserialize_u64<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self {
            Self::Json(Value::String(value)) => {
                visitor.visit_u64(value.parse::<u64>().map_err(serde::de::Error::custom)?)
            }
            other => serde::Deserializer::deserialize_any(other, visitor),
        }
    }
    fn deserialize_option<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self {
            Self::Json(Value::Null) => visitor.visit_none(),
            other => visitor.visit_some(other),
        }
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u128 f32 f64 char str string bytes byte_buf
        unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier ignored_any
    }
}
impl<'de> serde::de::IntoDeserializer<'de, serde_json::Error> for GenericValue {
    type Deserializer = Self;
    fn into_deserializer(self) -> Self {
        self
    }
}
#[test]
fn generic_byte_tag_values_match_the_reference_acceptance() {
    use serde::Deserialize;
    use serde::de::value::{BorrowedBytesDeserializer, MapAccessDeserializer, MapDeserializer};
    for status in [
        GenericValue::Json(json!("ok")),
        GenericValue::Bytes(b"ok"),
        GenericValue::Json(json!(0)),
    ] {
        for kind in [
            GenericValue::Json(json!("ack")),
            GenericValue::Bytes(b"ack"),
            GenericValue::Json(json!(0)),
        ] {
            let entries = vec![
                (
                    BorrowedBytesDeserializer::<serde_json::Error>::new(b"status"),
                    status.clone(),
                ),
                (BorrowedBytesDeserializer::new(b"type"), kind),
                (
                    BorrowedBytesDeserializer::new(b"unknown"),
                    GenericValue::Bytes(b"\xff"),
                ),
            ];
            let base = cases::LegacyEnvelope::deserialize(MapAccessDeserializer::new(
                MapDeserializer::new(entries.clone().into_iter()),
            ))
            .map(|v| ResponseEnvelope {
                id: v.id,
                result: v.result,
            });
            let head = ResponseEnvelope::deserialize(MapAccessDeserializer::new(
                MapDeserializer::new(entries.into_iter()),
            ));
            match (base, head) {
                (Ok(base), Ok(head)) => assert_eq!(base, head),
                (Err(_), Err(_)) => {}
                (base, head) => panic!("byte tag acceptance changed: base={base:?}, head={head:?}"),
            }
        }
    }
}

#[test]
fn generic_key_identifier_restrictions_are_not_bypassed() {
    use serde::Deserialize;
    use serde::de::value::{MapAccessDeserializer, MapDeserializer};
    // Value's identifier method rejects a numeric key even though its any
    // method accepts it. Keep the old entry point, not just the resulting type.
    let entries = vec![
        (json!(7), json!(null)),
        (json!("status"), json!("ok")),
        (json!("type"), json!("ack")),
    ];
    let base = cases::LegacyEnvelope::deserialize(MapAccessDeserializer::new(
        MapDeserializer::new(entries.clone().into_iter()),
    ));
    let head = ResponseEnvelope::deserialize(MapAccessDeserializer::new(MapDeserializer::new(
        entries.into_iter(),
    )));
    assert!(base.is_err());
    assert!(head.is_err());
}
#[test]
fn generic_id_option_and_integer_hints_match_the_reference() {
    use serde::Deserialize;
    use serde::de::value::{MapAccessDeserializer, MapDeserializer};
    for key in [json!("id"), json!(0)] {
        for id in [
            json!("17"),
            json!("18446744073709551615"),
            json!(null),
            json!("invalid"),
            json!("18446744073709551616"),
        ] {
            let entries = vec![
                (GenericKey(key.clone()), GenericValue::Json(id.clone())),
                (GenericKey(json!("status")), GenericValue::Json(json!("ok"))),
                (GenericKey(json!("type")), GenericValue::Json(json!("ack"))),
            ];
            let base = cases::LegacyEnvelope::deserialize(MapAccessDeserializer::new(
                MapDeserializer::new(entries.clone().into_iter()),
            ))
            .map(|v| ResponseEnvelope {
                id: v.id,
                result: v.result,
            });
            let head = ResponseEnvelope::deserialize(MapAccessDeserializer::new(
                MapDeserializer::new(entries.into_iter()),
            ));
            if key == json!("id") && id == json!("17") {
                assert_eq!(base.as_ref().unwrap().id, Some(17));
            }
            match (base, head) {
                (Ok(base), Ok(head)) => assert_eq!(base, head),
                (Err(base), Err(head)) => assert_eq!(base.to_string(), head.to_string()),
                (base, head) => panic!("id hint changed for {id}: base={base:?}, head={head:?}"),
            }
        }
    }
}
