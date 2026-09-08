export class TransformDecoder {
    install() {
        const owner = this;
        this.original = globalThis.VideoDecoder;
        globalThis.VideoDecoder = class {
            constructor(init) { owner.output = init.output; }
            configure() {}
            decode() {}
            close() {}
            get decodeQueueSize() { return 0; }
        };
    }

    restore() { globalThis.VideoDecoder = this.original; }
    finish() { this.finished(); }

    async oversized() {
        const frame = new VideoFrame(new Uint8Array(1280 * 721 * 4), {
            format: "RGBA", codedWidth: 1280, codedHeight: 721,
            timestamp: 1, rotation: 90, flip: true,
        });
        try {
            this.output(frame);
            await new Promise(resolve => setTimeout(resolve, 0));
        } finally {
            frame.close();
        }
    }

    async emit(timestamp, bits, rotation, flip, cropped = false) {
        const width = cropped ? 7 : 3, height = cropped ? 9 : 5;
        const pixels = new Uint8Array(width * height * 4);
        for (let i = 0; i < width * height; i++) {
            pixels.set([13 + i * 13, 211 - i * 7, 31 + i * 9, 255], i * 4);
        }
        const frame = new VideoFrame(pixels, {
            format: "RGBA", codedWidth: width, codedHeight: height,
            timestamp, rotation, flip,
            ...(cropped ? {visibleRect: {x: 1, y: 2, width: 3, height: 5}, displayWidth: 6, displayHeight: 10} : {}),
        });
        if (frame.rotation !== rotation || frame.flip !== flip) {
            frame.close();
            throw new Error(`VideoFrame metadata unsupported: requested ${rotation}/${flip}`);
        }
        return this.compareFrame(frame, bits);
    }

    async compareFrame(frame, bits) {
        const metadata = {
            rotation: frame.rotation, flip: frame.flip, format: frame.format,
            coded: [frame.codedWidth, frame.codedHeight],
            visible: frame.visibleRect.toJSON(),
            display: [frame.displayWidth, frame.displayHeight],
        };
        // Raster at visible resolution, retaining the display aspect. A uniform
        // display scale must not enlarge the application's pixel allocation.
        const scale = frame.rotation % 180 ? frame.visibleRect.height / frame.displayWidth : frame.visibleRect.width / frame.displayWidth;
        const width = Math.round(frame.displayWidth * scale), height = Math.round(frame.displayHeight * scale);
        const canvas = new OffscreenCanvas(bits % 2 ? height : width, bits % 2 ? width : height);
        const context = canvas.getContext("2d", {willReadFrequently: true});
        // Captured WAWebVoipVideoRasterRenderer.js lines 205-224, no remote mirror.
        // JgwtTQVeWPm function 828 maps RTP low bits to enum 1,4,3,2.
        const orientation = [1, 4, 3, 2][bits];
        context.save();
        context.clearRect(0, 0, canvas.width, canvas.height);
        context.translate(canvas.width / 2, canvas.height / 2);
        context.scale(1, 1);
        context.rotate(Math.PI * (orientation - 1) / 2);
        context.translate(-width / 2, -height / 2);
        context.drawImage(frame, 0, 0, width, height);
        context.restore();
        this.expected = context.getImageData(0, 0, canvas.width, canvas.height).data;
        // Canvas and copyTo may round YUV conversion differently. RGBA is exact.
        this.tolerance = frame.format === "RGBA" ? 0 : 3;
        this.dimensions = [canvas.width, canvas.height];
        const done = new Promise(resolve => { this.finished = resolve; });
        this.output(frame);
        let timer;
        try {
            await Promise.race([done, new Promise((_, reject) => {
                timer = setTimeout(() => reject(new Error("production publication timed out")), 5000);
            })]);
        } finally {
            clearTimeout(timer);
            frame.close();
        }
        return JSON.stringify(metadata);
    }

    matches(bytes, width, height) {
        if (width !== this.dimensions[0] || height !== this.dimensions[1]) return false;
        if (bytes.length !== this.expected.length) return false;
        return bytes.every((value, i) => Math.abs(value - this.expected[(i & ~3) + [2, 1, 0, 3][i % 4]]) <= this.tolerance);
    }

    async encode() {
        let failure;
        const chunks = [];
        const encoder = new VideoEncoder({
            output(chunk) {
                const bytes = new Uint8Array(chunk.byteLength);
                chunk.copyTo(bytes);
                chunks.push(bytes);
            },
            error(error) { failure = error; },
        });
        try {
            encoder.configure({codec: "avc1.42001e", width: 32, height: 48,
                bitrate: 1000000, framerate: 30, avc: {format: "annexb"}, latencyMode: "realtime"});
            const canvas = new OffscreenCanvas(32, 48);
            const context = canvas.getContext("2d");
            context.fillStyle = "#204080";
            context.fillRect(0, 0, 32, 48);
            context.fillStyle = "#e04020";
            context.fillRect(0, 0, 12, 20);
            context.fillStyle = "#40e060";
            context.fillRect(18, 30, 14, 18);
            const frame = new VideoFrame(canvas, {timestamp: 0});
            try { encoder.encode(frame, {keyFrame: true}); }
            finally { frame.close(); }
            await encoder.flush();
            if (failure) throw failure;
            if (chunks.length !== 1) throw new Error("Expected one H264 key chunk");
            this.key = chunks[0];
        } finally {
            if (encoder.state !== "closed") encoder.close();
        }
    }

    async decodeEmit(timestamp, bits, turn, horizontal, vertical) {
        // H.264 D.1.27, also FFmpeg cbs_h264_syntax_template.c sei_display_orientation.
        // cancel=0, hor/ver, anticlockwise_rotation u16, repetition ue(0), extension=0.
        const payload = (Number(horizontal) << 22) | (Number(vertical) << 21) |
            ((turn * 16384) << 5) | 0x14;
        const sei = [0, 0, 0, 1, 6, 47, 3, payload >>> 16, (payload >>> 8) & 255, payload & 255, 128];
        let start = -1;
        for (let i = 0; i + 4 < this.key.length; i++) {
            if (this.key[i] === 0 && this.key[i + 1] === 0 && this.key[i + 2] === 1 &&
                (this.key[i + 3] & 31) === 5) {
                start = i > 0 && this.key[i - 1] === 0 ? i - 1 : i;
                break;
            }
        }
        if (start < 0) throw new Error("No IDR in generated H264");
        const data = new Uint8Array([...this.key.subarray(0, start), ...sei, ...this.key.subarray(start)]);
        const frames = [];
        let failure;
        const decoder = new VideoDecoder({output(frame) { frames.push(frame); }, error(error) { failure = error; }});
        try {
            decoder.configure({codec: "avc1.42001e"});
            decoder.decode(new EncodedVideoChunk({type: "key", timestamp, data}));
            await decoder.flush();
            if (failure) throw failure;
            if (frames.length !== 1) throw new Error(`Expected one decoded frame, got ${frames.length}`);
            return await this.compareFrame(frames[0], bits);
        } finally {
            frames.forEach(frame => frame.close());
            if (decoder.state !== "closed") decoder.close();
        }
    }
}
