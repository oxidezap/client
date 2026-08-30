# autoreply, in AssemblyScript

The same plugin as `../autoreply`, against the same ABI, in the language
people ask for when they ask for TypeScript. It exists to answer two
questions with a file rather than an opinion: whether the ABI is really
language-neutral, and what a plugin weighs when it is not written in Rust.

```sh
npm install
npm run build     # -> autoreply-as.wasm
cp autoreply-as.wasm ~/.local/share/oxidezap/plugins/
```

Measured at `--runtime incremental --use abort= --optimize --shrinkLevel 2`,
and verified by running the module under wasmi with the daemon's own fuel
budget and a host answering the ten imports:

| | |
|---|---|
| module | **8,915 bytes** — ten imports, all `oxidezap`; no WASI, no SIMD |
| `oxi_init` | 39,445 fuel, publishing a 214-byte widget tree |
| one message carrying the keyword | **14,407 fuel — 0.029 % of a call's budget** |
| one message without it | 3,214 fuel |
| 1,000 messages | linear memory flat at one 64 KiB page |

`--runtime incremental` rather than `stub`, and the last row is why: this
plugin allocates per call — every string crossing the ABI is UTF-8 encoded
out of AssemblyScript's UTF-16 — so a bump allocator that never frees would
climb until it hit the 4 MiB limit and trap. Two kilobytes of collector buys
a plugin that runs for a month.

`--use abort=` removes AssemblyScript's `env.abort` import, which no host
function here answers: without it the module fails instantiation on a missing
import rather than on anything the author did. And nothing in
`assembly/index.ts` may run at the top level except constants and
`memory.data` — `asc` puts anything else in a start section, and the loader
runs that **with every import refusing**, so a plugin initialized there comes
up silently half-built.

## What this is not

AssemblyScript is not TypeScript. It has TypeScript's syntax and its own
semantics: no `any`, no union types, no `for…of`, no destructuring, no
spread, no `??`, no `try`/`catch`, no regular expressions, no `JSON`, no
`async`, and nominal rather than structural typing. Everything in this
directory is written inside that subset on purpose — the index loop in
`containsIgnoringCase` is an index loop because `for…of` does not exist here,
not because it reads better.

`docs/plugin-languages.md` is the measured survey of why this is the subset
on offer, and what the alternatives cost.
