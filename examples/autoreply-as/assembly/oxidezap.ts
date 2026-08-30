// The oxidezap plugin ABI, as an AssemblyScript module.
//
// This is what an `oxidezap-plugin-as` package would be: the eighteen host
// functions, the constants both sides agree on, and the three conveniences
// that keep a plugin from writing pointer arithmetic — a UTF-8 read, a UTF-8
// write, and the widget-tree encoder. Nothing here is privileged; it is the
// same contract `docs/plugin-abi.md` states, expressed in a language that
// happens to compile to a few kilobytes.

// ---------------------------------------------------------------- imports

@external("oxidezap", "oxi_subscribe")
declare function _subscribe(mask: i64): void;
@external("oxidezap", "oxi_request_caps")
declare function _requestCaps(mask: i64): void;
@external("oxidezap", "oxi_set_name")
declare function _setName(ptr: i32, len: i32): i32;
@external("oxidezap", "oxi_field_str")
declare function _fieldStr(ev: i32, field: i32, ptr: i32, cap: i32): i32;
@external("oxidezap", "oxi_field_i64")
declare function _fieldI64(ev: i32, field: i32): i64;
@external("oxidezap", "oxi_send_reply")
declare function _sendReply(jid: i32, jidLen: i32, text: i32, textLen: i32, quoted: i32, quotedLen: i32): i32;
@external("oxidezap", "oxi_send_text")
declare function _sendText(jid: i32, jidLen: i32, text: i32, textLen: i32): i32;
@external("oxidezap", "oxi_ui_set")
declare function _uiSet(ptr: i32, len: i32): i32;
@external("oxidezap", "oxi_kv_get")
declare function _kvGet(key: i32, keyLen: i32, ptr: i32, cap: i32): i32;
@external("oxidezap", "oxi_kv_set")
declare function _kvSet(key: i32, keyLen: i32, val: i32, valLen: i32): i32;
@external("oxidezap", "oxi_log")
declare function _log(level: i32, ptr: i32, len: i32): void;

// ---------------------------------------------------------------- constants

export namespace kinds {
  export const MESSAGE: i32 = 1;
  export const CONNECTION: i32 = 2;
  export const RECEIPT: i32 = 3;
  export const REACTION: i32 = 4;
  export const PRESENCE: i32 = 5;
  export const CALL: i32 = 6;
  export const UI_ACTION: i32 = 7;
  export const TIMER: i32 = 8;
}

export namespace caps {
  export const SEND: i64 = 1 << 0;
  export const MARK_READ: i64 = 1 << 1;
  export const TYPING: i64 = 1 << 2;
  export const UI: i64 = 1 << 3;
  export const STORAGE: i64 = 1 << 4;
  export const TIMERS: i64 = 1 << 5;
}

export namespace field {
  export const CHAT_JID: i32 = 1;
  export const IS_GROUP: i32 = 4;
  export const MESSAGE_ID: i32 = 10;
  export const TEXT: i32 = 11;
  export const FROM_ME: i32 = 12;
  export const REVOKED: i32 = 16;
  export const ACTION_ID: i32 = 80;
  export const ACTION_VALUE: i32 = 81;
}

export namespace level {
  export const ERROR: i32 = 1;
  export const WARN: i32 = 2;
  export const INFO: i32 = 3;
  export const DEBUG: i32 = 4;
}

/// A field that is not there. Distinct from a present-but-empty string, which
/// is the one distinction the absence rule leaves a plugin.
export const ABSENT: i32 = -1;

// ---------------------------------------------------------------- strings
//
// AssemblyScript strings are UTF-16 and the ABI speaks UTF-8, so every value
// crossing the boundary is encoded or decoded here rather than at each call
// site. `String.UTF8.encode` answers an ArrayBuffer whose address is what the
// host wants; holding it in a local is what keeps the collector off it for
// the length of the call.

class Utf8 {
  constructor(public buf: ArrayBuffer) {}
  @inline get ptr(): i32 { return changetype<i32>(this.buf); }
  @inline get len(): i32 { return this.buf.byteLength; }
}

@inline function utf8(s: string): Utf8 { return new Utf8(String.UTF8.encode(s, false)); }

/// How much room a read makes. One number for reads and writes both: a
/// setting stored whole and matched on its first 64 bytes would answer text
/// nobody configured.
const SCRATCH: i32 = 256;
const scratch = new ArrayBuffer(SCRATCH);

/// Read a string field, or `null` when it is absent or did not fit.
///
/// Truncation is not answered as a shorter string on purpose: a JID that did
/// not fit is not a shorter JID, it is somebody else.
export function fieldStr(ev: i32, id: i32): string | null {
  const n = _fieldStr(ev, id, changetype<i32>(scratch), SCRATCH);
  if (n == ABSENT || n > SCRATCH) return null;
  return String.UTF8.decodeUnsafe(changetype<usize>(scratch), n, false);
}

export function fieldBool(ev: i32, id: i32): bool { return _fieldI64(ev, id) != 0; }

export function get(key: string, fallback: string): string {
  const k = utf8(key);
  const n = _kvGet(k.ptr, k.len, changetype<i32>(scratch), SCRATCH);
  if (n <= 0 || n > SCRATCH) return fallback;
  return String.UTF8.decodeUnsafe(changetype<usize>(scratch), n, false);
}

export function set(key: string, value: string): i32 {
  const k = utf8(key); const v = utf8(value);
  return _kvSet(k.ptr, k.len, v.ptr, v.len);
}

export function flag(key: string): bool { return get(key, "") == "1"; }
export function setFlag(key: string, on: bool): void { set(key, on ? "1" : "0"); }

export function log(lvl: i32, line: string): void {
  const s = utf8(line);
  _log(lvl, s.ptr, s.len);
}

export function sendReply(jid: string, text: string, quotedId: string): i32 {
  const j = utf8(jid); const t = utf8(text); const q = utf8(quotedId);
  return _sendReply(j.ptr, j.len, t.ptr, t.len, q.ptr, q.len);
}

export function sendText(jid: string, text: string): i32 {
  const j = utf8(jid); const t = utf8(text);
  return _sendText(j.ptr, j.len, t.ptr, t.len);
}

// ---------------------------------------------------------------- declaring
//
// Each of these may be said once, from inside `oxi_init` and nowhere else.
// The Rust SDK makes a second call a missing method; AssemblyScript has no
// linear types, so the next best thing is a guard that says so in the log
// rather than leaving the loader to refuse the plugin with no explanation.

let declared: bool = false;

export function declare(name: string, subscribe: i64, capabilities: i64): void {
  if (declared) { log(level.ERROR, "declared twice; the second is refused"); return; }
  declared = true;
  const n = utf8(name);
  _setName(n.ptr, n.len);
  _subscribe(subscribe);
  _requestCaps(capabilities);
}

// ---------------------------------------------------------------- the tree

export namespace slot {
  export const CHAT_HEADER: u8 = 1;
  export const SETTINGS: u8 = 3;
}

namespace widget {
  export const BUTTON: u8 = 1;
  export const TOGGLE: u8 = 2;
  export const LABEL: u8 = 3;
  export const FIELD: u8 = 4;
  export const SECTION: u8 = 7;
}

const FLAG_ENABLED: u8 = 1;
const FLAG_CHECKED: u8 = 2;

/// The widget tree, written into a buffer the plugin owns.
///
/// Fixed-width little-endian, pre-order — so a node's children are counted
/// before they are written, which is why a section is opened, filled and
/// closed rather than built from a list.
export class Tree {
  private buf: Uint8Array;
  private at: i32 = 0;
  private roots: i32 = 0;
  private openAt: i32 = -1;
  private openChildren: i32 = 0;

  constructor(capacity: i32 = 1024) {
    this.buf = new Uint8Array(capacity);
    this.u8(1);      // format
    this.u32(0);     // root count, back-filled by `publish`
    this.at = 5;
  }

  @inline private u8(v: u8): void { this.buf[this.at++] = v; }
  @inline private u16(v: u16): void { store<u16>(this.buf.dataStart + this.at, v); this.at += 2; }
  @inline private u32(v: u32): void { store<u32>(this.buf.dataStart + this.at, v); this.at += 4; }
  private str(s: string): void {
    const b = String.UTF8.encode(s, false);
    this.u32(b.byteLength);
    memory.copy(this.buf.dataStart + this.at, changetype<usize>(b), b.byteLength);
    this.at += b.byteLength;
  }

  private node(kind: u8, slotByte: u8, flags: u8, id: string, label: string, value: string): void {
    this.u8(kind); this.u8(slotByte); this.u8(flags); this.u8(0);
    this.u16(0);   // child count, back-filled for a section
    this.str(id); this.str(label); this.str(value);
    if (slotByte != 0) this.roots++; else this.openChildren++;
  }

  /// Open a section pinned to a slot. Everything until `end` hangs off it.
  section(slotByte: u8, label: string): Tree {
    this.openAt = this.at + 4;          // where this node's child count sits
    this.node(widget.SECTION, slotByte, 0, "", label, "");
    this.openChildren = 0;
    return this;
  }

  end(): Tree {
    if (this.openAt >= 0) store<u16>(this.buf.dataStart + this.openAt, <u16>this.openChildren);
    this.openAt = -1;
    return this;
  }

  toggle(id: string, label: string, checked: bool, enabled: bool): Tree {
    let f: u8 = 0;
    if (enabled) f |= FLAG_ENABLED;
    if (checked) f |= FLAG_CHECKED;
    this.node(widget.TOGGLE, 0, f, id, label, "");
    return this;
  }

  field(id: string, label: string, value: string, enabled: bool): Tree {
    this.node(widget.FIELD, 0, enabled ? FLAG_ENABLED : 0, id, label, value);
    return this;
  }

  label(text: string): Tree { this.node(widget.LABEL, 0, 0, "", text, ""); return this; }

  button(id: string, label: string, enabled: bool): Tree {
    this.node(widget.BUTTON, 0, enabled ? FLAG_ENABLED : 0, id, label, "");
    return this;
  }

  /// Publish the whole tree. Whole every time, never a delta: the daemon
  /// compares what arrives against what it holds and publishes nothing when
  /// they match, so redrawing on every change costs a comparison.
  publish(): i32 {
    this.end();
    store<u32>(this.buf.dataStart + 1, <u32>this.roots);
    return _uiSet(<i32>this.buf.dataStart, this.at);
  }
}
