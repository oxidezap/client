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
    return listeners;
}

test("notification click opens the scoped focused page with the opaque tag", async () => {
    const delivered = [];
    const other = { url: "https://example.test/client/", focused: true, focus: async () => {
        throw new Error("wrong scope");
    } };
    const page = {
        id: "page-191",
        url: "https://example.test/client/pr/191/",
        focused: true,
        focus: async () => {},
        postMessage: (message) => delivered.push(message),
    };
    const listeners = workerWith([other, page]);
    listeners.get("message")({ data: { oxidezapClientId: "tab-191" }, source: { id: "page-191" } });
    const click = listeners.get("notificationclick");
    let closed = false;
    let completion;
    click({
        notification: {
            data: { oxidezapTag: "oxidezap-chat-123", oxidezapClientId: "tab-191", oxidezapAccountEpoch: "0" },
            close: () => { closed = true; },
        },
        waitUntil: (promise) => { completion = promise; },
    });
    await completion;
    assert.equal(closed, true);
    assert.equal(delivered.length, 1);
    assert.equal(delivered[0].oxidezapNotificationTag, "oxidezap-chat-123");
    assert.equal(delivered[0].oxidezapAccountEpoch, "0");
});

test("a click cannot open a matching chat in a different account tab", async () => {
    const received = [];
    const other = {
        id: "other-tab", url: "https://example.test/client/pr/191/", focused: true,
        focus: async () => {}, postMessage: (value) => received.push(value),
    };
    const listeners = workerWith([other]);
    listeners.get("message")({ data: { oxidezapClientId: "departed-tab" }, source: { id: "old-tab" } });
    let completion;
    listeners.get("notificationclick")({
        notification: { data: { oxidezapTag: "oxidezap-chat-123", oxidezapClientId: "departed-tab" }, close: () => {} },
        waitUntil: (promise) => { completion = promise; },
    });
    await completion;
    assert.deepEqual(received, []);
});

test("unrelated notification clicks are ignored", () => {
    const click = workerWith([]).get("notificationclick");
    let closed = false;
    click({ notification: { data: { oxidezapTag: "other" }, close: () => { closed = true; } } });
    assert.equal(closed, false);
});
