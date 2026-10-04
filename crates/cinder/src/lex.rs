//! Lexer: turns the logical text of one file into preprocessing tokens.
//!
//! The lexer is deliberately infallible except for unterminated block
//! comments: malformed literals become [`TokKind::BadLiteral`] / stray
//! characters become [`TokKind::Other`], and it is the preprocessor (or the
//! parser) that reports them *if they survive into live code*. That matters
//! for text inside `#if 0` blocks, which may legitimately contain a lone `'`.

use crate::diag::{DiagCtx, Diagnostic};
use crate::intern::Symbol;
use crate::source::{FileId, Span};
use std::collections::BTreeSet;
use std::rc::Rc;

/// The set of macro names a token must not be re-expanded as (Prosser's
/// hide-set algorithm for correct recursive-macro behaviour).
pub type HideSet = BTreeSet<Symbol>;

/// First token on its (logical) line — makes `#` a directive introducer.
pub const BOL: u8 = 1;
/// Whitespace (or a comment) preceded this token.
pub const SPACE: u8 = 2;

macro_rules! puncts {
    ($($name:ident => $s:literal,)*) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum Punct { $($name),* }
        impl Punct {
            pub fn spelling(self) -> &'static str {
                match self { $(Punct::$name => $s),* }
            }
        }
    };
}

puncts! {
    LBracket => "[", RBracket => "]", LParen => "(", RParen => ")", LBrace => "{", RBrace => "}",
    Dot => ".", Arrow => "->", PlusPlus => "++", MinusMinus => "--",
    Amp => "&", Star => "*", Plus => "+", Minus => "-", Tilde => "~", Bang => "!",
    Slash => "/", Percent => "%", Shl => "<<", Shr => ">>",
    Lt => "<", Gt => ">", Le => "<=", Ge => ">=", EqEq => "==", Ne => "!=",
    Caret => "^", Pipe => "|", AmpAmp => "&&", PipePipe => "||",
    Question => "?", Colon => ":", Semi => ";", Ellipsis => "...",
    Eq => "=", StarEq => "*=", SlashEq => "/=", PercentEq => "%=", PlusEq => "+=", MinusEq => "-=",
    ShlEq => "<<=", ShrEq => ">>=", AmpEq => "&=", CaretEq => "^=", PipeEq => "|=",
    Comma => ",", Hash => "#", HashHash => "##",
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokKind {
    Ident(Symbol),
    /// A pp-number (integer or floating constant, not yet validated).
    Number(Symbol),
    /// Character constant, full spelling including prefix and quotes.
    Char(Symbol),
    /// String literal, full spelling including prefix and quotes.
    Str(Symbol),
    /// `<...>` in `#include`; the symbol holds the text between the brackets.
    HeaderAngle(Symbol),
    Punct(Punct),
    /// An unterminated character constant or string literal.
    BadLiteral(Symbol),
    /// A character that starts no token (`@`, `` ` ``, stray `\`, ...).
    Other(char),
    /// `#pragma pack(...)`-style directive forwarded to the parser.
    Pragma(Symbol),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub kind: TokKind,
    pub span: Span,
    pub flags: u8,
    pub hide: Option<Rc<HideSet>>,
}

impl Token {
    pub fn new(kind: TokKind, span: Span) -> Token {
        Token { kind, span, flags: 0, hide: None }
    }

    pub fn is_punct(&self, p: Punct) -> bool {
        self.kind == TokKind::Punct(p)
    }

    pub fn ident(&self) -> Option<Symbol> {
        match self.kind {
            TokKind::Ident(s) => Some(s),
            _ => None,
        }
    }

    pub fn bol(&self) -> bool {
        self.flags & BOL != 0
    }

    pub fn space(&self) -> bool {
        self.flags & SPACE != 0
    }

    /// The token's spelling as it would be written in source.
    pub fn spelling(&self) -> String {
        match self.kind {
            TokKind::Ident(s) | TokKind::Number(s) | TokKind::Char(s) | TokKind::Str(s) | TokKind::BadLiteral(s) => {
                s.as_str().to_string()
            }
            TokKind::HeaderAngle(s) => format!("<{}>", s),
            TokKind::Punct(p) => p.spelling().to_string(),
            TokKind::Other(c) => c.to_string(),
            TokKind::Pragma(s) => format!("#pragma {}", s),
            TokKind::Eof => String::new(),
        }
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80
}

fn is_ident_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// Directives whose `<...>` operand must be scanned as a header name.
fn is_include_name(s: &str) -> bool {
    matches!(s, "include" | "include_next" | "import")
}

pub fn lex(file: FileId, text: &str, diags: &mut DiagCtx) -> Vec<Token> {
    let b = text.as_bytes();
    let mut toks: Vec<Token> = Vec::new();
    let mut i = 0usize;
    let mut bol = true;
    let mut space = false;
    // Per-line state so `#include <...>` lexes its operand as a header name.
    let mut line_toks = 0usize;
    let mut in_directive = false;
    let mut header_next = false;

    while i < b.len() {
        let c = b[i];
        // ── whitespace and comments ──
        if c == b'\n' {
            bol = true;
            space = true;
            line_toks = 0;
            in_directive = false;
            header_next = false;
            i += 1;
            continue;
        }
        if c == b' ' || c == b'\t' || c == 0x0B || c == 0x0C || c == b'\r' {
            space = true;
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            space = true;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            let mut closed = false;
            while i + 1 < b.len() {
                if b[i] == b'*' && b[i + 1] == b'/' {
                    i += 2;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                i = b.len();
                diags.emit(
                    Diagnostic::error(Span::new(file, start as u32, (start + 2) as u32), "unterminated /* comment")
                        .with_label(Span::new(file, start as u32, (start + 2) as u32)),
                );
            }
            space = true;
            continue;
        }

        let start = i;
        let mut flags = 0u8;
        if bol {
            flags |= BOL;
        }
        if space {
            flags |= SPACE;
        }
        bol = false;
        space = false;

        let kind: TokKind;

        // ── identifiers (and prefixed literals) ──
        if is_ident_start(c) {
            i += 1;
            while i < b.len() && is_ident_continue(b[i]) {
                i += 1;
            }
            let word = &text[start..i];
            let quote = b.get(i).copied();
            let is_prefix = matches!(word, "L" | "u" | "U" | "u8");
            if is_prefix && quote == Some(b'"') || (is_prefix && word != "u8" && quote == Some(b'\'')) {
                let q = quote.unwrap();
                let (end, closed) = scan_quoted(b, i, q);
                i = end;
                kind = literal_kind(&text[start..i], q, closed);
            } else {
                kind = TokKind::Ident(Symbol::new(word));
            }
        }
        // ── numbers ──
        else if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            i += 1;
            while i < b.len() {
                let d = b[i];
                // A sign continues the number only directly after an exponent letter.
                let signed_exp = (d == b'+' || d == b'-') && matches!(b[i - 1], b'e' | b'E' | b'p' | b'P');
                if !(signed_exp || d.is_ascii_alphanumeric() || d == b'_' || d == b'.') {
                    break;
                }
                i += 1;
            }
            kind = TokKind::Number(Symbol::new(&text[start..i]));
        }
        // ── string / char literals ──
        else if c == b'"' || c == b'\'' {
            let (end, closed) = scan_quoted(b, i, c);
            i = end;
            kind = literal_kind(&text[start..i], c, closed);
        }
        // ── header names ──
        else if c == b'<' && (header_next || after_has_include_paren(&toks)) {
            // `#include <foo/bar.h>` — scan to the closing '>' on this line.
            let mut j = i + 1;
            while j < b.len() && b[j] != b'>' && b[j] != b'\n' {
                j += 1;
            }
            if j < b.len() && b[j] == b'>' {
                kind = TokKind::HeaderAngle(Symbol::new(&text[i + 1..j]));
                i = j + 1;
            } else {
                kind = TokKind::Punct(Punct::Lt);
                i += 1;
            }
        }
        // ── punctuators ──
        else if let Some((p, len)) = punct_at(b, i) {
            i += len;
            kind = TokKind::Punct(p);
        } else {
            let ch = text[i..].chars().next().unwrap();
            i += ch.len_utf8();
            kind = TokKind::Other(ch);
        }

        match line_toks {
            0 => in_directive = matches!(kind, TokKind::Punct(Punct::Hash)),
            1 if in_directive => header_next = matches!(kind, TokKind::Ident(s) if is_include_name(s.as_str())),
            _ => header_next = false,
        }
        line_toks += 1;
        push(&mut toks, kind, file, start, i, flags);
    }
    toks
}

/// `__has_include(<...>)`: the `<` follows `(` which follows the keyword.
fn after_has_include_paren(toks: &[Token]) -> bool {
    let n = toks.len();
    if n < 2 {
        return false;
    }
    toks[n - 1].is_punct(Punct::LParen)
        && matches!(toks[n - 2].kind, TokKind::Ident(s) if s.as_str().starts_with("__has_include"))
}

fn push(toks: &mut Vec<Token>, kind: TokKind, file: FileId, start: usize, end: usize, flags: u8) {
    toks.push(Token { kind, span: Span::new(file, start as u32, end as u32), flags, hide: None });
}

/// Scan a quoted literal starting at the opening quote at `i`. Returns the
/// index one past the closing quote and whether the literal was terminated;
/// an unterminated literal stops at the end of the line.
fn scan_quoted(b: &[u8], mut i: usize, quote: u8) -> (usize, bool) {
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == quote => return (i + 1, true),
            b'\n' => return (i, false),
            _ => i += 1,
        }
    }
    (b.len(), false)
}

fn literal_kind(spelled: &str, quote: u8, closed: bool) -> TokKind {
    let sym = Symbol::new(spelled);
    match (closed, quote) {
        (true, b'"') => TokKind::Str(sym),
        (true, _) => TokKind::Char(sym),
        (false, _) => TokKind::BadLiteral(sym),
    }
}

fn punct_at(b: &[u8], i: usize) -> Option<(Punct, usize)> {
    use Punct::*;
    let c = b[i];
    let c1 = b.get(i + 1).copied().unwrap_or(0);
    let c2 = b.get(i + 2).copied().unwrap_or(0);
    // three-character forms
    let three = match (c, c1, c2) {
        (b'.', b'.', b'.') => Some(Ellipsis),
        (b'<', b'<', b'=') => Some(ShlEq),
        (b'>', b'>', b'=') => Some(ShrEq),
        _ => None,
    };
    if let Some(p) = three {
        return Some((p, 3));
    }
    // `%:%:` digraph for ##
    if c == b'%' && c1 == b':' && c2 == b'%' && b.get(i + 3) == Some(&b':') {
        return Some((HashHash, 4));
    }
    let two = match (c, c1) {
        (b'-', b'>') => Some(Arrow),
        (b'+', b'+') => Some(PlusPlus),
        (b'-', b'-') => Some(MinusMinus),
        (b'<', b'<') => Some(Shl),
        (b'>', b'>') => Some(Shr),
        (b'<', b'=') => Some(Le),
        (b'>', b'=') => Some(Ge),
        (b'=', b'=') => Some(EqEq),
        (b'!', b'=') => Some(Ne),
        (b'&', b'&') => Some(AmpAmp),
        (b'|', b'|') => Some(PipePipe),
        (b'*', b'=') => Some(StarEq),
        (b'/', b'=') => Some(SlashEq),
        (b'%', b'=') => Some(PercentEq),
        (b'+', b'=') => Some(PlusEq),
        (b'-', b'=') => Some(MinusEq),
        (b'&', b'=') => Some(AmpEq),
        (b'^', b'=') => Some(CaretEq),
        (b'|', b'=') => Some(PipeEq),
        (b'#', b'#') => Some(HashHash),
        // digraphs
        (b'<', b':') => Some(LBracket),
        (b':', b'>') => Some(RBracket),
        (b'<', b'%') => Some(LBrace),
        (b'%', b'>') => Some(RBrace),
        (b'%', b':') => Some(Hash),
        _ => None,
    };
    if let Some(p) = two {
        return Some((p, 2));
    }
    let one = match c {
        b'[' => LBracket,
        b']' => RBracket,
        b'(' => LParen,
        b')' => RParen,
        b'{' => LBrace,
        b'}' => RBrace,
        b'.' => Dot,
        b'&' => Amp,
        b'*' => Star,
        b'+' => Plus,
        b'-' => Minus,
        b'~' => Tilde,
        b'!' => Bang,
        b'/' => Slash,
        b'%' => Percent,
        b'<' => Lt,
        b'>' => Gt,
        b'^' => Caret,
        b'|' => Pipe,
        b'?' => Question,
        b':' => Colon,
        b';' => Semi,
        b'=' => Eq,
        b',' => Comma,
        b'#' => Hash,
        _ => return None,
    };
    Some((one, 1))
}

/// Lex a standalone snippet (used for `-D` definitions, `##` pasting and
/// stringification round-trips). Positions are relative to the snippet.
pub fn lex_snippet(text: &str) -> Vec<Token> {
    let mut d = DiagCtx::new();
    lex(u32::MAX - 1, text, &mut d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<String> {
        let mut d = DiagCtx::new();
        lex(0, src, &mut d).iter().map(|t| t.spelling()).collect()
    }

    fn toks(src: &str) -> Vec<Token> {
        let mut d = DiagCtx::new();
        lex(0, src, &mut d)
    }

    #[test]
    fn basic_tokens() {
        assert_eq!(
            kinds("int main(void) { return 0; }"),
            ["int", "main", "(", "void", ")", "{", "return", "0", ";", "}"]
        );
    }

    #[test]
    fn longest_match_punctuators() {
        assert_eq!(kinds("a<<=b>>=c->d++ --e..."), ["a", "<<=", "b", ">>=", "c", "->", "d", "++", "--", "e", "..."]);
        assert_eq!(
            kinds("a<=b>=c==d!=e&&f||g"),
            ["a", "<=", "b", ">=", "c", "==", "d", "!=", "e", "&&", "f", "||", "g"]
        );
        assert_eq!(
            kinds("x+=1;y-=2;z*=3;w/=4;v%=5;u&=6;t|=7;s^=8"),
            [
                "x", "+=", "1", ";", "y", "-=", "2", ";", "z", "*=", "3", ";", "w", "/=", "4", ";", "v", "%=", "5",
                ";", "u", "&=", "6", ";", "t", "|=", "7", ";", "s", "^=", "8"
            ]
        );
    }

    #[test]
    fn digraphs_map_to_real_punctuators() {
        let t = toks("<: :> <% %> %: %:%:");
        let ps: Vec<Punct> = t
            .iter()
            .map(|t| match t.kind {
                TokKind::Punct(p) => p,
                _ => panic!(),
            })
            .collect();
        use Punct::*;
        assert_eq!(ps, [LBracket, RBracket, LBrace, RBrace, Hash, HashHash]);
    }

    #[test]
    fn numbers_are_pp_numbers() {
        assert_eq!(
            kinds("0x1F 1.5e+10 1e-3f .5 0x1.8p-2 123u 1..2"),
            ["0x1F", "1.5e+10", "1e-3f", ".5", "0x1.8p-2", "123u", "1..2"]
        );
        // `1+2` must stay three tokens (sign only joins after an exponent letter)
        assert_eq!(kinds("1+2"), ["1", "+", "2"]);
        assert_eq!(kinds("0xe+1"), ["0xe+1"]); // pp-number rule: e+ continues the number
    }

    #[test]
    fn string_and_char_literals() {
        let t = toks(r#""a\"b" 'c' '\'' L"w" u8"x" U'y'"#);
        let k: Vec<_> = t.iter().map(|t| t.kind).collect();
        assert!(matches!(k[0], TokKind::Str(_)));
        assert!(matches!(k[1], TokKind::Char(_)));
        assert!(matches!(k[2], TokKind::Char(_)));
        assert!(matches!(k[3], TokKind::Str(_)));
        assert!(matches!(k[4], TokKind::Str(_)));
        assert!(matches!(k[5], TokKind::Char(_)));
        assert_eq!(t[0].spelling(), r#""a\"b""#);
        assert_eq!(t.len(), 6);
    }

    #[test]
    fn unterminated_literals_do_not_swallow_the_next_line() {
        let t = toks("'a\nint x;\n\"abc\nfoo");
        assert!(matches!(t[0].kind, TokKind::BadLiteral(_)));
        assert_eq!(t[1].spelling(), "int");
        assert!(t.iter().any(|t| matches!(t.kind, TokKind::BadLiteral(s) if s.as_str() == "\"abc")));
        assert_eq!(t.last().unwrap().spelling(), "foo");
    }

    #[test]
    fn comments_become_whitespace() {
        let t = toks("a/*x*/b // tail\nc");
        assert_eq!(t.len(), 3);
        assert!(t[1].space());
        assert!(t[2].bol());
        // a block comment spanning lines does not start a new logical line
        let t = toks("a /* one\ntwo */ b");
        assert!(!t[1].bol());
    }

    #[test]
    fn unterminated_comment_is_an_error() {
        let mut d = DiagCtx::new();
        let t = lex(0, "int x; /* oops", &mut d);
        assert_eq!(d.error_count(), 1);
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn bol_and_space_flags() {
        let t = toks("# define X 1\n  y");
        assert!(t[0].bol());
        assert!(!t[1].bol());
        assert!(t[1].space());
        assert!(t[4].bol());
    }

    #[test]
    fn header_names_in_include_directives() {
        let t = toks("#include <sys/types.h>\n#include \"x.h\"\na < b > c");
        assert!(matches!(t[2].kind, TokKind::HeaderAngle(s) if s.as_str() == "sys/types.h"));
        assert!(matches!(t[5].kind, TokKind::Str(_)));
        // outside an include, `<` is an ordinary operator
        let tail: Vec<_> = t[6..].iter().map(|t| t.spelling()).collect();
        assert_eq!(tail, ["a", "<", "b", ">", "c"]);
    }

    #[test]
    fn has_include_header_names() {
        let t = toks("#if __has_include(<stdio.h>)\n#endif");
        assert!(t.iter().any(|t| matches!(t.kind, TokKind::HeaderAngle(s) if s.as_str() == "stdio.h")));
    }

    #[test]
    fn spans_index_the_logical_text() {
        let t = toks("ab  cd");
        assert_eq!((t[0].span.lo, t[0].span.hi), (0, 2));
        assert_eq!((t[1].span.lo, t[1].span.hi), (4, 6));
    }

    #[test]
    fn stray_characters() {
        let t = toks("a @ b `");
        assert!(matches!(t[1].kind, TokKind::Other('@')));
        assert!(matches!(t[3].kind, TokKind::Other('`')));
    }
}
