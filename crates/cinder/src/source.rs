//! Source files, byte spans and offset -> line:column mapping.
//!
//! Each file is stored twice:
//!
//! * `orig` — the bytes exactly as read, used for printing source lines in
//!   diagnostics and for physical line numbers;
//! * `text` — the *logical* text after translation phase 1–2 (UTF-8 BOM
//!   dropped, `\r\n`/`\r` folded to `\n`, backslash-newline splices removed).
//!   The lexer works on this, and every [`Span`] is an offset into it.
//!
//! `removed` records where bytes were dropped so a logical offset can be
//! mapped back to the original file in O(log n).

use std::path::PathBuf;

pub type FileId = u32;

/// A half-open byte range `[lo, hi)` in the logical text of one file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Span {
    pub file: FileId,
    pub lo: u32,
    pub hi: u32,
}

impl Span {
    /// Used for tokens/nodes that have no source position (synthesized code,
    /// command-line `-D` macros, ...).
    pub const DUMMY: Span = Span { file: u32::MAX, lo: 0, hi: 0 };

    pub fn new(file: FileId, lo: u32, hi: u32) -> Span {
        Span { file, lo, hi }
    }

    pub fn is_dummy(self) -> bool {
        self.file == u32::MAX
    }

    /// Smallest span covering both. If the spans live in different files (or
    /// one is a dummy) the first non-dummy operand wins.
    pub fn to(self, other: Span) -> Span {
        if self.is_dummy() {
            return other;
        }
        if other.is_dummy() || other.file != self.file {
            return self;
        }
        Span { file: self.file, lo: self.lo.min(other.lo), hi: self.hi.max(other.hi) }
    }

    pub fn len(self) -> u32 {
        self.hi - self.lo
    }

    pub fn is_empty(self) -> bool {
        self.hi == self.lo
    }

    /// Zero-width span at the end of this one.
    pub fn end(self) -> Span {
        Span { file: self.file, lo: self.hi, hi: self.hi }
    }
}

impl Default for Span {
    fn default() -> Span {
        Span::DUMMY
    }
}

/// A resolved human-readable location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loc {
    pub file: String,
    pub line: u32,
    pub col: u32,
}

pub struct SourceFile {
    pub id: FileId,
    /// Name as it should appear in diagnostics and `__FILE__`.
    pub name: String,
    /// Filesystem location (if any); used for resolving `#include "..."`.
    pub path: Option<PathBuf>,
    pub orig: String,
    pub text: String,
    /// `(logical_pos, cumulative_removed_bytes)`, sorted by `logical_pos`.
    removed: Vec<(u32, u32)>,
    /// Byte offsets (in `orig`) of the first byte of each physical line.
    line_starts: Vec<u32>,
}

/// Translation phases 1–2: BOM, line endings, line splicing.
fn normalize(src: &str) -> (String, Vec<(u32, u32)>) {
    let b = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut removed: Vec<(u32, u32)> = Vec::new();
    let mut cum = 0u32;
    let mut i = 0usize;
    if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        i = 3;
        cum = 3;
        removed.push((0, 3));
    }
    while i < b.len() {
        let c = b[i];
        if c == b'\\' {
            let mut j = i + 1;
            if j < b.len() && b[j] == b'\r' {
                j += 1;
            }
            if j < b.len() && b[j] == b'\n' {
                j += 1;
                cum += (j - i) as u32;
                removed.push((out.len() as u32, cum));
                i = j;
                continue;
            }
        } else if c == b'\r' {
            if i + 1 < b.len() && b[i + 1] == b'\n' {
                cum += 1;
                removed.push((out.len() as u32, cum));
                i += 1;
                continue;
            }
            out.push(b'\n');
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    // Only ASCII bytes were removed/replaced, so UTF-8 validity is preserved.
    (String::from_utf8(out).expect("normalization preserves UTF-8"), removed)
}

fn compute_line_starts(s: &str) -> Vec<u32> {
    let mut v = vec![0u32];
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\n' => v.push((i + 1) as u32),
            b'\r' => {
                if i + 1 < b.len() && b[i + 1] == b'\n' {
                    i += 1;
                }
                v.push((i + 1) as u32);
            }
            _ => {}
        }
        i += 1;
    }
    v
}

impl SourceFile {
    /// Map an offset in `text` to the corresponding offset in `orig`.
    pub fn orig_offset(&self, logical: u32) -> u32 {
        let idx = self.removed.partition_point(|e| e.0 <= logical);
        let cum = if idx == 0 { 0 } else { self.removed[idx - 1].1 };
        logical + cum
    }

    /// Inclusive-start / exclusive-end byte range of a span within `orig`.
    pub fn orig_range(&self, lo: u32, hi: u32) -> (u32, u32) {
        let a = self.orig_offset(lo);
        let b = if hi > lo { self.orig_offset(hi - 1) + 1 } else { a };
        (a, b)
    }

    /// 1-based line and 1-based byte column of an offset in `orig`.
    pub fn line_col_orig(&self, off: u32) -> (u32, u32) {
        let line_idx = self.line_starts.partition_point(|&s| s <= off) - 1;
        (line_idx as u32 + 1, off - self.line_starts[line_idx] + 1)
    }

    pub fn line_count(&self) -> u32 {
        self.line_starts.len() as u32
    }

    /// Text of a 1-based physical line, without its line terminator.
    pub fn line_text(&self, line: u32) -> &str {
        let idx = (line - 1) as usize;
        let start = self.line_starts[idx] as usize;
        let end = if idx + 1 < self.line_starts.len() { self.line_starts[idx + 1] as usize } else { self.orig.len() };
        self.orig[start..end].trim_end_matches(['\n', '\r'])
    }

    /// Byte offset (in `orig`) where the 1-based line begins.
    pub fn line_start(&self, line: u32) -> u32 {
        self.line_starts[(line - 1) as usize]
    }
}

#[derive(Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> SourceMap {
        SourceMap::default()
    }

    pub fn add_file(&mut self, name: impl Into<String>, path: Option<PathBuf>, text: String) -> FileId {
        let id = self.files.len() as FileId;
        let (logical, removed) = normalize(&text);
        let line_starts = compute_line_starts(&text);
        self.files.push(SourceFile { id, name: name.into(), path, orig: text, text: logical, removed, line_starts });
        id
    }

    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id as usize]
    }

    pub fn files(&self) -> &[SourceFile] {
        &self.files
    }

    pub fn text(&self, id: FileId) -> &str {
        &self.files[id as usize].text
    }

    /// The exact source text covered by `span` (logical text).
    pub fn snippet(&self, span: Span) -> &str {
        if span.is_dummy() {
            return "";
        }
        &self.files[span.file as usize].text[span.lo as usize..span.hi as usize]
    }

    /// Line/column of the *start* of the span, in the original file.
    pub fn loc(&self, span: Span) -> Option<Loc> {
        if span.is_dummy() {
            return None;
        }
        let f = &self.files[span.file as usize];
        let (line, col) = f.line_col_orig(f.orig_offset(span.lo));
        Some(Loc { file: f.name.clone(), line, col })
    }

    pub fn line_of(&self, span: Span) -> u32 {
        self.loc(span).map(|l| l.line).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(src: &str) -> (SourceMap, FileId) {
        let mut sm = SourceMap::new();
        let id = sm.add_file("t.c", None, src.to_string());
        (sm, id)
    }

    #[test]
    fn plain_lines_and_columns() {
        let (sm, id) = one("int x;\nint y;\n");
        let loc = sm.loc(Span::new(id, 7, 10)).unwrap();
        assert_eq!((loc.line, loc.col), (2, 1));
        let loc = sm.loc(Span::new(id, 4, 5)).unwrap();
        assert_eq!((loc.line, loc.col), (1, 5));
    }

    #[test]
    fn crlf_is_folded_but_positions_stay_physical() {
        let (sm, id) = one("a\r\nbc\r\n");
        assert_eq!(sm.text(id), "a\nbc\n");
        // 'b' is logical offset 2, physical line 2 column 1.
        let loc = sm.loc(Span::new(id, 2, 3)).unwrap();
        assert_eq!((loc.line, loc.col), (2, 1));
        let loc = sm.loc(Span::new(id, 3, 4)).unwrap();
        assert_eq!((loc.line, loc.col), (2, 2));
    }

    #[test]
    fn line_splices_are_removed_from_logical_text() {
        let (sm, id) = one("ab\\\ncd\nef\n");
        assert_eq!(sm.text(id), "abcd\nef\n");
        // 'c' is logical offset 2 but sits on physical line 2, col 1.
        let loc = sm.loc(Span::new(id, 2, 3)).unwrap();
        assert_eq!((loc.line, loc.col), (2, 1));
        // 'e' is on physical line 3.
        let loc = sm.loc(Span::new(id, 5, 6)).unwrap();
        assert_eq!((loc.line, loc.col), (3, 1));
    }

    #[test]
    fn crlf_splice() {
        let (sm, id) = one("a\\\r\nb\r\n");
        assert_eq!(sm.text(id), "ab\n");
        let loc = sm.loc(Span::new(id, 1, 2)).unwrap();
        assert_eq!((loc.line, loc.col), (2, 1));
    }

    #[test]
    fn bom_is_dropped() {
        let (sm, id) = one("\u{feff}int");
        assert_eq!(sm.text(id), "int");
        let loc = sm.loc(Span::new(id, 0, 3)).unwrap();
        assert_eq!((loc.line, loc.col), (1, 4));
    }

    #[test]
    fn line_text_strips_terminators() {
        let (sm, id) = one("one\r\ntwo\nthree");
        let f = sm.file(id);
        assert_eq!(f.line_text(1), "one");
        assert_eq!(f.line_text(2), "two");
        assert_eq!(f.line_text(3), "three");
    }

    #[test]
    fn span_merge() {
        let a = Span::new(0, 2, 4);
        let b = Span::new(0, 6, 9);
        assert_eq!(a.to(b), Span::new(0, 2, 9));
        assert_eq!(Span::DUMMY.to(b), b);
        assert_eq!(a.to(Span::DUMMY), a);
    }
}
