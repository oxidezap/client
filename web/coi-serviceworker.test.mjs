import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";

const script = readFileSync(new URL("./coi-serviceworker.js", import.meta.url), "utf8");

function workerWith(clients) {
    const listeners = new Map();
    const scope = "https://example.test/client/pr/191/";
    const context = {
        self: {
            registration: { scope },
            clients: {
                claim: async () => {},
                matchAll: async () => clients,
                openWindow: async () => {},
            },
            skipWaiting: async () => {},
            addEventListener: (name, callback) => listeners.set(name, callback),
        },
        console,
    };
    vm.runInNewContext(script, context);
    return listeners.get("notificationclick");
}

test("notification click opens the scoped focused page with the opaque tag", async () => {
    const delivered = [];
    const other = { url: "https://example.test/client/", focused: true, focus: async () => {
        throw new Error("wrong scope");
    } };
    const page = {
        url: "https://example.test/client/pr/191/",
        focused: true,
        focus: async () => {},
        postMessage: (message) => delivered.push(message.oxidezapNotificationTag),
    };
    const click = workerWith([other, page]);
    let closed = false;
    let completion;
    click({
        notification: {
            data: { oxidezapTag: "oxidezap-chat-123" },
            close: () => { closed = true; },
        },
        waitUntil: (promise) => { completion = promise; },
    });
    await completion;
    assert.equal(closed, true);
    assert.deepEqual(delivered, ["oxidezap-chat-123"]);
});

test("unrelated notification clicks are ignored", () => {
    const click = workerWith([]);
    let closed = false;
    click({ notification: { data: { oxidezapTag: "other" }, close: () => { closed = true; } } });
    assert.equal(closed, false);
});
