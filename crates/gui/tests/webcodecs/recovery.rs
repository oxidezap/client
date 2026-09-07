#![cfg(target_family = "wasm")]

use oxidezap_webcodecs_tests::h264::{AccessUnits, nal_unit_type, split_annexb};
use wasm_bindgen::prelude::*;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

mod h264_fixture;

#[wasm_bindgen(module = "/recovery.js")]
extern "C" {
    #[wasm_bindgen(catch, js_name = encodeRecoveryFrames)]
    async fn encode() -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch, js_name = decodeAcrossReset)]
    async fn decode(key: js_sys::Uint8Array, delta: js_sys::Uint8Array)
    -> Result<JsValue, JsValue>;
}

fn annexb(nals: impl IntoIterator<Item = impl AsRef<[u8]>>) -> Vec<u8> {
    let mut data = Vec::new();
    for nal in nals {
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(nal.as_ref());
    }
    data
}

#[wasm_bindgen_test]
async fn generated_idr_and_delta_decode_before_and_after_reset() {
    let chunks = js_sys::Array::from(&encode().await.expect("browser H.264 encoder"));
    let idr = js_sys::Uint8Array::new(&chunks.get(0)).to_vec();
    let delta = js_sys::Uint8Array::new(&chunks.get(1)).to_vec();
    let mut units = AccessUnits::default();
    let (prepared, key) = units
        .prepare(&idr)
        .expect("generated IDR references its sets")
        .unwrap();
    assert!(key);
    let (_, key) = units.prepare(&delta).expect("generated delta").unwrap();
    assert!(!key);
    let outputs = decode(
        js_sys::Uint8Array::from(prepared.as_ref()),
        js_sys::Uint8Array::from(delta.as_slice()),
    )
    .await
    .expect("real decoder before and after reset");
    assert_eq!(
        js_sys::Array::from(&outputs)
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect::<Vec<_>>(),
        [0.0, 33333.0, 66666.0, 99999.0]
    );
}

#[wasm_bindgen_test]
async fn split_parameter_sets_reconstruct_a_decodable_recovery_chunk() {
    let chunks = js_sys::Array::from(&encode().await.unwrap());
    let idr = js_sys::Uint8Array::new(&chunks.get(0)).to_vec();
    let delta = js_sys::Uint8Array::new(&chunks.get(1)).to_vec();
    let picture = annexb(split_annexb(&idr).filter(|nal| !matches!(nal_unit_type(nal), 7..=9)));
    assert!(AccessUnits::default().prepare(&picture).is_err());
    let mut units = AccessUnits::default();
    for nal in split_annexb(&idr).filter(|nal| matches!(nal_unit_type(nal), 7 | 8)) {
        assert!(units.prepare(&annexb([nal])).unwrap().is_none());
    }
    assert!(!units.prepare(&delta).unwrap().unwrap().1);
    // Filler-payload SEI with an empty payload, followed by rbsp_trailing_bits.
    let mut leading = annexb([&[9, 0xf0][..], &[6, 3, 0, 0x80]]);
    leading.extend(picture);
    let (prepared, key) = units.prepare(&leading).unwrap().unwrap();
    assert!(key);
    assert_eq!(nal_unit_type(split_annexb(&prepared).next().unwrap()), 9);
    let outputs = decode(
        js_sys::Uint8Array::from(prepared.as_ref()),
        js_sys::Uint8Array::from(delta.as_slice()),
    )
    .await
    .unwrap();
    assert_eq!(js_sys::Array::from(&outputs).length(), 4);
}

#[wasm_bindgen_test]
async fn repeated_parameter_sets_do_not_break_the_delta_chain() {
    let chunks = js_sys::Array::from(&encode().await.unwrap());
    let idr = js_sys::Uint8Array::new(&chunks.get(0)).to_vec();
    let delta = js_sys::Uint8Array::new(&chunks.get(1)).to_vec();
    let sets = annexb(split_annexb(&idr).filter(|nal| matches!(nal_unit_type(nal), 7 | 8)));
    let mut units = AccessUnits::default();
    let (key, _) = units.prepare(&idr).unwrap().unwrap();
    for _ in 0..2 {
        assert!(units.prepare(&sets).unwrap().is_none());
    }
    let (prepared_delta, is_key) = units.prepare(&delta).unwrap().unwrap();
    assert!(!is_key);
    let outputs = decode(
        js_sys::Uint8Array::from(key.as_ref()),
        js_sys::Uint8Array::from(prepared_delta.as_ref()),
    )
    .await
    .unwrap();
    assert_eq!(js_sys::Array::from(&outputs).length(), 4);
    assert!(matches!(
        units.prepare(&delta).unwrap().unwrap().0,
        std::borrow::Cow::Borrowed(_)
    ));
}

#[wasm_bindgen_test]
async fn a_later_delta_can_use_pps_one_announced_with_or_after_the_idr() {
    let chunks = js_sys::Array::from(&encode().await.unwrap());
    let idr = js_sys::Uint8Array::new(&chunks.get(0)).to_vec();
    let delta = js_sys::Uint8Array::new(&chunks.get(1)).to_vec();
    assert_eq!(
        split_annexb(&idr)
            .find(|nal| nal_unit_type(nal) == 7)
            .unwrap()[1],
        66
    );
    let pps1 = h264_fixture::with_pps_id(
        split_annexb(&idr)
            .find(|nal| nal_unit_type(nal) == 8)
            .unwrap(),
        1,
    );
    let delta1 = annexb(split_annexb(&delta).map(|nal| {
        if nal_unit_type(nal) == 1 {
            h264_fixture::with_pps_id(nal, 1)
        } else {
            nal.to_vec()
        }
    }));
    let mut announced = Vec::new();
    for nal in split_annexb(&idr) {
        announced.extend(annexb([nal]));
        if nal_unit_type(nal) == 8 {
            announced.extend(annexb([&pps1]));
        }
    }
    let raw_outputs = decode(
        js_sys::Uint8Array::from(announced.as_slice()),
        js_sys::Uint8Array::from(delta1.as_slice()),
    )
    .await
    .unwrap();
    assert_eq!(
        js_sys::Array::from(&raw_outputs).length(),
        4,
        "header edits must decode directly"
    );
    for separate in [false, true] {
        let mut units = AccessUnits::default();
        let (key, _) = units
            .prepare(if separate { &idr } else { &announced })
            .unwrap()
            .unwrap();
        if separate {
            assert!(units.prepare(&annexb([&pps1])).unwrap().is_none());
        }
        let (next, is_key) = units.prepare(&delta1).unwrap().unwrap();
        assert!(!is_key);
        let outputs = decode(
            js_sys::Uint8Array::from(key.as_ref()),
            js_sys::Uint8Array::from(next.as_ref()),
        )
        .await
        .unwrap();
        assert_eq!(js_sys::Array::from(&outputs).length(), 4);
        let (recovery_key, _) = units.prepare(&idr).unwrap().unwrap();
        let (next, _) = units.prepare(&delta1).unwrap().unwrap();
        let outputs = decode(
            js_sys::Uint8Array::from(recovery_key.as_ref()),
            js_sys::Uint8Array::from(next.as_ref()),
        )
        .await
        .unwrap();
        assert_eq!(
            js_sys::Array::from(&outputs).length(),
            4,
            "reset must retain cached PPS 1 for later deltas"
        );
    }
}
