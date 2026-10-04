//! The C preprocessor.
//!
//! Tokens are read from a stack of file frames plus a `pending` stack of
//! tokens produced by macro expansion (re-scanned before the file continues).
//! Macro expansion follows Prosser's hide-set algorithm, so recursive and
//! mutually recursive macros terminate exactly as the standard requires.
//!
//! Supported: `#include` (quoted, angled, computed, `__has_include`), object
//! and function-like `#define` (including `#`, `##`, variadics, GNU named
//! variadics, `, ## __VA_ARGS__`, `__VA_OPT__`), `#undef`, the whole
//! `#if`/`#ifdef`/`#ifndef`/`#elif`/`#else`/`#endif` family with `defined`,
//! `#error`, `#warning`, `#pragma` (`once` handled here, others forwarded),
//! `_Pragma`, and the predefined macros `__FILE__`, `__LINE__`, `__COUNTER__`,
//! `__INCLUDE_LEVEL__`, `__DATE__`, `__TIME__`.
//!
//! Known limitation: `#line` is accepted but does not renumber anything.

pub mod expr;
pub mod output;

use crate::diag::{Diagnostic, Warn};
use crate::headers;
use crate::intern::Symbol;
use crate::lex::{self, HideSet, Punct, TokKind, Token, BOL, SPACE};
use crate::session::Session;
use crate::source::{FileId, Span};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

const MAX_INCLUDE_DEPTH: usize = 200;

#[derive(Clone, Debug)]
pub enum MacroCmd {
    /// `-DNAME`, `-DNAME=value`, `-D'F(x)=x'`
    Define(String),
    /// `-UNAME`
    Undef(String),
}

#[derive(Clone, Debug, Default)]
pub struct PpOptions {
    /// `-I` directories (searched for both `"..."` and `<...>`).
    pub include_dirs: Vec<PathBuf>,
    /// Extra real system directories, searched after the bundled headers.
    pub system_dirs: Vec<PathBuf>,
    /// `-nostdinc`: do not use the bundled headers.
    pub no_bundled_headers: bool,
    /// `--restrict-includes`: no absolute paths, no `..`, no `-I`/system directories.
    pub restrict_includes: bool,
    pub defines: Vec<MacroCmd>,
    pub opt_level: u8,
    /// Work budget for macro expansion (tokens produced); `None` = [`DEFAULT_EXPANSION_BUDGET`].
    pub max_expansion_tokens: Option<u64>,
}

/// Far beyond any real translation unit, yet small enough that an exponential macro
/// (`#define B(x) A(A(x))` ...) fails fast with a diagnostic instead of eating memory.
pub const DEFAULT_EXPANSION_BUDGET: u64 = 1_000_000;

#[derive(Debug)]
enum MacroKind {
    Object,
    Function { params: Vec<Symbol>, variadic: bool },
}

#[derive(Debug)]
struct Macro {
    name: Symbol,
    kind: MacroKind,
    body: Vec<Token>,
    def_span: Span,
}

struct Cond {
    span: Span,
    /// Tokens in the current group are being emitted.
    active: bool,
    /// Some group of this `#if` chain has already been taken.
    taken: bool,
    seen_else: bool,
}

struct Frame {
    file: FileId,
    toks: Rc<Vec<Token>>,
    pos: usize,
    conds: Vec<Cond>,
}

pub struct Preprocessor<'s> {
    sess: &'s mut Session,
    opts: PpOptions,
    macros: HashMap<Symbol, Rc<Macro>>,
    frames: Vec<Frame>,
    /// Reversed stack: the last element is the next token to read.
    pending: Vec<Token>,
    /// >0 while expanding an isolated token list (macro arguments, `#if`).
    isolated: u32,
    out: Vec<Token>,
    once: HashSet<String>,
    file_cache: HashMap<String, FileId>,
    tok_cache: HashMap<FileId, Rc<Vec<Token>>>,
    counter: u32,
    /// Includes currently open (excluding the prelude).
    depth: usize,
    /// Tokens produced by macro expansion so far, and whether the budget ran out.
    expanded: u64,
    overflowed: bool,
}

struct Resolved {
    key: String,
    file: FileId,
}

// ───────────────────────────── hide-set helpers ─────────────────────────────

fn hs_contains(t: &Token, name: Symbol) -> bool {
    t.hide.as_ref().is_some_and(|h| h.contains(&name))
}

fn hs_union(a: &Option<Rc<HideSet>>, b: &Rc<HideSet>) -> Rc<HideSet> {
    match a {
        None => b.clone(),
        Some(a) if a.is_empty() => b.clone(),
        Some(a) => {
            let mut s: HideSet = (**a).clone();
            s.extend(b.iter().copied());
            Rc::new(s)
        }
    }
}

fn hs_intersect(a: &Option<Rc<HideSet>>, b: &Option<Rc<HideSet>>) -> Option<Rc<HideSet>> {
    match (a, b) {
        (Some(a), Some(b)) => {
            let s: HideSet = a.intersection(b).copied().collect();
            if s.is_empty() {
                None
            } else {
                Some(Rc::new(s))
            }
        }
        _ => None,
    }
}

fn single(name: Symbol) -> Rc<HideSet> {
    let mut s = HideSet::new();
    s.insert(name);
    Rc::new(s)
}

// ───────────────────────────── entry points ─────────────────────────────

/// Preprocess `main`, returning the token stream terminated by an `Eof` token.
pub fn preprocess(sess: &mut Session, main: FileId, opts: &PpOptions) -> Vec<Token> {
    let mut pp = Preprocessor::new(sess, opts.clone());
    pp.run(main)
}

impl<'s> Preprocessor<'s> {
    pub fn new(sess: &'s mut Session, opts: PpOptions) -> Preprocessor<'s> {
        Preprocessor {
            sess,
            opts,
            macros: HashMap::new(),
            frames: Vec::new(),
            pending: Vec::new(),
            isolated: 0,
            out: Vec::new(),
            once: HashSet::new(),
            file_cache: HashMap::new(),
            tok_cache: HashMap::new(),
            counter: 0,
            depth: 0,
            expanded: 0,
            overflowed: false,
        }
    }

    /// Account for `n` tokens of macro-expansion work; once the budget is spent, report it once
    /// and make every further macro expand to nothing.
    fn charge(&mut self, n: usize, span: Span) {
        self.expanded = self.expanded.saturating_add(n as u64);
        let limit = self.opts.max_expansion_tokens.unwrap_or(DEFAULT_EXPANSION_BUDGET);
        if self.expanded > limit && !self.overflowed {
            self.overflowed = true;
            self.sess.diags.emit(Diagnostic::error(
                span,
                format!(
                    "macro expansion produced too many tokens (limit {}); is a macro expanding exponentially?",
                    limit
                ),
            ));
        }
    }

    fn run(&mut self, main: FileId) -> Vec<Token> {
        let main_frame = self.frame_for(main);
        // The prelude frame sits on top so predefined/-D macros are defined first.
        let prelude = self.build_prelude();
        let prelude_id = self.sess.sources.add_file("<built-in>", None, prelude);
        self.frames.push(main_frame);
        let pf = self.frame_for(prelude_id);
        self.frames.push(pf);
        self.main_loop();
        let eof_span = {
            let f = self.sess.sources.file(main);
            Span::new(main, f.text.len() as u32, f.text.len() as u32)
        };
        let mut out = std::mem::take(&mut self.out);
        out.push(Token::new(TokKind::Eof, eof_span));
        out
    }

    fn build_prelude(&self) -> String {
        let mut s = String::new();
        for (name, val) in predefined_macros(self.opts.opt_level) {
            s.push_str(&format!("#define {} {}\n", name, val));
        }
        for cmd in &self.opts.defines {
            match cmd {
                MacroCmd::Define(spec) => {
                    // `NAME` -> 1; `NAME=body` -> body; `F(x)=body` -> function-like
                    let (lhs, rhs) = match spec.find('=') {
                        Some(i) => (&spec[..i], &spec[i + 1..]),
                        None => (spec.as_str(), "1"),
                    };
                    s.push_str(&format!("#define {} {}\n", lhs, rhs));
                }
                MacroCmd::Undef(name) => s.push_str(&format!("#undef {}\n", name)),
            }
        }
        s
    }

    // ───────────────────────────── token streams ─────────────────────────────

    fn frame_for(&mut self, file: FileId) -> Frame {
        let toks = match self.tok_cache.get(&file) {
            Some(t) => t.clone(),
            None => {
                let text = self.sess.sources.text(file).to_string();
                let t = Rc::new(lex::lex(file, &text, &mut self.sess.diags));
                self.tok_cache.insert(file, t.clone());
                t
            }
        };
        Frame { file, toks, pos: 0, conds: Vec::new() }
    }

    /// Next token from the pending stack, else the current file. The bool is
    /// true when the token came straight from a file (so `#` may be a directive).
    fn read_raw(&mut self) -> Option<(Token, bool)> {
        if let Some(t) = self.pending.pop() {
            return Some((t, false));
        }
        if self.isolated > 0 {
            return None;
        }
        loop {
            let f = self.frames.last_mut()?;
            if f.pos < f.toks.len() {
                let t = f.toks[f.pos].clone();
                f.pos += 1;
                return Some((t, true));
            }
            self.end_frame();
        }
    }

    fn end_frame(&mut self) {
        let f = self.frames.pop().expect("frame");
        for c in &f.conds {
            self.sess.diags.emit(Diagnostic::error(c.span, "unterminated conditional directive"));
        }
        // `depth` counts real includes: the prelude and main file are not nested.
        if !self.frames.is_empty() && self.depth > 0 {
            self.depth -= 1;
        }
    }

    fn peek_next(&self) -> Option<&Token> {
        if let Some(t) = self.pending.last() {
            return Some(t);
        }
        if self.isolated > 0 {
            return None;
        }
        let f = self.frames.last()?;
        f.toks.get(f.pos)
    }

    /// Next token while collecting macro arguments; never crosses a frame end.
    fn next_arg_token(&mut self) -> Option<Token> {
        if let Some(t) = self.pending.pop() {
            return Some(t);
        }
        if self.isolated > 0 {
            return None;
        }
        let f = self.frames.last_mut()?;
        if f.pos < f.toks.len() {
            let t = f.toks[f.pos].clone();
            f.pos += 1;
            Some(t)
        } else {
            None
        }
    }

    fn skipping(&self) -> bool {
        self.frames.last().is_some_and(|f| f.conds.last().is_some_and(|c| !c.active))
    }

    fn fatal(&self) -> bool {
        self.sess.diags.is_fatal()
    }

    // ───────────────────────────── main loop ─────────────────────────────

    fn main_loop(&mut self) {
        while let Some((tok, from_file)) = self.read_raw() {
            if self.fatal() {
                break;
            }
            if from_file && tok.bol() && tok.is_punct(Punct::Hash) {
                self.directive(tok);
                continue;
            }
            if self.skipping() {
                continue;
            }
            match tok.kind {
                TokKind::Ident(sym) => {
                    if sym.as_str() == "_Pragma" && self.try_pragma_operator(&tok) {
                        continue;
                    }
                    if self.try_expand(&tok) {
                        continue;
                    }
                    self.out.push(tok);
                }
                TokKind::Eof => {}
                _ => self.out.push(tok),
            }
        }
    }

    // ───────────────────────────── directives ─────────────────────────────

    fn read_directive_line(&mut self) -> Vec<Token> {
        let mut line = Vec::new();
        if let Some(f) = self.frames.last_mut() {
            while f.pos < f.toks.len() && !f.toks[f.pos].bol() {
                line.push(f.toks[f.pos].clone());
                f.pos += 1;
            }
        }
        line
    }

    fn directive(&mut self, hash: Token) {
        let line = self.read_directive_line();
        let Some(first) = line.first() else { return }; // null directive
        let span = hash.span.to(first.span);
        let name = match first.kind {
            TokKind::Ident(s) => s.as_str(),
            // `# 33 "file"` linemarkers
            TokKind::Number(_) => return,
            _ => {
                if !self.skipping() {
                    self.sess
                        .diags
                        .error(first.span, format!("invalid preprocessing directive '{}'", first.spelling()));
                }
                return;
            }
        };
        let rest = &line[1..];

        // Conditional directives are processed even in skipped groups.
        match name {
            "if" => return self.d_if(span, rest),
            "ifdef" => return self.d_ifdef(span, rest, false),
            "ifndef" => return self.d_ifdef(span, rest, true),
            "elif" => return self.d_elif(span, rest),
            "else" => return self.d_else(span, rest),
            "endif" => return self.d_endif(span, rest),
            _ => {}
        }
        if self.skipping() {
            return;
        }
        match name {
            "define" => self.d_define(first.span, rest),
            "undef" => self.d_undef(first.span, rest),
            "include" | "include_next" => self.d_include(span, rest, false),
            "import" => self.d_include(span, rest, true),
            "error" => {
                let msg = join_tokens(rest);
                self.sess.diags.error(first.span, format!("#error {}", msg).trim_end().to_string());
            }
            "warning" => {
                let msg = join_tokens(rest);
                self.sess.diags.warn(Warn::PpWarnings, first.span, format!("#warning {}", msg).trim_end().to_string());
            }
            "pragma" => self.d_pragma(span, rest),
            "line" | "ident" | "sccs" => {}
            _ => {
                self.sess.diags.error(first.span, format!("invalid preprocessing directive '#{}'", name));
            }
        }
    }

    fn warn_extra(&mut self, name: &str, rest: &[Token]) {
        if let Some(t) = rest.first() {
            self.sess.diags.warn(Warn::ExtraTokens, t.span, format!("extra tokens at end of #{} directive", name));
        }
    }

    fn d_if(&mut self, span: Span, rest: &[Token]) {
        if self.skipping() {
            self.push_cond(Cond { span, active: false, taken: true, seen_else: false });
            return;
        }
        let v = self.eval_if(rest, span);
        self.push_cond(Cond { span, active: v, taken: v, seen_else: false });
    }

    fn d_ifdef(&mut self, span: Span, rest: &[Token], negate: bool) {
        if self.skipping() {
            self.push_cond(Cond { span, active: false, taken: true, seen_else: false });
            return;
        }
        let name = if negate { "ifndef" } else { "ifdef" };
        let v = match rest.first().map(|t| t.kind) {
            Some(TokKind::Ident(s)) => {
                self.warn_extra(name, &rest[1..]);
                self.macros.contains_key(&s) != negate
            }
            Some(_) => {
                self.sess.diags.error(rest[0].span, "macro name must be an identifier");
                false
            }
            None => {
                self.sess.diags.error(span, "macro name missing");
                false
            }
        };
        self.push_cond(Cond { span, active: v, taken: v, seen_else: false });
    }

    fn push_cond(&mut self, c: Cond) {
        if let Some(f) = self.frames.last_mut() {
            f.conds.push(c);
        }
    }

    fn d_elif(&mut self, span: Span, rest: &[Token]) {
        let parent_active = {
            let Some(f) = self.frames.last() else { return };
            if f.conds.is_empty() {
                self.sess.diags.error(span, "#elif without #if");
                return;
            }
            // The enclosing group is active iff the cond below this one is.
            f.conds.len() < 2 || f.conds[f.conds.len() - 2].active
        };
        let (seen_else, taken) = {
            let c = self.frames.last().unwrap().conds.last().unwrap();
            (c.seen_else, c.taken)
        };
        if seen_else {
            self.sess.diags.error(span, "#elif after #else");
            return;
        }
        let new_active = if parent_active && !taken { self.eval_if(rest, span) } else { false };
        let c = self.frames.last_mut().unwrap().conds.last_mut().unwrap();
        c.active = new_active;
        if new_active {
            c.taken = true;
        }
    }

    fn d_else(&mut self, span: Span, rest: &[Token]) {
        let Some(f) = self.frames.last_mut() else { return };
        let Some(c) = f.conds.last_mut() else {
            self.sess.diags.error(span, "#else without #if");
            return;
        };
        if c.seen_else {
            self.sess.diags.error(span, "#else after #else");
            return;
        }
        c.seen_else = true;
        let parent_active = f.conds.len() < 2 || f.conds[f.conds.len() - 2].active;
        let c = f.conds.last_mut().unwrap();
        c.active = parent_active && !c.taken;
        if c.active {
            c.taken = true;
        }
        self.warn_extra("else", rest);
    }

    fn d_endif(&mut self, span: Span, rest: &[Token]) {
        let Some(f) = self.frames.last_mut() else { return };
        if f.conds.pop().is_none() {
            self.sess.diags.error(span, "#endif without #if");
            return;
        }
        self.warn_extra("endif", rest);
    }

    /// Evaluate an `#if` expression: resolve `defined` and `__has_*`, expand
    /// macros, then evaluate.
    fn eval_if(&mut self, toks: &[Token], span: Span) -> bool {
        let mut resolved: Vec<Token> = Vec::with_capacity(toks.len());
        let mut i = 0;
        while i < toks.len() {
            let t = &toks[i];
            if let TokKind::Ident(s) = t.kind {
                match s.as_str() {
                    "defined" => {
                        // defined X | defined ( X )
                        let (name, adv) = match (toks.get(i + 1), toks.get(i + 2), toks.get(i + 3)) {
                            (Some(a), Some(b), Some(c)) if a.is_punct(Punct::LParen) && c.is_punct(Punct::RParen) => {
                                (b.ident(), 4)
                            }
                            (Some(a), _, _) if a.ident().is_some() => (a.ident(), 2),
                            _ => (None, 1),
                        };
                        match name {
                            Some(n) => {
                                let v = self.macros.contains_key(&n) || is_builtin_macro(n.as_str());
                                resolved.push(num_token(v as u64, t.span));
                                i += adv;
                                continue;
                            }
                            None => {
                                self.sess.diags.error(t.span, "macro name must be an identifier after 'defined'");
                                return false;
                            }
                        }
                    }
                    "__has_include" | "__has_include_next" => {
                        let Some(close) = matching_paren(toks, i + 1) else {
                            self.sess.diags.error(t.span, format!("missing '(' after '{}'", s));
                            return false;
                        };
                        let inner = toks[i + 2..close].to_vec();
                        let found = match self.parse_include_operand(inner, t.span) {
                            Some((name, angle)) => self.resolve_include(&name, angle, span.file).is_some(),
                            None => false,
                        };
                        resolved.push(num_token(found as u64, t.span));
                        i = close + 1;
                        continue;
                    }
                    "__has_builtin"
                    | "__has_attribute"
                    | "__has_feature"
                    | "__has_extension"
                    | "__has_cpp_attribute"
                    | "__has_warning" => {
                        let Some(close) = matching_paren(toks, i + 1) else {
                            self.sess.diags.error(t.span, format!("missing '(' after '{}'", s));
                            return false;
                        };
                        let arg = toks.get(i + 2).and_then(|t| t.ident());
                        let v = match (s.as_str(), arg) {
                            ("__has_builtin", Some(a)) => crate::headers::is_known_builtin(a.as_str()),
                            _ => false,
                        };
                        resolved.push(num_token(v as u64, t.span));
                        i = close + 1;
                        continue;
                    }
                    _ => {}
                }
            }
            resolved.push(t.clone());
            i += 1;
        }
        let expanded = self.expand_list(resolved);
        expr::eval_condition(self.sess, &expanded, span)
    }

    // ───────────────────────────── #define / #undef ─────────────────────────────

    fn d_define(&mut self, dir_span: Span, rest: &[Token]) {
        let Some(name_tok) = rest.first() else {
            self.sess.diags.error(dir_span, "macro name missing");
            return;
        };
        let TokKind::Ident(name) = name_tok.kind else {
            self.sess.diags.error(name_tok.span, "macro name must be an identifier");
            return;
        };
        if name.as_str() == "defined" {
            self.sess.diags.error(name_tok.span, "'defined' cannot be used as a macro name");
            return;
        }
        let mut idx = 1;
        let mut kind = MacroKind::Object;
        if rest.get(1).is_some_and(|t| t.is_punct(Punct::LParen) && !t.space()) {
            // function-like: parse parameter list
            idx = 2;
            let mut params: Vec<Symbol> = Vec::new();
            let mut variadic = false;
            let mut first = true;
            loop {
                let Some(t) = rest.get(idx) else {
                    self.sess.diags.error(name_tok.span, "missing ')' in macro parameter list");
                    return;
                };
                if first && t.is_punct(Punct::RParen) {
                    idx += 1;
                    break;
                }
                match t.kind {
                    TokKind::Ident(p) => {
                        if params.contains(&p) {
                            self.sess.diags.error(t.span, format!("duplicate macro parameter '{}'", p));
                            return;
                        }
                        params.push(p);
                        idx += 1;
                        // GNU named variadic: `args...`
                        if rest.get(idx).is_some_and(|t| t.is_punct(Punct::Ellipsis)) {
                            variadic = true;
                            idx += 1;
                            match rest.get(idx) {
                                Some(t) if t.is_punct(Punct::RParen) => {
                                    idx += 1;
                                    break;
                                }
                                _ => {
                                    self.sess.diags.error(
                                        rest.get(idx).map(|t| t.span).unwrap_or(t.span),
                                        "missing ')' in macro parameter list",
                                    );
                                    return;
                                }
                            }
                        }
                    }
                    TokKind::Punct(Punct::Ellipsis) => {
                        params.push(Symbol::new("__VA_ARGS__"));
                        variadic = true;
                        idx += 1;
                        match rest.get(idx) {
                            Some(t) if t.is_punct(Punct::RParen) => {
                                idx += 1;
                                break;
                            }
                            _ => {
                                self.sess.diags.error(t.span, "missing ')' after '...' in macro parameter list");
                                return;
                            }
                        }
                    }
                    _ => {
                        self.sess.diags.error(t.span, "invalid token in macro parameter list");
                        return;
                    }
                }
                first = false;
                match rest.get(idx) {
                    Some(t) if t.is_punct(Punct::Comma) => idx += 1,
                    Some(t) if t.is_punct(Punct::RParen) => {
                        idx += 1;
                        break;
                    }
                    Some(t) => {
                        self.sess.diags.error(t.span, "expected ',' or ')' in macro parameter list");
                        return;
                    }
                    None => {
                        self.sess.diags.error(name_tok.span, "missing ')' in macro parameter list");
                        return;
                    }
                }
            }
            kind = MacroKind::Function { params, variadic };
        }

        let mut body: Vec<Token> = rest[idx..]
            .iter()
            .cloned()
            .map(|mut t| {
                t.flags &= !BOL;
                t
            })
            .collect();
        if let Some(f) = body.first_mut() {
            f.flags &= !SPACE;
        }
        // `#` must be followed by a parameter in function-like macros.
        if let MacroKind::Function { params, .. } = &kind {
            for (k, t) in body.iter().enumerate() {
                if t.is_punct(Punct::Hash) {
                    let ok = body.get(k + 1).and_then(|n| n.ident()).is_some_and(|n| params.contains(&n));
                    if !ok {
                        self.sess.diags.error(t.span, "'#' is not followed by a macro parameter");
                        return;
                    }
                }
            }
        }
        if let (Some(a), Some(b)) = (body.first(), body.last()) {
            if a.is_punct(Punct::HashHash) || b.is_punct(Punct::HashHash) {
                let bad = if a.is_punct(Punct::HashHash) { a } else { b };
                self.sess.diags.error(bad.span, "'##' cannot appear at either end of a macro expansion");
                return;
            }
        }

        let new = Macro { name, kind, body, def_span: name_tok.span };
        if let Some(old) = self.macros.get(&name) {
            if !macros_equal(old, &new) {
                self.sess.diags.emit(
                    Diagnostic::warning(Warn::MacroRedefined, name_tok.span, format!("'{}' macro redefined", name))
                        .with_note(old.def_span, "previous definition is here"),
                );
            }
        }
        self.macros.insert(name, Rc::new(new));
    }

    fn d_undef(&mut self, dir_span: Span, rest: &[Token]) {
        match rest.first().map(|t| (t.kind, t.span)) {
            Some((TokKind::Ident(s), _)) => {
                self.macros.remove(&s);
                self.warn_extra("undef", &rest[1..]);
            }
            Some((_, sp)) => self.sess.diags.error(sp, "macro name must be an identifier"),
            None => self.sess.diags.error(dir_span, "macro name missing"),
        }
    }

    // ───────────────────────────── #include ─────────────────────────────

    /// Turn the operand of `#include`/`__has_include` into (name, angled).
    fn parse_include_operand(&mut self, toks: Vec<Token>, span: Span) -> Option<(String, bool)> {
        let direct = |t: &Token| -> Option<(String, bool)> {
            match t.kind {
                TokKind::HeaderAngle(s) => Some((s.as_str().to_string(), true)),
                TokKind::Str(s) => {
                    let text = s.as_str();
                    if text.starts_with('"') {
                        Some((text[1..text.len() - 1].to_string(), false))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        if let Some(first) = toks.first() {
            if let Some(r) = direct(first) {
                return Some(r);
            }
        }
        // Computed include: macro-expand, then re-read.
        let exp = self.expand_list(toks);
        if let Some(first) = exp.first() {
            if let Some(r) = direct(first) {
                return Some(r);
            }
            if first.is_punct(Punct::Lt) {
                let mut name = String::new();
                for t in &exp[1..] {
                    if t.is_punct(Punct::Gt) {
                        return Some((name, true));
                    }
                    if t.space() && !name.is_empty() {
                        name.push(' ');
                    }
                    name.push_str(&t.spelling());
                }
            }
        }
        self.sess.diags.error(span, "expected \"FILENAME\" or <FILENAME>");
        None
    }

    fn d_include(&mut self, span: Span, rest: &[Token], import: bool) {
        let Some((name, angle)) = self.parse_include_operand(rest.to_vec(), span) else { return };
        let cur = self.frames.last().map(|f| f.file).unwrap_or(0);
        let Some(res) = self.resolve_include(&name, angle, cur) else {
            self.sess.diags.emit(Diagnostic::fatal(
                rest.first().map(|t| t.span).unwrap_or(span),
                format!("'{}' file not found", name),
            ));
            return;
        };
        if self.once.contains(&res.key) {
            return;
        }
        if import {
            self.once.insert(res.key.clone());
        }
        if self.depth >= MAX_INCLUDE_DEPTH {
            self.sess.diags.emit(Diagnostic::fatal(span, "#include nested too deeply"));
            return;
        }
        let frame = self.frame_for(res.file);
        self.frames.push(frame);
        self.depth += 1;
    }

    fn read_real_file(&mut self, path: &Path) -> Option<Resolved> {
        let bytes = std::fs::read(path).ok()?;
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().to_string();
        if let Some(&id) = self.file_cache.get(&key) {
            return Some(Resolved { key, file: id });
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let display = path.to_string_lossy().replace('\\', "/");
        let id = self.sess.sources.add_file(display, Some(path.to_path_buf()), text);
        self.file_cache.insert(key.clone(), id);
        Some(Resolved { key, file: id })
    }

    fn bundled(&mut self, name: &str) -> Option<Resolved> {
        if self.opts.no_bundled_headers {
            return None;
        }
        let text = headers::bundled(name)?;
        let key = format!("<cinder>/{}", name);
        if let Some(&id) = self.file_cache.get(&key) {
            return Some(Resolved { key, file: id });
        }
        let id = self.sess.sources.add_file(key.clone(), None, text.to_string());
        self.file_cache.insert(key.clone(), id);
        Some(Resolved { key, file: id })
    }

    fn resolve_include(&mut self, name: &str, angle: bool, from: FileId) -> Option<Resolved> {
        let p = Path::new(name);
        if self.opts.restrict_includes {
            let escapes = p.is_absolute()
                || p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::Prefix(_)));
            if escapes {
                return None;
            }
        }
        if p.is_absolute() {
            return self.read_real_file(p);
        }
        if !angle {
            // Relative to the including file first.
            let dir = self.sess.sources.file(from).path.as_ref().and_then(|p| p.parent().map(|d| d.to_path_buf()));
            match dir {
                Some(d) => {
                    if let Some(r) = self.read_real_file(&d.join(name)) {
                        return Some(r);
                    }
                }
                None => {
                    // A bundled header including "other.h" stays in the bundle.
                    if self.sess.sources.file(from).name.starts_with("<cinder>/") {
                        if let Some(r) = self.bundled(name) {
                            return Some(r);
                        }
                    }
                }
            }
        }
        let real = !self.opts.restrict_includes;
        for d in self.opts.include_dirs.clone() {
            if !real {
                break;
            }
            if let Some(r) = self.read_real_file(&d.join(name)) {
                return Some(r);
            }
        }
        if let Some(r) = self.bundled(name) {
            return Some(r);
        }
        for d in self.opts.system_dirs.clone() {
            if !real {
                break;
            }
            if let Some(r) = self.read_real_file(&d.join(name)) {
                return Some(r);
            }
        }
        None
    }

    // ───────────────────────────── #pragma ─────────────────────────────

    fn d_pragma(&mut self, span: Span, rest: &[Token]) {
        if rest.first().and_then(|t| t.ident()).is_some_and(|s| s.as_str() == "once") {
            if let Some(f) = self.frames.last() {
                let name = &self.sess.sources.file(f.file).name;
                let key = self
                    .file_cache
                    .iter()
                    .find(|(_, &id)| id == f.file)
                    .map(|(k, _)| k.clone())
                    .unwrap_or_else(|| name.clone());
                self.once.insert(key);
            }
            return;
        }
        let text = join_tokens(rest);
        let mut t = Token::new(TokKind::Pragma(Symbol::new(&text)), span);
        t.flags |= BOL;
        self.out.push(t);
    }

    /// `_Pragma("text")` behaves like `#pragma text`.
    fn try_pragma_operator(&mut self, tok: &Token) -> bool {
        let is_paren = self.peek_next().is_some_and(|t| t.is_punct(Punct::LParen));
        if !is_paren {
            return false;
        }
        self.next_arg_token();
        let s = self.next_arg_token();
        let close = self.next_arg_token();
        match (s, close) {
            (Some(s), Some(c)) if c.is_punct(Punct::RParen) => {
                if let TokKind::Str(sym) = s.kind {
                    let mut issues = Vec::new();
                    if let Some(lit) = crate::literal::parse_string(sym.as_str(), &mut issues) {
                        let text: String = lit.units.iter().filter_map(|&u| char::from_u32(u)).collect();
                        let toks = lex::lex_snippet(&text);
                        let toks: Vec<Token> = toks
                            .into_iter()
                            .map(|mut t| {
                                t.span = tok.span;
                                t
                            })
                            .collect();
                        self.d_pragma(tok.span, &toks);
                        return true;
                    }
                }
                self.sess.diags.error(tok.span, "_Pragma takes a parenthesized string literal");
                true
            }
            _ => {
                self.sess.diags.error(tok.span, "_Pragma takes a parenthesized string literal");
                true
            }
        }
    }

    // ───────────────────────────── macro expansion ─────────────────────────────

    /// Fully macro-expand a standalone token list (used for macro arguments
    /// and `#if` expressions). Never reads from the file stream.
    fn expand_list(&mut self, toks: Vec<Token>) -> Vec<Token> {
        let saved = std::mem::take(&mut self.pending);
        let saved_out = std::mem::take(&mut self.out);
        self.pending = toks.into_iter().rev().collect();
        self.isolated += 1;
        while let Some((tok, _)) = self.read_raw() {
            if let TokKind::Ident(_) = tok.kind {
                if self.try_expand(&tok) {
                    continue;
                }
            }
            self.out.push(tok);
        }
        self.isolated -= 1;
        self.pending = saved;
        std::mem::replace(&mut self.out, saved_out)
    }

    fn push_expansion(&mut self, toks: Vec<Token>) {
        if let Some(first) = toks.first() {
            self.charge(toks.len(), first.span);
        }
        if self.overflowed {
            return;
        }
        self.pending.extend(toks.into_iter().rev());
    }

    /// Try to expand `tok` (an identifier). Returns true if it was consumed.
    fn try_expand(&mut self, tok: &Token) -> bool {
        let TokKind::Ident(name) = tok.kind else { return false };
        if hs_contains(tok, name) {
            return false;
        }
        if let Some(rep) = self.builtin_macro(tok, name) {
            self.push_expansion(vec![rep]);
            return true;
        }
        let Some(m) = self.macros.get(&name).cloned() else { return false };
        if self.overflowed {
            return true; // budget exhausted: drop the macro use, keep scanning so the error stands alone
        }
        match &m.kind {
            MacroKind::Object => {
                let hide = hs_union(&tok.hide, &single(name));
                let mut out: Vec<Token> = Vec::with_capacity(m.body.len());
                for (i, b) in m.body.iter().enumerate() {
                    let mut t = b.clone();
                    t.hide = Some(hs_union(&t.hide, &hide));
                    t.span = tok.span;
                    t.flags &= !BOL;
                    if i == 0 {
                        t.flags = (t.flags & !SPACE) | (tok.flags & SPACE);
                    }
                    out.push(t);
                }
                self.push_expansion(out);
                true
            }
            MacroKind::Function { params, variadic } => {
                // A function-like macro name not followed by '(' is just an identifier.
                if !self.peek_next().is_some_and(|t| t.is_punct(Punct::LParen)) {
                    return false;
                }
                self.next_arg_token(); // '('
                let Some((args, rparen)) = self.collect_args(tok, &m, params.len(), *variadic) else {
                    return true;
                };
                let hide_base = hs_intersect(&tok.hide, &rparen.hide);
                let hide = match &hide_base {
                    Some(h) => hs_union(&Some(h.clone()), &single(name)),
                    None => single(name),
                };
                let sub = self.substitute(&m, params, *variadic, &args, tok);
                let mut out: Vec<Token> = Vec::with_capacity(sub.len());
                for (i, mut t) in sub.into_iter().enumerate() {
                    t.hide = Some(hs_union(&t.hide, &hide));
                    t.flags &= !BOL;
                    if i == 0 {
                        t.flags = (t.flags & !SPACE) | (tok.flags & SPACE);
                    }
                    out.push(t);
                }
                self.push_expansion(out);
                true
            }
        }
    }

    /// Collect the arguments of a function-like invocation (the opening
    /// parenthesis has been consumed). Returns the raw argument token lists
    /// and the closing parenthesis token.
    fn collect_args(
        &mut self,
        name_tok: &Token,
        m: &Macro,
        nparams: usize,
        variadic: bool,
    ) -> Option<(Vec<Vec<Token>>, Token)> {
        let mut args: Vec<Vec<Token>> = vec![Vec::new()];
        let mut depth = 0usize;
        let close;
        loop {
            let Some(t) = self.next_arg_token() else {
                self.sess.diags.emit(
                    Diagnostic::error(name_tok.span, "unterminated function-like macro invocation")
                        .with_note(m.def_span, format!("macro '{}' defined here", m.name)),
                );
                return None;
            };
            match t.kind {
                TokKind::Punct(Punct::LParen) => {
                    depth += 1;
                    args.last_mut().unwrap().push(t);
                }
                TokKind::Punct(Punct::RParen) => {
                    if depth == 0 {
                        close = t;
                        break;
                    }
                    depth -= 1;
                    args.last_mut().unwrap().push(t);
                }
                TokKind::Punct(Punct::Comma) if depth == 0 && !(variadic && args.len() >= nparams) => {
                    args.push(Vec::new());
                }
                TokKind::Eof => {}
                _ => args.last_mut().unwrap().push(t),
            }
        }
        // `f()` for a zero-parameter macro supplies no arguments.
        if nparams == 0 && args.len() == 1 && args[0].is_empty() {
            args.clear();
        }
        let ok = if variadic {
            args.len() >= nparams.saturating_sub(1)
        } else {
            args.len() == nparams || (nparams == 1 && args.is_empty())
        };
        if !ok {
            let many = args.len() > nparams;
            let msg = if many {
                "too many arguments provided to function-like macro invocation"
            } else {
                "too few arguments provided to function-like macro invocation"
            };
            self.sess.diags.emit(
                Diagnostic::error(close.span, msg).with_note(m.def_span, format!("macro '{}' defined here", m.name)),
            );
            return None;
        }
        while args.len() < nparams {
            args.push(Vec::new());
        }
        Some((args, close))
    }

    fn substitute(
        &mut self,
        m: &Macro,
        params: &[Symbol],
        variadic: bool,
        args: &[Vec<Token>],
        call: &Token,
    ) -> Vec<Token> {
        let mut expanded_cache: Vec<Option<Vec<Token>>> = vec![None; params.len()];
        let body = &m.body;
        self.subst_range(body, params, variadic, args, &mut expanded_cache, call)
    }

    fn subst_range(
        &mut self,
        body: &[Token],
        params: &[Symbol],
        variadic: bool,
        args: &[Vec<Token>],
        cache: &mut Vec<Option<Vec<Token>>>,
        call: &Token,
    ) -> Vec<Token> {
        let param_index = |t: &Token| -> Option<usize> { t.ident().and_then(|n| params.iter().position(|&p| p == n)) };
        let va_idx = if variadic { Some(params.len() - 1) } else { None };
        let mut out: Vec<Token> = Vec::new();
        let n = body.len();
        let mut i = 0;
        while i < n {
            let t = &body[i];

            // `# param` — stringification
            if t.is_punct(Punct::Hash) && !params.is_empty() {
                if let Some(pi) = body.get(i + 1).and_then(param_index) {
                    let mut s = stringify(&args[pi]);
                    s.span = call.span;
                    s.flags |= t.flags & SPACE;
                    out.push(s);
                    i += 2;
                    continue;
                }
            }

            // GNU `, ## __VA_ARGS__`: drop the comma when the variadic part is empty
            if t.is_punct(Punct::Comma) && i + 2 < n && body[i + 1].is_punct(Punct::HashHash) {
                if let (Some(pi), Some(va)) = (param_index(&body[i + 2]), va_idx) {
                    if pi == va {
                        if args[va].is_empty() {
                            i += 3;
                        } else {
                            out.push(t.clone());
                            out.extend(args[va].iter().cloned());
                            i += 3;
                        }
                        continue;
                    }
                }
            }

            // `__VA_OPT__ ( ... )`
            if let (TokKind::Ident(s), Some(va)) = (t.kind, va_idx) {
                if s.as_str() == "__VA_OPT__" && body.get(i + 1).is_some_and(|x| x.is_punct(Punct::LParen)) {
                    if let Some(close) = matching_paren(body, i + 1) {
                        if !args[va].is_empty() {
                            let inner = self.subst_range(&body[i + 2..close], params, variadic, args, cache, call);
                            out.extend(inner);
                        }
                        i = close + 1;
                        continue;
                    }
                }
            }

            // `a ## b ## c ...`
            if body.get(i + 1).is_some_and(|x| x.is_punct(Punct::HashHash)) {
                let mut acc: Vec<Token> = match param_index(t) {
                    Some(pi) => args[pi].clone(),
                    None => vec![t.clone()],
                };
                let mut j = i + 1;
                while j < n && body[j].is_punct(Punct::HashHash) {
                    let Some(r) = body.get(j + 1) else { break };
                    let right: Vec<Token> = match param_index(r) {
                        Some(pi) => args[pi].clone(),
                        None => vec![r.clone()],
                    };
                    if acc.is_empty() {
                        acc = right;
                    } else if !right.is_empty() {
                        let l = acc.pop().unwrap();
                        let pasted = self.paste(&l, &right[0], call);
                        acc.push(pasted);
                        acc.extend(right.into_iter().skip(1));
                    }
                    j += 2;
                }
                out.extend(acc);
                i = j;
                continue;
            }

            // plain parameter: use the fully expanded argument
            if let Some(pi) = param_index(t) {
                if cache[pi].is_none() {
                    let e = self.expand_list(args[pi].clone());
                    cache[pi] = Some(e);
                }
                let mut e = cache[pi].clone().unwrap();
                self.charge(e.len(), call.span);
                if self.overflowed {
                    return out;
                }
                if let Some(f) = e.first_mut() {
                    f.flags = (f.flags & !SPACE) | (t.flags & SPACE);
                }
                out.extend(e);
                i += 1;
                continue;
            }

            let mut c = t.clone();
            c.span = call.span;
            out.push(c);
            i += 1;
        }
        out
    }

    fn paste(&mut self, l: &Token, r: &Token, call: &Token) -> Token {
        let text = format!("{}{}", l.spelling(), r.spelling());
        let lexed = lex::lex_snippet(&text);
        if lexed.len() != 1 {
            self.sess.diags.error(call.span, format!("pasting formed '{}', an invalid preprocessing token", text));
            // Fall back to the left operand so expansion can continue.
            return l.clone();
        }
        let mut t = lexed.into_iter().next().unwrap();
        t.span = l.span;
        t.flags = l.flags & SPACE;
        t.hide = hs_intersect(&l.hide, &r.hide);
        t
    }

    fn builtin_macro(&mut self, tok: &Token, name: Symbol) -> Option<Token> {
        let s = name.as_str();
        if !s.starts_with("__") || self.macros.contains_key(&name) {
            return None;
        }
        let kind = match s {
            "__FILE__" => {
                let f = if tok.span.is_dummy() {
                    "<built-in>".to_string()
                } else {
                    self.sess.sources.file(tok.span.file).name.clone()
                };
                TokKind::Str(Symbol::new(&quote_string(&f)))
            }
            "__LINE__" => {
                let l = self.sess.sources.line_of(tok.span);
                TokKind::Number(Symbol::new(&l.to_string()))
            }
            "__COUNTER__" => {
                let c = self.counter;
                self.counter += 1;
                TokKind::Number(Symbol::new(&c.to_string()))
            }
            "__INCLUDE_LEVEL__" => TokKind::Number(Symbol::new(&self.depth.to_string())),
            "__DATE__" => TokKind::Str(Symbol::new(&format!("\"{}\"", build_date().0))),
            "__TIME__" => TokKind::Str(Symbol::new(&format!("\"{}\"", build_date().1))),
            _ => return None,
        };
        let mut t = Token::new(kind, tok.span);
        t.flags = tok.flags & SPACE;
        t.hide = tok.hide.clone();
        Some(t)
    }
}

// ───────────────────────────── helpers ─────────────────────────────

fn is_builtin_macro(s: &str) -> bool {
    matches!(s, "__FILE__" | "__LINE__" | "__COUNTER__" | "__INCLUDE_LEVEL__" | "__DATE__" | "__TIME__")
}

fn num_token(v: u64, span: Span) -> Token {
    Token::new(TokKind::Number(Symbol::new(&v.to_string())), span)
}

/// Index of the `)` matching the `(` at `open`, if any.
fn matching_paren(toks: &[Token], open: usize) -> Option<usize> {
    if !toks.get(open)?.is_punct(Punct::LParen) {
        return None;
    }
    let mut depth = 0;
    for (k, t) in toks.iter().enumerate().skip(open) {
        if t.is_punct(Punct::LParen) {
            depth += 1;
        } else if t.is_punct(Punct::RParen) {
            depth -= 1;
            if depth == 0 {
                return Some(k);
            }
        }
    }
    None
}

fn join_tokens(toks: &[Token]) -> String {
    let mut s = String::new();
    for (i, t) in toks.iter().enumerate() {
        if i > 0 && t.space() {
            s.push(' ');
        }
        s.push_str(&t.spelling());
    }
    s
}

fn quote_string(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        if c == '"' || c == '\\' {
            o.push('\\');
        }
        o.push(c);
    }
    o.push('"');
    o
}

/// `#x`: spell the raw argument tokens as a string literal.
fn stringify(arg: &[Token]) -> Token {
    let mut s = String::new();
    for (i, t) in arg.iter().enumerate() {
        if i > 0 && t.space() {
            s.push(' ');
        }
        let sp = t.spelling();
        match t.kind {
            TokKind::Str(_) | TokKind::Char(_) => {
                for c in sp.chars() {
                    if c == '"' || c == '\\' {
                        s.push('\\');
                    }
                    s.push(c);
                }
            }
            _ => s.push_str(&sp),
        }
    }
    Token::new(TokKind::Str(Symbol::new(&format!("\"{}\"", s))), Span::DUMMY)
}

fn macros_equal(a: &Macro, b: &Macro) -> bool {
    let same_kind = match (&a.kind, &b.kind) {
        (MacroKind::Object, MacroKind::Object) => true,
        (MacroKind::Function { params: p1, variadic: v1 }, MacroKind::Function { params: p2, variadic: v2 }) => {
            p1 == p2 && v1 == v2
        }
        _ => false,
    };
    same_kind
        && a.body.len() == b.body.len()
        && a.body.iter().zip(&b.body).all(|(x, y)| x.kind == y.kind && x.space() == y.space())
}

/// (`"Mmm dd yyyy"`, `"hh:mm:ss"`) of the build, honouring `SOURCE_DATE_EPOCH`.
fn build_date() -> (String, String) {
    let secs = std::env::var("SOURCE_DATE_EPOCH").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    });
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // civil_from_days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    (
        format!("{} {:>2} {}", MONTHS[(m - 1) as usize], d, y),
        format!("{:02}:{:02}:{:02}", rem / 3600, (rem % 3600) / 60, rem % 60),
    )
}

/// Macros every translation unit starts with.
fn predefined_macros(opt_level: u8) -> Vec<(&'static str, &'static str)> {
    let mut v = vec![
        ("__STDC__", "1"),
        ("__STDC_VERSION__", "201112L"),
        ("__STDC_HOSTED__", "1"),
        ("__STDC_UTF_16__", "1"),
        ("__STDC_UTF_32__", "1"),
        ("__STDC_NO_ATOMICS__", "1"),
        ("__STDC_NO_COMPLEX__", "1"),
        ("__STDC_NO_THREADS__", "1"),
        ("__CINDER__", "1"),
        ("__x86_64__", "1"),
        ("__x86_64", "1"),
        ("__amd64__", "1"),
        ("__amd64", "1"),
        ("__linux__", "1"),
        ("__linux", "1"),
        ("__gnu_linux__", "1"),
        ("__unix__", "1"),
        ("__unix", "1"),
        ("__ELF__", "1"),
        ("__LP64__", "1"),
        ("_LP64", "1"),
        ("__CHAR_BIT__", "8"),
        ("__SIZEOF_SHORT__", "2"),
        ("__SIZEOF_INT__", "4"),
        ("__SIZEOF_LONG__", "8"),
        ("__SIZEOF_LONG_LONG__", "8"),
        ("__SIZEOF_POINTER__", "8"),
        ("__SIZEOF_FLOAT__", "4"),
        ("__SIZEOF_DOUBLE__", "8"),
        ("__SIZEOF_SIZE_T__", "8"),
        ("__SIZEOF_WCHAR_T__", "4"),
        ("__SIZEOF_PTRDIFF_T__", "8"),
        ("__SIZE_TYPE__", "unsigned long"),
        ("__PTRDIFF_TYPE__", "long"),
        ("__WCHAR_TYPE__", "int"),
        ("__WINT_TYPE__", "unsigned int"),
        ("__INTMAX_TYPE__", "long"),
        ("__UINTMAX_TYPE__", "unsigned long"),
        ("__INTPTR_TYPE__", "long"),
        ("__UINTPTR_TYPE__", "unsigned long"),
        ("__CHAR16_TYPE__", "unsigned short"),
        ("__CHAR32_TYPE__", "unsigned int"),
        ("__SCHAR_MAX__", "127"),
        ("__SHRT_MAX__", "32767"),
        ("__INT_MAX__", "2147483647"),
        ("__LONG_MAX__", "9223372036854775807L"),
        ("__LONG_LONG_MAX__", "9223372036854775807LL"),
        ("__ORDER_LITTLE_ENDIAN__", "1234"),
        ("__ORDER_BIG_ENDIAN__", "4321"),
        ("__BYTE_ORDER__", "__ORDER_LITTLE_ENDIAN__"),
    ];
    if opt_level > 0 {
        v.push(("__OPTIMIZE__", "1"));
    }
    v
}

#[cfg(test)]
mod tests;
