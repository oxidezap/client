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
        const size = format === "I420" ? width * height * 1.5 : width * height * 4;
        const data = new Uint8Array(size).fill(128);
        const samples = [[], []];
        const copies = [[], []];
        const firstOutputMs = [];
        const copyFormats = [];
        const template = new VideoFrame(data, {
            format, codedWidth: width, codedHeight: height, timestamp: 0,
        });
        for (let i = 0; i < rounds + 50; i++) {
            for (const index of i % 2 ? [1, 0] : [0, 1]) {
                const fixture = index === 0 ? this : peer;
                const frame = template.clone();
                const nativeCopy = frame.copyTo.bind(frame);
                let copyMs = 0;
                frame.copyTo = (destination, options) => {
                    copyFormats[index] = options.format;
                    const start = performance.now();
                    return nativeCopy(destination, options).then(layout => {
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
                if (i >= 50) {
                    samples[index].push(performance.now() - start);
                    copies[index].push(copyMs);
                }
            }
        }
        template.close();
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
            rounds, firstOutputMs, copyFormats, baseline: { outputMs: stats(samples[0]), copyMs: stats(copies[0]) },
            candidate: { outputMs: stats(samples[1]), copyMs: stats(copies[1]) } });
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
