#![cfg(not(target_family = "wasm"))]

//! The mock half of the outbound-video proof, owned by the mock lane.
//!
//! A scripted video peer behind the bartender mock is the only way to replay
//! a failing call without the phone: tonight's joint capture shows the phone
//! PLI-storming our SSRC while its decoder renders nothing, and no unit test
//! can produce that peer. This test states exactly what the mock must do,
//! step by step, and fails until it does. It stays `ignore`d so CI is green
//! while the mock lane builds the peer; run it with
//! `MOCK_SERVER_URL=ws://127.0.0.1:8080/ws/chat cargo test -p oxidezap-session
//! --test mock_video_peer_contract -- --ignored`.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// The same convention as the upstream e2e suite: `MOCK_SERVER_URL` names the
/// chat WebSocket, and the admin surface lives on the same host and port over
/// plain HTTP.
fn admin_base() -> (String, String) {
    let url =
        std::env::var("MOCK_SERVER_URL").unwrap_or_else(|_| "ws://127.0.0.1:8080/ws/chat".into());
    let after_scheme = url.split("://").nth(1).unwrap_or(&url);
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    (host_port.into(), format!("http://{host_port}"))
}

/// Minimal HTTP GET over a plain TCP stream: no client dependency for a probe.
fn http_get(host_port: &str, path: &str) -> String {
    let addr = host_port
        .to_socket_addrs()
        .expect("MOCK_SERVER_URL must resolve to host:port")
        .next()
        .expect("MOCK_SERVER_URL must resolve to host:port");
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .expect("mock lane: the bartender mock must be listening (step 1)");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("socket timeout");
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {host_port}\r\nConnection: close\r\n\r\n"
    )
    .expect("mock lane: the mock must accept the admin GET (step 1)");
    let mut body = String::new();
    stream
        .read_to_string(&mut body)
        .expect("mock lane: the mock must answer the admin GET (step 1)");
    body
}

#[test]
#[ignore = "needs a bartender mock with a scripted video peer (mock lane); see body"]
fn mock_video_peer_renders_our_outbound_stream() {
    let (host_port, base) = admin_base();
    // Step 1, runnable today: the mock answers its admin surface and names
    // the WebRTC relay fingerprint a real peer would bind against.
    let answer = http_get(&host_port, "/admin/relay-fingerprint");
    assert!(
        answer.starts_with("HTTP/1.0 200") || answer.starts_with("HTTP/1.1 200"),
        "mock lane: GET {base}/admin/relay-fingerprint must return 200, got: {}",
        answer.chars().take(200).collect::<String>()
    );
    // Step 2, the mock lane's build: a scripted video peer that answers our
    // video offer through the mock, binds the relay, emits PLI naming our
    // SSRC while it cannot decode, and then — the verdict this whole lane
    // waits for — reports the first rendered frame. Until that peer exists,
    // the replay tests in the session's call registry (fed by production and
    // device fixtures) are the proof that stands.
    unimplemented!(
        "mock lane: scripted video peer missing (offer accept + relay bind + \
         PLI-then-render verdict for our outbound SSRC)"
    );
}
