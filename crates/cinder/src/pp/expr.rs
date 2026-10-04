//! `#if` / `#elif` constant-expression evaluation.
//!
//! Operates on a token list that has already had `defined`/`__has_include`
//! resolved and macros expanded. Arithmetic follows C: `intmax_t` / `uintmax_t`
//! (64-bit here) with the usual arithmetic conversions between them.

use crate::diag::{Diagnostic, Warn};
use crate::lex::{Punct, TokKind, Token};
use crate::literal;
use crate::session::Session;
use crate::source::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Val {
    v: u64,
    uns: bool,
}

impl Val {
    fn signed(v: i64) -> Val {
        Val { v: v as u64, uns: false }
    }

    fn truth(b: bool) -> Val {
        Val::signed(b as i64)
    }

    fn is_true(self) -> bool {
        self.v != 0
    }
}

struct Eval<'a> {
    toks: &'a [Token],
    pos: usize,
    sess: &'a mut Session,
    /// Span used when the expression runs out of tokens.
    eol: Span,
}

type R = Result<Val, ()>;

/// Evaluate the controlling expression of an `#if`/`#elif`. Emits diagnostics
/// itself; on an error the condition is treated as false.
pub fn eval_condition(sess: &mut Session, toks: &[Token], directive_span: Span) -> bool {
    if toks.is_empty() {
        sess.diags.error(directive_span, "#if with no expression");
        return false;
    }
    let mut e = Eval { toks, pos: 0, sess, eol: directive_span.end() };
    match e.cond(true) {
        Ok(v) => {
            if e.pos < e.toks.len() {
                let t = &e.toks[e.pos];
                let msg = format!("token is not valid in the preprocessor expression: '{}'", t.spelling());
                e.sess.diags.error(t.span, msg);
                return false;
            }
            v.is_true()
        }
        Err(()) => false,
    }
}

impl<'a> Eval<'a> {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    fn peek_punct(&self) -> Option<Punct> {
        match self.peek()?.kind {
            TokKind::Punct(p) => Some(p),
            _ => None,
        }
    }

    fn here(&self) -> Span {
        self.peek().map(|t| t.span).unwrap_or(self.eol)
    }

    fn err<T>(&mut self, span: Span, msg: impl Into<String>) -> Result<T, ()> {
        self.sess.diags.emit(Diagnostic::error(span, msg));
        Err(())
    }

    fn eat(&mut self, p: Punct) -> bool {
        if self.peek_punct() == Some(p) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    // cond := lor ( '?' expr ':' cond )?
    fn cond(&mut self, ev: bool) -> R {
        let c = self.binary(0, ev)?;
        if self.peek_punct() == Some(Punct::Question) {
            self.pos += 1;
            let t = self.cond(ev && c.is_true())?;
            if !self.eat(Punct::Colon) {
                let sp = self.here();
                return self.err(sp, "expected ':' in conditional expression");
            }
            let f = self.cond(ev && !c.is_true())?;
            let uns = t.uns || f.uns;
            let pick = if c.is_true() { t } else { f };
            return Ok(Val { v: pick.v, uns });
        }
        Ok(c)
    }

    fn prec(p: Punct) -> Option<u8> {
        use Punct::*;
        Some(match p {
            PipePipe => 1,
            AmpAmp => 2,
            Pipe => 3,
            Caret => 4,
            Amp => 5,
            EqEq | Ne => 6,
            Lt | Gt | Le | Ge => 7,
            Shl | Shr => 8,
            Plus | Minus => 9,
            Star | Slash | Percent => 10,
            _ => return None,
        })
    }

    fn binary(&mut self, min: u8, ev: bool) -> R {
        let mut lhs = self.unary(ev)?;
        while let Some(op) = self.peek_punct() {
            let Some(p) = Self::prec(op) else { break };
            if p <= min {
                break;
            }
            let op_span = self.here();
            self.pos += 1;
            // Short-circuit operators suppress evaluation (and so, errors) of the unneeded side.
            let rhs_ev = match op {
                Punct::AmpAmp => ev && lhs.is_true(),
                Punct::PipePipe => ev && !lhs.is_true(),
                _ => ev,
            };
            let rhs = self.binary(p, rhs_ev)?;
            lhs = self.apply(op, lhs, rhs, op_span, ev && rhs_ev)?;
        }
        Ok(lhs)
    }

    fn apply(&mut self, op: Punct, a: Val, b: Val, span: Span, ev: bool) -> R {
        use Punct::*;
        let uns = a.uns || b.uns;
        let (x, y) = (a.v, b.v);
        let (sx, sy) = (x as i64, y as i64);
        Ok(match op {
            PipePipe => Val::truth(a.is_true() || b.is_true()),
            AmpAmp => Val::truth(a.is_true() && b.is_true()),
            Pipe => Val { v: x | y, uns },
            Caret => Val { v: x ^ y, uns },
            Amp => Val { v: x & y, uns },
            EqEq => Val::truth(x == y),
            Ne => Val::truth(x != y),
            Lt => Val::truth(if uns { x < y } else { sx < sy }),
            Gt => Val::truth(if uns { x > y } else { sx > sy }),
            Le => Val::truth(if uns { x <= y } else { sx <= sy }),
            Ge => Val::truth(if uns { x >= y } else { sx >= sy }),
            Shl | Shr => {
                // The result has the type of the (promoted) left operand.
                let left_uns = a.uns;
                let count = if b.uns || sy >= 0 { y } else { 0u64.wrapping_sub(y) };
                let neg_count = !b.uns && sy < 0;
                let left = (op == Shl) != neg_count;
                if count >= 64 {
                    if left || left_uns || sx >= 0 {
                        Val { v: 0, uns: left_uns }
                    } else {
                        Val { v: u64::MAX, uns: left_uns }
                    }
                } else if left {
                    Val { v: x << count, uns: left_uns }
                } else if left_uns {
                    Val { v: x >> count, uns: true }
                } else {
                    Val { v: (sx >> count) as u64, uns: false }
                }
            }
            Plus => Val { v: x.wrapping_add(y), uns },
            Minus => Val { v: x.wrapping_sub(y), uns },
            Star => Val { v: x.wrapping_mul(y), uns },
            Slash | Percent => {
                if y == 0 {
                    if ev {
                        let what = if op == Slash { "division" } else { "remainder" };
                        return self.err(span, format!("{} by zero in preprocessor expression", what));
                    }
                    return Ok(Val { v: 0, uns });
                }
                if uns {
                    Val { v: if op == Slash { x / y } else { x % y }, uns }
                } else if op == Slash {
                    Val::signed(sx.wrapping_div(sy))
                } else {
                    Val::signed(sx.wrapping_rem(sy))
                }
            }
            _ => unreachable!("not a binary operator"),
        })
    }

    fn unary(&mut self, ev: bool) -> R {
        let Some(tok) = self.peek().cloned() else {
            let sp = self.eol;
            return self.err(sp, "expected value in expression");
        };
        match tok.kind {
            TokKind::Punct(Punct::Plus) => {
                self.pos += 1;
                self.unary(ev)
            }
            TokKind::Punct(Punct::Minus) => {
                self.pos += 1;
                let v = self.unary(ev)?;
                Ok(Val { v: v.v.wrapping_neg(), uns: v.uns })
            }
            TokKind::Punct(Punct::Tilde) => {
                self.pos += 1;
                let v = self.unary(ev)?;
                Ok(Val { v: !v.v, uns: v.uns })
            }
            TokKind::Punct(Punct::Bang) => {
                self.pos += 1;
                let v = self.unary(ev)?;
                Ok(Val::truth(!v.is_true()))
            }
            TokKind::Punct(Punct::LParen) => {
                self.pos += 1;
                let v = self.cond(ev)?;
                if !self.eat(Punct::RParen) {
                    let sp = self.here();
                    return self.err(sp, "expected ')' in preprocessor expression");
                }
                Ok(v)
            }
            TokKind::Number(s) => {
                self.pos += 1;
                let text = s.as_str();
                if literal::is_float_spelling(text) {
                    return self.err(tok.span, "floating constant in preprocessor expression");
                }
                match literal::parse_int(text) {
                    Ok(l) => {
                        let uns = l.unsigned || l.value > i64::MAX as u64;
                        if !l.unsigned && l.value > i64::MAX as u64 && l.decimal {
                            self.sess.diags.warn(
                                Warn::Overflow,
                                tok.span,
                                "integer literal is too large to be represented in a signed integer type, interpreting as unsigned",
                            );
                        }
                        Ok(Val { v: l.value, uns })
                    }
                    Err(m) => self.err(tok.span, m),
                }
            }
            TokKind::Char(s) => {
                self.pos += 1;
                let mut issues = Vec::new();
                let c = literal::parse_char(s.as_str(), &mut issues);
                for i in &issues {
                    if i.is_error {
                        return self.err(tok.span, i.message.clone());
                    }
                }
                match c {
                    Some(c) => Ok(Val::signed(c.value)),
                    None => self.err(tok.span, "invalid character constant"),
                }
            }
            TokKind::Str(_) => self.err(tok.span, "string literal in preprocessor expression"),
            TokKind::Ident(s) => {
                // Identifiers that survive macro expansion are 0 (C11 6.10.1p4).
                // `__has_*` forms were resolved earlier; anything else is just 0.
                let _ = s;
                self.pos += 1;
                Ok(Val::signed(0))
            }
            _ => {
                self.pos += 1;
                self.err(tok.span, format!("token is not valid in the preprocessor expression: '{}'", tok.spelling()))
            }
        }
    }
}
