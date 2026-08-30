# Writing a plugin in something other than Rust

`docs/plugin-abi.md` says a module produced by TinyGo, Zig, AssemblyScript or
`wat` is a plugin on the same terms as one built with the SDK. This is the
survey behind that sentence for the language people actually ask for —
JavaScript, and TypeScript — written after measuring the candidates rather
than reading their front pages. It is research: nothing here is implemented.

## What the host decides before any compiler is chosen

Four facts about this daemon rule out most of the field, and they are worth
stating first because every project below is otherwise perfectly good.

**The interpreter is wasmi, and wasmi has no GC, no exception handling, no
function references and no threads.** It has reference types, bulk memory,
tail calls, multi-memory, SIMD, memory64 and custom page sizes; the four
missing ones are under development and not shipped. So a compiler that lowers
JavaScript objects onto wasm-GC structs, or `try`/`catch` onto the exception
handling proposal, does not produce a module this host can instantiate — not
"runs slowly", cannot load.

**There is no WASI.** Not a restricted one, none. A toolchain that emits
`wasi_snapshot_preview1` imports for `fd_write` and `proc_exit` because its C
library wanted stdio produces a module that fails instantiation on a missing
import. Shimming those imports is possible and is a decision about the
sentence in `AGENTS.md` — a plugin's whole outside world is the `oxidezap`
module — rather than a build flag.

**The budgets are small on purpose.** 4 MiB of linear memory, 200 M fuel for
init, 50 M per call, and a tenth of a core over a rolling ten seconds. An
embedded JS engine spends the first two on existing.

**And wasmi is itself an interpreter.** A QuickJS module here is an
interpreter interpreting an interpreter, under a 10 % duty share. That is the
argument that outlives every version number below: it is not that the engine
is too big for the module limit — 869 KiB against 32 MiB is nothing — it is
that the work it does is priced twice and then rationed.

Nothing rules out an *approach* on taste. What rules things out is that list.

## The candidates

| | What it is | Verdict here |
|---|---|---|
| [AssemblyScript](https://www.assemblyscript.org/) | Strict TypeScript subset → core wasm, via Binaryen | **Works today.** Measured below: 541 bytes, imports only from `oxidezap`. |
| [Porffor](https://github.com/CanadaHonk/porffor) | AOT JS/TS engine: JS → IR → C → native or wasm | Watch. The wasm path now goes through a C compiler, so the question is a freestanding libc-less build nobody has done here. |
| [jz](https://github.com/dy/jz) | "Good parts" JS subset → wasm, no runtime, no GC | Right shape, wrong domain: numeric/DSP code, not strings and objects. |
| [Javy](https://github.com/bytecodealliance/javy) / [Extism js-pdk](https://github.com/extism/js-pdk) | QuickJS in wasm, snapshotted with Wizer | Real JavaScript, at the cost of WASI imports, the memory budget and double interpretation. |
| [Jawsm](https://github.com/drogus/jawsm) | JS → wasm, no interpreter, Rust | Blocked: built on wasm-GC, exception handling and tail calls. Two of the three are what wasmi lacks. |
| [ComponentizeJS](https://github.com/bytecodealliance/ComponentizeJS) / StarlingMonkey | SpiderMonkey embedding → wasm component | Blocked twice: the component model is the trade this ABI is built around, and the embedding is ~8 MB. |
| [Static Hermes](https://github.com/facebook/hermes) | Meta's AOT JS compiler; can target wasm | Targets wasm through a C/WASI toolchain, same shape of problem as Porffor with a much larger runtime. |
| [MoonBit](https://www.moonbitlang.com/) | TS-flavoured language, wasm-first, linear-memory backend | Not JS, but the closest thing to "TypeScript that produces a 30 KiB module". Worth a line in the ABI doc if anyone tries it. |

## AssemblyScript, measured

Not extrapolated from the project's own numbers. `asc` 0.28, a handler that
names itself, subscribes to messages, reads `CHAT_JID` off the event and
sends a reply:

```ts
@external("oxidezap", "oxi_subscribe")
declare function subscribe(mask: i64): void;
@external("oxidezap", "oxi_set_name")
declare function setName(ptr: i32, len: i32): i32;
@external("oxidezap", "oxi_field_str")
declare function fieldStr(ev: i32, field: i32, ptr: i32, cap: i32): i32;
@external("oxidezap", "oxi_send_text")
declare function sendText(jid: i32, jidLen: i32, text: i32, textLen: i32): i32;

let scratch = memory.data(512);

export function oxi_abi_version(): i32 { return 1; }
export function oxi_init(): i32 { /* setName, subscribe */ return 0; }
export function oxi_on_event(kind: i32, ev: i32): i32 { /* … */ return 0; }
```

```sh
asc plugin.ts -o plugin.wasm --runtime stub --use abort= \
  --optimize --shrinkLevel 2 --noAssert
```

**541 bytes.** Five imports, every one of them `oxidezap`. Four exports:
`memory`, `oxi_abi_version`, `oxi_init`, `oxi_on_event` — exactly the four the
ABI asks for, with no `env.abort`, no `env.seed`, no WASI and no start
section. `examples/autoreply` in Rust is 6 KiB, so this is not a compromise
that has to be argued for on size.

Three flags carry that and each answers a real default:

- `--runtime stub` is the bump allocator that never frees (~400 B). The
  default incremental GC is ~2 KiB gzipped and is the right answer for a
  plugin that allocates; a handler working out of a static scratch buffer,
  which is what the ABI's pull-based reads encourage, wants neither.
- `--use abort=` removes the `env.abort` import. Without it the module asks
  the host for a function no host function answers, and instantiation fails
  on a missing import rather than on anything AssemblyScript did wrong.
- Nothing imports `env.memory`: AssemblyScript exports its memory by default,
  which is what the loader needs to hand a plugin anything at all.

**The one trap is the start section.** Add a top-level dynamic initializer —
`let table = new StaticArray<i32>(4)` — and `asc` emits `(start $0)`, which
this host permits and runs with *every import refusing*
(`a_module_cannot_act_on_the_account_before_it_is_loaded`). A module whose
globals are set up there loads, silently, half-initialized. `--exportStart`
moves it to an ordinary export, and nothing calls it: so an AssemblyScript SDK
would export the start under a name of its own and call it as the first line
of `oxi_init`, and a plugin without an SDK should keep top-level state to
`memory.data` and constants. Verified both ways — 541 bytes with no start
section, 296 bytes with one.

What is missing is not capability, it is the SDK: the Rust one's value is the
two mask types, the `Setup` whose methods vanish once used, the field sizes
and `Event::which`, and TypeScript's type system can express every one of
those. That is an `oxidezap-plugin-as` package, and it is a day of work rather
than a research question.

## Porffor, in fairness to the question

Porffor is the project worth asking about, and the answer moved. It is an AOT
JS/TS engine — no interpreter in the output, which is exactly the property
that makes a 6 KiB plugin conceivable in a language with objects — and the
pipeline as of August 2026 is JS/TS → Porffor IR → **C** → a native binary or
wasm. The tree at `a415d19` contains no wasm backend at all; the wasm target
is reached by compiling the emitted C.

That relocates the question rather than answering it. What decides it here is
what that C needs: a freestanding `wasm32-unknown-unknown` build against no
libc emits no WASI imports and could satisfy this ABI, while an ordinary
wasi-sdk build emits `fd_write` and friends and cannot. Nobody in this tree
has built it either way, and the honest next step is one afternoon: compile
`console.log('hi')` for wasm, read the import section, and measure the module.
Its own claim is that modules "come out drastically smaller" than the
interpreter-embedding route, which is the right comparison and not the one
that matters here — the comparison that matters is against 541 bytes of
AssemblyScript.

Two further notes for whoever does that. Porffor is pre-1.0 with releases cut
per push, so a plugin ecosystem pinned to it inherits that cadence. And its
TypeScript support is real but is the engine's own annotated dialect
(`.porf.ts`), not `tsc` semantics — which puts it nearer AssemblyScript on the
"is this really TypeScript" axis than the framing suggests.

## Javy, and what running real JavaScript would cost

The only route that runs JavaScript as written — npm-shaped code, `JSON`,
regexes, closures — is an embedded engine, and Javy is the mature one:
QuickJS-ng compiled to wasm, the user's script parsed at build time and the
whole VM snapshotted with Wizer so nothing parses at startup. Static linking
is ~869 KiB; dynamic linking gets the per-plugin module to 1–16 KiB by
importing a shared `javy_quickjs_provider` module, which the host would have
to supply.

Four costs, in the order they would bite:

1. WASI imports for stdin/stdout, which either get stubbed in the host or the
   categorical sentence in `AGENTS.md` stops being true.
2. The 4 MiB memory limit, against an engine sized for hundreds of KiB of
   heap before the plugin's own data. Not obviously fatal; unmeasured.
3. Fuel. Wizer removes the parse, not the instantiation, and 200 M fuel for
   init is a budget written for a module that memsets a scratch buffer.
4. The duty share. Interpreted JS inside an interpreted host, rationed to a
   tenth of a core, is the cost that does not go away with tuning.

It is a coherent thing to want and it is a different product decision from
"which compiler" — closer in kind to `oxi_http_fetch` than to a build flag.

## What a decision needs

- An `oxidezap-plugin-as` SDK and an `examples/` plugin built with it, which
  is what turns "AssemblyScript works" into something somebody can copy.
- One afternoon on Porffor's wasm output: import section and size.
- A line in `docs/plugin-abi.md` naming the start-section trap, since it is
  the first thing an AssemblyScript author hits and nothing in the ABI says
  the loader will run that code with every import refusing.
