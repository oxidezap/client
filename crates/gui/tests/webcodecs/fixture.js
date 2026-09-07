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

    emit(timestamp, width, height) {
        const pixels = new Uint8Array(width * height * 4);
        for (let i = 0; i < width * height; i++) {
            pixels.set([i, 40, 80, 255], i * 4);
        }
        const frame = new VideoFrame(pixels, {
            format: "RGBA", codedWidth: width, codedHeight: height, timestamp,
        });
        const nativeCopy = frame.copyTo.bind(frame);
        frame.copyTo = (destination, options) => {
            const read = { destination, timestamp, lengthReads: 0 };
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
    closed(index) { return this.frames[index].codedWidth === 0; }
    reused(a, b) { return this.reads[a].destination === this.reads[b].destination; }
    lengthReads(index) { return this.reads[index].lengthReads; }
    closeCount() { return this.closes; }
    decodeCount() { return this.decodes; }

    cleanup() {
        for (const read of this.reads) read.reject(new Error("test cleanup"));
        for (const frame of this.frames) frame.close();
    }
}

export function drain() {
    return new Promise(resolve => setTimeout(resolve, 0));
}
