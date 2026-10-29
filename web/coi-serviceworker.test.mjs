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

// These are the actual templates Trunk expands, not a second loader. A
// browser test can then exercise the same templates with synthetic assets.
const trunk = readFileSync(new URL("./Trunk.toml", import.meta.url), "utf8");
const startup = Object.fromEntries([...trunk.matchAll(
    /pattern_(preload|script)\s*=\s*'''([\s\S]*?)'''/g,
)].map((match) => [match[1], match[2]]));
const inlineScript = (html) => html.match(/<script(?: type="module")?>([\s\S]*?)<\/script>/)[1];

test("an uncontrolled page leaves the WASM and glue preloads inert", () => {
    const activated = [];
    vm.runInNewContext(inlineScript(startup.preload), {
        window: { crossOriginIsolated: false },
        document: { head: { append: (value) => activated.push(value) } },
    });
    assert.deepEqual(activated, []);
    assert.match(startup.preload, /<template[^>]*>[\s\S]*href="\{base\}\{js\}"[\s\S]*href="\{base\}\{wasm\}"[\s\S]*<\/template>/);
});

test("an isolated page activates both preloads before starting the module", () => {
    const content = {};
    const activated = [];
    vm.runInNewContext(inlineScript(startup.preload), {
        window: { crossOriginIsolated: true },
        document: {
            getElementById: (id) => { assert.equal(id, "wasm-preloads"); return { content }; },
            head: { append: (value) => activated.push(value) },
        },
    });
    assert.deepEqual(activated, [content]);
});

for (const isolated of [false, true]) {
    test(`the loader ${isolated ? "starts and announces" : "does not import"} the application when isolation is ${isolated}`, async () => {
        const imports = [];
        const events = [];
        const wasm = {};
        // vm's ordinary script runner has no module loader. Substitute only
        // the dynamic import expression; the branch and init call stay exact.
        const script = inlineScript(startup.script).replace("await import(", "await importModule(");
        await vm.runInNewContext(`(async () => { ${script} })()`, {
            window: { crossOriginIsolated: isolated },
            importModule: async (url) => {
                imports.push(url);
                return { default: async (options) => {
                    assert.equal(options.module_or_path, "{base}{wasm}");
                    return wasm;
                } };
            },
            CustomEvent: class { constructor(name, options) { this.name = name; this.detail = options.detail; } },
            dispatchEvent: (event) => events.push(event),
        });
        assert.equal(imports.length, isolated ? 1 : 0);
        assert.equal(events.length, isolated ? 1 : 0);
        if (isolated) {
            assert.equal(imports[0], "{base}{js}");
            assert.equal(events[0].name, "TrunkApplicationStarted");
            assert.equal(events[0].detail.wasm, wasm);
        }
    });
}

function pageWith({ isolated = false, secure = true, worker, reloaded = false, stored = new Map() } = {}) {
    const errors = [];
    let reloads = 0;
    if (reloaded) stored.set("coi-serviceworker:reloaded", "1");
    const context = {
        window: {
            crossOriginIsolated: isolated,
            isSecureContext: secure,
            document: { currentScript: { src: "https://example.test/client/coi-serviceworker.js" } },
            location: { reload: () => { reloads++; } },
            sessionStorage: {
                getItem: (key) => stored.get(key) ?? null,
                setItem: (key, value) => stored.set(key, value),
                removeItem: (key) => stored.delete(key),
            },
        },
        navigator: { serviceWorker: worker },
        console: { error: (...args) => errors.push(args.join(" ")), warn: () => {} },
    };
    vm.runInNewContext(script, context);
    return { errors, stored, reloads: () => reloads };
}

test("a host already providing isolation needs no service worker", () => {
    const page = pageWith({ isolated: true, reloaded: true });
    assert.deepEqual(page.errors, []);
    assert.equal(page.reloads(), 0);
    assert.equal(page.stored.has("coi-serviceworker:reloaded"), false);
});

test("a browser without service workers reports the failure without reloading", () => {
    const page = pageWith();
    assert.match(page.errors.join(" "), /no service workers/);
    assert.equal(page.reloads(), 0);
});

test("service worker registration failure reports the failure without reloading", async () => {
    const page = pageWith({ worker: {
        addEventListener: () => {},
        register: async () => { throw new Error("registration unavailable"); },
    } });
    await new Promise((resolve) => setImmediate(resolve));
    assert.match(page.errors.join(" "), /registration failed/);
    assert.equal(page.reloads(), 0);
});

test("the first visit reloads once and an unsuccessful reload stops", async () => {
    const listeners = new Map();
    const page = pageWith({ worker: {
        addEventListener: (name, listener) => listeners.set(name, listener),
        controller: null,
        register: async () => ({ active: true }),
    } });
    await new Promise((resolve) => setImmediate(resolve));
    listeners.get("controllerchange")();
    listeners.get("controllerchange")();
    assert.equal(page.reloads(), 1);
    assert.equal(page.stored.get("coi-serviceworker:reloaded"), "1");
    const retry = pageWith({ stored: page.stored, worker: {
        register: () => { throw new Error("must not register twice"); },
    } });
    assert.equal(retry.reloads(), 0);
    assert.match(retry.errors.join(" "), /still not isolated/);
});
