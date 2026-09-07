export class ControlledDecoder {
    constructor() {
        this.frames = [];
        this.reads = [];
        this.closes = 0;
        this.decodes = 0;
    }

    install(failConfigure) {
        this.original = globalThis.VideoDecoder;
        const fixture = this;
        globalThis.VideoDecoder = class {
            constructor(init) { fixture.output = init.output; }
            configure() {
                if (failConfigure) throw new Error("forced configure failure");
            }
            reset() {}
            close() { fixture.closes++; }
            decode() { fixture.decodes++; }
            get decodeQueueSize() { return 0; }
        };
    }

    restore() {
        globalThis.VideoDecoder = this.original;
    }

    finish() { this.published(); }

    rgbaOnly() {
        this.originalCopy = VideoFrame.prototype.copyTo;
        const native = this.originalCopy;
        VideoFrame.prototype.copyTo = function(destination, options) {
            if (options?.format === "BGRA") {
                return Promise.reject(new DOMException("BGRA disabled", "NotSupportedError"));
            }
            return native.call(this, destination, options);
        };
    }

    ignoreFormat() {
        this.originalCopy = VideoFrame.prototype.copyTo;
        const native = this.originalCopy;
        VideoFrame.prototype.copyTo = function(destination) {
            return native.call(this, destination);
        };
    }

    async benchmark(width, height, format, rounds, peer) {
        const decoded = format.startsWith("decoded:");
        const mode = format.split(":")[1];
        const codec = "avc1.42001f";
        const encoderConfig = {codec, width, height, bitrate: 2500000,
            framerate: 20, latencyMode: "realtime", avc: {format: "annexb"}};
        const decoderConfig = {codec, codedWidth: width, codedHeight: height,
            hardwareAcceleration: mode, optimizeForLatency: true};
        const support = [];
        const chunks = [];
        const bursts = [];
        let encoder, decoder, canvas, context, encoded, decodedFrame, failure;
        let encodedCount = 0, decodedCount = 0, encodedBytes = 0;
        if (decoded) {
            for (const hardwareAcceleration of ["no-preference", "prefer-hardware", "prefer-software"]) {
                support.push({hardwareAcceleration,
                    encoder: await VideoEncoder.isConfigSupported({...encoderConfig, hardwareAcceleration}),
                    decoder: await VideoDecoder.isConfigSupported({...decoderConfig, hardwareAcceleration})});
            }
            if (!support.find(s => s.hardwareAcceleration === mode).decoder.supported) {
                if (this.originalCopy) {
                    VideoFrame.prototype.copyTo = this.originalCopy;
                    this.originalCopy = undefined;
                }
                return JSON.stringify({format, support, decodedCount: 0, copiedFrames: [0, 0],
                    copiedBytes: [0, 0], skipped: "decoder configuration unsupported"});
            }
            encoder = new VideoEncoder({output(chunk) {
                encodedCount++; encodedBytes += chunk.byteLength; encoded = chunk;
                if (chunks.length < 66) chunks.push(chunk);
            }, error(error) { failure = error; }});
            encoder.configure(encoderConfig);
            decoder = new VideoDecoder({output(frame) {
                decodedCount++; decodedFrame(frame);
            }, error(error) { failure = error; }});
            decoder.configure(decoderConfig);
            canvas = new OffscreenCanvas(width, height);
            context = canvas.getContext("2d");
        }
        const size = format === "I420" ? width * height * 1.5 : width * height * 4;
        const data = new Uint8Array(size).fill(128);
        const samples = [[], []];
        const copies = [[], []];
        const calls = [[], []];
        const materialization = [[], []];
        const bytes = [0, 0];
        const sourceFormats = new Set();
        const firstOutputMs = [];
        const copyFormats = [];
        let template = decoded ? null : new VideoFrame(data, {
            format, codedWidth: width, codedHeight: height, timestamp: 0,
        });
        const runStart = performance.now();
        let elapsedMs;
        try {
            for (let i = 0; i < rounds + 20; i++) {
                if (decoded) {
                    await new Promise(resolve => setTimeout(resolve,
                        Math.max(0, runStart + i * 50 - performance.now())));
                    context.fillStyle = `rgb(${i % 256},73,151)`;
                    context.fillRect(0, 0, width, height);
                    for (let j = 0; j < 32; j++) {
                        context.fillStyle = `rgb(${j * 7},${(i + j * 13) % 256},${255 - j * 5})`;
                        context.fillRect((j * 47 + i * 9) % width, j * 23 % height, 90, 65);
                    }
                    const input = new VideoFrame(canvas, {timestamp: i * 50000});
                    encoder.encode(input, {keyFrame: i % 20 === 0});
                    input.close();
                    await encoder.flush();
                    if (failure) throw failure;
                    const ready = new Promise((resolve, reject) => {
                        const timer = setTimeout(() => reject(failure ?? new Error(`decode output timeout at ${i}`)), 5000);
                        decodedFrame = frame => { clearTimeout(timer); resolve(frame); };
                    });
                    decoder.decode(encoded);
                    template = await ready;
                    if (template.visibleRect.width !== width || template.visibleRect.height !== height) {
                        throw new Error("decoded resolution mismatch");
                    }
                }
                sourceFormats.add(template.format);
                const destinations = [];
                for (const index of i % 2 ? [1, 0] : [0, 1]) {
                    const fixture = index === 0 ? this : peer;
                    const frame = template.clone();
                    const nativeCopy = frame.copyTo.bind(frame);
                    let copyMs = 0;
                    let callMs = 0, completedAt = 0, copiedBytes = 0;
                    frame.copyTo = (destination, options) => {
                        copyFormats[index] = options.format;
                        destinations[index] = destination;
                        const start = performance.now();
                        const promise = nativeCopy(destination, options);
                        callMs += performance.now() - start;
                        copiedBytes += destination.byteLength;
                        return promise.then(layout => {
                            completedAt = performance.now();
                            copyMs += performance.now() - start;
                            return layout;
                        });
                    };
                    const done = new Promise(resolve => { fixture.published = resolve; });
                    const start = performance.now();
                    fixture.output(frame);
                    await done;
                    if (i === 0) firstOutputMs[index] = performance.now() - start;
                    if (this.originalCopy) {
                        VideoFrame.prototype.copyTo = this.originalCopy;
                        this.originalCopy = undefined;
                    }
                    if (i >= 20) {
                        samples[index].push(performance.now() - start);
                        copies[index].push(copyMs);
                        calls[index].push(callMs);
                        materialization[index].push(performance.now() - completedAt);
                        bytes[index] += copiedBytes;
                    }
                    if (copiedBytes !== width * height * 4) throw new Error("copy byte count mismatch");
                }
                if (i === 0 || i === rounds + 19) {
                    const [rgba, bgra] = destinations;
                    for (let p = 0; p < rgba.length; p += 4) {
                        if (rgba[p] !== bgra[p + 2] || rgba[p + 1] !== bgra[p + 1] ||
                            rgba[p + 2] !== bgra[p] || rgba[p + 3] !== bgra[p + 3]) {
                            throw new Error(`RGBA/BGRA pixel mismatch at ${p / 4}`);
                        }
                    }
                }
                if (decoded) { template.close(); template = null; }
            }
            elapsedMs = performance.now() - runStart;
            if (decoded) {
                for (const fixture of [this, peer]) {
                    bursts.push(await fixture.benchmarkBurst(chunks, decoderConfig));
                }
            }
        } finally {
            template?.close();
            if (encoder && encoder.state !== "closed") encoder.close();
            if (decoder && decoder.state !== "closed") decoder.close();
            // A throw before the per-iteration restore must not leak the
            // format override into later tests sharing this browser realm.
            if (this.originalCopy) {
                VideoFrame.prototype.copyTo = this.originalCopy;
                this.originalCopy = undefined;
            }
        }
        if (copyFormats[0] !== "RGBA" || copyFormats[1] !== "BGRA") {
            throw new Error(`benchmark did not compare RGBA/BGRA: ${copyFormats}`);
        }
        const stats = values => {
            values.sort((a, b) => a - b);
            return { mean: values.reduce((a, b) => a + b, 0) / values.length,
                median: values[Math.floor(values.length / 2)],
                p95: values[Math.floor(values.length * .95)] };
        };
        return JSON.stringify({ browser: navigator.userAgent, width, height, format,
            rounds, warmup: 20, elapsedMs,
            support, encodedCount, decodedCount, encodedBytes, sourceFormats: [...sourceFormats],
            bursts,
            copiedFrames: samples.map(s => s.length), copiedBytes: bytes,
            firstOutputMs, copyFormats, baseline: { outputMs: stats(samples[0]), copyMs: stats(copies[0]),
                callMs: stats(calls[0]), postCopyMs: stats(materialization[0]) },
            candidate: { outputMs: stats(samples[1]), copyMs: stats(copies[1]),
                callMs: stats(calls[1]), postCopyMs: stats(materialization[1]) } });
    }

    async benchmarkBurst(chunks, config) {
        const result = {submitted: chunks.length, decoded: 0, copied: 0, closed: 0,
            published: 0, copiedBytes: 0, timerTicks: 0, animationFrames: 0, maxTimerGapMs: 0};
        let failure, animation;
        const start = performance.now();
        let lastTick = start;
        const timer = setInterval(() => {
            const now = performance.now();
            result.maxTimerGapMs = Math.max(result.maxTimerGapMs, now - lastTick);
            lastTick = now;
            result.timerTicks++;
        }, 0);
        const tick = () => { result.animationFrames++; animation = requestAnimationFrame(tick); };
        animation = requestAnimationFrame(tick);
        this.published = () => { result.published++; };
        const decoder = new VideoDecoder({
            output: frame => {
                result.decoded++;
                const copy = frame.copyTo.bind(frame);
                const close = frame.close.bind(frame);
                let closed = false;
                frame.close = () => {
                    if (!closed) { closed = true; result.closed++; }
                    close();
                };
                frame.copyTo = (destination, options) => {
                    result.copied++;
                    result.copiedBytes += destination.byteLength;
                    result.format = options.format;
                    return copy(destination, options);
                };
                this.output(frame);
            },
            error(error) { failure = error; },
        });
        try {
            decoder.configure({...config, optimizeForLatency: false});
            for (const chunk of chunks) decoder.decode(chunk);
            await decoder.flush();
            while (result.closed !== chunks.length) {
                if (failure) throw failure;
                if (performance.now() - start > 5000) throw new Error("burst readback timeout");
                await drain();
            }
            result.elapsedMs = performance.now() - start;
            result.maxTimerGapMs = Math.max(result.maxTimerGapMs, performance.now() - lastTick);
            if (result.decoded !== chunks.length || result.copied !== result.published) {
                throw new Error(`incomplete burst: ${JSON.stringify(result)}`);
            }
            return result;
        } finally {
            clearInterval(timer);
            cancelAnimationFrame(animation);
            if (decoder.state !== "closed") decoder.close();
        }
    }

    rejectAllocation() { this.rejectBgraAllocation = true; }

    emit(timestamp, width, height) {
        const pixels = new Uint8Array(width * height * 4);
        for (let i = 0; i < width * height; i++) {
            pixels.set([i, 40, 80, 255], i * 4);
        }
        const frame = new VideoFrame(pixels, {
            format: "RGBA", codedWidth: width, codedHeight: height, timestamp,
        });
        if (this.rejectBgraAllocation) {
            const nativeAllocation = frame.allocationSize.bind(frame);
            frame.allocationSize = options => {
                if (options?.format === "BGRA") {
                    throw new DOMException("BGRA allocation disabled", "NotSupportedError");
                }
                return nativeAllocation(options);
            };
        }
        const nativeCopy = frame.copyTo.bind(frame);
        frame.copyTo = (destination, options) => {
            const read = { destination, timestamp, format: options.format, lengthReads: 0 };
            this.reads.push(read);
            Object.defineProperty(destination, "length", {
                configurable: true,
                get() { read.lengthReads++; return destination.byteLength; },
            });
            // The real copy starts now. Only delivery of its completion is gated.
            read.copied = nativeCopy(destination, options);
            const gate = new Promise((resolve, reject) => {
                read.release = resolve;
                read.reject = reject;
            });
            read.completed = Promise.all([read.copied, gate]).then(([layout]) => layout);
            return read.completed;
        };
        this.frames.push(frame);
        this.output(frame);
    }

    async settle(index, reject) {
        const read = this.reads[index];
        if (!read) throw new Error(`copy ${index} never started`);
        await read.copied;
        if (reject) read.reject(new Error("forced copy rejection"));
        else read.release();
        try {
            await read.completed;
            if (reject) throw new Error("copy did not reject");
        } catch (error) {
            if (!reject || error.message !== "forced copy rejection") throw error;
        }
        // Cross a task boundary so both JS and Rust promise continuations drain.
        await drain();
    }

    count() { return this.reads.length; }
    stamp(index) { return this.reads[index].timestamp; }
    format(index) { return this.reads[index].format; }
    closed(index) { return this.frames[index].codedWidth === 0; }
    reused(a, b) { return this.reads[a].destination === this.reads[b].destination; }
    lengthReads(index) { return this.reads[index].lengthReads; }
    closeCount() { return this.closes; }
    decodeCount() { return this.decodes; }

    cleanup() {
        if (this.originalCopy) VideoFrame.prototype.copyTo = this.originalCopy;
        for (const read of this.reads) read.reject(new Error("test cleanup"));
        for (const frame of this.frames) frame.close();
    }
}

export function drain() {
    return new Promise(resolve => setTimeout(resolve, 0));
}
