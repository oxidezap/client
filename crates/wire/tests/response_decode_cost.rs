//! Synthetic decode-and-drop timing; excludes socket, startup, stdout and daemon.
#[path = "support/response_cases.rs"]
mod cases;
use std::hint::black_box;

// Native measurement only; the wire crate has no wacore dependency.
#[allow(clippy::disallowed_methods)]
fn measure(
    frame: &[u8],
    decode: fn(&[u8]) -> serde_json::Result<oxidezap_wire::ResponseEnvelope>,
    rounds: usize,
) -> u128 {
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        drop(black_box(decode(black_box(frame))));
    }
    start.elapsed().as_nanos() / rounds as u128
}
#[allow(clippy::print_stdout)]
fn paired(label: &str, frame: &[u8], rounds: usize) {
    assert_eq!(cases::baseline(frame).ok(), cases::candidate(frame).ok());
    drop(cases::baseline(frame));
    drop(cases::candidate(frame));
    for pair in 0..12 {
        let (base, head) = if pair % 2 == 0 {
            (
                measure(frame, cases::baseline, rounds),
                measure(frame, cases::candidate, rounds),
            )
        } else {
            let head = measure(frame, cases::candidate, rounds);
            (measure(frame, cases::baseline, rounds), head)
        };
        println!(
            "{label} pair={pair} rounds={rounds} bytes={} base_ns={base} head_ns={head}",
            frame.len()
        );
    }
}
#[test]
#[ignore = "release-only paired timing comparison"]
fn response_decode_timing() {
    let rounds = std::env::var("OXIDEZAP_BENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(200);
    for count in [1, 100, 1000] {
        for escaped in [false, true] {
            paired(
                &format!("chats={count} escaped={escaped}"),
                &cases::chats(count, escaped),
                rounds,
            );
        }
    }
    for (i, response) in cases::fixtures().iter().enumerate() {
        paired(
            &format!("small={i}"),
            &serde_json::to_vec(response).unwrap(),
            rounds * 5,
        );
    }
    for (i, frame) in [
        r#"{}"#,
        r#"{"id":1,"status":"wrong"}"#,
        r#"{"status":0,"type":"ack"}"#,
        r#"{"status":1,"code":"synthetic","message":"synthetic"}"#,
    ]
    .iter()
    .enumerate()
    {
        paired(&format!("edge={i}"), frame.as_bytes(), rounds * 5);
    }
}
#[test]
#[ignore = "isolated Callgrind baseline entry point"]
fn response_decode_instructions_baseline() {
    let frame = cases::chats(100, true);
    for _ in 0..100 {
        drop(black_box(cases::baseline(black_box(&frame)).unwrap()));
    }
}
#[test]
#[ignore = "isolated Callgrind production entry point"]
fn response_decode_instructions_candidate() {
    let frame = cases::chats(100, true);
    for _ in 0..100 {
        drop(black_box(cases::candidate(black_box(&frame)).unwrap()));
    }
}
