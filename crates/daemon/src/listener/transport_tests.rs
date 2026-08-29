//! What the transport has to do, on whichever platform is running.
//!
//! These are here rather than in `oxidezap-ipc` because they need both ends: a
//! real listener and a real client, talking over the real thing. And they earn
//! their keep on exactly one platform at a time — the properties below are
//! free on a Unix socket and were both broken on Windows, where a pipe opened
//! the ordinary way serializes reads against writes.
//!
//! That is the shape of every bug this file exists for: code that compiles
//! everywhere and only *works* somewhere. `cargo check` cannot see it, which
//! is why CI runs this on all three.

use std::io::{BufRead as _, BufReader, Write as _};
use std::sync::mpsc;
use std::time::Duration;

use oxidezap_ipc::Endpoint;

use super::Listener;

/// Long enough that a slow CI runner is not the reason, short enough that a
/// deadlock is reported rather than waited out.
const PATIENCE: Duration = Duration::from_secs(10);

/// A listener, and the runtime it is registered with.
///
/// Binding registers with tokio's reactor, so it has to happen inside a
/// runtime — and before the client connects, or the connect races the bind.
fn bound(path: &std::path::Path) -> (tokio::runtime::Runtime, Listener) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let listener = runtime
        .block_on(async { Listener::bind(path) })
        .expect("bind");
    (runtime, listener)
}

/// An endpoint name nothing else is using.
///
/// A path under the temporary directory where the endpoint is a file, and a
/// pipe name where it is a name — the two are not the same kind of thing, and
/// `Listener` takes whichever the platform wants.
fn scratch_endpoint(test: &str) -> std::path::PathBuf {
    use portable_atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let unique = format!(
        "oxidezap-test-{}-{}-{test}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );

    #[cfg(windows)]
    {
        std::path::PathBuf::from(format!(r"\\.\pipe\{unique}"))
    }
    #[cfg(not(windows))]
    {
        let dir = std::env::temp_dir().join(&unique);
        let _ = std::fs::create_dir_all(&dir);
        dir.join("daemon.sock")
    }
}

/// The one that matters, and the one no amount of compiling would have found.
///
/// A front end parks a thread in a read for as long as the connection lives —
/// that is the steady state, not an edge — and writes requests from another
/// thread while it does. On Windows a pipe opened without `FILE_FLAG_OVERLAPPED`
/// serializes the two, so the write waits for the read, and the read is
/// waiting for the answer to the write. Nothing arrives, so nothing returns:
/// the window stops responding the first time the user opens an unread chat,
/// opens Settings, or places a call.
///
/// The daemon deliberately says nothing here. That is what makes the read park
/// and the test mean something.
#[test]
fn a_write_does_not_wait_for_a_parked_read() {
    let path = scratch_endpoint("parked-read");
    let (runtime, mut listener) = bound(&path);

    // Accept and then hold the connection, silently, until this test says so.
    // Both halves of that matter: a daemon that answered would complete the
    // parked read, and a daemon that hung up would complete it with EOF —
    // either one lets the write through and hides the bug.
    let (release, until_told) = mpsc::channel::<()>();
    let held = std::thread::spawn(move || {
        let stream = runtime.block_on(listener.accept()).expect("accept");
        let _ = until_told.recv();
        drop(stream);
    });

    let (reader, mut writer) = Endpoint::connect_at(&path)
        .expect("connect")
        .split()
        .expect("split");

    // Parked for the rest of the test: nothing is ever sent to it.
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
    });
    // Give the read time to actually be pending. Without this the write can
    // win the race and pass on a platform where it would otherwise hang.
    std::thread::sleep(Duration::from_millis(200));

    let (done, wrote) = mpsc::channel();
    std::thread::spawn(move || {
        let result = writer.write_all(b"{\"request\":\"snapshot\"}\n");
        let _ = done.send(result.is_ok());
    });

    let outcome = wrote.recv_timeout(PATIENCE);
    drop(release);
    let _ = held.join();
    match outcome {
        Ok(true) => {}
        Ok(false) => panic!("the write failed"),
        Err(_) => panic!(
            "a write blocked behind a read that nothing will complete — \
             the transport serializes the two directions"
        ),
    }
}

/// And the bytes actually make it, both ways.
///
/// The guard above would pass against a transport that accepted writes and
/// dropped them on the floor, which is a way to get overlapped I/O wrong:
/// start the operation, never wait for it. This is the other half.
#[test]
fn frames_survive_the_transport_in_both_directions() {
    let path = scratch_endpoint("round-trip");
    let (runtime, mut listener) = bound(&path);

    let served = std::thread::spawn(move || {
        runtime.block_on(async move {
            use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

            let stream = listener.accept().await.expect("accept");
            let (reader, mut writer) = tokio::io::split(stream);
            let mut reader = tokio::io::BufReader::new(reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("read the request");
            // Answered, so the client's own read has something to complete on.
            writer
                .write_all(b"{\"type\":\"accepted\"}\n")
                .await
                .expect("answer");
            line
        })
    });

    let (reader, mut writer) = Endpoint::connect_at(&path)
        .expect("connect")
        .split()
        .expect("split");
    writer
        .write_all(b"{\"request\":\"snapshot\"}\n")
        .expect("write");

    let mut reader = BufReader::new(reader);
    let mut answer = String::new();
    reader.read_line(&mut answer).expect("read the answer");
    assert_eq!(answer.trim_end(), r#"{"type":"accepted"}"#);

    let request = served.join().expect("the server thread");
    assert_eq!(request.trim_end(), r#"{"request":"snapshot"}"#);
}

/// A frame larger than one read, so a short read is not mistaken for the end
/// of it. Overlapped reads return what one operation moved, which for a big
/// payload is less than was asked for.
#[test]
fn a_frame_larger_than_a_buffer_arrives_whole() {
    let path = scratch_endpoint("large-frame");
    let (runtime, mut listener) = bound(&path);

    let payload = "x".repeat(256 * 1024);
    let sent = payload.clone();
    let (read_it_all, until_read) = mpsc::channel::<()>();
    let served = std::thread::spawn(move || {
        runtime.block_on(async move {
            use tokio::io::AsyncWriteExt as _;
            let mut stream = listener.accept().await.expect("accept");
            stream.write_all(sent.as_bytes()).await.expect("write");
            stream.write_all(b"\n").await.expect("terminate");
            stream.flush().await.expect("flush");
            // Held open until the client says it has the whole thing, so a
            // slow read cannot be cut short by the server going away.
            let _ = until_read.recv();
        });
    });

    let (reader, _writer) = Endpoint::connect_at(&path)
        .expect("connect")
        .split()
        .expect("split");
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read");
    drop(read_it_all);
    served.join().expect("the server thread");

    assert_eq!(line.trim_end().len(), payload.len());
}

/// A front end reconnects by dropping its connection and opening another, and
/// the read half is a thread parked in the kernel that nothing wakes: the
/// daemon went on counting a connection with nobody behind it. Thirty-two
/// network blips filled `MAX_CLIENTS` and the window never connected again.
///
/// Counted on the *server's* side, because that is the count that ran out.
#[test]
fn reconnecting_does_not_leak_a_client() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Enough that a leak shows as a count above one rather than as luck.
    const RECONNECTS: usize = 8;

    let path = scratch_endpoint("reconnect-leak");
    let (runtime, mut listener) = bound(&path);

    let live = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&live);
    // A tokio channel rather than a `std` one: this runtime is
    // single-threaded, and a blocking wait inside it would starve the reads
    // spawned below — the count would then never come down and the test
    // would be measuring the wait rather than the connections.
    let (all_done, mut until_done) = tokio::sync::mpsc::unbounded_channel::<()>();
    let served = std::thread::spawn(move || {
        runtime.block_on(async move {
            for _ in 0..RECONNECTS {
                let mut stream = listener.accept().await.expect("accept");
                counted.fetch_add(1, Ordering::SeqCst);
                let held = Arc::clone(&counted);
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt as _;
                    // Says nothing, so only the client hanging up ends this.
                    let mut sink = Vec::new();
                    let _ = stream.read_to_end(&mut sink).await;
                    held.fetch_sub(1, Ordering::SeqCst);
                });
            }
            let _ = until_done.recv().await;
        });
    });

    let mut highest = 0;
    for _ in 0..RECONNECTS {
        let (reader, writer) = Endpoint::connect_at(&path)
            .expect("connect")
            .split()
            .expect("split");
        let hangup = reader.hangup().expect("a way to end the read");
        let (alive, until_gone) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _alive = alive;
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            // Parked exactly as the front end's reader is: the daemon has
            // nothing to say, so nothing completes this but the hangup.
            let _ = reader.read_line(&mut line);
        });
        std::thread::sleep(Duration::from_millis(50));
        highest = highest.max(live.load(Ordering::SeqCst));

        // The order a `Session` goes in: the write half first, then the
        // hangup that ends the read. It is load-bearing on a named pipe,
        // where cancelling the read does not disconnect anything — the pipe
        // breaks when the last handle to it closes, and the reader's is the
        // last only once this one has gone. A socket is shut down instead,
        // so there the order does not show.
        drop(writer);
        hangup.hang_up();
        assert!(
            matches!(
                until_gone.recv_timeout(PATIENCE),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ),
            "the reader did not leave after the connection was hung up on"
        );
        // The server side is told by the same close, which it learns of on
        // its own runtime rather than on this thread.
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(all_done);
    served.join().expect("the server thread");

    assert_eq!(
        highest, 1,
        "a reconnect left the last connection open: the daemon saw {highest} at once"
    );
    assert_eq!(
        live.load(Ordering::SeqCst),
        0,
        "connections outlived the front ends that opened them"
    );
}
