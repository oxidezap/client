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
