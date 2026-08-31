//! A source map for the module, built out of the DWARF the module carries.
//!
//! What a browser needs to name a Rust file and a line in a wasm stack trace
//! is one of two things, and only one of them works in an unmodified browser.
//! DWARF is read by an extension (Chrome's "C/C++ DevTools Support"); a source
//! map is read by DevTools itself, in every engine, with nothing installed.
//! Both start from the same bytes — a source map for wasm *is* a projection of
//! `.debug_line` — so this reads the sections the compiler already emitted and
//! writes the projection beside the module.
//!
//! The mapping is one generated line and one generated column per row, and the
//! column is a byte offset into the wasm file. That is the whole of the wasm
//! source-map convention: the module is treated as a single line of text whose
//! columns are its bytes. DWARF's own addresses are offsets from the start of
//! the *code section's payload* rather than from the start of the file, which
//! is the one adjustment here that is not spelled out in a specification —
//! measured against a module built for this purpose, where the single
//! function's first instruction sits at file offset 110 and DWARF calls it 3,
//! against a code payload beginning at 107.
//!
//! No dependencies, like everything else in this directory. What that costs is
//! a line-number program interpreter and a form-skipping walk over the first
//! DIE of each compilation unit, which are together about the size of the JSON
//! scanner next door and are here for the same reason: pulling `gimli` and
//! `serde` in would cost this directory the property that it builds on its own
//! in a job that checked out one directory.
//!
//! Deliberately not general. It understands version 4 line tables, which is
//! what every Rust toolchain this repository has been built with emits for
//! `wasm32-unknown-unknown`, and it refuses anything else by name rather than
//! guessing — a source map that is confidently wrong about which line a frame
//! is on is worse than no source map, because nothing about it looks broken.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::err;
use crate::util::Result;

/// What the map turned out to be, for the line the build prints.
#[derive(Debug)]
pub struct Summary {
    pub map: PathBuf,
    pub sources: usize,
    pub embedded: usize,
    pub rows: usize,
    pub bytes: u64,
}

/// Write `<wasm>.map` beside the module and point the module at it.
///
/// The pointer is a custom section named `sourceMappingURL`, appended at the
/// end of the file — which is what makes appending safe at all: every offset
/// the map has just been written against is an offset into what is now a
/// prefix of the same file.
///
/// `root` is the repository, and it decides one thing: whose source text is
/// embedded in the map. A file under it is ours and is embedded, so the map
/// works with nothing serving the tree; a file that is not — the standard
/// library, a registry dependency — is named and not embedded, because
/// embedding every source `build-std` compiles would be most of a gigabyte of
/// JSON to answer a question a file name and a line number have already
/// answered.
pub fn write(wasm: &Path, root: &Path) -> Result<Summary> {
    let bytes = fs::read(wasm).map_err(|e| err!("could not read {}: {e}", wasm.display()))?;
    let module = Module::parse(&bytes)?;

    if module.custom("sourceMappingURL").is_some() {
        return Err(err!(
            "{} already carries a sourceMappingURL section; this build has \
             been mapped once already",
            wasm.display()
        ));
    }

    let line = module.custom(".debug_line").ok_or_else(|| {
        err!(
            "{} carries no .debug_line: it was built without debug information, \
             or something stripped it. `WEB_PROFILE=dwarf cargo xtask web build` \
             is the build that keeps it.",
            wasm.display()
        )
    })?;

    let dirs = module.comp_dirs();
    let mut rows = line_rows(line, &dirs)?;
    // The code section's payload is where DWARF counts from; a source map's
    // columns are counted from the start of the file.
    for row in &mut rows {
        row.offset += module.code_payload as u64;
    }

    let map_path = map_beside(wasm);
    let map_name = map_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| err!("{} has no usable file name", map_path.display()))?
        .to_string();
    let module_name = wasm
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();

    let map = render(&rows, &module_name, root);
    fs::write(&map_path, &map.text)
        .map_err(|e| err!("could not write {}: {e}", map_path.display()))?;

    let mut out = bytes;
    append_custom(&mut out, "sourceMappingURL", &encode_name(&map_name));
    fs::write(wasm, &out).map_err(|e| err!("could not write {}: {e}", wasm.display()))?;

    Ok(Summary {
        map: map_path,
        sources: map.sources,
        embedded: map.embedded,
        rows: map.rows,
        bytes: map.text.len() as u64,
    })
}

/// `foo_bg.wasm` -> `foo_bg.wasm.map`, which is the name the section carries
/// and therefore a URL resolved against the module's own — so the two travel
/// together whatever directory the bundle is unpacked into.
fn map_beside(wasm: &Path) -> PathBuf {
    let mut name = wasm.as_os_str().to_os_string();
    name.push(".map");
    PathBuf::from(name)
}

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

struct Module<'a> {
    bytes: &'a [u8],
    /// Where the code section's payload begins, which is what DWARF addresses
    /// are relative to.
    code_payload: usize,
    customs: Vec<(&'a str, usize, usize)>,
}

impl<'a> Module<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < 8 || &bytes[..4] != b"\0asm" {
            return Err(err!("not a wasm module"));
        }
        let mut r = Cursor::new(bytes, 8);
        let mut customs = Vec::new();
        let mut code_payload = None;
        while !r.done() {
            let id = r.u8()?;
            let size = r.uleb()? as usize;
            let start = r.pos();
            let end = start
                .checked_add(size)
                .filter(|e| *e <= bytes.len())
                .ok_or_else(|| err!("a section runs past the end of the module"))?;
            match id {
                0 => {
                    let mut inner = Cursor::new(bytes, start);
                    let n = inner.uleb()? as usize;
                    let at = inner.pos();
                    let name = std::str::from_utf8(bytes.get(at..at + n).ok_or_else(|| {
                        err!("a custom section's name runs past the end of the module")
                    })?)
                    .map_err(|_| err!("a custom section's name is not utf-8"))?;
                    customs.push((name, at + n, end));
                }
                10 => code_payload = Some(start),
                _ => {}
            }
            r.seek(end);
        }
        Ok(Module {
            bytes,
            code_payload: code_payload
                .ok_or_else(|| err!("the module has no code section to map"))?,
            customs,
        })
    }

    fn custom(&self, name: &str) -> Option<&'a [u8]> {
        self.customs
            .iter()
            .find(|(n, ..)| *n == name)
            .map(|(_, start, end)| &self.bytes[*start..*end])
    }

    /// The compilation directory of each unit, keyed by the `.debug_line`
    /// offset that unit's `DW_AT_stmt_list` names.
    ///
    /// Best effort, on purpose. A file table's paths are relative to a
    /// directory that lives in `.debug_info` rather than beside them, so
    /// without this a map names `src/main.rs` where it could name the file —
    /// but a map that names `src/main.rs` is still a map, and a `.debug_info`
    /// this walk does not understand is not a reason to fail a build. Every
    /// failure in here is answered by having no answer for that unit.
    fn comp_dirs(&self) -> HashMap<u64, String> {
        let (Some(info), Some(abbrev)) = (self.custom(".debug_info"), self.custom(".debug_abbrev"))
        else {
            return HashMap::new();
        };
        let strs = self.custom(".debug_str").unwrap_or_default();
        match comp_dirs(info, abbrev, strs) {
            Ok(d) => d,
            Err(e) => {
                if std::env::var("XTASK_DEBUG").is_ok() {
                    crate::note!("comp_dirs: {e}");
                }
                HashMap::new()
            }
        }
    }
}

/// A custom section appended whole: the id, the size, the section's name, and
/// the payload.
fn append_custom(out: &mut Vec<u8>, name: &str, payload: &[u8]) {
    let mut body = encode_name(name);
    body.extend_from_slice(payload);
    out.push(0);
    write_uleb(out, body.len() as u64);
    out.extend_from_slice(&body);
}

/// A wasm `name`: its length, then its bytes.
fn encode_name(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 1);
    write_uleb(&mut v, s.len() as u64);
    v.extend_from_slice(s.as_bytes());
    v
}

fn write_uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A position in a byte slice, with the reads DWARF and wasm are both made of.
///
/// Every read is checked and answers an error rather than panicking: this
/// walks bytes a compiler wrote, and a wrong turn in a line program is an
/// ordinary bug in this file rather than something worth aborting a build
/// process over with an index panic.
struct Cursor<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn new(d: &'a [u8], p: usize) -> Self {
        Cursor { d, p }
    }

    fn pos(&self) -> usize {
        self.p
    }

    fn seek(&mut self, p: usize) {
        self.p = p;
    }

    fn done(&self) -> bool {
        self.p >= self.d.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .p
            .checked_add(n)
            .filter(|e| *e <= self.d.len())
            .ok_or_else(|| err!("read past the end of a section"))?;
        let out = &self.d[self.p..end];
        self.p = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn uleb(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            if shift < 64 {
                value |= ((byte & 0x7f) as u64) << shift;
            }
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            if shift > 70 {
                return Err(err!("a LEB128 value runs on forever"));
            }
        }
    }

    fn sleb(&mut self) -> Result<i64> {
        let mut value = 0i64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            if shift < 64 {
                value |= ((byte & 0x7f) as i64) << shift;
            }
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Ok(value);
            }
            if shift > 70 {
                return Err(err!("a LEB128 value runs on forever"));
            }
        }
    }

    /// A NUL-terminated string, answered as it stands — a path a compiler
    /// wrote is not necessarily UTF-8 and is not worth failing a build over.
    fn cstr(&mut self) -> Result<String> {
        let start = self.p;
        while self.p < self.d.len() && self.d[self.p] != 0 {
            self.p += 1;
        }
        if self.p >= self.d.len() {
            return Err(err!("an unterminated string runs to the end of a section"));
        }
        let s = String::from_utf8_lossy(&self.d[start..self.p]).into_owned();
        self.p += 1;
        Ok(s)
    }
}

// ---------------------------------------------------------------------------
// .debug_line
// ---------------------------------------------------------------------------

/// One mapped byte: where in the module, and where in the source.
#[derive(Debug)]
struct Row {
    offset: u64,
    file: String,
    line: u32,
    column: u32,
}

/// LLVM marks the line rows of code the linker dropped by setting their
/// address to all-ones rather than by removing them. Mapping those would put
/// every discarded function's source lines at the very end of the module,
/// over whatever really is there.
const TOMBSTONE: u64 = u64::MAX;
const TOMBSTONE32: u64 = u32::MAX as u64;

fn line_rows(section: &[u8], dirs: &HashMap<u64, String>) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    let mut r = Cursor::new(section, 0);
    while !r.done() {
        let unit_start = r.pos() as u64;
        let length = r.u32()?;
        if length == 0xffff_ffff {
            return Err(err!(
                "this module carries 64-bit DWARF, which this generator does \
                 not read"
            ));
        }
        let end = r.pos() + length as usize;
        let version = r.u16()?;
        if version != 4 {
            return Err(err!(
                "this module carries version {version} line tables and this \
                 generator reads version 4. Rust has emitted 4 for \
                 wasm32-unknown-unknown for as long as this build has existed, \
                 so a bump is news: `xtask/src/sourcemap.rs` is what has to \
                 learn the new header."
            ));
        }
        let header_length = r.u32()?;
        let program = r.pos() + header_length as usize;

        let min_inst = r.u8()? as u64;
        let _max_ops = r.u8()?;
        let default_is_stmt = r.u8()?;
        let line_base = r.i8()? as i64;
        let line_range = r.u8()? as i64;
        let opcode_base = r.u8()?;
        if line_range == 0 || opcode_base == 0 {
            return Err(err!("a line program header declares no opcode range"));
        }
        let mut std_lengths = Vec::new();
        for _ in 1..opcode_base {
            std_lengths.push(r.u8()?);
        }
        let _ = default_is_stmt;

        // The unit's own directory, which is not in the table: index 0 means
        // "where this unit was compiled", and every relative entry that *is*
        // in the table is relative to it too.
        let comp_dir = dirs.get(&unit_start).cloned().unwrap_or_default();
        let mut include_dirs = vec![String::new()];
        loop {
            let dir = r.cstr()?;
            if dir.is_empty() {
                break;
            }
            include_dirs.push(dir);
        }
        // Index 0 is the unit's own directory and is not in the table; every
        // entry a file names is one-based against what follows.
        let mut files = vec![String::new()];
        loop {
            let name = r.cstr()?;
            if name.is_empty() {
                break;
            }
            let dir = r.uleb()? as usize;
            let _mtime = r.uleb()?;
            let _length = r.uleb()?;
            files.push(resolve(
                &comp_dir,
                include_dirs.get(dir).map(String::as_str),
                &name,
            ));
        }

        r.seek(program);
        run_program(
            &mut r,
            end,
            &Machine {
                min_inst,
                line_base,
                line_range,
                opcode_base,
                std_lengths: &std_lengths,
            },
            &files,
            &mut rows,
        )?;
        r.seek(end);
    }
    Ok(rows)
}

struct Machine<'a> {
    min_inst: u64,
    line_base: i64,
    line_range: i64,
    opcode_base: u8,
    std_lengths: &'a [u8],
}

fn run_program(
    r: &mut Cursor<'_>,
    end: usize,
    m: &Machine<'_>,
    files: &[String],
    rows: &mut Vec<Row>,
) -> Result<()> {
    let mut address = 0u64;
    let mut file = 1usize;
    let mut line = 1i64;
    let mut column = 0u64;
    // A sequence whose start address was tombstoned is a dropped function, and
    // every row in it is about bytes that are not there.
    let mut dropped = false;

    let emit = |address: u64, file: usize, line: i64, column: u64, rows: &mut Vec<Row>| {
        if line <= 0 {
            return;
        }
        let Some(name) = files.get(file) else { return };
        rows.push(Row {
            offset: address,
            file: name.clone(),
            line: line as u32,
            column: column.min(u32::MAX as u64) as u32,
        });
    };

    while r.pos() < end {
        let op = r.u8()?;
        if op >= m.opcode_base {
            let adjusted = (op - m.opcode_base) as i64;
            address += m.min_inst * (adjusted / m.line_range) as u64;
            line += m.line_base + adjusted % m.line_range;
            if !dropped {
                emit(address, file, line, column, rows);
            }
            continue;
        }
        match op {
            // An extended opcode, whose length is what makes an unknown one
            // skippable.
            0 => {
                let len = r.uleb()? as usize;
                let next = r.pos() + len;
                let sub = r.u8()?;
                match sub {
                    // end_sequence
                    1 => {
                        address = 0;
                        file = 1;
                        line = 1;
                        column = 0;
                        dropped = false;
                    }
                    // set_address
                    2 => {
                        address = match len - 1 {
                            4 => r.u32()? as u64,
                            8 => {
                                let lo = r.u32()? as u64;
                                let hi = r.u32()? as u64;
                                lo | (hi << 32)
                            }
                            other => {
                                return Err(err!("a set_address of {other} bytes is not wasm32"));
                            }
                        };
                        dropped = address == TOMBSTONE || address == TOMBSTONE32;
                    }
                    _ => {}
                }
                r.seek(next);
            }
            // copy
            1 => {
                if !dropped {
                    emit(address, file, line, column, rows);
                }
            }
            // advance_pc
            2 => address += m.min_inst * r.uleb()?,
            // advance_line
            3 => line += r.sleb()?,
            // set_file
            4 => file = r.uleb()? as usize,
            // set_column
            5 => column = r.uleb()?,
            // negate_stmt, basic_block: state this does not carry
            6 | 7 => {}
            // const_add_pc
            8 => {
                let adjusted = (255 - m.opcode_base) as i64;
                address += m.min_inst * (adjusted / m.line_range) as u64;
            }
            // fixed_advance_pc, which is the one that does not scale
            9 => address += r.u16()? as u64,
            // prologue_end, epilogue_begin
            10 | 11 => {}
            // A standard opcode this does not know, skipped by the operand
            // count the header declared — which is the whole reason that table
            // is in the header.
            other => {
                let n = m.std_lengths.get(other as usize - 1).copied().unwrap_or(0);
                for _ in 0..n {
                    r.uleb()?;
                }
            }
        }
    }
    Ok(())
}

/// A file entry's compilation directory, its directory and its name, resolved
/// into the one path a source map carries.
///
/// Three things can already be absolute and each ends the resolution where it
/// stands: a file name, the directory it names, and nothing else. Which is why
/// the standard library's own units come out as `/rustc/<hash>/library/...`
/// while ours come out under the checkout — the compiler remapped one and not
/// the other, and this is not the place to have an opinion about that.
fn resolve(comp_dir: &str, dir: Option<&str>, name: &str) -> String {
    if name.starts_with('/') {
        return name.to_string();
    }
    let path = match dir {
        Some(d) if !d.is_empty() => format!("{d}/{name}"),
        _ => name.to_string(),
    };
    if path.starts_with('/') || comp_dir.is_empty() {
        return path;
    }
    format!("{}/{path}", comp_dir.trim_end_matches('/'))
}

// ---------------------------------------------------------------------------
// .debug_info, for one attribute
// ---------------------------------------------------------------------------

/// `DW_AT_stmt_list` -> `DW_AT_comp_dir`, read off the first DIE of every
/// compilation unit.
fn comp_dirs(info: &[u8], abbrev: &[u8], strs: &[u8]) -> Result<HashMap<u64, String>> {
    let mut out = HashMap::new();
    let mut r = Cursor::new(info, 0);
    while !r.done() {
        let length = r.u32()?;
        if length == 0xffff_ffff || length == 0 {
            break;
        }
        let end = r.pos() + length as usize;
        let version = r.u16()?;
        if version > 4 {
            break;
        }
        let abbrev_offset = r.u32()? as u64;
        let address_size = r.u8()?;

        let code = r.uleb()?;
        if code != 0 {
            let table = abbrev_table(abbrev, abbrev_offset as usize)?;
            if let Some(attrs) = table.get(&code) {
                let mut stmt_list = None;
                let mut comp_dir = None;
                for &(at, form, implicit) in attrs {
                    let value = read_form(&mut r, form, implicit, address_size, strs)?;
                    match at {
                        // DW_AT_stmt_list
                        0x10 => stmt_list = value.number(),
                        // DW_AT_comp_dir
                        0x1b => comp_dir = value.text(),
                        _ => {}
                    }
                }
                if let (Some(offset), Some(dir)) = (stmt_list, comp_dir) {
                    out.insert(offset, dir);
                }
            }
        }
        r.seek(end);
    }
    Ok(out)
}

/// One attribute of one abbreviation: what it is, how it is encoded, and the
/// value the encoding carries here rather than in the unit.
type Attribute = (u64, u64, i64);

/// The abbreviations at one offset, by code.
type Abbreviations = HashMap<u64, Vec<Attribute>>;

fn abbrev_table(abbrev: &[u8], at: usize) -> Result<Abbreviations> {
    let mut out = HashMap::new();
    let mut r = Cursor::new(abbrev, at);
    loop {
        let code = r.uleb()?;
        if code == 0 {
            return Ok(out);
        }
        let _tag = r.uleb()?;
        let _children = r.u8()?;
        let mut attrs = Vec::new();
        loop {
            let attribute = r.uleb()?;
            let form = r.uleb()?;
            // DW_FORM_implicit_const carries its value here rather than in the
            // unit, which is the one form whose abbreviation is longer than
            // two numbers.
            let implicit = if form == 0x21 { r.sleb()? } else { 0 };
            if attribute == 0 && form == 0 {
                break;
            }
            attrs.push((attribute, form, implicit));
        }
        out.insert(code, attrs);
    }
}

enum Value {
    Number(u64),
    Text(String),
    Skipped,
}

impl Value {
    fn number(self) -> Option<u64> {
        match self {
            Value::Number(n) => Some(n),
            _ => None,
        }
    }

    fn text(self) -> Option<String> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }
}

/// Read one attribute, which mostly means skipping it: two of the sixty forms
/// carry something this wants, and the rest have to be walked past exactly.
fn read_form(
    r: &mut Cursor<'_>,
    form: u64,
    implicit: i64,
    address_size: u8,
    strs: &[u8],
) -> Result<Value> {
    let block = |r: &mut Cursor<'_>, n: usize| -> Result<Value> {
        r.take(n)?;
        Ok(Value::Skipped)
    };
    Ok(match form {
        // addr
        0x01 => return block(r, address_size as usize),
        // block2 / block4 / block / block1 / exprloc
        0x03 => {
            let n = r.u16()? as usize;
            return block(r, n);
        }
        0x04 => {
            let n = r.u32()? as usize;
            return block(r, n);
        }
        0x09 | 0x18 => {
            let n = r.uleb()? as usize;
            return block(r, n);
        }
        0x0a => {
            let n = r.u8()? as usize;
            return block(r, n);
        }
        // data1 / flag / ref1 / strx1 / addrx1
        0x0b | 0x0c | 0x11 | 0x25 | 0x29 => Value::Number(r.u8()? as u64),
        // data2 / ref2 / strx2 / addrx2
        0x05 | 0x12 | 0x26 | 0x2a => Value::Number(r.u16()? as u64),
        // strx3 / addrx3
        0x27 | 0x2b => {
            let b = r.take(3)?;
            Value::Number(u32::from_le_bytes([b[0], b[1], b[2], 0]) as u64)
        }
        // data4 / ref4 / ref_addr / sec_offset / line_strp / ref_sup4 /
        // strp_sup / strx4 / addrx4
        0x06 | 0x13 | 0x10 | 0x17 | 0x1f | 0x1c | 0x1d | 0x28 | 0x2c => {
            Value::Number(r.u32()? as u64)
        }
        // data8 / ref8 / ref_sig8
        0x07 | 0x14 | 0x20 => {
            let lo = r.u32()? as u64;
            let hi = r.u32()? as u64;
            Value::Number(lo | (hi << 32))
        }
        // data16
        0x1e => return block(r, 16),
        // string, inline
        0x08 => Value::Text(r.cstr()?),
        // sdata
        0x0d => Value::Number(r.sleb()? as u64),
        // udata / ref_udata / strx / addrx / loclistx / rnglistx
        0x0f | 0x15 | 0x1a | 0x1b | 0x22 | 0x23 => Value::Number(r.uleb()?),
        // strp, into .debug_str
        0x0e => {
            let at = r.u32()? as usize;
            match Cursor::new(strs, at).cstr() {
                Ok(s) => Value::Text(s),
                Err(_) => Value::Skipped,
            }
        }
        // flag_present
        0x19 => Value::Number(1),
        // implicit_const, whose value was in the abbreviation
        0x21 => Value::Number(implicit as u64),
        // indirect: the form itself is in the unit
        0x16 => {
            let inner = r.uleb()?;
            return read_form(r, inner, implicit, address_size, strs);
        }
        other => {
            return Err(err!("DW_FORM {other:#x} is one this does not know"));
        }
    })
}

// ---------------------------------------------------------------------------
// The map
// ---------------------------------------------------------------------------

struct Rendered {
    text: String,
    sources: usize,
    embedded: usize,
    rows: usize,
}

/// The rows, as the one thing DevTools reads.
///
/// A wasm source map is a source map for a file with one line: every mapping
/// is a segment of that line and its "column" is a byte offset. So the
/// mappings are sorted by offset, one segment each, and there is no `;`
/// anywhere in the string.
fn render(rows: &[Row], module: &str, root: &Path) -> Rendered {
    let mut ordered: Vec<&Row> = rows.iter().collect();
    ordered.sort_by_key(|r| r.offset);

    let mut sources: Vec<String> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();

    let mut mappings = String::new();
    let (mut last_offset, mut last_source, mut last_line, mut last_column) =
        (0i64, 0i64, 0i64, 0i64);
    let mut previous: Option<u64> = None;
    let mut emitted = 0usize;

    for row in ordered {
        // One mapping per byte. A row repeating an offset is the same
        // instruction described twice, and the first description is the one
        // the segment before it already carries.
        if previous == Some(row.offset) {
            continue;
        }
        previous = Some(row.offset);

        let source = *index.entry(row.file.as_str()).or_insert_with(|| {
            sources.push(row.file.clone());
            sources.len() - 1
        }) as i64;

        let offset = row.offset as i64;
        let line = row.line as i64 - 1;
        let column = row.column as i64;

        if emitted > 0 {
            mappings.push(',');
        }
        vlq(&mut mappings, offset - last_offset);
        vlq(&mut mappings, source - last_source);
        vlq(&mut mappings, line - last_line);
        vlq(&mut mappings, column - last_column);
        last_offset = offset;
        last_source = source;
        last_line = line;
        last_column = column;
        emitted += 1;
    }

    let contents: Vec<Option<String>> = sources
        .iter()
        .map(|s| {
            let path = Path::new(s);
            // Ours and only ours. `starts_with` is a path comparison rather
            // than a string one, so `/home/me/oxidezap-notes` is not under
            // `/home/me/oxidezap`.
            if !path.is_absolute() || !path.starts_with(root) {
                return None;
            }
            fs::read_to_string(path).ok()
        })
        .collect();

    let mut text = String::from("{\"version\":3,\"file\":");
    json_string(&mut text, module);
    text.push_str(",\"sources\":[");
    for (i, s) in sources.iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        json_string(&mut text, s);
    }
    text.push_str("],\"sourcesContent\":[");
    for (i, c) in contents.iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        match c {
            Some(body) => json_string(&mut text, body),
            None => text.push_str("null"),
        }
    }
    text.push_str("],\"names\":[],\"mappings\":");
    json_string(&mut text, &mappings);
    text.push('}');

    Rendered {
        sources: sources.len(),
        embedded: contents.iter().filter(|c| c.is_some()).count(),
        rows: emitted,
        text,
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 VLQ: the sign in the low bit, six bits at a time, the continuation
/// in the top one.
fn vlq(out: &mut String, value: i64) {
    let mut v = if value < 0 {
        ((-value) as u64) << 1 | 1
    } else {
        (value as u64) << 1
    };
    loop {
        let mut digit = (v & 0x1f) as usize;
        v >>= 5;
        if v > 0 {
            digit |= 0x20;
        }
        out.push(BASE64[digit] as char);
        if v == 0 {
            return;
        }
    }
}

/// A JSON string. Source text is arbitrary, so this escapes what the grammar
/// requires and nothing else — a `sourcesContent` entry is a whole file, and
/// escaping every non-ASCII byte would double a map that is already the
/// largest thing in the directory.
fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TempDir;

    /// A version 4 line program, assembled by hand.
    ///
    /// Which is the only way to test this without a compiler in the loop, and
    /// worth the assembler: the interpreter is the part of this file where a
    /// mistake is silent — every operand is a number and every wrong number is
    /// still a number, so a table that decodes to the wrong lines decodes
    /// perfectly.
    struct Program {
        dirs: Vec<&'static str>,
        files: Vec<(&'static str, u64)>,
        body: Vec<u8>,
    }

    impl Program {
        fn new() -> Self {
            Program {
                dirs: Vec::new(),
                files: Vec::new(),
                body: Vec::new(),
            }
        }

        fn set_address(mut self, address: u32) -> Self {
            self.body.extend_from_slice(&[0, 5, 2]);
            self.body.extend_from_slice(&address.to_le_bytes());
            self
        }

        fn advance_pc(mut self, by: u64) -> Self {
            self.body.push(2);
            write_uleb(&mut self.body, by);
            self
        }

        fn advance_line(mut self, by: i64) -> Self {
            self.body.push(3);
            // One byte is enough for everything these tests advance by.
            self.body.push((by as i8 as u8) & 0x7f);
            self
        }

        fn set_column(mut self, column: u64) -> Self {
            self.body.push(5);
            write_uleb(&mut self.body, column);
            self
        }

        fn set_file(mut self, file: u64) -> Self {
            self.body.push(4);
            write_uleb(&mut self.body, file);
            self
        }

        fn copy(mut self) -> Self {
            self.body.push(1);
            self
        }

        fn end_sequence(mut self) -> Self {
            self.body.extend_from_slice(&[0, 1, 1]);
            self
        }

        /// The header in front of it. `opcode_base` is 13 and the standard
        /// lengths are the standard ones, so a special opcode means what it
        /// means everywhere.
        fn section(&self) -> Vec<u8> {
            let mut header = vec![
                1,            // minimum_instruction_length
                1,            // maximum_operations_per_instruction
                1,            // default_is_stmt
                (-5i8) as u8, // line_base
                14,           // line_range
                13,           // opcode_base
            ];
            // The standard opcodes' operand counts, which are what makes an
            // opcode this does not know skippable.
            header.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
            for dir in &self.dirs {
                header.extend_from_slice(dir.as_bytes());
                header.push(0);
            }
            header.push(0);
            for (name, dir) in &self.files {
                header.extend_from_slice(name.as_bytes());
                header.push(0);
                write_uleb(&mut header, *dir);
                write_uleb(&mut header, 0);
                write_uleb(&mut header, 0);
            }
            header.push(0);

            let mut unit = Vec::new();
            unit.extend_from_slice(&4u16.to_le_bytes()); // version
            unit.extend_from_slice(&(header.len() as u32).to_le_bytes());
            unit.extend_from_slice(&header);
            unit.extend_from_slice(&self.body);

            let mut out = Vec::new();
            out.extend_from_slice(&(unit.len() as u32).to_le_bytes());
            out.extend_from_slice(&unit);
            out
        }
    }

    fn rows_of(p: &Program) -> Vec<Row> {
        line_rows(&p.section(), &HashMap::new()).expect("a program this file assembled")
    }

    #[test]
    fn a_program_of_standard_opcodes_reads_back_as_the_rows_it_describes() {
        let program = Program::new()
            .set_address(0)
            .advance_line(9)
            .set_column(4)
            .copy()
            .advance_pc(16)
            .advance_line(1)
            .set_column(8)
            .copy()
            .advance_pc(4)
            .end_sequence();
        let mut p = Program::new();
        p.dirs = vec!["src"];
        p.files = vec![("main.rs", 1)];
        p.body = program.body;

        let rows = rows_of(&p);
        let seen: Vec<(u64, &str, u32, u32)> = rows
            .iter()
            .map(|r| (r.offset, r.file.as_str(), r.line, r.column))
            .collect();
        assert_eq!(
            seen,
            vec![(0, "src/main.rs", 10, 4), (16, "src/main.rs", 11, 8)]
        );
    }

    /// The whole reason `dropped` exists. The linker does not remove the rows
    /// of code it discarded; it points them at all-ones, and mapping them puts
    /// a dead function's source lines over whatever is really at the end of
    /// the module.
    #[test]
    fn a_tombstoned_sequence_maps_nothing() {
        let mut p = Program::new();
        p.files = vec![("gone.rs", 0)];
        p.body = Program::new()
            .set_address(u32::MAX)
            .advance_line(41)
            .copy()
            .advance_pc(8)
            .end_sequence()
            .body;
        assert!(rows_of(&p).is_empty());
    }

    /// And that the flag is cleared again: a sequence after a discarded one is
    /// ordinary code, and reading the tombstone as a property of the unit
    /// would lose every function after the first dead one.
    #[test]
    fn a_sequence_after_a_tombstoned_one_maps_normally() {
        let mut p = Program::new();
        p.files = vec![("live.rs", 0)];
        p.body = Program::new()
            .set_address(u32::MAX)
            .copy()
            .end_sequence()
            .set_address(64)
            .advance_line(2)
            .copy()
            .end_sequence()
            .body;
        let rows = rows_of(&p);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].offset, rows[0].line), (64, 3));
    }

    /// A special opcode is an address advance and a line advance in one byte,
    /// and it is how most of a real table is spelled.
    #[test]
    fn a_special_opcode_advances_both() {
        let mut p = Program::new();
        p.files = vec![("s.rs", 0)];
        let mut body = Program::new().set_address(0).body;
        // opcode_base 13, line_base -5, line_range 14: adjusted = op - 13,
        // address += adjusted / 14, line += -5 + adjusted % 14.
        // 13 + 14 + 6 = 33 advances the address by one and the line by one.
        body.push(33);
        body.extend_from_slice(&[0, 1, 1]);
        p.body = body;
        let rows = rows_of(&p);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].offset, rows[0].line), (1, 2));
    }

    #[test]
    fn set_file_selects_from_the_table() {
        let mut p = Program::new();
        p.dirs = vec!["a", "b"];
        p.files = vec![("one.rs", 1), ("two.rs", 2)];
        p.body = Program::new()
            .set_address(0)
            .copy()
            .advance_pc(1)
            .set_file(2)
            .copy()
            .end_sequence()
            .body;
        let rows = rows_of(&p);
        assert_eq!(
            rows.iter().map(|r| r.file.as_str()).collect::<Vec<_>>(),
            vec!["a/one.rs", "b/two.rs"]
        );
    }

    #[test]
    fn a_line_table_this_does_not_read_is_refused_by_name() {
        let mut section = Program::new().section();
        // The version, which is the third and fourth bytes.
        section[4] = 5;
        let e = line_rows(&section, &HashMap::new()).expect_err("version 5");
        assert!(e.0.contains("version 5"), "{}", e.0);
    }

    #[test]
    fn paths_resolve_against_the_directory_they_were_compiled_in() {
        assert_eq!(
            resolve("/repo", Some("src"), "main.rs"),
            "/repo/src/main.rs"
        );
        assert_eq!(resolve("/repo", None, "main.rs"), "/repo/main.rs");
        assert_eq!(resolve("/repo", Some(""), "main.rs"), "/repo/main.rs");
        // An absolute directory is its own answer, which is how a registry
        // dependency's sources are named.
        assert_eq!(
            resolve("/repo", Some("/dep/src"), "lib.rs"),
            "/dep/src/lib.rs"
        );
        // And so is an absolute file name.
        assert_eq!(resolve("/repo", Some("src"), "/x/gen.rs"), "/x/gen.rs");
        // Nothing to resolve against leaves the path as the compiler wrote it.
        assert_eq!(resolve("", Some("src"), "main.rs"), "src/main.rs");
    }

    #[test]
    fn base64_vlq_is_the_encoding_devtools_reads() {
        let mut s = String::new();
        vlq(&mut s, 0);
        vlq(&mut s, 1);
        vlq(&mut s, -1);
        vlq(&mut s, 16);
        vlq(&mut s, -16);
        vlq(&mut s, 110);
        assert_eq!(s, "ACDgBhB8G");
    }

    #[test]
    fn a_json_string_escapes_what_the_grammar_requires() {
        let mut s = String::new();
        json_string(&mut s, "a\"b\\c\nd\te\u{1}f\u{e9}");
        assert_eq!(s, "\"a\\\"b\\\\c\\nd\\te\\u0001f\u{e9}\"");
    }

    #[test]
    fn a_custom_section_is_a_name_and_a_payload_appended_whole() {
        let mut out = Vec::new();
        append_custom(&mut out, "sourceMappingURL", &encode_name("m.map"));
        assert_eq!(
            out,
            [&[0u8, 23, 16][..], b"sourceMappingURL", &[5][..], b"m.map",].concat()
        );
    }

    /// The projection itself: sorted by offset, one segment per byte, deltas
    /// all the way down — and the source text of what is ours.
    #[test]
    fn the_map_carries_the_rows_in_offset_order_and_our_own_sources_with_them() {
        let dir = TempDir::new("xtask-map").expect("a temporary directory");
        let root = dir.path();
        let mine = root.join("mine.rs");
        fs::write(&mine, "fn main() {}\n").expect("write");

        let rows = vec![
            Row {
                offset: 20,
                file: "/elsewhere/dep.rs".to_string(),
                line: 7,
                column: 3,
            },
            Row {
                offset: 10,
                file: mine.to_string_lossy().into_owned(),
                line: 1,
                column: 0,
            },
            // The same byte described twice, which a real table does at every
            // inlining boundary.
            Row {
                offset: 10,
                file: "/elsewhere/dep.rs".to_string(),
                line: 9,
                column: 0,
            },
        ];

        let out = render(&rows, "m_bg.wasm", root);
        assert_eq!(out.rows, 2);
        assert_eq!(out.sources, 2);
        assert_eq!(out.embedded, 1);
        // Offset 10 first, and the row that lost the tie is not in it.
        assert!(
            out.text.contains("\"mappings\":\"UAAA,UCMG\""),
            "{}",
            out.text
        );
        assert!(out.text.contains("fn main() {}"));
        // A source outside the repository is named and not embedded.
        assert!(out.text.contains("null"));
    }

    #[test]
    fn a_module_with_no_debug_line_says_which_build_has_one() {
        let dir = TempDir::new("xtask-map").expect("a temporary directory");
        let wasm = dir.path().join("bare_bg.wasm");
        // The smallest module with a code section: a header, and an empty
        // vector of function bodies.
        fs::write(
            &wasm,
            [b"\0asm".as_slice(), &[1, 0, 0, 0], &[10, 1, 0]].concat(),
        )
        .expect("write");
        let e = write(&wasm, dir.path()).expect_err("no debug information");
        assert!(e.0.contains("WEB_PROFILE=dwarf"), "{}", e.0);
    }
}
