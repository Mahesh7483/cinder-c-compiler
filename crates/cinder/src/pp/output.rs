//! `-E` output: print the preprocessed token stream as text with line
//! markers (`# <line> "<file>"`), preserving line structure and indentation
//! where it can, and inserting spaces only where tokens would otherwise fuse.

use crate::lex::{self, TokKind, Token};
use crate::session::Session;
use crate::source::FileId;

/// Would printing `a` immediately followed by `b` change how they re-lex?
fn needs_space(a: &Token, b: &Token) -> bool {
    let joined = format!("{}{}", a.spelling(), b.spelling());
    lex::lex_snippet(&joined).len() != 2
}

/// Maximum number of blank lines emitted before falling back to a line marker.
const MAX_BLANK_RUN: u32 = 8;

pub fn print(sess: &Session, toks: &[Token]) -> String {
    let mut out = String::new();
    let mut cur_file: Option<FileId> = None;
    let mut cur_line = 0u32;
    let mut at_line_start = true;
    let mut prev: Option<&Token> = None;

    let marker = |out: &mut String, line: u32, file: FileId| {
        out.push_str(&format!("# {} \"{}\"\n", line, sess.sources.file(file).name.replace('\\', "\\\\")));
    };

    for t in toks {
        if matches!(t.kind, TokKind::Eof) {
            break;
        }
        if t.span.is_dummy() {
            // Synthesized token: print wherever we are.
            if !at_line_start && (t.space() || prev.is_some_and(|p| needs_space(p, t))) {
                out.push(' ');
            }
            out.push_str(&t.spelling());
            at_line_start = false;
            prev = Some(t);
            continue;
        }
        let file = t.span.file;
        let line = sess.sources.line_of(t.span);
        if cur_file != Some(file) {
            if !at_line_start {
                out.push('\n');
            }
            marker(&mut out, line, file);
            cur_file = Some(file);
            cur_line = line;
            at_line_start = true;
            prev = None;
        } else if line > cur_line {
            if !at_line_start {
                out.push('\n');
                cur_line += 1;
            }
            let gap = line - cur_line;
            if gap <= MAX_BLANK_RUN {
                for _ in 0..gap {
                    out.push('\n');
                }
            } else {
                marker(&mut out, line, file);
            }
            cur_line = line;
            at_line_start = true;
            prev = None;
        }

        if let TokKind::Pragma(_) = t.kind {
            if !at_line_start {
                out.push('\n');
                cur_line += 1;
            }
            out.push_str(&t.spelling());
            out.push('\n');
            cur_line += 1;
            at_line_start = true;
            prev = None;
            continue;
        }

        if at_line_start {
            // Preserve source indentation for tokens that came straight from the file.
            let f = sess.sources.file(file);
            let (olo, _) = f.orig_range(t.span.lo, t.span.hi);
            let (_, col) = f.line_col_orig(olo);
            for _ in 1..col {
                out.push(' ');
            }
            at_line_start = false;
        } else if t.space() || prev.is_some_and(|p| needs_space(p, t)) {
            out.push(' ');
        }
        out.push_str(&t.spelling());
        prev = Some(t);
    }
    if !at_line_start {
        out.push('\n');
    }
    out
}
