// The autoreply plugin, in AssemblyScript.
//
// The same plugin as `examples/autoreply`, written against the same ABI, to
// answer one question with a file rather than an opinion: what does a plugin
// look like when it is not written in Rust, and what does it weigh. Read it
// beside the Rust one — the shape is the same because the ABI is the same.

import {
  Tree, caps, field, kinds, level, slot,
  declare, fieldBool, fieldStr, flag, get, log, sendReply, set, setFlag,
} from "./oxidezap";

/// Keys in this plugin's own store.
const ON = "enabled";
const KEYWORD = "keyword";
const REPLY = "reply";

/// Widget ids. The same strings identify a widget in the tree and name the
/// action that comes back, which is what makes `handleAction` a comparison
/// rather than a lookup.
const ID_ON = "enabled";
const ID_KEYWORD = "keyword";
const ID_REPLY = "reply";

const DEFAULT_KEYWORD = "ping";
const DEFAULT_REPLY = "pong";

export function oxi_abi_version(): i32 { return 1; }

export function oxi_init(): i32 {
  // Messages, and nothing else. An account's whole traffic is receipts and
  // presence; asking for kinds this never looks at would have the daemon
  // convert and queue every one of them for nothing.
  //
  // Three capabilities, and the user sees all three before enabling this:
  // "send messages", "add buttons and settings", "keep its own settings".
  declare("Auto-reply (AS)", 1 << kinds.MESSAGE, caps.SEND | caps.UI | caps.STORAGE);
  // After the declaration, because publishing a tree needs UI and a
  // capability is not held until it has been asked for.
  draw();
  return 0;
}

export function oxi_on_event(kind: i32, ev: i32): i32 {
  if (kind == kinds.MESSAGE) handleMessage(ev);
  else if (kind == kinds.UI_ACTION) handleAction(ev);
  return 0;
}

function handleMessage(ev: i32): void {
  if (!flag(ON)) return;

  // Our own messages, and messages the author has taken back. Answering
  // either is the classic way an autoreply embarrasses somebody: the first
  // makes it talk to itself, the second answers a message that is gone.
  if (fieldBool(ev, field.FROM_ME) || fieldBool(ev, field.REVOKED)) return;
  // Groups are left alone: a keyword that fires in a conversation of forty
  // people fires forty times.
  if (fieldBool(ev, field.IS_GROUP)) return;

  const text = fieldStr(ev, field.TEXT);
  if (text == null) return;
  const keyword = get(KEYWORD, DEFAULT_KEYWORD);
  if (!containsIgnoringCase(<string>text, keyword)) return;

  // `null` here is a value that did not fit rather than a shorter one: a JID
  // that was truncated is somebody else, and a truncated id is a message that
  // does not exist — the peer would see the answer quoting nothing.
  const chat = fieldStr(ev, field.CHAT_JID);
  const messageId = fieldStr(ev, field.MESSAGE_ID);
  if (chat == null || messageId == null) return;

  // As a reply rather than a fresh message: an automatic answer that does not
  // say what it is answering is indistinguishable from a person suddenly
  // speaking.
  sendReply(<string>chat, get(REPLY, DEFAULT_REPLY), <string>messageId);
}

function handleAction(ev: i32): void {
  const id = fieldStr(ev, field.ACTION_ID);
  const value = fieldStr(ev, field.ACTION_VALUE);
  if (id == null || value == null) {
    // A value that did not fit is not a shorter value: storing it would drop
    // the end of somebody's keyword and then match a word they never typed.
    log(level.WARN, "ignoring a setting longer than this plugin makes room for");
    return;
  }
  const key = <string>id;
  const text = <string>value;
  // A toggle's value is the state it is now in, not the one it was in: the
  // front end has already flipped it.
  if (key == ID_ON) setFlag(ON, text == "1");
  else if (key == ID_KEYWORD) set(KEYWORD, text);
  else if (key == ID_REPLY) set(REPLY, text);
  else return;
  // Redraw, because the tree carries the values: the toggle it published a
  // moment ago still says what it used to.
  draw();
}

function draw(): void {
  const enabled = flag(ON);
  new Tree()
    .section(slot.SETTINGS, "Auto-reply")
      .toggle(ID_ON, "Reply automatically", enabled, true)
      // Drawn inert while the plugin is off rather than hidden: a setting
      // that disappears reads as a setting that was lost.
      .field(ID_KEYWORD, "When a message contains", get(KEYWORD, DEFAULT_KEYWORD), enabled)
      .field(ID_REPLY, "Reply with", get(REPLY, DEFAULT_REPLY), enabled)
      .label("One-to-one conversations only.")
    .end()
    .publish();
}

/// Whether `haystack` contains `needle`, ignoring case for ASCII.
///
/// Ignoring case only where the answer is unambiguous: folding beyond ASCII
/// is a table this plugin will not carry, and a keyword that matched
/// differently depending on the language it was typed in would be worse than
/// one that is plainly case-sensitive there.
function containsIgnoringCase(haystack: string, needle: string): bool {
  if (needle.length == 0 || needle.length > haystack.length) return false;
  const last = haystack.length - needle.length;
  for (let i = 0; i <= last; i++) {
    let hit = true;
    for (let j = 0; j < needle.length; j++) {
      if (lowerAscii(haystack.charCodeAt(i + j)) != lowerAscii(needle.charCodeAt(j))) { hit = false; break; }
    }
    if (hit) return true;
  }
  return false;
}

@inline function lowerAscii(c: i32): i32 { return c >= 65 && c <= 90 ? c + 32 : c; }
