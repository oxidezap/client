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
init, 50 M per call, and a tenth of a core over a rolling ten seconds.

**And wasmi is itself an interpreter**, so an embedded JS engine is an
interpreter interpreting an interpreter, its work priced twice and then
rationed. This document's first draft called that the argument outliving
every version number below, and then measured it: QuickJS running a real
handler spends 5.5 % of one call's fuel and 1.4 MiB of the 4 MiB. The
prediction was wrong, and the measurement is in *What real TypeScript costs*
below. What the budgets do rule out is anything embedding SpiderMonkey, and
what actually blocks QuickJS turned out to be three smaller things nobody had
predicted at all.

Nothing rules out an *approach* on taste. What rules things out is that list.

## The candidates

| | What it is | Verdict here |
|---|---|---|
| [AssemblyScript](https://www.assemblyscript.org/) | A language with TypeScript's syntax → core wasm, via Binaryen | **Smallest by far** (541 B–9 KiB, imports only `oxidezap`) and **not TypeScript**: no `any`, unions, `for…of`, destructuring, exceptions, regex, `JSON` or `async`. |
| [Porffor](https://github.com/CanadaHonk/porffor) | AOT JS/TS engine: JS → IR → C → native or wasm | **Measured, and no.** 328 KiB for a ten-line handler, eight WASI imports, `_start` as its only export, and `setjmp` under every `throw`. |
| [jz](https://github.com/dy/jz) | "Good parts" JS subset → wasm, no runtime, no GC | Right shape, wrong domain: numeric/DSP code, not strings and objects. |
| [Javy](https://github.com/bytecodealliance/javy) / [Extism js-pdk](https://github.com/extism/js-pdk) | QuickJS in wasm, snapshotted with Wizer | **The only route to real TypeScript, and it fits**: measured at 5.5 % of a call's fuel and 1.4 MiB of the 4 MiB, needing a WASI shim, wasmi's SIMD feature and a QuickJS plugin of our own. |
| [Jawsm](https://github.com/drogus/jawsm) | JS → wasm, no interpreter, Rust | Blocked: built on wasm-GC, exception handling and tail calls. Two of the three are what wasmi lacks. |
| [Wasmnizer-ts](https://github.com/intel/Wasmnizer-ts) | Intel's TypeScript → WasmGC toolchain | Blocked, and by three at once: WasmGC, exception handling and stringref. |
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

**What real code costs.** The 541-byte figure is a handler working out of a
static buffer, which is the shape the ABI's pull-based reads encourage. The
same ten lines measured against Porffor below — `toLowerCase`, `trim`,
`startsWith`, `includes`, a loop over an array of strings, string
concatenation — with AssemblyScript's real `String` and `Array` behind them:

| runtime | with `env.abort` | with `--use abort=` |
|---|---|---|
| `stub` (bump allocator, never frees) | 9,660 B | **9,225 B, zero imports** |
| `minimal` (GC called externally) | 11,210 B | — |
| `incremental` (default GC) | 12,508 B | 11,954 B, zero imports |

Under ten kilobytes for the whole thing, against 328 KiB from Porffor for the
same source. `stub` is the honest default for a plugin: a call is fuel-bounded
and every event handle dies when it returns, so a handler that leaks its
per-call garbage into a 4 MiB linear memory is a plugin that eventually traps
rather than one that misbehaves — and the SDK can say so and offer
`incremental` to anyone who wants the two kilobytes spent.

**The one trap is the start section.** Add a top-level dynamic initializer —
`let table = new StaticArray<i32>(4)` — and `asc` emits `(start $0)`, which
this host permits and runs with *every import refusing*
(`a_module_cannot_act_on_the_account_before_it_is_loaded`). A module whose
globals are set up there loads, silently, half-initialized. `--exportStart`
moves it to an ordinary export, and nothing calls it: so an AssemblyScript SDK
would export the start under a name of its own and call it as the first line
of `oxi_init`, and a plugin without an SDK should keep top-level state to
`memory.data` and constants. Verified both ways — 541 bytes with no start
section, 296 bytes with one — and note that string constants alone are enough
to produce one: the handler above emits `(start $9)` to lay its strings out in
memory. That start is harmless, because it calls no import; the rule the SDK
has to hold is not *no start section*, it is *nothing in it may talk to the
host*.

What is missing is not capability, it is the SDK: the Rust one's value is the
two mask types, the `Setup` whose methods vanish once used, the field sizes
and `Event::which`, and TypeScript's type system can express every one of
those. That is an `oxidezap-plugin-as` package, and it is a day of work rather
than a research question.

## A plugin in AssemblyScript, end to end

`examples/autoreply-as` is the Rust `examples/autoreply` rewritten against the
same ABI: the same three settings, the same widget tree, the same refusal to
answer its own messages or a group. It is here because the survey above is
about compilers and the decision is about plugins.

Verified by running the module under wasmi with this daemon's fuel budget and
a host answering the ten imports — declaring, publishing, reading fields,
storing settings, sending:

| | |
|---|---|
| module | **8,915 bytes**, ten imports, all `oxidezap` |
| `oxi_init` | 39,445 fuel; the widget tree it publishes is 214 bytes and decodes clean |
| a message carrying the keyword | **14,407 fuel — 0.029 % of a call's budget**, and the reply comes out quoting the right id |
| a message without it | 3,214 fuel |
| a UI action turning the plugin off | 18,602 fuel, and the store holds `enabled = "0"` |
| 1,000 messages in a row | linear memory flat at one 64 KiB page |

Against the QuickJS route measured above — 2,765,416 fuel and 1,408 KiB for
comparable work — that is 190× the fuel and 22× the memory saved, and it is
the whole argument for a compiled subset stated in numbers rather than in
principle.

Two things the example had to get right, and both are host rules rather than
language ones. `--use abort=` removes `env.abort`, which no host function
answers. And nothing may run at the top level but constants: `asc` puts
anything else in a start section, which the loader runs **with every import
refusing**, so a plugin that initializes there comes up silently half-built.

What it costs to read is visible in one function: `containsIgnoringCase` walks
indices because `for…of` does not exist, and the JID it sends to is checked
for `null` because a truncated read is somebody else rather than a shorter
string. Neither is idiomatic TypeScript. Both are the price of 8,915 bytes.

## AssemblyScript is not TypeScript

The section above is about what the *compiler* produces. This one is about
what an author has to write, and the answer decides more than the byte counts
do: **AssemblyScript is a separate language that borrows TypeScript's
syntax.** A `.ts` file that `tsc` accepts is, more often than not, refused by
`asc` — not because it is bad TypeScript, but because the construct does not
exist in the language.

Measured by compiling one construct at a time with `asc` 0.28. Every "no"
below is a first-line compiler error, not a runtime surprise:

| Construct | `asc` |
|---|---|
| `any` | **no** — `TS2304: Cannot find name 'any'` |
| union types (`i32 \| string`) | **no** — `AS100: Not implemented: union types` |
| `string \| null` | yes — references are nullable, so this one narrows |
| optional properties (`b?: bool`) | **no** — `AS219: Optional properties are not supported` |
| object literals (`{ a: 1, b: 2 }`) | **no** — every object is a class |
| `for (const x of xs)` | **no** — `AS100: Not implemented: Iterators` |
| destructuring, array or object (`const [a, b] = xs`) | **no** — `TS1003` |
| spread (`[...xs, 3]`) | **no** — `AS100: Not implemented: Spread operator` |
| `??` (nullish coalescing) | **no** — `TS1109: Expression expected` |
| `try` / `catch` | **no** — `AS100: Not implemented: Exceptions` |
| `throw` | yes — but it aborts; nothing can catch it |
| regular expressions | **no** — `AS100: Not implemented: Regular expressions` |
| `JSON.parse` / `JSON.stringify` | **no** — `TS2304: Cannot find name 'JSON'` |
| `async` / `await`, promises | **no** — parse error |
| closures over a parameter | **no** — `TS2454: Variable 'k' is used before being assigned` |
| closures over a `const` in scope | yes |
| structural typing (`class P` where an `interface Named` is wanted) | **no** — `implements` is required; typing is nominal |
| anything from npm (`import { x } from "fs"`) | **no** — there is no npm, only `~lib` |
| classes, getters, generics, `Map`, template literals, `filter`/`map`/`reduce`, `enum`, default parameters, `interface` + `implements`, `Date.now`, string methods | yes |

The right hand column is not a to-do list. `any` and unions are absent
because there are no runtime types to switch on; exceptions are absent
because there is no unwinder; `for…of` is absent because there is no iterator
protocol; structural typing is absent because objects are laid out like C
structs. Each is a consequence of compiling to a 9 KiB module with no engine
in it — which is exactly the property that made the numbers above possible.

So an author does not write TypeScript with a few libraries missing. They
write a statically typed, nominally typed, exception-free, `any`-free
language whose numbers are `i32` and `f64` rather than `number`, and they
find that out one error at a time, from `tsc`-shaped error codes, in code
that looks like TypeScript. **If the goal is "people write normal TypeScript",
AssemblyScript does not meet it** — and the value it does have, a real 9 KiB
module, is a different goal.

## Porffor, measured

Porffor is the project worth asking about, and it is now the one with numbers
against it. Built at `a415d19` (29 Aug 2026), compiled with wasi-sdk 27
(clang 20) — the toolchain its own C output is written for, since the file
carries `#ifdef __wasi__` branches.

There is no wasm target in the CLI. `porf` takes `c` and `native`; wasm is
reached by taking the emitted C and compiling it yourself, and the C does not
compile for wasm as it stands: it includes `sys/mman.h`, `signal.h` and
`setjmp.h`, and wasi-libc refuses all three by `#error` until you ask for the
emulations. The build that works is

```sh
porf c plugin.js -o plugin.c
clang --target=wasm32-wasip1 -O2 \
  -D_WASI_EMULATED_SIGNAL -D_WASI_EMULATED_MMAN -mllvm -wasm-enable-sjlj \
  -o plugin.wasm plugin.c -lwasi-emulated-signal -lwasi-emulated-mman
```

and what comes out of it:

| Source | C | wasm |
|---|---|---|
| empty file | 93 KiB | **88.5 KiB** |
| `console.log('hello world!')` | 94 KiB | **145 KiB** |
| a 10-line handler: `toLowerCase`, `startsWith`, `includes`, a `for…of` | 237 KiB | **328 KiB** (306 KiB after `wasm-opt -Oz`) |

The 88.5 KiB floor is the runtime — an arena the C reserves with `mmap` and
commits with `mprotect`, plus the GC metadata beside it. The `hello world`
module runs: under Node's WASI it prints `hello world!`, so this is a real
pipeline and not a toolchain accident.

Four things stop it being a plugin toolchain here, and they are structural
rather than a matter of flags.

**It asks for WASI.** Eight `wasi_snapshot_preview1` imports — `fd_write`,
`fd_seek`, `fd_close`, `fd_pread`, `fd_fdstat_get`, `args_get`,
`args_sizes_get`, `proc_exit` — because the runtime writes `console.log`
through stdio. There is no WASI here.

**`throw` is `setjmp`.** `compiler/render.js` renders every JS `try` as
`_setjmp(porf_try_stack[…])`, so exceptions are C longjmps. On wasm that is
the exception-handling proposal, which wasmi does not implement; with
`-mllvm -wasm-enable-sjlj` clang instead emits three imports —
`env.__wasm_setjmp`, `env.__wasm_setjmp_test`, `env.__wasm_longjmp` — for
glue the embedder is expected to supply. The handler module fails to
instantiate even under a full WASI host for exactly that reason: *Import #0
module="env": module is not an object or function*. `hello world` escapes only
because it never reaches a try frame; the ten-line handler does, without a
single `try` in its source, because `for…of` has one.

**It exports `_start` and nothing else.** The ABI needs `oxi_abi_version`,
`oxi_init` and `oxi_on_event` as exports. Porffor compiles a *program*, not a
reactor: there is no `export_name` on anything it emits and no
`-mexec-model=reactor` path through its C.

**Its FFI is `Porffor.dlopen`.** A native shared library by name — which is
the right design for the thing Porffor is, and is not a route to an
`oxidezap` import. Reaching the host would mean hand-written C shims with
`__attribute__((import_module("oxidezap")))` linked beside the generated
file, i.e. maintaining a C layer per ABI function.

None of that is a criticism of Porffor: it compiles JS to a 33 KiB *native*
binary and runs on things with no operating system, which is a genuinely
remarkable achievement. Its wasm claim is against the interpreter-embedding
route — 328 KiB against Javy's 869 KiB, and that comparison is honestly won.
The comparison this daemon cares about is against 9 KiB of AssemblyScript
doing the same ten lines, and it loses that one by thirty-five times before
the four blockers above are even reached.

Worth revisiting if two things change: wasmi ships exception handling (it is
under development, and it would also unblock Jawsm), and Porffor grows a
reactor-shaped wasm output with importable host functions. Neither is ours to
do, and neither is far-fetched.

## What real TypeScript costs, measured

There is exactly one way to run TypeScript that `tsc` accepts: strip the types
(`tsc`, `esbuild`) and run the JavaScript in an engine. So the question is not
which compiler — it is whether a JS engine fits inside this host's budgets.
Nobody here had measured that, and the guess in an earlier draft of this
document was wrong.

The measurement: the handler from the section above — interfaces, optional
properties, a union return, `filter`/`find`, `??`, destructuring, a template
literal, a regex `split`, `Map`, `JSON.stringify` — bundled by esbuild (634
bytes of JS), built with Javy 6.0.0, and run under **wasmi 1.1 with this
daemon's own numbers**: fuel metering on, a 4 MiB `StoreLimits`, and a
hand-written shim for the nine WASI imports (~60 lines; `fd_write` to a
buffer, the rest zeroed).

| | |
|---|---|
| module, static linking | 1,284,838 B (1.22 MiB) — of 32 MiB allowed |
| module, dynamic linking | **1,453 B**, against a shared 1.26 MiB QuickJS provider |
| memory | 22 pages, **1,408 KiB** — of 4 MiB allowed |
| instantiation | 0 fuel, ~1 ms (Wizer snapshotted the engine) |
| one call, running the real handler | **2,765,416 fuel — 5.5 % of the per-call budget**, 5 ms |
| output | `{"out":"pong (1) !apf"}` — correct, regex and all |

It fits. Not "fits if we raise the limits": it fits inside the limits as
written, with the whole language present — regexes, exceptions, `any`,
promises, closures, npm-shaped code.

Four things stand between that and a plugin, none of them the ones this
document previously predicted:

1. **wasmi rejects the module as built.** `SIMD support is not enabled` —
   QuickJS-ng uses SIMD, and wasmi puts SIMD behind a cargo feature that
   `default-features = false` leaves off. One line in `Cargo.toml`, and a
   deliberate widening of what any plugin may contain.
2. **Nine WASI imports.** Sixty lines of shim, or the categorical sentence in
   `AGENTS.md` stops being true. The shim is the smaller half: what it means
   is that "no WASI" becomes "no WASI except the nine we answer".
3. **A Javy export is one-shot.** Measured: the first call to an exported
   function runs; every call after it traps with *out of bounds memory
   access* at ~500 fuel. Javy's model is an instance per invocation, which is
   cheap for it (1 ms, 0 fuel) and wrong for us — a plugin's `Map` of counts,
   and everything else it holds between events, would not survive to the next
   one. Reaching a plugin that keeps state means our own QuickJS build rather
   than the shipped Javy one.
4. **Nothing bridges the ABI.** A Javy export takes no arguments and returns
   nothing, and the JS inside cannot call `oxi_send_text` because no binding
   for it exists. Those bindings are what a *custom Javy plugin* is
   (`javy-plugin-api`), and building one is precisely what Extism's js-pdk
   did.

Which reframes the project. If "people write normal TypeScript" is the
requirement, the thing to build is **not a compiler** — it is a QuickJS
plugin: a Rust crate compiled to wasm that keeps a JS context alive across
calls and exposes the eighteen `oxi_*` imports as JS globals. Every plugin
then ships as ~1.5 KiB of bytecode against one provider module the daemon
ships with itself, and the fuel and memory numbers above are what it costs.
That is weeks of work against a compiler's years, and it is the only route
measured here that ends with a `.ts` file `tsc` would accept.

What it costs in exchange is the sentence this sandbox has been able to make
so far: a plugin would carry a JS engine, the host would answer WASI calls,
SIMD would be allowed, and the interpreter-in-an-interpreter overhead is real
even though it turned out to be 5.5 % rather than the disaster predicted here.
That is a product decision — the same shape as `oxi_http_fetch` — and it
should be made deliberately rather than as a consequence of picking a
toolchain.

## Writing our own, in Rust

The honest version of "let's build a minimal TS/JS → wasm compiler in Rust,
maybe on [oxc](https://oxc.rs/)": the front end is the part that is free, and
it is not the part that decides the project. Read this after *What real
TypeScript costs* — if the requirement is TypeScript that `tsc` accepts, a
compiler is the wrong project and an engine is the right one, and what
follows is about the other requirement.

oxc gives a `.ts`/`.tsx` parser that passes Test262 and 99 % of the
TypeScript suite, plus semantic analysis, scopes and symbol resolution — and
[`wasm-encoder`](https://crates.io/crates/wasm-encoder) or
[`walrus`](https://github.com/wasm-bindgen/walrus) gives the binary at the
other end. A week gets `export function add(a: i32, b: i32): i32` down to a
valid module. What oxc does not give is a *type checker*: it parses TypeScript
types, it does not infer or check them. Everything after the parse — a
checker, a layout for objects and strings, an allocator, `String`, `Array`,
`Map`, `JSON`, the numeric tower, and an optimizer to stand in for Binaryen —
is the compiler.

That is AssemblyScript's entire body of work, and rewriting it to get what
AssemblyScript already produces is not a plan. So the question worth asking is
narrower: **is there a compiler that is not a general one?**

There is a real argument that there is, and it comes from this host rather
than from language design. Every plugin call is bounded — 50 M fuel, event
handles invalid the moment `oxi_on_event` returns, nothing survives a call but
the key-value store. A compiler that only ever has to serve *that* shape can
reset an arena at the end of each call and have no GC at all, no finalizers,
no cycle collector, no shadow stack: the three hardest parts of AssemblyScript
are things a plugin dialect does not need. Give it strings, arrays, plain
objects, closures that do not escape a call, and the ABI's imports as
first-class functions, and the compiler is a few thousand lines rather than a
few hundred thousand — with modules plausibly under 2 KiB, since nothing but
the plugin's own code is in them.

What that buys, honestly: no Node in the plugin toolchain (`asc` is npm), the
ABI known to the compiler rather than declared by hand, and sizes we set. What
it costs is the long tail — the first person to write `array.reduce(…)`, or a
regex, or `async`, files a bug, and the answer is either "not in this dialect"
forever or an unbounded queue of standard-library work. jz is the honest
precedent: it took the same bet on a JS subset and the subset it kept is
numeric, because that is what stays small.

So: worth designing on paper, not worth starting instead of an
`oxidezap-plugin-as` package. The one thing that would change that is wanting
plugins written in something we control end to end — a decision about the
product, not about compilers.

Two cheaper bets to make first, in order: watch wasmi's exception-handling
issue, because shipping it makes Porffor-via-C and Jawsm live options and
costs us a version bump; and if npm in a plugin author's path is the real
objection, note that `asc` is itself a wasm bundle — vendoring it is a
smaller project than writing a compiler by three orders of magnitude.

## What a decision needs

The two goals are in tension and the tension is not resolvable by picking a
better toolchain — it is the same trade in every row of this document. A
compiled plugin is a subset language; real TypeScript is an engine.

**If minimal and compiled is the requirement**, the work is:

- an `oxidezap-plugin-as` package, which is `examples/autoreply-as/assembly/oxidezap.ts`
  moved out of the example and given the guarantees the Rust SDK has — masks
  that cannot be passed where the other is wanted, an event narrowed to the
  fields its kind carries, a declaration that cannot be made twice;
- a line in `docs/plugin-abi.md` about the start section, which is the first
  thing a non-Rust author hits and which nothing currently states: the loader
  runs it with every import refusing;
- and saying plainly, wherever plugins are documented, that this is
  AssemblyScript rather than TypeScript. The subset is defensible; a plugin
  author discovering it one `tsc`-shaped error code at a time is not.

`json-as` compiles and works if a plugin needs JSON (15,299 bytes for parse
and stringify of one class); `assemblyscript-regex` does not compile against
`asc` 0.28 at all, so a plugin wanting a regex has to write the matcher it
needs. Neither changes the answer, and both are worth knowing before somebody
promises a user a regex.

**If real TypeScript is the requirement**, it is a QuickJS plugin of our own
and the four costs in *What real TypeScript costs* — an engine inside the
sandbox, nine answered WASI imports, wasmi's SIMD feature, and 190× the fuel
of the compiled route for identical work.

Nothing further on Porffor until wasmi has exception handling.
