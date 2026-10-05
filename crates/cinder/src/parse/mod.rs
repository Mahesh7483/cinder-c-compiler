//! Hand-written recursive-descent parser for C11.
//!
//! The parser consumes the preprocessor's token stream and builds an
//! [`ast::TranslationUnit`]. It keeps a scoped table of typedef names (the
//! classic "lexer hack") so `T * x;` parses as a declaration when `T` names a
//! type and as an expression otherwise.
//!
//! Error recovery follows Clang's lead: a missing `;` or `)` is reported and
//! parsing continues as if it were present; otherwise the parser
//! resynchronizes at the next `;` / `}` so several independent errors are
//! reported in one run.

mod decl;
mod expr;
mod stmt;
#[cfg(test)]
mod tests;

use crate::ast::*;
use crate::diag::{Diagnostic, Warn};
use crate::intern::Symbol;
use crate::lex::{Punct, TokKind, Token};
use crate::literal;
use crate::session::Session;
use crate::source::Span;
use std::collections::HashMap;

/// `Err(())` means "an error was already reported; unwind to a recovery point".
pub(crate) type PResult<T> = Result<T, ()>;

macro_rules! keywords {
    ($($variant:ident => $($spell:literal),+;)*) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum Kw { $($variant),* }

        impl Kw {
            pub fn lookup(s: &str) -> Option<Kw> {
                match s { $($($spell)|+ => Some(Kw::$variant),)* _ => None }
            }

            pub fn spelling(self) -> &'static str {
                match self { $(Kw::$variant => keywords!(@first $($spell),+)),* }
            }
        }
    };
    (@first $a:literal $(, $rest:literal)*) => { $a };
}

keywords! {
    Auto => "auto";
    Break => "break";
    Case => "case";
    Char => "char";
    Const => "const", "__const", "__const__";
    Continue => "continue";
    Default => "default";
    Do => "do";
    Double => "double";
    Else => "else";
    Enum => "enum";
    Extern => "extern";
    Float => "float";
    For => "for";
    Goto => "goto";
    If => "if";
    Inline => "inline", "__inline", "__inline__";
    Int => "int";
    Long => "long";
    Register => "register";
    Restrict => "restrict", "__restrict", "__restrict__";
    Return => "return";
    Short => "short";
    Signed => "signed", "__signed", "__signed__";
    Sizeof => "sizeof";
    Static => "static";
    Struct => "struct";
    Switch => "switch";
    Typedef => "typedef";
    Union => "union";
    Unsigned => "unsigned";
    Void => "void";
    Volatile => "volatile", "__volatile", "__volatile__";
    While => "while";
    Alignas => "_Alignas";
    Alignof => "_Alignof", "__alignof", "__alignof__";
    Atomic => "_Atomic";
    Bool => "_Bool";
    Complex => "_Complex", "__complex__";
    Generic => "_Generic";
    Imaginary => "_Imaginary";
    Noreturn => "_Noreturn";
    StaticAssert => "_Static_assert";
    ThreadLocal => "_Thread_local", "__thread";
    Attribute => "__attribute__", "__attribute";
    Asm => "__asm__", "__asm", "asm";
    Extension => "__extension__";
    Typeof => "__typeof__", "__typeof", "typeof";
    BuiltinVaList => "__builtin_va_list";
    BuiltinVaArg => "__builtin_va_arg";
    BuiltinOffsetof => "__builtin_offsetof";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PKind {
    Ident(Symbol),
    Kw(Kw),
    Int(Symbol),
    Float(Symbol),
    Char(Symbol),
    Str(Symbol),
    Punct(Punct),
    Eof,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PTok {
    pub kind: PKind,
    pub span: Span,
}

pub struct Parser<'a> {
    pub(crate) sess: &'a mut Session,
    toks: Vec<PTok>,
    pos: usize,
    prev_span: Span,
    /// name -> "is a typedef name" for each open scope (innermost last).
    scopes: Vec<HashMap<Symbol, bool>>,
    /// `#pragma` text attached to the token index it precedes.
    pragmas: HashMap<usize, Vec<Symbol>>,
    pack_stack: Vec<Option<u32>>,
    pub(crate) cur_pack: Option<u32>,
    /// Token index of the last reported error (suppresses duplicate noise).
    last_error_pos: Option<usize>,
    /// `>0` while parsing inside a function body.
    pub(crate) in_function: u32,
    /// `#pragma omp` has been reported once already.
    warned_omp: bool,
}

/// Parse a preprocessed token stream.
pub fn parse(sess: &mut Session, toks: Vec<Token>) -> TranslationUnit {
    let mut p = Parser::new(sess, toks);
    p.parse_translation_unit()
}

impl<'a> Parser<'a> {
    pub fn new(sess: &'a mut Session, toks: Vec<Token>) -> Parser<'a> {
        let (ptoks, pragmas) = convert_tokens(sess, toks);
        let mut p = Parser {
            sess,
            toks: ptoks,
            pos: 0,
            prev_span: Span::DUMMY,
            scopes: vec![HashMap::new()],
            pragmas,
            pack_stack: Vec::new(),
            cur_pack: None,
            last_error_pos: None,
            in_function: 0,
            warned_omp: false,
        };
        p.apply_pragmas_at(0);
        p
    }

    // ───────────────────────────── token access ─────────────────────────────

    pub(crate) fn peek(&self) -> PTok {
        self.toks[self.pos.min(self.toks.len() - 1)]
    }

    pub(crate) fn peek_n(&self, n: usize) -> PTok {
        self.toks[(self.pos + n).min(self.toks.len() - 1)]
    }

    pub(crate) fn kind(&self) -> PKind {
        self.peek().kind
    }

    pub(crate) fn at_eof(&self) -> bool {
        matches!(self.kind(), PKind::Eof)
    }

    pub(crate) fn bump(&mut self) -> PTok {
        let t = self.peek();
        if !matches!(t.kind, PKind::Eof) {
            self.pos += 1;
            self.prev_span = t.span;
            self.apply_pragmas_at(self.pos);
        }
        t
    }

    pub(crate) fn at_punct(&self, p: Punct) -> bool {
        self.kind() == PKind::Punct(p)
    }

    pub(crate) fn at_kw(&self, k: Kw) -> bool {
        self.kind() == PKind::Kw(k)
    }

    pub(crate) fn eat_punct(&mut self, p: Punct) -> bool {
        if self.at_punct(p) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn eat_kw(&mut self, k: Kw) -> bool {
        if self.at_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn span(&self) -> Span {
        self.peek().span
    }

    pub(crate) fn prev_span(&self) -> Span {
        self.prev_span
    }

    /// Span from `start` through the last consumed token.
    pub(crate) fn span_from(&self, start: Span) -> Span {
        start.to(self.prev_span)
    }

    fn apply_pragmas_at(&mut self, idx: usize) {
        let Some(list) = self.pragmas.remove(&idx) else { return };
        let span = self.toks[idx.min(self.toks.len() - 1)].span;
        for text in list {
            self.apply_pragma(text.as_str(), span);
        }
    }

    fn apply_pragma(&mut self, text: &str, span: Span) {
        let mut words = text.split_whitespace();
        let first = words.next().unwrap_or("");
        let head = first.split('(').next().unwrap_or("");
        match head {
            "pack" => {
                // pack(n) | pack(push[, n]) | pack(pop) | pack()
                let inner =
                    text.split_once('(').and_then(|(_, r)| r.rsplit_once(')')).map(|(a, _)| a.trim()).unwrap_or("");
                let parts: Vec<&str> = inner.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                let num = |s: &str| s.parse::<u32>().ok().filter(|n| matches!(n, 1 | 2 | 4 | 8 | 16));
                match parts.as_slice() {
                    [] => self.cur_pack = None,
                    ["push"] => self.pack_stack.push(self.cur_pack),
                    ["push", n] => {
                        self.pack_stack.push(self.cur_pack);
                        self.cur_pack = num(n);
                    }
                    ["pop"] => {
                        if let Some(p) = self.pack_stack.pop() {
                            self.cur_pack = p;
                        }
                    }
                    [n] => self.cur_pack = num(n),
                    _ => {}
                }
            }
            "omp" => {
                // not implemented: say so once instead of silently running parallel code serially
                if !self.warned_omp {
                    self.warned_omp = true;
                    self.sess.diags.warn(
                        Warn::UnknownPragmas,
                        span,
                        "'#pragma omp' is ignored: OpenMP is not implemented, parallel regions run on one thread",
                    );
                }
            }
            "STDC" | "GCC" | "clang" | "message" | "warning" | "once" | "weak" | "comment" => {}
            _ => {
                self.sess.diags.warn(Warn::UnknownPragmas, span, format!("unknown pragma ignored: '{}'", first));
            }
        }
    }

    // ───────────────────────────── scopes ─────────────────────────────

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub(crate) fn declare(&mut self, name: Symbol, is_typedef: bool) {
        self.scopes.last_mut().expect("scope").insert(name, is_typedef);
    }

    pub(crate) fn is_typedef_name(&self, name: Symbol) -> bool {
        for s in self.scopes.iter().rev() {
            if let Some(&t) = s.get(&name) {
                return t;
            }
        }
        false
    }

    // ───────────────────────────── diagnostics ─────────────────────────────

    /// Report an error at the current token unless one was already reported
    /// at this exact position (avoids cascades).
    pub(crate) fn error_here(&mut self, msg: impl Into<String>) -> PResult<()> {
        let sp = self.span();
        self.error_at(sp, msg)
    }

    pub(crate) fn error_at(&mut self, span: Span, msg: impl Into<String>) -> PResult<()> {
        self.emit(Diagnostic::error(span, msg))
    }

    pub(crate) fn emit(&mut self, d: Diagnostic) -> PResult<()> {
        if self.last_error_pos != Some(self.pos) {
            self.sess.diags.emit(d);
            self.last_error_pos = Some(self.pos);
        }
        Err(())
    }

    /// Error positioned just after the previous token (where a missing
    /// `;` / `)` would go), with a fix-it hint.
    pub(crate) fn error_after_prev(&mut self, msg: impl Into<String>, insert: &str) -> PResult<()> {
        let at = self.prev_span.end();
        let d = Diagnostic::error(at, msg).with_fixit(at, insert);
        self.emit(d)
    }

    /// Expect a `;`. If it is missing, report it and carry on as though it
    /// were there (the next token almost always starts the next construct).
    pub(crate) fn expect_semi(&mut self, what: &str) {
        if !self.eat_punct(Punct::Semi) {
            let _ = self.error_after_prev(format!("expected ';' {}", what), ";");
        }
    }

    /// Expect a closing delimiter, noting the opener on failure.
    pub(crate) fn expect_close(&mut self, close: Punct, open_span: Span) -> PResult<()> {
        if self.eat_punct(close) {
            return Ok(());
        }
        let open = match close {
            Punct::RParen => "(",
            Punct::RBracket => "[",
            _ => "{",
        };
        let at = self.prev_span.end();
        let sp = if matches!(self.kind(), PKind::Eof) { self.span() } else { at };
        let d = Diagnostic::error(sp, format!("expected '{}'", close.spelling()))
            .with_fixit(sp, close.spelling())
            .with_note(open_span, format!("to match this '{}'", open));
        let _ = self.emit(d);
        // Treat as present; callers continue with whatever follows.
        Ok(())
    }

    pub(crate) fn expect_ident(&mut self, what: &str) -> PResult<Ident> {
        match self.kind() {
            PKind::Ident(name) => {
                let t = self.bump();
                Ok(Ident { name, span: t.span })
            }
            _ => {
                self.error_here(format!("expected {}", what))?;
                unreachable!()
            }
        }
    }

    // ───────────────────────────── recovery ─────────────────────────────

    /// Skip to the end of the current statement/declaration: past a `;` at
    /// nesting depth 0, or up to (not including) an unmatched `}`.
    pub(crate) fn sync_stmt(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.kind() {
                PKind::Eof => return,
                PKind::Punct(Punct::Semi) if depth == 0 => {
                    self.bump();
                    return;
                }
                PKind::Punct(Punct::LBrace | Punct::LParen | Punct::LBracket) => {
                    depth += 1;
                    self.bump();
                }
                PKind::Punct(Punct::RBrace) => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return;
                    }
                }
                PKind::Punct(Punct::RParen | Punct::RBracket) => {
                    depth = (depth - 1).max(0);
                    self.bump();
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// Skip a damaged top-level declaration.
    fn sync_top_level(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.kind() {
                PKind::Eof => return,
                PKind::Punct(Punct::Semi) if depth == 0 => {
                    self.bump();
                    return;
                }
                PKind::Punct(Punct::LBrace) => {
                    depth += 1;
                    self.bump();
                }
                PKind::Punct(Punct::RBrace) => {
                    self.bump();
                    depth -= 1;
                    if depth <= 0 {
                        // Consume an optional trailing ';' (`struct S { ... };`).
                        self.eat_punct(Punct::Semi);
                        return;
                    }
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    // ───────────────────────────── top level ─────────────────────────────

    pub fn parse_translation_unit(&mut self) -> TranslationUnit {
        let mut tu = TranslationUnit::default();
        while !self.at_eof() {
            if self.sess.diags.is_fatal() {
                break;
            }
            let before = self.pos;
            match self.parse_external_decl() {
                Ok(Some(d)) => tu.decls.push(d),
                Ok(None) => {}
                Err(()) => self.sync_top_level(),
            }
            if self.pos == before {
                // Guarantee progress on malformed input.
                self.bump();
            }
        }
        tu
    }

    fn parse_external_decl(&mut self) -> PResult<Option<ExternalDecl>> {
        match self.kind() {
            PKind::Punct(Punct::Semi) => {
                self.bump();
                Ok(None)
            }
            PKind::Punct(Punct::RBrace) => {
                self.error_here("extraneous closing brace ('}')")?;
                Ok(None)
            }
            PKind::Kw(Kw::StaticAssert) => Ok(Some(ExternalDecl::StaticAssert(self.parse_static_assert()?))),
            PKind::Kw(Kw::Asm) => {
                self.error_here("not yet supported: top-level inline assembly")?;
                Ok(None)
            }
            _ => self.parse_declaration_or_function(),
        }
    }
}

// ───────────────────────────── token conversion ─────────────────────────────

fn convert_tokens(sess: &mut Session, toks: Vec<Token>) -> (Vec<PTok>, HashMap<usize, Vec<Symbol>>) {
    let mut out: Vec<PTok> = Vec::with_capacity(toks.len());
    let mut pragmas: HashMap<usize, Vec<Symbol>> = HashMap::new();
    let mut last_span = Span::DUMMY;
    for t in toks {
        let span = t.span;
        let kind = match t.kind {
            TokKind::Ident(s) => match Kw::lookup(s.as_str()) {
                Some(k) => PKind::Kw(k),
                None => PKind::Ident(s),
            },
            TokKind::Number(s) => {
                if literal::is_float_spelling(s.as_str()) {
                    PKind::Float(s)
                } else {
                    PKind::Int(s)
                }
            }
            TokKind::Char(s) => PKind::Char(s),
            TokKind::Str(s) => PKind::Str(s),
            TokKind::Punct(p) => PKind::Punct(p),
            TokKind::HeaderAngle(_) => {
                sess.diags.error(span, "unexpected header name");
                continue;
            }
            TokKind::BadLiteral(s) => {
                let q = if s.as_str().ends_with('\'') || s.as_str().contains('\'') && !s.as_str().contains('"') {
                    '\''
                } else {
                    '"'
                };
                sess.diags.error(span, format!("missing terminating {} character", q));
                // Substitute an empty literal so parsing can continue.
                PKind::Str(Symbol::new("\"\""))
            }
            TokKind::Other(c) => {
                sess.diags.error(span, format!("stray '{}' in program", c));
                continue;
            }
            TokKind::Pragma(s) => {
                pragmas.entry(out.len()).or_default().push(s);
                continue;
            }
            TokKind::Eof => PKind::Eof,
        };
        last_span = span;
        out.push(PTok { kind, span });
    }
    if !matches!(out.last().map(|t| t.kind), Some(PKind::Eof)) {
        out.push(PTok { kind: PKind::Eof, span: last_span.end() });
    }
    (out, pragmas)
}
