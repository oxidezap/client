# The oxidezap plugin ABI

What a `.wasm` has to do to be a plugin, written for somebody who is not using
the Rust SDK. The SDK is a convenience over exactly this — nothing in it is
privileged — so a module produced by TinyGo, Zig, AssemblyScript or `wat` by
hand is a plugin on the same terms.

Everything below is `i32` and `i64`. There are no strings in the type system,
no structs, no component model: a string is a pointer and a length into the
module's own linear memory, and the host reads it from there.

**There is no WASI.** Not a restricted one — none. The `oxidezap` import
module is a plugin's entire outside world, so a plugin cannot open a file or a
socket because no function exists that would.

## What a module must export

| Export | Signature | |
|---|---|---|
| `memory` | — | The default linear memory. A module that exports none can be handed nothing, and is refused at load. |
| `oxi_abi_version` | `() -> i32` | Must return `1`. Called before anything else. A function rather than a global, because neither Rust nor TinyGo emits an exported wasm global without post-processing the module. |
| `oxi_init` | `() -> i32` | Called once. Return `0`, or the plugin is refused. |
| `oxi_on_event` | `(kind: i32, ev: i32) -> i32` | The only entry point after init. The return value is ignored; trap to stop. |

A module may not do anything in a start section: the loader has not accepted
it yet, so every import refuses until the module is instantiated, its version
answered and its exports found.

## What the host provides

All in the import module `oxidezap`. Pointers are byte offsets into the
module's exported memory.

### Declaring — callable only from inside `oxi_init`

Anywhere else these answer `-5` (`STATE`), which says *too early or too late*
rather than *not allowed*. Each may be called once; a second call is recorded
and refuses the load, because a plugin that could widen what it asked for
after the user was shown the first list would make that list a lie.

| Import | Signature |
|---|---|
| `oxi_subscribe` | `(mask: i64)` — which event kinds to deliver |
| `oxi_request_caps` | `(mask: i64)` — which commands this plugin wants |
| `oxi_set_name` | `(ptr: i32, len: i32) -> i32` — the name a user sees |

A plugin may not act on the account during `oxi_init` at all: plugins load
before the task that consumes the command channel exists, so a send there
would wait for an answer nothing can produce.

### Reading the event being handled

Every one takes the `ev` handle `oxi_on_event` was given. Handles are
arena-scoped: every handle becomes invalid when that call returns.

| Import | Signature | |
|---|---|---|
| `oxi_field_str` | `(ev, field, ptr, cap) -> i32` | Writes at most `cap` bytes; returns the value's **full** length. `n > cap` means it did not fit — size a buffer and ask again. `-1` when the field is absent. |
| `oxi_field_i64` | `(ev, field) -> i64` | `0` when absent. Booleans are `0` and `1`. |
| `oxi_field_len` | `(ev, field) -> i32` | How many elements a repeated field has; `0` when absent. |
| `oxi_field_at` | `(ev, field, index) -> i32` | A child handle, or `-1` past the end. Read the child with `oxi_field_str(child, 0, …)` — field `0` is `SELF`. |

The host cuts a short write at a **byte**, not at a character, so a value that
did not fit can end mid-sequence. Trimming that back to a whole character is
the plugin's job.

### Acting

One import per command rather than one `oxi_request` taking a serialized
request: that is what spares a plugin from carrying an encoder at all. Each
returns an outcome (below).

| Import | Signature | Capability |
|---|---|---|
| `oxi_send_text` | `(jid, jid_len, text, text_len) -> i32` | `SEND` |
| `oxi_send_reply` | `(jid, jid_len, text, text_len, quoted_id, quoted_id_len) -> i32` | `SEND` |
| `oxi_mark_read` | `(jid, jid_len, message_id, message_id_len) -> i32` | `MARK_READ` |
| `oxi_typing` | `(jid, jid_len, composing: i32) -> i32` | `TYPING` |
| `oxi_ui_set` | `(ptr, len) -> i32` | `UI` |
| `oxi_kv_get` | `(key, key_len, ptr, cap) -> i32` | `STORAGE` |
| `oxi_kv_set` | `(key, key_len, val, val_len) -> i32` | `STORAGE` |
| `oxi_timer_set` | `(delay_ms: i64, token: i64) -> i32` | `TIMERS` |
| `oxi_log` | `(level: i32, ptr: i32, len: i32)` | none |
| `oxi_now_ms` | `() -> i64` | none |

`oxi_kv_get` follows the same short-buffer convention as `oxi_field_str`. An
empty value passed to `oxi_kv_set` deletes the key. `oxi_ui_set` replaces the
plugin's whole published tree.

`oxi_now_ms` is the only clock — Unix milliseconds, deliberately coarse. A
finer one is a side channel, and nothing a plugin legitimately does needs
better.

### Outcomes

| | | |
|---|---|---|
| `0` | `ACCEPTED` | The session took it. What the network makes of it arrives as an event. |
| `-1` | `NO_SESSION` | Nothing to carry it out. Worth retrying. |
| `-2` | `REFUSED` | The daemon will not do this as asked. |
| `-3` | `DENIED` | The plugin did not declare this capability, or the user has not agreed to it. |
| `-4` | `INVALID` | A pointer outside memory, bytes that are not UTF-8, a length past what the host accepts. |
| `-5` | `STATE` | Right call, wrong moment — declaring outside `oxi_init`, or acting inside it. |

`-1` is also `ABSENT` for the field reads, where there is no outcome to
confuse it with.

## Capabilities

A mask, declared once through `oxi_request_caps`.

| Bit | | Asked of the user? |
|---|---|---|
| `1 << 0` | `SEND` | yes |
| `1 << 1` | `MARK_READ` | yes |
| `1 << 2` | `TYPING` | yes |
| `1 << 3` | `UI` | no |
| `1 << 4` | `STORAGE` | no |
| `1 << 5` | `TIMERS` | no |

**Declaring is not being allowed.** The three that act on the account are
withheld until somebody says yes, and the answer is recorded against the exact
mask it answered: a plugin that comes back wanting more is not partly
approved, it is unapproved again. Every check reads the answer live, so a
withdrawal bites on the next command rather than once a backlog has drained.

The other three take effect on declaration, because they are things a plugin
does only to itself — and because a plugin that could not draw its settings
panel before being allowed would leave the user agreeing to a name with
nothing to look at.

## Events

`oxi_on_event` is called with a `kind` and a handle. The subscription mask is
`1 << kind` — so messages are `2`, not `1`. **Bit zero names no kind**, and a
mask carrying it is refused at load along with any bit above the table: a
subscription that can never be delivered would otherwise leave a plugin
loaded, drawn, and permanently deaf.

| Kind | | Delivered without subscribing |
|---|---|---|
| 1 | `MESSAGE` | |
| 2 | `CONNECTION` | |
| 3 | `RECEIPT` | |
| 4 | `REACTION` | |
| 5 | `PRESENCE` | |
| 6 | `CALL` | |
| 7 | `UI_ACTION` | yes — a plugin's own widget |
| 8 | `TIMER` | yes — a plugin's own timer |

### Fields, by kind

Field ids are stable numbers, not one accessor each: that is what keeps the
import surface fixed as the table grows.

| Id | Name | Type | On |
|---|---|---|---|
| 0 | `SELF` | str | a child handle from `oxi_field_at` |
| 1 | `CHAT_JID` | str | message, receipt, reaction, presence, ui action |
| 4 | `IS_GROUP` | bool | message, receipt, reaction, presence |
| 10 | `MESSAGE_ID` | str | message, reaction |
| 11 | `TEXT` | str | message |
| 12 | `FROM_ME` | bool | message |
| 13 | `TIMESTAMP_MS` | i64 | message |
| 14 | `SENDER_JID` | str | message, reaction, presence |
| 15 | `SENDER_NAME` | str | message, presence |
| 16 | `REVOKED` | bool | message |
| 17 | `MEDIA_KIND` | i64 | message |
| 18 | `QUOTED_ID` | str | message |
| 30 | `CONNECTION_STATE` | i64 | connection |
| 31 | `REASON` | str | connection |
| 40 | `RECEIPT_KIND` | i64 | receipt |
| 41 | `MESSAGE_IDS` | list | receipt |
| 50 | `EMOJI` | str | reaction — empty when the reaction was removed |
| 60 | `COMPOSING` | bool | presence |
| 70 | `CALL_ID` | str | call |
| 71 | `CALL_EVENT` | i64 | call |
| 72 | `CALL_IS_VIDEO` | bool | call |
| 73 | `PEER_JID` | str | call |
| 80 | `ACTION_ID` | str | ui action |
| 81 | `ACTION_VALUE` | str | ui action |
| 90 | `TIMER_TOKEN` | i64 | timer |

Enumerated values: `MEDIA_KIND` is none/image/video/audio/document/sticker as
`0..=5`; `CONNECTION_STATE` is connecting/pairing/syncing/connected/
disconnected/logged out as `0..=5`; `RECEIPT_KIND` is delivered/read/played as
`0..=2`; `CALL_EVENT` is incoming/outgoing/answered/ended as `0..=3`.

### The absence rule

**A field's absence reads back as that type's default** — an empty string,
`0`, an empty list. It is the same rule the daemon's own wire format holds
itself to, and it is what makes adding a field a non-event for a plugin built
against an older table: reading an id this host does not know answers absent
rather than failing.

The cost is that a value cleared and a value never carried are
indistinguishable — a reaction that was removed arrives as an empty emoji,
exactly like an event that never had one. Telling those apart needs a field
that says so, which is a decision the ABI has not taken.

## The UI tree

A plugin does not draw. It declares a small tree of named widgets, each root
pinned to a slot the front end already has a place for, and the front end
decides what that looks like — which is why nothing in this format can express
a colour, a size or a position.

Fixed-width little-endian, pre-order, no varints:

```text
u8   format (1)
u32  number of roots
node:
  u8   kind
  u8   slot          (roots only; 0 below them)
  u8   flags
  u8   reserved, 0
  u16  number of children
  u32  id length,    then that many bytes
  u32  label length, then that many bytes
  u32  value length, then that many bytes
  ...children, pre-order
```

Kinds: `1` button, `2` toggle, `3` label, `4` text field, `5` row, `6` column,
`7` section. Only rows, columns and sections are drawn with children.

Slots: `1` chat header, `3` settings. `2` is **reserved and undefined** — the
composer's number, unassigned on purpose, because a slot the front end
silently ignores is a button whose author never finds out why it did not
appear.

Flags: `1` enabled, `2` checked.

Bounds: 8 deep, 256 nodes, 4096 bytes per id/label/value, 64 KiB encoded.

An id names one widget **within a slot**. Across slots it may repeat, because
an action says which slot it came from; twice in one slot is refused, since
nothing would tell the two presses apart. Ids are compared, never displayed,
so they are decoded strictly — a broken byte in an id is an error, where a
broken byte in a label is replaced and drawn.

When somebody uses a widget, `UI_ACTION` arrives carrying `ACTION_ID`,
`ACTION_VALUE` (a toggle's *new* state as `1`/`0`, a field's contents, empty
for a button) and `CHAT_JID` for a slot that has a chat behind it. The daemon
checks the action against the tree it currently publishes — kind and slot
included — so a press from a window whose frame is older than the daemon's
does not land as a real one.

## What the host holds a plugin to

Fuel meters the instructions a plugin runs, not the work the host does when
asked — an import is a handful of guest instructions and can be a kilobyte
copied or a task spawned. So the host bounds both.

| | |
|---|---|
| 50,000,000 fuel per call, 200,000,000 for init | Running out is a trap, and a trap stops the plugin. |
| 10 % of a core over a rolling 10 s | A plugin over its share is slept, not stopped. |
| 4 MiB of linear memory, 4 tables, 10,000 elements | Refused at instantiation, before an instruction runs. |
| 32 MiB module, 32 plugins | Asked of the file and of the folder, before a `Store` exists. |
| 512-deep event queue | Overflowing **stops** the plugin rather than skipping an event: a plugin's whole contract is having seen the messages. |
| 4096 event handles per call | Strings a handle clones into the host. |
| 2 KiB per log line, 64 KiB per call, 256 KiB per window | Writing a line is host I/O fuel does not price. Newlines are escaped, so a plugin cannot forge a second log entry. |
| 16 UI publishes per call | |
| 32 commands per call, 256 per window | |
| 1 MiB of key/value traffic per call, charged on reads as much as writes | 8 KiB per entry, 256 KiB per plugin. |
| 16 timers, 100 ms floor, 7 day ceiling | The floor is why a plugin cannot spin on its own timer. |
| 1 KiB name, 64 KiB action value | |

Nothing here is configurable, and none of it is negotiable from inside the
sandbox.

## Where a plugin lives

`~/.local/share/oxidezap/plugins` on Linux, `%LOCALAPPDATA%` on Windows, and
`OXIDEZAP_PLUGIN_DIR` overrides it. **The file's name is the plugin's id** —
`autoreply.wasm` is `autoreply` — and that id is what a user's answer is
recorded against, what its settings are filed under, and what two files may
not share.

Nothing loads out of a directory another local account can write, owner and
mode both, the directory and every module in it: an answer recorded against a
name is one somebody else's file under that name would inherit.

An approval is recorded against the **id and the mask, not a hash of the
bytes**, so replacing the file with a new build does not ask again. That is
defensible because the mask is the whole authority — there is no WASI, so what
the new code can do is exactly the sentence the user agreed to — and it is a
real trade against asking on every release, which is the surest way to teach
somebody to dismiss the question.

## A minimal module

The whole contract, in `wat`, for a plugin that names itself and answers
nothing:

```wat
(module
  (import "oxidezap" "oxi_subscribe" (func $subscribe (param i64)))
  (import "oxidezap" "oxi_set_name"  (func $set_name (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "Minimal")
  (func (export "oxi_abi_version") (result i32) (i32.const 1))
  (func (export "oxi_init") (result i32)
    (drop (call $set_name (i32.const 0) (i32.const 7)))
    (call $subscribe (i64.const 2))   ;; 1 << kinds::MESSAGE
    (i32.const 0))
  (func (export "oxi_on_event") (param i32 i32) (result i32) (i32.const 0)))
```

In Rust, that is `examples/template`. Whatever the language, the shape is the
same: export four things, declare inside `oxi_init`, read fields through
handles, and act through one import per command.
