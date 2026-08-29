//! What a plugin hands to `oxi_ui_set`, and how the host reads it back.
//!
//! A plugin does not draw. It *declares* a small tree of named widgets, each
//! root attached to a [`slot`] the front end already has a place for, and the
//! front end decides what that looks like. Which is also why there is no
//! colour, no size and no position anywhere in this format: a literal colour
//! in a component is invisible to theme switching, and a plugin holding one
//! would be a component the theme cannot reach.
//!
//! # The encoding
//!
//! Fixed-width little-endian, pre-order, no varints:
//!
//! ```text
//! u8   FORMAT
//! u32  number of roots
//! node:
//!   u8   kind
//!   u8   slot          (roots only; 0 below them)
//!   u8   flags
//!   u8   reserved, 0
//!   u16  number of children
//!   u32  id length,    then that many bytes
//!   u32  label length, then that many bytes
//!   u32  value length, then that many bytes
//!   ...children, pre-order
//! ```
//!
//! It exists because this is the one payload that travels *from* the plugin,
//! and a plugin that had to serialize JSON would need a JSON encoder — which
//! is the dependency this whole ABI is arranged to avoid. [`Writer`] writes
//! it into a buffer the caller already owns and never allocates, so a plugin
//! with no allocator can still publish a settings panel.

/// The format byte, distinct from the ABI version.
///
/// A tree written by an older plugin stays readable when the ABI moves for an
/// unrelated reason, and a format change is caught here rather than by
/// misreading a length.
pub const FORMAT: u8 = 1;

/// How deep a tree may nest.
///
/// [`Writer`] needs a fixed stack to patch child counts without allocating,
/// and the host needs a bound so a malicious tree cannot recurse the parser
/// into the stack guard. One number serves both.
pub const MAX_DEPTH: usize = 8;

/// How many nodes one plugin's whole tree may hold.
///
/// A budget on what the daemon will carry in its state and hand to every
/// front end, not an opinion about interface design. A plugin that wants more
/// than this wants a list the front end pages through, which is a different
/// feature.
pub const MAX_NODES: usize = 256;

/// The longest id, label or value on any one node.
pub const MAX_TEXT: usize = 4096;

/// The largest encoded tree the host will read out of a plugin.
pub const MAX_BYTES: usize = 64 * 1024;

/// What a widget *is*.
///
/// Small on purpose. It is enough for an honest settings panel and a button
/// in a header, and short of what anyone would need to start rebuilding the
/// conversation view inside a plugin.
pub mod kind {
    /// Runs an action when pressed. Carries no value.
    pub const BUTTON: u8 = 1;
    /// On or off. Its value is `1` or `0`.
    pub const TOGGLE: u8 = 2;
    /// Text the user reads and cannot change.
    pub const LABEL: u8 = 3;
    /// Text the user types. Its value is the contents.
    pub const TEXT_FIELD: u8 = 4;
    /// Lays its children out along the line.
    pub const ROW: u8 = 5;
    /// Lays its children out down the page.
    pub const COLUMN: u8 = 6;
    /// A titled group. Its label is the title.
    pub const SECTION: u8 = 7;

    /// Whether this kind is drawn with whatever it holds inside it.
    ///
    /// The three containers, and nothing else: a front end renders children
    /// only for these, so children anywhere else are children nobody draws.
    #[must_use]
    pub fn holds_children(kind: u8) -> bool {
        matches!(kind, ROW | COLUMN | SECTION)
    }

    /// Whether a byte names a widget this format defines.
    #[must_use]
    pub const fn is_known(kind: u8) -> bool {
        matches!(
            kind,
            BUTTON | TOGGLE | LABEL | TEXT_FIELD | ROW | COLUMN | SECTION
        )
    }

    /// Whether this widget produces a `UI_ACTION` when used.
    ///
    /// The host refuses an interactive node with no id: an action nobody can
    /// name is one the plugin could never tell apart from another.
    #[must_use]
    pub const fn is_interactive(kind: u8) -> bool {
        matches!(kind, BUTTON | TOGGLE | TEXT_FIELD)
    }
}

/// Where a root node attaches.
///
/// Every one of these names a component that already exists in the GPUI front
/// end. A slot is a promise about *where*, never about how it is drawn, so a
/// front end that is not a window — a TUI, a notifier — reads the same tree
/// and renders it its own way or ignores it.
pub mod slot {
    /// Not attached anywhere: what a node below a root carries.
    pub const NONE: u8 = 0;
    /// Beside the conversation's name. The action carries the open chat.
    pub const CHAT_HEADER: u8 = 1;
    /// A section of the Settings screen, which is where a plugin's own
    /// configuration belongs.
    pub const SETTINGS: u8 = 3;

    // 2 is reserved for the composer, which is not drawn yet. It is *not*
    // defined here for that reason: a slot the front end silently ignores is
    // a button whose author never finds out why it did not appear, which is
    // the same failure `ParseError::Misplaced` exists to refuse. A slot
    // arrives when something draws it, and arriving is a new constant — an
    // addition, not a change.

    /// Whether a byte names a slot this format defines.
    #[must_use]
    pub const fn is_known(slot: u8) -> bool {
        matches!(slot, CHAT_HEADER | SETTINGS)
    }
}

/// Bits in a node's `flags` byte.
pub mod flags {
    /// The widget accepts interaction. Absent means it is drawn greyed.
    ///
    /// Set rather than cleared for "usable" so a zero flags byte is a
    /// *disabled* widget: a plugin that forgets to say draws something inert,
    /// not something that acts.
    pub const ENABLED: u8 = 1 << 0;
    /// A toggle that is on.
    pub const CHECKED: u8 = 1 << 1;
}

/// The buffer ran out, or the tree broke a limit.
///
/// One error rather than several: every case ends the same way — the host
/// never sees the tree — and a plugin that overflowed its buffer has a bug
/// rather than a decision to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overflow;

impl core::fmt::Display for Overflow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the ui tree did not fit, or broke a limit")
    }
}

/// Builds the encoding into a buffer the caller owns.
///
/// Never allocates, which is the point: a plugin compiled for
/// `wasm32-unknown-unknown` with no allocator can still publish an interface.
/// The child counts are patched in place when a node is closed, so a caller
/// declares nodes in the order it thinks of them rather than counting ahead.
///
/// Errors are latched rather than returned per call. A caller that had to
/// check every `leaf` would write six `?`s to draw a row of buttons, and the
/// only thing it could do with an early one is stop — which is what
/// [`finish`](Self::finish) does for it.
pub struct Writer<'a> {
    buf: &'a mut [u8],
    at: usize,
    roots: u32,
    nodes: usize,
    /// Where each open node's child-count field sits, so closing one can
    /// patch it. Doubles as the depth.
    open: [usize; MAX_DEPTH],
    depth: usize,
    failed: bool,
}

impl<'a> Writer<'a> {
    /// Start a tree. The buffer must hold at least the five-byte header.
    #[must_use]
    pub fn new(buf: &'a mut [u8]) -> Self {
        let mut w = Self {
            buf,
            at: 0,
            roots: 0,
            nodes: 0,
            open: [0; MAX_DEPTH],
            depth: 0,
            failed: false,
        };
        w.put(&[FORMAT]);
        w.put(&0u32.to_le_bytes());
        w
    }

    /// Open a node. Every `begin` needs its [`end`](Self::end).
    ///
    /// `slot` is read only at the root; a node opened inside another belongs
    /// to its parent and the host rejects a tree that says otherwise, rather
    /// than quietly ignoring it — a slot that was silently dropped is a
    /// button the author never sees appear.
    pub fn begin(&mut self, kind: u8, slot: u8, flags: u8, id: &str, label: &str, value: &str) {
        if self.depth >= MAX_DEPTH || self.nodes >= MAX_NODES {
            self.failed = true;
            return;
        }
        if self.depth == 0 {
            self.roots = self.roots.saturating_add(1);
        }
        self.nodes += 1;

        self.put(&[
            kind,
            if self.depth == 0 { slot } else { slot::NONE },
            flags,
            0,
        ]);
        let children_at = self.at;
        self.put(&0u16.to_le_bytes());
        self.put_str(id);
        self.put_str(label);
        self.put_str(value);

        self.open[self.depth] = children_at;
        self.depth += 1;
    }

    /// Close the innermost open node, recording how many children it got.
    pub fn end(&mut self) {
        let Some(depth) = self.depth.checked_sub(1) else {
            self.failed = true;
            return;
        };
        self.depth = depth;
        if self.depth > 0 {
            // One more child for whatever encloses this one. Counted on
            // close rather than on open so a node's count is final by the
            // time anything reads it.
            let parent = self.open[self.depth - 1];
            self.bump_child_count(parent);
        }
    }

    /// A node with no children: `begin` and `end` in one call.
    pub fn leaf(&mut self, kind: u8, slot: u8, flags: u8, id: &str, label: &str, value: &str) {
        self.begin(kind, slot, flags, id, label, value);
        self.end();
    }

    /// Finish and report how many bytes the tree occupies.
    ///
    /// Fails when anything overflowed along the way, when a node was left
    /// open, or when the tree is larger than [`MAX_BYTES`]. Checking here and
    /// only here is what lets the calls above be plain statements.
    pub fn finish(self) -> Result<usize, Overflow> {
        if self.failed || self.depth != 0 || self.at > MAX_BYTES {
            return Err(Overflow);
        }
        let roots = self.roots.to_le_bytes();
        self.buf
            .get_mut(1..5)
            .ok_or(Overflow)?
            .copy_from_slice(&roots);
        Ok(self.at)
    }

    fn bump_child_count(&mut self, at: usize) {
        let Some(slice) = self.buf.get_mut(at..at + 2) else {
            self.failed = true;
            return;
        };
        let current = u16::from_le_bytes([slice[0], slice[1]]);
        let Some(next) = current.checked_add(1) else {
            self.failed = true;
            return;
        };
        slice.copy_from_slice(&next.to_le_bytes());
    }

    fn put_str(&mut self, s: &str) {
        if s.len() > MAX_TEXT {
            self.failed = true;
            return;
        }
        // `as u32` is checked by the bound above; MAX_TEXT is far under u32.
        self.put(&(s.len() as u32).to_le_bytes());
        self.put(s.as_bytes());
    }

    fn put(&mut self, bytes: &[u8]) {
        let Some(slice) = self.buf.get_mut(self.at..self.at + bytes.len()) else {
            self.failed = true;
            return;
        };
        slice.copy_from_slice(bytes);
        self.at += bytes.len();
    }
}

#[cfg(feature = "std")]
pub use parse::{Node, ParseError, parse};

#[cfg(feature = "std")]
mod parse {
    use super::{FORMAT, MAX_DEPTH, MAX_NODES, MAX_TEXT, flags, kind, slot};

    /// One widget, as the host reads it back.
    ///
    /// Owned strings rather than borrows into the plugin's memory: this
    /// outlives the call it was read in, travels into daemon state and is
    /// serialized to every front end.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Node {
        pub kind: u8,
        pub slot: u8,
        pub flags: u8,
        pub id: String,
        pub label: String,
        pub value: String,
        pub children: Vec<Node>,
    }

    /// Why a tree was refused.
    ///
    /// Distinguished because these reach the author as a log line, and
    /// "your tree is too deep" is actionable where "bad ui payload" is not.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ParseError {
        /// The first byte was not [`FORMAT`].
        Format(u8),
        /// A length or a count ran off the end of the payload.
        Truncated,
        /// Trailing bytes after the last root. A tree that decodes and then
        /// has more behind it is not a tree this writer produced.
        Trailing,
        /// A widget or slot byte this ABI does not define.
        Unknown { kind: u8, slot: u8 },
        /// A slot on a node that is not a root, or none on one that is.
        Misplaced,
        /// A button, toggle or field with no id, which nothing could ever
        /// name in an action.
        Anonymous,
        /// Past [`MAX_DEPTH`], [`MAX_NODES`] or [`MAX_TEXT`].
        TooBig,
        /// Children hung off a widget that is drawn without any.
        Childless(u8),
        /// An action id that was not valid UTF-8.
        ///
        /// A label is decoded lossily, and an id may not be: an id is
        /// compared, not read. A replacement character put in a broken byte's
        /// place gives the front end an id the plugin will never recognise
        /// coming back, so its own button answers nothing — where refusing
        /// the tree says what is wrong while the plugin's author can still
        /// fix it.
        MangledId,
        /// The padding byte was not zero.
        ///
        /// Refused rather than ignored, so the byte stays free to mean
        /// something later: a reader that skipped it would let payloads
        /// carrying a value circulate, and the day it acquires a meaning
        /// those become trees whose author never agreed to it.
        Reserved(u8),
    }

    impl std::fmt::Display for ParseError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Format(v) => write!(f, "ui format {v}, expected {FORMAT}"),
                Self::Truncated => f.write_str("the ui tree ends mid-node"),
                Self::Trailing => f.write_str("bytes after the last root node"),
                Self::Reserved(v) => write!(f, "reserved byte {v}, expected 0"),
                Self::MangledId => f.write_str("an action id that is not valid utf-8"),
                Self::Childless(kind) => {
                    write!(f, "widget kind {kind} is drawn without children")
                }
                Self::Unknown { kind, slot } => {
                    write!(f, "unknown widget {kind} or slot {slot}")
                }
                Self::Misplaced => f.write_str("a slot on a node that is not a root"),
                Self::Anonymous => f.write_str("an interactive widget with no id"),
                Self::TooBig => f.write_str("the ui tree is past a limit"),
            }
        }
    }

    impl std::error::Error for ParseError {}

    /// Read a tree a plugin wrote.
    ///
    /// Every limit is enforced here rather than trusted, because every number
    /// in this payload is one the guest chose: a child count is a loop bound,
    /// a string length is an allocation, and a depth is stack. Refusing the
    /// whole tree on the first thing that does not add up is the only answer
    /// that cannot half-apply — a plugin's interface is all of it or none.
    pub fn parse(bytes: &[u8]) -> Result<Vec<Node>, ParseError> {
        // The host already refuses a longer read, but this is a public
        // function and `MAX_BYTES` is this format's limit rather than that
        // caller's: a 256-node tree of long strings fits the per-node rules
        // and still exceeds it.
        if bytes.len() > super::MAX_BYTES {
            return Err(ParseError::TooBig);
        }
        let mut r = Reader {
            bytes,
            at: 0,
            nodes: 0,
        };
        let format = r.u8()?;
        if format != FORMAT {
            return Err(ParseError::Format(format));
        }
        let roots = r.u32()?;
        // Bounded before it is used as a loop count, and against the node
        // budget rather than the byte length: a root is at least 16 bytes,
        // but saying so here would be one more thing to keep in step.
        if roots as usize > MAX_NODES {
            return Err(ParseError::TooBig);
        }
        let mut out = Vec::new();
        for _ in 0..roots {
            out.push(r.node(0)?);
        }
        if r.at != bytes.len() {
            return Err(ParseError::Trailing);
        }
        Ok(out)
    }

    struct Reader<'a> {
        bytes: &'a [u8],
        at: usize,
        nodes: usize,
    }

    impl Reader<'_> {
        fn node(&mut self, depth: usize) -> Result<Node, ParseError> {
            if depth >= MAX_DEPTH {
                return Err(ParseError::TooBig);
            }
            self.nodes += 1;
            if self.nodes > MAX_NODES {
                return Err(ParseError::TooBig);
            }

            let kind = self.u8()?;
            let slot = self.u8()?;
            let flags = self.u8()?;
            let reserved = self.u8()?;
            if reserved != 0 {
                return Err(ParseError::Reserved(reserved));
            }
            let children = self.u16()?;
            // A leaf is a leaf. Only a row, a column and a section are drawn
            // with anything inside them, so children hung off a button or a
            // field are children a front end silently never renders — and a
            // plugin whose control disappeared with an `ACCEPTED` in hand has
            // nothing to go on. The same reason a slot nobody draws is
            // refused rather than dropped.
            if children != 0 && !kind::holds_children(kind) {
                return Err(ParseError::Childless(kind));
            }
            let id = self.ident()?;
            let label = self.string()?;
            let value = self.string()?;

            if !kind::is_known(kind) || (slot != slot::NONE && !slot::is_known(slot)) {
                return Err(ParseError::Unknown { kind, slot });
            }
            // A root must land somewhere and a child must not claim to: a
            // slot that was silently ignored is a button whose author never
            // finds out why it did not appear.
            if (depth == 0) != (slot != slot::NONE) {
                return Err(ParseError::Misplaced);
            }
            if kind::is_interactive(kind) && id.is_empty() {
                return Err(ParseError::Anonymous);
            }
            // Reserved bits are refused rather than masked off. A plugin
            // setting one is built against an ABI this host does not have,
            // and drawing it anyway would draw something other than what it
            // asked for.
            if flags & !(flags::ENABLED | flags::CHECKED) != 0 {
                return Err(ParseError::Unknown { kind, slot });
            }

            let mut kids = Vec::new();
            for _ in 0..children {
                kids.push(self.node(depth + 1)?);
            }
            Ok(Node {
                kind,
                slot,
                flags,
                id,
                label,
                value,
                children: kids,
            })
        }

        fn string(&mut self) -> Result<String, ParseError> {
            let raw = self.raw()?;
            // Lossy rather than an error: a label with a broken code point is
            // a bug in the plugin's own string handling, and refusing its
            // whole interface over one is a worse answer than drawing a
            // replacement character where the mistake is.
            Ok(String::from_utf8_lossy(raw).into_owned())
        }

        /// An action id, which is decoded strictly.
        ///
        /// The opposite trade from a label, because an id is never read by
        /// anybody: it goes out with the tree and comes back on the press,
        /// and the plugin recognises it by comparison. Substituting a
        /// replacement character for a broken byte hands the front end an id
        /// that is *almost* the one declared — pressed, matched against
        /// nothing, and silently doing nothing at all — where a label with
        /// the same mistake is still a label somebody can read around.
        fn ident(&mut self) -> Result<String, ParseError> {
            let raw = self.raw()?;
            String::from_utf8(raw.to_vec()).map_err(|_| ParseError::MangledId)
        }

        /// The bytes of one length-prefixed string, unvalidated.
        fn raw(&mut self) -> Result<&[u8], ParseError> {
            let len = self.u32()? as usize;
            if len > MAX_TEXT {
                return Err(ParseError::TooBig);
            }
            let end = self.at.checked_add(len).ok_or(ParseError::Truncated)?;
            let raw = self.bytes.get(self.at..end).ok_or(ParseError::Truncated)?;
            self.at = end;
            Ok(raw)
        }

        fn u8(&mut self) -> Result<u8, ParseError> {
            let b = *self.bytes.get(self.at).ok_or(ParseError::Truncated)?;
            self.at += 1;
            Ok(b)
        }

        fn u16(&mut self) -> Result<u16, ParseError> {
            let end = self.at + 2;
            let raw = self.bytes.get(self.at..end).ok_or(ParseError::Truncated)?;
            self.at = end;
            Ok(u16::from_le_bytes([raw[0], raw[1]]))
        }

        fn u32(&mut self) -> Result<u32, ParseError> {
            let end = self.at + 4;
            let raw = self.bytes.get(self.at..end).ok_or(ParseError::Truncated)?;
            self.at = end;
            Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    fn write(f: impl FnOnce(&mut Writer<'_>)) -> Result<Vec<u8>, Overflow> {
        let mut buf = vec![0u8; MAX_BYTES];
        let mut w = Writer::new(&mut buf);
        f(&mut w);
        let n = w.finish()?;
        buf.truncate(n);
        Ok(buf)
    }

    #[test]
    fn a_tree_survives_a_round_trip() {
        let bytes = write(|w| {
            w.leaf(
                kind::BUTTON,
                slot::CHAT_HEADER,
                flags::ENABLED,
                "translate",
                "Traduzir",
                "",
            );
            w.begin(
                kind::SECTION,
                slot::SETTINGS,
                flags::ENABLED,
                "cfg",
                "Resposta automática",
                "",
            );
            w.leaf(
                kind::TOGGLE,
                slot::NONE,
                flags::ENABLED | flags::CHECKED,
                "on",
                "Ligada",
                "1",
            );
            w.leaf(kind::LABEL, slot::NONE, 0, "", "Responde a 'ping'", "");
            w.end();
        })
        .expect("fits");

        let tree = parse(&bytes).expect("parses");
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].id, "translate");
        assert_eq!(tree[0].slot, slot::CHAT_HEADER);
        assert_eq!(tree[1].children.len(), 2);
        assert_eq!(tree[1].children[0].value, "1");
        assert_eq!(tree[1].children[1].kind, kind::LABEL);
    }

    /// The writer's whole error story: nothing is reported until `finish`, so
    /// a caller that ignored an intermediate failure still cannot ship one.
    #[test]
    fn an_unclosed_node_is_refused_at_finish() {
        let mut buf = [0u8; 512];
        let mut w = Writer::new(&mut buf);
        w.begin(kind::SECTION, slot::SETTINGS, flags::ENABLED, "a", "A", "");
        assert_eq!(w.finish(), Err(Overflow));
    }

    #[test]
    fn a_buffer_too_small_fails_rather_than_truncating() {
        let mut buf = [0u8; 8];
        let mut w = Writer::new(&mut buf);
        w.leaf(
            kind::BUTTON,
            slot::CHAT_HEADER,
            flags::ENABLED,
            "x",
            "X",
            "",
        );
        assert_eq!(w.finish(), Err(Overflow));
    }

    #[test]
    fn a_slot_below_a_root_is_refused() {
        let bytes = write(|w| {
            w.begin(kind::COLUMN, slot::SETTINGS, flags::ENABLED, "c", "C", "");
            // A child claiming a slot of its own: nothing could attach it in
            // two places, and ignoring it would hide the mistake.
            w.leaf(
                kind::BUTTON,
                slot::CHAT_HEADER,
                flags::ENABLED,
                "b",
                "B",
                "",
            );
            w.end();
        })
        .expect("fits");
        // The writer zeroes a non-root's slot, so this round-trips; the
        // parser's own guard is what a hand-built payload meets.
        let tree = parse(&bytes).expect("parses");
        assert_eq!(tree[0].children[0].slot, slot::NONE);
    }

    #[test]
    fn a_hand_built_payload_cannot_smuggle_a_slot_downward() {
        // Assembled by hand rather than written and patched: the guard being
        // tested exists for payloads the writer did not produce, so the test
        // has to be one of those.
        let mut bytes = vec![FORMAT];
        bytes.extend_from_slice(&1u32.to_le_bytes()); // one root
        // A column in Settings, holding one child.
        bytes.extend_from_slice(&[kind::COLUMN, slot::SETTINGS, flags::ENABLED, 0]);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        for text in ["c", "C", ""] {
            bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        // A child claiming a slot of its own, which nothing could attach in
        // two places.
        bytes.extend_from_slice(&[kind::BUTTON, slot::CHAT_HEADER, flags::ENABLED, 0]);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        for text in ["b", "B", ""] {
            bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        assert_eq!(parse(&bytes), Err(ParseError::Misplaced));
    }

    /// And the mirror: a root that names no slot has nowhere to go.
    #[test]
    fn a_root_with_no_slot_is_refused() {
        let mut bytes = vec![FORMAT];
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&[kind::LABEL, slot::NONE, 0, 0]);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        for _ in 0..3 {
            bytes.extend_from_slice(&0u32.to_le_bytes());
        }
        assert_eq!(parse(&bytes), Err(ParseError::Misplaced));
    }

    /// The padding byte is refused rather than skipped, so it stays free to
    /// mean something later: a reader that ignored it would let payloads
    /// carrying a value circulate, and the day it acquires a meaning those
    /// become trees whose author never agreed to it.
    /// `MAX_BYTES` is the format's limit, not one caller's: a tree that
    /// satisfies every per-node rule can still be longer than this format
    /// says a tree may be.
    #[test]
    fn a_payload_past_the_format_limit_is_refused() {
        let mut bytes = write(|w| {
            w.leaf(kind::LABEL, slot::SETTINGS, flags::ENABLED, "", "x", "");
        })
        .expect("fits");
        assert!(parse(&bytes).is_ok(), "and it parses at its own size");

        bytes.resize(MAX_BYTES + 1, 0);
        assert_eq!(parse(&bytes), Err(ParseError::TooBig));
    }

    #[test]
    fn a_non_zero_reserved_byte_is_refused() {
        let mut bytes = vec![FORMAT];
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&[kind::LABEL, slot::SETTINGS, flags::ENABLED, 0x7f]);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        for _ in 0..3 {
            bytes.extend_from_slice(&0u32.to_le_bytes());
        }
        assert_eq!(parse(&bytes), Err(ParseError::Reserved(0x7f)));

        // And the same payload with a zero there is a tree, so the test is
        // about the byte rather than about anything else being wrong.
        bytes[8] = 0;
        assert!(parse(&bytes).is_ok());
    }

    /// A broken byte in an id is refused and the same byte in a label is
    /// drawn around. The two are decoded differently on purpose: a label is
    /// read by a person, an id is compared by the plugin that wrote it.
    #[test]
    fn a_mangled_id_is_refused_and_a_mangled_label_is_not() {
        // One button, with each of the three strings given explicitly so a
        // byte can be invalid where a `&str` could not carry one.
        let tree = |id: &[u8], label: &[u8]| {
            let mut bytes = vec![FORMAT];
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&[kind::BUTTON, slot::CHAT_HEADER, flags::ENABLED, 0]);
            bytes.extend_from_slice(&0u16.to_le_bytes());
            for text in [id, label, b""] {
                bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
                bytes.extend_from_slice(text);
            }
            parse(&bytes)
        };

        // 0xff is not a byte any UTF-8 sequence contains.
        assert_eq!(
            tree(b"tr\xffanslate", b"Traduzir"),
            Err(ParseError::MangledId)
        );

        let drawn = tree(b"translate", b"Trad\xffuzir").expect("a label is decoded lossily");
        assert_eq!(drawn[0].id, "translate");
        assert_eq!(drawn[0].label, "Trad\u{fffd}uzir");
    }

    /// A slot number this build does not draw is refused, not dropped. A
    /// button that silently never appears is the failure the whole
    /// slot-checking exists for.
    #[test]
    fn a_slot_this_build_does_not_draw_is_refused() {
        let mut bytes = vec![FORMAT];
        bytes.extend_from_slice(&1u32.to_le_bytes());
        // 2 is reserved for the composer, which nothing renders yet.
        bytes.extend_from_slice(&[kind::BUTTON, 2, flags::ENABLED, 0]);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        for text in ["b", "B", ""] {
            bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        assert_eq!(
            parse(&bytes),
            Err(ParseError::Unknown {
                kind: kind::BUTTON,
                slot: 2
            })
        );
    }

    #[test]
    fn an_interactive_widget_needs_an_id() {
        let bytes = write(|w| {
            w.leaf(kind::BUTTON, slot::CHAT_HEADER, flags::ENABLED, "", "B", "");
        })
        .expect("fits");
        assert_eq!(parse(&bytes), Err(ParseError::Anonymous));
    }

    #[test]
    fn a_label_needs_no_id() {
        let bytes = write(|w| {
            w.leaf(kind::LABEL, slot::SETTINGS, 0, "", "just words", "");
        })
        .expect("fits");
        assert_eq!(parse(&bytes).expect("parses").len(), 1);
    }

    #[test]
    fn a_child_count_that_lies_is_truncation_not_a_panic() {
        // A section, because a leaf claiming a child is now refused for
        // *being* a leaf — which would answer this test with the wrong error
        // and stop it saying anything about truncation.
        let mut bytes = write(|w| {
            w.begin(kind::SECTION, slot::SETTINGS, 0, "", "t", "");
            w.end();
        })
        .expect("fits");
        // Claim a child that is not there: the node head is at offset 5,
        // and its child count is the two bytes after the four-byte head.
        bytes[5 + 4] = 1;
        assert_eq!(parse(&bytes), Err(ParseError::Truncated));
    }

    /// Only the three containers are drawn with anything inside them, so a
    /// child hung off a button is a control a front end silently never
    /// renders — with an `ACCEPTED` handed back to whoever wrote it.
    #[test]
    fn children_on_a_leaf_are_refused() {
        let mut bytes = write(|w| {
            w.begin(kind::SECTION, slot::SETTINGS, flags::ENABLED, "", "t", "");
            w.leaf(kind::BUTTON, slot::NONE, flags::ENABLED, "b", "B", "");
            w.end();
        })
        .expect("fits");
        // The button is the second node; give it a child it does not have.
        // Its head follows the header, the section's head and child count,
        // and the section's three strings — an empty id, a one-byte title
        // and an empty value, each behind a four-byte length.
        let header = 5;
        let section_head = 4 + 2;
        // Three strings, each a four-byte length and its bytes: an empty
        // id, a one-byte title, an empty value.
        let section_strings = 4 + 4 + 1 + 4;
        let button = header + section_head + section_strings;
        assert_eq!(bytes[button], kind::BUTTON, "found the button's head");
        bytes[button + 4] = 1;
        assert_eq!(parse(&bytes), Err(ParseError::Childless(kind::BUTTON)));
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = write(|w| {
            w.leaf(kind::LABEL, slot::SETTINGS, 0, "", "x", "");
        })
        .expect("fits");
        bytes.push(0);
        assert_eq!(parse(&bytes), Err(ParseError::Trailing));
    }

    #[test]
    fn an_empty_tree_is_a_plugin_withdrawing_its_ui() {
        let bytes = write(|_| {}).expect("fits");
        assert_eq!(parse(&bytes), Ok(Vec::new()));
    }

    /// The parser's bounds are checked against a payload nobody wrote with
    /// the writer: a root count that would allocate before anything is read.
    #[test]
    fn an_enormous_root_count_is_refused_before_it_is_looped_on() {
        let mut bytes = vec![FORMAT];
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse(&bytes), Err(ParseError::TooBig));
    }

    #[test]
    fn a_wrong_format_byte_is_named() {
        assert_eq!(parse(&[9, 0, 0, 0, 0]), Err(ParseError::Format(9)));
    }

    #[test]
    fn nesting_past_the_limit_is_refused() {
        let deep = write(|w| {
            for i in 0..MAX_DEPTH {
                let s = if i == 0 { slot::SETTINGS } else { slot::NONE };
                w.begin(kind::COLUMN, s, flags::ENABLED, "", "", "");
            }
            for _ in 0..MAX_DEPTH {
                w.end();
            }
        })
        .expect("exactly at the limit fits");
        assert!(parse(&deep).is_ok());

        let too_deep = write(|w| {
            for i in 0..=MAX_DEPTH {
                let s = if i == 0 { slot::SETTINGS } else { slot::NONE };
                w.begin(kind::COLUMN, s, flags::ENABLED, "", "", "");
            }
            for _ in 0..=MAX_DEPTH {
                w.end();
            }
        });
        assert_eq!(too_deep, Err(Overflow));
    }
}
