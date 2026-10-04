//! Diagnostics: collection, warning configuration and Clang-style rendering.
//!
//! A [`Diagnostic`] carries a primary span (with an optional explicit caret
//! position), secondary labelled ranges, an optional fix-it, and attached
//! notes. Rendering produces output in the familiar shape:
//!
//! ```text
//! t.c:3:12: error: use of undeclared identifier 'y'
//!    3 |     int x = y + 1;
//!      |             ^
//! t.c:1:5: note: 'x' declared here
//! ```

use crate::source::{SourceMap, Span};
use std::fmt::Write as _;

// ───────────────────────────── warning flags ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WarnGroup {
    /// On unless disabled.
    Default,
    /// Enabled by `-Wall`.
    All,
    /// Enabled by `-Wextra`.
    Extra,
    /// Only enabled when asked for by name (or `-Weverything`).
    Off,
}

macro_rules! warn_table {
    ($($variant:ident, $name:literal, $group:ident;)*) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum Warn { $($variant),* }

        impl Warn {
            pub const ALL: &'static [Warn] = &[$(Warn::$variant),*];

            /// The flag spelling without the `-W` prefix.
            pub fn name(self) -> &'static str {
                match self { $(Warn::$variant => $name),* }
            }

            pub fn group(self) -> WarnGroup {
                match self { $(Warn::$variant => WarnGroup::$group),* }
            }

            pub fn from_name(n: &str) -> Option<Warn> {
                match n { $($name => Some(Warn::$variant),)* _ => None }
            }

            fn bit(self) -> u64 {
                1u64 << (self as u32)
            }
        }
    };
}

warn_table! {
    ImplicitFunctionDeclaration, "implicit-function-declaration", Default;
    IntConversion,               "int-conversion", Default;
    IncompatiblePointerTypes,    "incompatible-pointer-types", Default;
    DiscardedQualifiers,         "incompatible-pointer-types-discards-qualifiers", Default;
    ReturnType,                  "return-type", Default;
    DivisionByZero,              "division-by-zero", Default;
    Overflow,                    "overflow", Default;
    MacroRedefined,              "macro-redefined", Default;
    UnknownAttributes,           "unknown-attributes", Default;
    ExtraTokens,                 "extra-tokens", Default;
    ReturnStackAddress,          "return-stack-address", Default;
    UnknownEscape,               "unknown-escape-sequence", Default;
    Multichar,                   "multichar", Default;
    PpWarnings,                  "#warnings", Default;
    UnusedVariable,              "unused-variable", All;
    UnusedFunction,              "unused-function", All;
    UnusedValue,                 "unused-value", All;
    UnusedLabel,                 "unused-label", All;
    Uninitialized,               "uninitialized", All;
    Parentheses,                 "parentheses", All;
    Conversion,                  "conversion", All;
    UnknownPragmas,              "unknown-pragmas", All;
    UnusedParameter,             "unused-parameter", Extra;
    SignCompare,                 "sign-compare", Extra;
    EmptyBody,                   "empty-body", Extra;
    SignConversion,              "sign-conversion", Off;
    Shadow,                      "shadow", Off;
}

/// Which warnings are on, and which are promoted to errors.
#[derive(Clone, Debug)]
pub struct WarnConfig {
    enabled: u64,
    errors: u64,
    /// `-Werror`: every enabled warning becomes an error.
    pub werror_all: bool,
    /// `-w`: suppress every warning.
    pub suppress_all: bool,
}

impl Default for WarnConfig {
    fn default() -> WarnConfig {
        let mut c = WarnConfig { enabled: 0, errors: 0, werror_all: false, suppress_all: false };
        c.enable_group(WarnGroup::Default);
        c
    }
}

impl WarnConfig {
    fn enable_group(&mut self, upto: WarnGroup) {
        let rank = |g: WarnGroup| match g {
            WarnGroup::Default => 0,
            WarnGroup::All => 1,
            WarnGroup::Extra => 2,
            WarnGroup::Off => 3,
        };
        for &w in Warn::ALL {
            if rank(w.group()) <= rank(upto) {
                self.enabled |= w.bit();
            }
        }
    }

    pub fn enable_all(&mut self) {
        self.enable_group(WarnGroup::All);
    }

    pub fn enable_extra(&mut self) {
        self.enable_group(WarnGroup::Extra);
    }

    pub fn enable_everything(&mut self) {
        self.enable_group(WarnGroup::Off);
    }

    pub fn enable(&mut self, w: Warn) {
        self.enabled |= w.bit();
    }

    pub fn disable(&mut self, w: Warn) {
        self.enabled &= !w.bit();
    }

    pub fn set_error(&mut self, w: Warn, on: bool) {
        if on {
            self.errors |= w.bit();
            self.enabled |= w.bit();
        } else {
            self.errors &= !w.bit();
        }
    }

    pub fn is_enabled(&self, w: Warn) -> bool {
        !self.suppress_all && self.enabled & w.bit() != 0
    }

    pub fn is_error(&self, w: Warn) -> bool {
        self.werror_all || self.errors & w.bit() != 0
    }

    /// Apply one `-W...` command-line flag (without the leading `-W`).
    /// Returns `false` if the flag is not recognised.
    pub fn apply_flag(&mut self, flag: &str) -> bool {
        match flag {
            "all" => self.enable_all(),
            "extra" => {
                self.enable_all();
                self.enable_extra();
            }
            "everything" => self.enable_everything(),
            "error" => self.werror_all = true,
            "no-error" => self.werror_all = false,
            _ => {
                if let Some(name) = flag.strip_prefix("error=") {
                    match Warn::from_name(name) {
                        Some(w) => self.set_error(w, true),
                        None => return false,
                    }
                } else if let Some(name) = flag.strip_prefix("no-error=") {
                    match Warn::from_name(name) {
                        Some(w) => self.set_error(w, false),
                        None => return false,
                    }
                } else if let Some(name) = flag.strip_prefix("no-") {
                    match Warn::from_name(name) {
                        Some(w) => self.disable(w),
                        None => return false,
                    }
                } else {
                    match Warn::from_name(flag) {
                        Some(w) => self.enable(w),
                        None => return false,
                    }
                }
            }
        }
        true
    }
}

// ───────────────────────────── diagnostics ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Note,
    Warning,
    Error,
    /// An error after which compilation cannot continue (missing `#include`,
    /// `#error`, too many errors).
    Fatal,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Note => "note",
            Level::Warning => "warning",
            Level::Error => "error",
            Level::Fatal => "fatal error",
        }
    }
}

/// A suggested edit shown beneath the caret line (`expected ';'` etc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fixit {
    pub at: Span,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub level: Level,
    pub flag: Option<Warn>,
    /// Set when a warning was promoted to an error by `-Werror`.
    pub promoted: bool,
    pub message: String,
    pub span: Span,
    /// Offset (logical) of the `^`; defaults to the start of `span`.
    pub caret: Option<u32>,
    /// Extra ranges to underline with `~`.
    pub labels: Vec<Span>,
    pub fixit: Option<Fixit>,
    pub notes: Vec<Diagnostic>,
}

impl Diagnostic {
    fn base(level: Level, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            level,
            flag: None,
            promoted: false,
            message: message.into(),
            span,
            caret: None,
            labels: Vec::new(),
            fixit: None,
            notes: Vec::new(),
        }
    }

    pub fn error(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::base(Level::Error, span, message)
    }

    pub fn fatal(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::base(Level::Fatal, span, message)
    }

    pub fn warning(flag: Warn, span: Span, message: impl Into<String>) -> Diagnostic {
        let mut d = Diagnostic::base(Level::Warning, span, message);
        d.flag = Some(flag);
        d
    }

    pub fn note(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::base(Level::Note, span, message)
    }

    pub fn with_caret(mut self, off: u32) -> Diagnostic {
        self.caret = Some(off);
        self
    }

    pub fn with_label(mut self, span: Span) -> Diagnostic {
        if !span.is_dummy() {
            self.labels.push(span);
        }
        self
    }

    pub fn with_note(mut self, span: Span, message: impl Into<String>) -> Diagnostic {
        self.notes.push(Diagnostic::note(span, message));
        self
    }

    pub fn with_fixit(mut self, at: Span, text: impl Into<String>) -> Diagnostic {
        self.fixit = Some(Fixit { at, text: text.into() });
        self
    }

    pub fn is_error(&self) -> bool {
        matches!(self.level, Level::Error | Level::Fatal)
    }
}

/// Collects diagnostics for a compilation, applying warning configuration.
pub struct DiagCtx {
    pub config: WarnConfig,
    diags: Vec<Diagnostic>,
    errors: usize,
    warnings: usize,
    fatal: bool,
    /// Stop after this many errors (0 = unlimited).
    pub error_limit: usize,
    limit_hit: bool,
}

impl Default for DiagCtx {
    fn default() -> DiagCtx {
        DiagCtx::new()
    }
}

impl DiagCtx {
    pub fn new() -> DiagCtx {
        DiagCtx {
            config: WarnConfig::default(),
            diags: Vec::new(),
            errors: 0,
            warnings: 0,
            fatal: false,
            error_limit: 20,
            limit_hit: false,
        }
    }

    pub fn emit(&mut self, mut d: Diagnostic) {
        if self.limit_hit {
            return;
        }
        if d.level == Level::Warning {
            let flag = d.flag.expect("warnings carry a flag");
            if !self.config.is_enabled(flag) {
                return;
            }
            if self.config.is_error(flag) {
                d.level = Level::Error;
                d.promoted = true;
            }
        }
        // Collapse exact repeats (the same problem hit twice by recovery).
        if let Some(last) = self.diags.last() {
            if last.level == d.level && last.message == d.message && last.span == d.span {
                return;
            }
        }
        match d.level {
            Level::Warning => self.warnings += 1,
            Level::Error => self.errors += 1,
            Level::Fatal => {
                self.errors += 1;
                self.fatal = true;
            }
            Level::Note => {}
        }
        self.diags.push(d);
        if self.error_limit != 0 && self.errors >= self.error_limit && !self.fatal {
            self.limit_hit = true;
            self.fatal = true;
            self.diags.push(Diagnostic::fatal(
                Span::DUMMY,
                format!("too many errors emitted, stopping now [-ferror-limit={}]", self.error_limit),
            ));
        }
    }

    pub fn error(&mut self, span: Span, message: impl Into<String>) {
        self.emit(Diagnostic::error(span, message));
    }

    pub fn warn(&mut self, flag: Warn, span: Span, message: impl Into<String>) {
        self.emit(Diagnostic::warning(flag, span, message));
    }

    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }

    /// True after a fatal error or hitting the error limit.
    pub fn is_fatal(&self) -> bool {
        self.fatal
    }

    pub fn error_count(&self) -> usize {
        self.errors
    }

    pub fn warning_count(&self) -> usize {
        self.warnings
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diags
    }

    pub fn take(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diags)
    }

    /// "1 warning and 2 errors generated." (None if there was nothing to report.)
    pub fn summary(&self) -> Option<String> {
        let w = self.warnings;
        let e = self.errors;
        let plural = |n: usize, s: &str| format!("{} {}{}", n, s, if n == 1 { "" } else { "s" });
        match (w, e) {
            (0, 0) => None,
            (w, 0) => Some(format!("{} generated.", plural(w, "warning"))),
            (0, e) => Some(format!("{} generated.", plural(e, "error"))),
            (w, e) => Some(format!("{} and {} generated.", plural(w, "warning"), plural(e, "error"))),
        }
    }
}

// ───────────────────────────── rendering ─────────────────────────────

/// ANSI styling. When `enabled` is false every method returns the text as-is.
#[derive(Clone, Copy)]
pub struct Painter {
    pub enabled: bool,
}

impl Painter {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.enabled {
            format!("\x1b[{}m{}\x1b[0m", code, s)
        } else {
            s.to_string()
        }
    }

    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }

    fn level(&self, l: Level, s: &str) -> String {
        match l {
            Level::Error | Level::Fatal => self.paint("1;31", s),
            Level::Warning => self.paint("1;35", s),
            Level::Note => self.paint("1;36", s),
        }
    }

    fn caret(&self, s: &str) -> String {
        self.paint("1;32", s)
    }

    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
}

const TAB_WIDTH: usize = 4;

/// Expand tabs / control characters, returning the display text and a table
/// mapping each *byte offset* in the original line to its display column.
fn expand_line(line: &str) -> (String, Vec<usize>) {
    let mut out = String::new();
    let mut cols = vec![0usize; line.len() + 1];
    let mut col = 0usize;
    for (i, ch) in line.char_indices() {
        for k in 0..ch.len_utf8() {
            cols[i + k] = col;
        }
        match ch {
            '\t' => {
                let n = TAB_WIDTH - (col % TAB_WIDTH);
                for _ in 0..n {
                    out.push(' ');
                }
                col += n;
            }
            c if c.is_control() => {
                out.push('?');
                col += 1;
            }
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    cols[line.len()] = col;
    (out, cols)
}

fn digits(mut n: u32) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

pub struct Renderer<'a> {
    pub sources: &'a SourceMap,
    pub painter: Painter,
}

impl<'a> Renderer<'a> {
    pub fn new(sources: &'a SourceMap, color: bool) -> Renderer<'a> {
        Renderer { sources, painter: Painter { enabled: color } }
    }

    pub fn render(&self, d: &Diagnostic) -> String {
        let mut out = String::new();
        self.render_into(d, &mut out);
        for n in &d.notes {
            self.render_into(n, &mut out);
        }
        out
    }

    fn render_into(&self, d: &Diagnostic, out: &mut String) {
        let p = &self.painter;
        let label = d.level.label();
        let mut msg = d.message.clone();
        if let Some(flag) = d.flag {
            if d.promoted {
                let _ = write!(msg, " [-Werror,-W{}]", flag.name());
            } else {
                let _ = write!(msg, " [-W{}]", flag.name());
            }
        }

        // Like Clang, the header names the caret position, not the range start.
        let header_span = match d.caret {
            Some(c) if !d.span.is_dummy() => Span::new(d.span.file, c, c),
            _ => d.span,
        };
        let loc = self.sources.loc(header_span);
        match &loc {
            Some(l) => {
                let _ = writeln!(
                    out,
                    "{} {}: {}",
                    p.bold(&format!("{}:{}:{}:", l.file, l.line, l.col)),
                    p.level(d.level, label),
                    p.bold(&msg)
                );
            }
            None => {
                let _ = writeln!(out, "{}: {}: {}", p.bold("cinder"), p.level(d.level, label), p.bold(&msg));
            }
        }
        if loc.is_some() {
            self.render_snippet(d, out);
        }
    }

    fn render_snippet(&self, d: &Diagnostic, out: &mut String) {
        let p = &self.painter;
        let file = self.sources.file(d.span.file);
        let caret_logical = d.caret.unwrap_or(d.span.lo);
        let caret_orig = file.orig_offset(caret_logical);
        let (line_no, _) = file.line_col_orig(caret_orig);
        let line = file.line_text(line_no);
        let line_start = file.line_start(line_no);
        let (display, cols) = expand_line(line);
        let to_col = |orig_off: u32| -> usize {
            let rel = (orig_off.saturating_sub(line_start)) as usize;
            cols[rel.min(line.len())]
        };

        let width = digits(line_no).max(4);
        let gutter_blank = format!("{:>w$} |", "", w = width);
        let _ = writeln!(out, "{} {}", p.dim(&format!("{:>w$} |", line_no, w = width)), display);

        // Underline: `~` for ranges on this line, `^` at the caret.
        let mut under: Vec<char> = vec![' '; display.chars().count() + 1];
        let mark = |lo: u32, hi: u32, under: &mut Vec<char>| {
            let (olo, ohi) = file.orig_range(lo, hi);
            if ohi <= line_start || olo > line_start + line.len() as u32 {
                return;
            }
            let c0 = to_col(olo.max(line_start));
            let c1 = to_col(ohi.min(line_start + line.len() as u32));
            for c in c0..c1.max(c0 + 1).min(under.len()) {
                if under[c] == ' ' {
                    under[c] = '~';
                }
            }
        };
        for &l in &d.labels {
            if l.file == d.span.file {
                mark(l.lo, l.hi, &mut under);
            }
        }
        if !d.span.is_empty() {
            mark(d.span.lo, d.span.hi, &mut under);
        }
        let cc = to_col(caret_orig);
        if cc < under.len() {
            under[cc] = '^';
        } else {
            under.resize(cc + 1, ' ');
            under[cc] = '^';
        }
        let mut under_s: String = under.into_iter().collect();
        while under_s.ends_with(' ') {
            under_s.pop();
        }
        let _ = writeln!(out, "{} {}", p.dim(&gutter_blank), p.caret(&under_s));

        if let Some(fx) = &d.fixit {
            if !fx.at.is_dummy() && fx.at.file == d.span.file {
                let fo = file.orig_offset(fx.at.lo);
                if fo >= line_start && fo <= line_start + line.len() as u32 {
                    let fc = to_col(fo);
                    let _ = writeln!(out, "{} {}{}", p.dim(&gutter_blank), " ".repeat(fc), p.paint("1;32", &fx.text));
                }
            }
        }
    }

    /// Render a whole run's diagnostics followed by the summary line.
    pub fn render_all(&self, diags: &[Diagnostic], summary: Option<String>) -> String {
        let mut s = String::new();
        for d in diags {
            s.push_str(&self.render(d));
        }
        if let Some(sum) = summary {
            s.push_str(&sum);
            s.push('\n');
        }
        s
    }
}

// ───────────────────────────── JSON ─────────────────────────────

pub fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

/// Machine-readable form used by the web playground.
pub fn to_json(d: &Diagnostic, sources: &SourceMap) -> String {
    let mut s = String::from("{");
    let _ = write!(s, "\"level\":\"{}\"", d.level.label());
    if let Some(f) = d.flag {
        let _ = write!(s, ",\"flag\":\"-W{}\"", f.name());
    }
    let _ = write!(s, ",\"message\":\"{}\"", json_escape(&d.message));
    if !d.span.is_dummy() {
        let file = sources.file(d.span.file);
        let (olo, ohi) = file.orig_range(d.span.lo, d.span.hi);
        let (l0, c0) = file.line_col_orig(file.orig_offset(d.caret.unwrap_or(d.span.lo)));
        let (l1, c1) = file.line_col_orig(ohi.max(olo));
        let _ = write!(
            s,
            ",\"file\":\"{}\",\"line\":{},\"col\":{},\"endLine\":{},\"endCol\":{}",
            json_escape(&file.name),
            l0,
            c0,
            l1,
            c1.max(c0)
        );
    }
    if !d.notes.is_empty() {
        s.push_str(",\"notes\":[");
        for (i, n) in d.notes.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&to_json(n, sources));
        }
        s.push(']');
    }
    s.push('}');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(src: &str) -> SourceMap {
        let mut sm = SourceMap::new();
        sm.add_file("t.c", None, src.to_string());
        sm
    }

    #[test]
    fn error_with_caret() {
        let sm = setup("int main() {\n    return y;\n}\n");
        let off = sm.text(0).find('y').unwrap() as u32;
        let d = Diagnostic::error(Span::new(0, off, off + 1), "use of undeclared identifier 'y'");
        let r = Renderer::new(&sm, false).render(&d);
        assert_eq!(r, "t.c:2:12: error: use of undeclared identifier 'y'\n   2 |     return y;\n     |            ^\n");
    }

    #[test]
    fn range_with_caret_in_the_middle() {
        let sm = setup("int z = a + b;\n");
        let t = sm.text(0);
        let a = t.find('a').unwrap() as u32;
        let plus = t.find('+').unwrap() as u32;
        let b = t.find('b').unwrap() as u32;
        let d = Diagnostic::error(Span::new(0, plus, plus + 1), "invalid operands")
            .with_label(Span::new(0, a, a + 1))
            .with_label(Span::new(0, b, b + 1));
        let r = Renderer::new(&sm, false).render(&d);
        assert!(r.contains("t.c:1:11: error: invalid operands"), "{r}");
        assert!(r.contains("   1 | int z = a + b;"), "{r}");
        assert!(r.contains("     |         ~ ^ ~"), "{r}");
    }

    #[test]
    fn tabs_are_expanded_and_caret_follows() {
        let sm = setup("\tint x = $;\n");
        let off = sm.text(0).find('$').unwrap() as u32;
        let d = Diagnostic::error(Span::new(0, off, off + 1), "stray '$'");
        let r = Renderer::new(&sm, false).render(&d);
        assert!(r.contains("   1 |     int x = $;"), "{r}");
        assert!(r.contains("     |             ^"), "{r}");
    }

    #[test]
    fn zero_width_span_with_fixit() {
        let sm = setup("int x = 1\nint y;\n");
        let end = sm.text(0).find('\n').unwrap() as u32;
        let d = Diagnostic::error(Span::new(0, end, end), "expected ';' after declaration")
            .with_fixit(Span::new(0, end, end), ";");
        let r = Renderer::new(&sm, false).render(&d);
        assert!(r.contains("t.c:1:10: error: expected ';' after declaration"), "{r}");
        assert!(r.contains("     |          ^"), "{r}");
        assert!(r.contains("     |          ;"), "{r}");
    }

    #[test]
    fn notes_render_after_primary() {
        let sm = setup("int x;\nint x;\n");
        let d = Diagnostic::error(Span::new(0, 11, 12), "redefinition of 'x'")
            .with_note(Span::new(0, 4, 5), "previous definition is here");
        let r = Renderer::new(&sm, false).render(&d);
        let e = r.find("error: redefinition").unwrap();
        let n = r.find("note: previous definition").unwrap();
        assert!(e < n);
        assert!(r.contains("t.c:1:5: note: previous definition is here"), "{r}");
    }

    #[test]
    fn color_wraps_with_ansi() {
        let sm = setup("x\n");
        let d = Diagnostic::error(Span::new(0, 0, 1), "boom");
        let r = Renderer::new(&sm, true).render(&d);
        assert!(r.contains("\x1b[1;31merror\x1b[0m"), "{r:?}");
        assert!(r.contains("\x1b[1;32m^\x1b[0m"), "{r:?}");
    }

    #[test]
    fn spans_without_location() {
        let sm = setup("");
        let d = Diagnostic::error(Span::DUMMY, "no input files");
        assert_eq!(Renderer::new(&sm, false).render(&d), "cinder: error: no input files\n");
    }

    #[test]
    fn warnings_follow_configuration() {
        let mut ctx = DiagCtx::new();
        // Off by default (group All), so dropped.
        ctx.warn(Warn::UnusedVariable, Span::DUMMY, "unused");
        assert_eq!(ctx.warning_count(), 0);
        ctx.config.apply_flag("all");
        ctx.warn(Warn::UnusedVariable, Span::DUMMY, "unused");
        assert_eq!(ctx.warning_count(), 1);
        ctx.config.apply_flag("no-unused-variable");
        ctx.warn(Warn::UnusedVariable, Span::new(0, 1, 2), "unused again");
        assert_eq!(ctx.warning_count(), 1);
    }

    #[test]
    fn werror_promotes() {
        let mut ctx = DiagCtx::new();
        ctx.config.apply_flag("error=return-type");
        ctx.warn(Warn::ReturnType, Span::DUMMY, "control reaches end");
        assert_eq!(ctx.error_count(), 1);
        assert_eq!(ctx.warning_count(), 0);
        let sm = SourceMap::new();
        let r = Renderer::new(&sm, false).render(&ctx.diagnostics()[0]);
        assert!(r.contains("error: control reaches end [-Werror,-Wreturn-type]"), "{r}");
    }

    #[test]
    fn error_limit_stops_output() {
        let mut ctx = DiagCtx::new();
        ctx.error_limit = 3;
        for i in 0..10 {
            ctx.error(Span::new(0, i, i + 1), format!("e{i}"));
        }
        assert!(ctx.is_fatal());
        assert_eq!(ctx.error_count(), 3);
        let last = ctx.diagnostics().last().unwrap();
        assert!(last.message.contains("too many errors"));
    }

    #[test]
    fn summary_text() {
        let mut ctx = DiagCtx::new();
        assert_eq!(ctx.summary(), None);
        ctx.error(Span::DUMMY, "a");
        assert_eq!(ctx.summary().unwrap(), "1 error generated.");
        ctx.config.apply_flag("all");
        ctx.warn(Warn::UnusedVariable, Span::DUMMY, "w");
        ctx.warn(Warn::UnusedVariable, Span::new(0, 0, 1), "w2");
        assert_eq!(ctx.summary().unwrap(), "2 warnings and 1 error generated.");
    }

    #[test]
    fn json_shape() {
        let sm = setup("int x = ;\n");
        let off = sm.text(0).find(';').unwrap() as u32;
        let d = Diagnostic::error(Span::new(0, off, off + 1), "expected expression");
        let j = to_json(&d, &sm);
        assert_eq!(
            j,
            "{\"level\":\"error\",\"message\":\"expected expression\",\"file\":\"t.c\",\"line\":1,\"col\":9,\"endLine\":1,\"endCol\":10}"
        );
    }

    #[test]
    fn flag_names_round_trip() {
        for &w in Warn::ALL {
            assert_eq!(Warn::from_name(w.name()), Some(w));
        }
        assert!(Warn::ALL.len() <= 64);
    }
}
