#![cfg(target_family = "wasm")]

use wasm_bindgen::prelude::*;
use wasm_bindgen_test::wasm_bindgen_test;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen(inline_js = r#"
export function probe(mode) {
    const Context = globalThis.AudioContext;
    const gum = navigator.mediaDevices.getUserMedia;
    const clone = MediaStream.prototype.clone;
    const trackClone = MediaStreamTrack.prototype.clone;
    const set = globalThis.setTimeout, clear = globalThis.clearTimeout;
    const tracks = new Set(), contexts = [], nodes = [], timers = new Map();
    const sourceContext = new Context();
    const oscillator = sourceContext.createOscillator();
    const destination = sourceContext.createMediaStreamDestination();
    oscillator.connect(destination);
    oscillator.start();
    sourceContext.resume();
    let resolvePermission, resolveResume;
    let calls = 0;
    const remember = stream => { for (const t of stream.getTracks()) tracks.add(t); return stream; };
    const stream = remember(destination.stream);
    MediaStream.prototype.clone = function() { return remember(clone.call(this)); };
    MediaStreamTrack.prototype.clone = function() { const t = trackClone.call(this); tracks.add(t); return t; };
    navigator.mediaDevices.getUserMedia = function() {
        calls++;
        if (mode === 'permission') return new Promise(resolve => { resolvePermission = resolve; });
        return Promise.resolve(stream);
    };
    const create = BaseAudioContext.prototype.createScriptProcessor;
    const connect = AudioNode.prototype.connect;
    const resume = Context.prototype.resume;
    const sampleRate = Object.getOwnPropertyDescriptor(BaseAudioContext.prototype, 'sampleRate');
    Object.defineProperty(BaseAudioContext.prototype, 'sampleRate', { ...sampleRate, get() {
        if (!contexts.includes(this)) contexts.push(this);
        return sampleRate.get.call(this);
    }});
    BaseAudioContext.prototype.createScriptProcessor = function(...args) {
        if (!contexts.includes(this)) contexts.push(this);
        if (mode === 'node' && nodes.length === 1) throw new Error('injected node failure');
        const node = create.apply(this, args);
        nodes.push(node);
        return node;
    };
    AudioNode.prototype.connect = function(...args) {
        if (mode === 'connect' && this === nodes[1]) throw new Error('injected connect failure');
        return connect.apply(this, args);
    };
    Context.prototype.resume = function() {
        if (mode === 'resume') return Promise.reject(new Error('injected resume failure'));
        if (mode === 'cancel') return new Promise(resolve => { resolveResume = resolve; });
        return resume.call(this);
    };
    globalThis.setTimeout = function(callback, ms, ...args) {
        const id = set(() => { timers.delete(id); callback(...args); }, ms);
        if (ms === 5000 || ms === 30000) timers.set(id, callback);
        return id;
    };
    globalThis.clearTimeout = function(id) { timers.delete(id); clear(id); };
    return {
        stream, tracks, contexts, nodes, timers,
        permission() { resolvePermission(stream); },
        ready() { return mode === 'permission' ? calls > 0 : !!resolveResume; },
        timeout() { for (const [id, callback] of timers) { clear(id); timers.delete(id); callback(); } },
        async restore() {
            globalThis.AudioContext = Context;
            BaseAudioContext.prototype.createScriptProcessor = create;
            AudioNode.prototype.connect = connect;
            Context.prototype.resume = resume;
            Object.defineProperty(BaseAudioContext.prototype, 'sampleRate', sampleRate);
            navigator.mediaDevices.getUserMedia = gum;
            MediaStream.prototype.clone = clone;
            MediaStreamTrack.prototype.clone = trackClone;
            globalThis.setTimeout = set; globalThis.clearTimeout = clear;
            for (const id of timers.keys()) clear(id);
            for (const t of tracks) { t.onended = null; t.stop(); }
            for (const n of nodes) { n.onaudioprocess = null; n.disconnect(); }
            for (const c of contexts) if (c.state !== 'closed') await c.close();
            oscillator.stop(); await sourceContext.close();
        }
    };
}
export function ready(p) { return p.ready(); }
export function permission(p) { p.permission(); }
export function timeout(p) { p.timeout(); }
export async function settle() { await new Promise(resolve => setTimeout(resolve, 50)); }
export function metrics(p) {
    return JSON.stringify({tracks:p.tracks.size, live:[...p.tracks].filter(t=>t.readyState==='live').length,
        endedHandlers:[...p.tracks].filter(t=>t.onended!==null).length,
        audioHandlers:p.nodes.filter(n=>n.onaudioprocess!==null).length,
        openContexts:p.contexts.filter(c=>c.state!=='closed').length, timers:p.timers.size});
}
export function clean(p) {
    return p.contexts.length===1 && [...p.tracks].every(t=>t.readyState==='ended' && t.onended===null)
        && p.nodes.every(n=>n.onaudioprocess===null)
        && p.contexts.every(c=>c.state==='closed') && p.timers.size===0;
}
export async function restore(p) { await p.restore(); }
export function assertShared(memory) { if (!(memory.buffer instanceof SharedArrayBuffer)) throw new Error('not shared'); }
export function nowNanos() { return performance.now() * 1000000; }
"#)]
extern "C" {
    fn probe(mode: &str) -> JsValue;
    fn ready(p: &JsValue) -> bool;
    fn permission(p: &JsValue);
    fn timeout(p: &JsValue);
    async fn settle();
    fn metrics(p: &JsValue) -> String;
    fn clean(p: &JsValue) -> bool;
    async fn restore(p: &JsValue);
    #[wasm_bindgen(js_name = assertShared)]
    fn assert_shared(memory: &JsValue);
    #[wasm_bindgen(js_name = nowNanos)]
    fn now_nanos() -> f64;
}

struct Logger;
impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, _: &log::Record<'_>) {}
    fn flush(&self) {}
}

#[wasm_bindgen_test]
async fn production_call_audio_releases_every_track_and_callback() {
    #[cfg(target_feature = "atomics")]
    assert_shared(&wasm_bindgen::memory());
    let _ = log::set_logger(&Logger);
    log::set_max_level(log::LevelFilter::Warn);
    struct Clock;
    impl wacore::time::MonotonicProvider for Clock {
        fn now_nanos(&self) -> u64 {
            now_nanos() as u64
        }
    }
    let _ = wacore::time::set_monotonic_provider(Clock);
    let mut failures = Vec::new();
    for mode in [
        "speaker",
        "mic",
        "node",
        "connect",
        "resume",
        "cancel",
        "permission",
        "timeout",
    ] {
        let p = probe(if mode == "timeout" {
            "permission"
        } else {
            mode
        });
        if matches!(mode, "cancel" | "permission" | "timeout") {
            let mut opening = Box::pin(oxidezap_audio::open_call_audio());
            for _ in 0..20 {
                assert!(
                    futures_lite::future::poll_once(&mut opening)
                        .await
                        .is_none()
                );
                if ready(&p) {
                    break;
                }
                settle().await;
            }
            assert!(ready(&p));
            if mode == "timeout" {
                timeout(&p);
                assert!(opening.await.is_err());
            } else {
                drop(opening);
            }
            if mode != "cancel" {
                permission(&p);
            }
        } else {
            let result = oxidezap_audio::open_call_audio().await;
            if matches!(mode, "speaker" | "mic") {
                let (mic, speaker, _) = result.unwrap();
                let frame = futures_lite::future::or(async { mic.recv().await.unwrap() }, async {
                    for _ in 0..100 {
                        settle().await;
                    }
                    panic!("capture callback did not deliver audio");
                })
                .await;
                assert_eq!(frame.len(), 960);
                assert!(frame.iter().any(|&s| s != 0));
                speaker.send(frame).await.unwrap();
                settle().await;
                if mode == "speaker" {
                    drop(speaker);
                    settle().await;
                    drop(mic);
                } else {
                    drop(mic);
                    settle().await;
                    drop(speaker);
                }
            } else {
                assert!(result.is_err());
            }
        }
        settle().await;
        let measured = format!("{mode}: {}", metrics(&p));
        wasm_bindgen_test::console_log!("{measured}");
        if !clean(&p) {
            failures.push(measured);
        }
        restore(&p).await;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
