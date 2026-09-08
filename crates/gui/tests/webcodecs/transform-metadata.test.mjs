import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const source = await readFile(new URL("transform.js", import.meta.url), "utf8");

for (const [rotation, flip] of [[0, true], [90, false], [undefined, undefined], [90, true]]) {
    test(`oversized metadata ${rotation}/${flip} closes its frame`, async () => {
        let closed = 0, published = 0;
        const context = vm.createContext({
            Uint8Array, setTimeout,
            VideoFrame: class {
                constructor(_, init) {
                    assert.equal(init.rotation, 90);
                    assert.equal(init.flip, true);
                    this.rotation = rotation;
                    this.flip = flip;
                }
                close() { closed++; }
            },
        });
        const Decoder = vm.runInContext(source.replace("export class", "class") + ";TransformDecoder", context);
        const decoder = new Decoder();
        decoder.output = () => { published++; };
        if (rotation === 90 && flip === true) {
            await decoder.oversized();
            assert.equal(published, 1);
        } else {
            await assert.rejects(decoder.oversized(), /VideoFrame metadata unsupported: requested 90\/true/);
            assert.equal(published, 0);
        }
        assert.equal(closed, 1);
    });
}
