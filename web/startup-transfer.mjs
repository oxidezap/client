// Run with `node web/startup-transfer.mjs`; needs Chromium on PATH.
// ISOLATED=1 checks a host that supplies COOP/COEP without a reload.
// Alternating base/head order, fresh scopes, one Chromium process, no packages.
import http from "node:http";
import fs from "node:fs";
import { spawn } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../", import.meta.url));
const worker = fs.readFileSync(root + "/web/coi-serviceworker.js", "utf8");
const config = fs.readFileSync(root + "/web/Trunk.toml", "utf8");
const patterns = [
  ...config.matchAll(/pattern_(preload|script)\s*=\s*'''([\s\S]*?)'''/g),
];
if (patterns.length !== 2) throw new Error("Trunk startup patterns are missing");
// Synthetic module with a 22 MiB custom section. Measures requests and transfer,
// not application execution, CPU, heap, RSS, or production startup latency.
const moduleSize = 22 * 1024 * 1024;
function leb(n) {
  const b = [];
  do {
    let x = n & 127;
    n >>>= 7;
    b.push(x | (n ? 128 : 0));
  } while (n);
  return b;
}
const payload = Buffer.concat([
  Buffer.from([0, 97, 115, 109, 1, 0, 0, 0, 0, ...leb(moduleSize), 0]),
  Buffer.alloc(moduleSize - 1),
]);
const stats = new Map();
const basePreload =
  '<link rel="modulepreload" href="{base}{js}"><link rel="preload" href="{base}{wasm}" as="fetch" type="application/wasm" crossorigin="anonymous">';
const baseScript =
  '<script type="module">import init from "{base}{js}";const wasm=await init({module_or_path:"{base}{wasm}"});dispatchEvent(new CustomEvent("TrunkApplicationStarted",{detail:{wasm:wasm}}));</script>';
const server = http.createServer((req, res) => {
  const parts = req.url.split("/");
  const key = parts[1];
  const path = parts.at(-1);
  const s = stats.get(key);
  if (path === "coi-serviceworker.js") {
    res.setHeader("Content-Type", "application/javascript");
    res.end(worker);
    return;
  }
  if (path === "app.js") {
    res.setHeader("Content-Type", "application/javascript");
    res.end(
      "export default async function init({module_or_path}){await WebAssembly.compileStreaming(fetch(module_or_path));return {}}",
    );
    return;
  }
  if (path === "app_bg.wasm") {
    s.requests++;
    res.setHeader("Content-Type", "application/wasm");
    res.setHeader("Content-Length", payload.length);
    let offset = 0;
    const timer = setInterval(() => {
      if (offset >= payload.length) {
        clearInterval(timer);
        res.end();
        return;
      }
      const chunk = payload.subarray(offset, offset + 65536);
      offset += chunk.length;
      s.bytes += chunk.length;
      res.write(chunk);
    }, 5);
    res.on("close", () => {
      clearInterval(timer);
      if (offset < payload.length) s.cancelled++;
    });
    return;
  }
  const replace = (x) =>
    x
      .replaceAll("{base}", `/${key}/`)
      .replaceAll("{js}", "app.js")
      .replaceAll("{wasm}", "app_bg.wasm")
      .replaceAll("{crossorigin}", "anonymous");
  const variant = key.split("-")[0];
  const selected =
    variant === "head" && patterns.length === 2
      ? Object.fromEntries(patterns.map((m) => [m[1], m[2]]))
      : { preload: basePreload, script: baseScript };
  if (process.env.ISOLATED === "1") {
    res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
  }
  res.setHeader("Content-Type", "text/html");
  res.end(
    '<!doctype html><head><script src="coi-serviceworker.js"></script>' +
      replace(selected.preload) +
      replace(selected.script) +
      "</head><body>test</body>",
  );
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const profile = fs.mkdtempSync(join(tmpdir(), "web-startup-profile-"));
const chrome = spawn("chromium", [
  "--headless",
  "--no-sandbox",
  "--disable-gpu",
  "--remote-debugging-port=0",
  "--user-data-dir=" + profile,
  "about:blank",
]);
const browserUrl = await new Promise((resolve, reject) => {
  let output = "";
  chrome.stderr.on("data", (x) => {
    output += x;
    const m = output.match(/DevTools listening on (ws:\/\/[^\s]+)/);
    if (m) resolve(m[1]);
  });
  chrome.on("exit", reject);
});
const debugOrigin = "http://" + new URL(browserUrl).host;
async function connect(url) {
  const ws = new WebSocket(url);
  await new Promise((r) => ws.addEventListener("open", r, { once: true }));
  let id = 0;
  const waiting = new Map();
  ws.addEventListener("message", (e) => {
    const x = JSON.parse(e.data);
    if (x.id) {
      waiting.get(x.id)?.(x);
      waiting.delete(x.id);
    }
  });
  return {
    ws,
    send: (method, params = {}) =>
      new Promise((resolve) => {
        const n = ++id;
        waiting.set(n, resolve);
        ws.send(JSON.stringify({ id: n, method, params }));
      }),
  };
}
const rows = [];
try {
  for (let pair = 0; pair < 8; pair++)
    for (const variant of pair % 2 ? ["head", "base"] : ["base", "head"]) {
      const key = variant + "-" + pair;
      const s = { requests: 0, bytes: 0, cancelled: 0 };
      stats.set(key, s);
      const target = await (
        await fetch(debugOrigin + "/json/new?about:blank", { method: "PUT" })
      ).json();
      const c = await connect(target.webSocketDebuggerUrl);
      await c.send("Runtime.enable");
      await c.send("Page.enable");
      await c.send("Page.addScriptToEvaluateOnNewDocument", {
        source:
          'window.started=false;addEventListener("TrunkApplicationStarted",()=>window.started=true)',
      });
      const start = performance.now();
      await c.send("Page.navigate", {
        url: `http://127.0.0.1:${port}/${key}/`,
      });
      let done = false;
      for (let n = 0; n < 200; n++) {
        await new Promise((r) => setTimeout(r, 30));
        const r = await c.send("Runtime.evaluate", {
          expression: "window.started === true",
        });
        if (r.result?.result?.value) {
          done = true;
          break;
        }
      }
      rows.push({
        pair,
        variant,
        ...s,
        ms: Math.round(performance.now() - start),
        done,
      });
      c.ws.close();
      await fetch(debugOrigin + "/json/close/" + target.id);
    }
  console.log(
    JSON.stringify(
      {
        payloadBytes: payload.length,
        patterns: patterns.length,
        isolatedHost: process.env.ISOLATED === "1",
        rows,
      },
      null,
      2,
    ),
  );
  if (rows.some((row) => !row.done)) process.exitCode = 1;
} finally {
  chrome.kill();
  await new Promise((resolve) => chrome.once("exit", resolve));
  server.close();
  fs.rmSync(profile, { recursive: true, force: true });
}
