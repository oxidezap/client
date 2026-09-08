import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const source = await readFile(new URL("fixture.js", import.meta.url), "utf8");
const modes = ["no-preference", "prefer-hardware", "prefer-software"];
const stages = [
    ...modes.flatMap(mode => [`encoder.support.${mode}`, `decoder.support.${mode}`]),
    "encoder.constructor", "encoder.configure", "decoder.constructor", "decoder.configure",
    "canvas.constructor", "canvas.context", "frame.constructor", "clock",
];

for (const stage of [...stages, "unsupported", "frame.close", "encoder.close", "decoder.close"]) {
    test(`benchmark restores copyTo and closes resources after ${stage}`, async () => {
        const injected = new Error(stage);
        const created = [];
        const closed = [];
        const fail = name => { if (stage === name) throw injected; };
        const codec = name => class {
            static async isConfigSupported(config) {
                fail(`${name}.support.${config.hardwareAcceleration}`);
                return {supported: stage !== "unsupported", config};
            }
            constructor() {
                fail(`${name}.constructor`);
                created.push(name);
                this.state = "unconfigured";
            }
            configure() { fail(`${name}.configure`); this.state = "configured"; }
            close() { closed.push(name); this.state = "closed"; fail(`${name}.close`); }
        };
        class Frame {
            constructor() { fail("frame.constructor"); created.push("frame"); }
            copyTo() {}
            close() { closed.push("frame"); fail("frame.close"); }
        }
        const originalCopy = Frame.prototype.copyTo;
        const realm = vm.createContext({
            VideoEncoder: codec("encoder"), VideoDecoder: codec("decoder"), VideoFrame: Frame,
            OffscreenCanvas: class {
                constructor() { fail("canvas.constructor"); }
                getContext() { fail("canvas.context"); return {}; }
            },
            performance: {now() { fail("clock"); return 0; }},
        });
        const ControlledDecoder = vm.runInContext(
            source.replaceAll("export ", "") + "\nControlledDecoder;", realm);
        const fixture = new ControlledDecoder();
        fixture.rgbaOnly();
        assert.notEqual(Frame.prototype.copyTo, originalCopy);
        // Negative rounds bypass frame processing; codec bursts are irrelevant to setup cleanup.
        fixture.benchmarkBurst = async () => ({});
        const raw = stage.startsWith("frame.") || stage === "clock";
        let result, error;
        try {
            result = await fixture.benchmark(2, 2, raw ? "RGBA" : "decoded:no-preference", -20, fixture);
        } catch (caught) {
            error = caught;
        }
        assert.equal(Frame.prototype.copyTo, originalCopy, "copyTo override leaked");
        assert.equal(fixture.originalCopy, undefined, "saved override was not cleared");
        assert.deepEqual(closed.slice().sort(), created.slice().sort(), "each created resource must close once");
        if (stage === "unsupported") {
            assert.equal(error, undefined);
            const report = JSON.parse(result);
            assert.equal(report.skipped, "decoder configuration unsupported");
            assert.equal(report.decodedCount, 0);
            assert.deepEqual(report.copiedFrames, [0, 0]);
            assert.deepEqual(report.copiedBytes, [0, 0]);
            assert.equal(report.support.length, 3);
        } else {
            assert.equal(error, injected, "injected failure must propagate");
        }
    });
}
