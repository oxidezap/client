const config = {codec: "avc1.42001e", width: 32, height: 32};

export async function encodeRecoveryFrames() {
    const chunks = [];
    let failure;
    const encoder = new VideoEncoder({
        output(chunk) {
            const bytes = new Uint8Array(chunk.byteLength);
            chunk.copyTo(bytes);
            chunks.push(bytes);
        },
        error(error) { failure = error; },
    });
    try {
        encoder.configure({
            ...config, bitrate: 100000, framerate: 30,
            avc: {format: "annexb"}, latencyMode: "realtime",
        });
        const canvas = new OffscreenCanvas(32, 32);
        const context = canvas.getContext("2d");
        for (let i = 0; i < 2; i++) {
            context.fillStyle = i === 0 ? "#336699" : "#669933";
            context.fillRect(0, 0, 32, 32);
            const frame = new VideoFrame(canvas, {timestamp: i * 33333});
            try {
                encoder.encode(frame, {keyFrame: i === 0});
            } finally {
                frame.close();
            }
        }
        await encoder.flush();
        if (failure) throw failure;
        if (chunks.length !== 2) throw new Error(`Expected two encoded pictures, got ${chunks.length}`);
        return chunks;
    } finally {
        if (encoder.state !== "closed") encoder.close();
    }
}

// The call's own encoder settings: Constrained Baseline 3.1 at call
// resolution, Annex-B, realtime. One keyframe is all the SPS verdict needs.
export async function encodeProductionFrames() {
    const chunks = [];
    let failure;
    const encoder = new VideoEncoder({
        output(chunk) {
            const bytes = new Uint8Array(chunk.byteLength);
            chunk.copyTo(bytes);
            chunks.push(bytes);
        },
        error(error) { failure = error; },
    });
    try {
        encoder.configure({
            codec: "avc1.42e01f", width: 1280, height: 720,
            bitrate: 1980000, framerate: 20,
            avc: {format: "annexb"}, latencyMode: "realtime",
        });
        const canvas = new OffscreenCanvas(1280, 720);
        const context = canvas.getContext("2d");
        context.fillStyle = "#336699";
        context.fillRect(0, 0, 1280, 720);
        const frame = new VideoFrame(canvas, {timestamp: 0});
        try {
            encoder.encode(frame, {keyFrame: true});
        } finally {
            frame.close();
        }
        await encoder.flush();
        if (failure) throw failure;
        if (chunks.length !== 1) throw new Error(`Expected one keyframe, got ${chunks.length}`);
        return chunks;
    } finally {
        if (encoder.state !== "closed") encoder.close();
    }
}

export async function decodeAcrossReset(key, delta) {
    const timestamps = [];
    let failure;
    const decoder = new VideoDecoder({
        output(frame) {
            try {
                if (frame.displayWidth !== 32 || frame.displayHeight !== 32) {
                    failure = new Error("Unexpected decoded dimensions");
                }
                timestamps.push(frame.timestamp);
            } finally {
                frame.close();
            }
        },
        error(error) { failure = error; },
    });
    try {
        for (let generation = 0; generation < 2; generation++) {
            decoder.configure({codec: config.codec});
            decoder.decode(new EncodedVideoChunk({
                type: "key", timestamp: generation * 66666, data: key,
            }));
            decoder.decode(new EncodedVideoChunk({
                type: "delta", timestamp: generation * 66666 + 33333, data: delta,
            }));
            await decoder.flush();
            if (failure) throw failure;
            decoder.reset();
        }
        return timestamps;
    } finally {
        if (decoder.state !== "closed") decoder.close();
    }
}
