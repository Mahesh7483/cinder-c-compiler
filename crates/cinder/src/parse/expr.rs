//! Expressions: precedence climbing over the C operator table.

use super::*;
use crate::literal::StrKind;

fn binop(p: Punct) -> Option<(BinOp, u8)> {
    use Punct::*;
    Some(match p {
        PipePipe => (BinOp::LogOr, 1),
        AmpAmp => (BinOp::LogAnd, 2),
        Pipe => (BinOp::BitOr, 3),
        Caret => (BinOp::BitXor, 4),
        Amp => (BinOp::BitAnd, 5),
        EqEq => (BinOp::Eq, 6),
        Ne => (BinOp::Ne, 6),
        Lt => (BinOp::Lt, 7),
        Gt => (BinOp::Gt, 7),
        Le => (BinOp::Le, 7),
        Ge => (BinOp::Ge, 7),
        Shl => (BinOp::Shl, 8),
        Shr => (BinOp::Shr, 8),
        Plus => (BinOp::Add, 9),
        Minus => (BinOp::Sub, 9),
        Star => (BinOp::Mul, 10),
        Slash => (BinOp::Div, 10),
        Percent => (BinOp::Rem, 10),
        _ => return None,
    })
}

/// `Some(None)` for `=`, `Some(Some(op))` for `op=`.
fn assign_op(p: Punct) -> Option<Option<BinOp>> {
    use Punct::*;
    Some(match p {
        Eq => None,
        PlusEq => Some(BinOp::Add),
        MinusEq => Some(BinOp::Sub),
        StarEq => Some(BinOp::Mul),
        SlashEq => Some(BinOp::Div),
        PercentEq => Some(BinOp::Rem),
        ShlEq => Some(BinOp::Shl),
        ShrEq => Some(BinOp::Shr),
        AmpEq => Some(BinOp::BitAnd),
        CaretEq => Some(BinOp::BitXor),
        PipeEq => Some(BinOp::BitOr),
        _ => return None,
    })
}

impl<'a> Parser<'a> {
    /// expression: assignment-expression (',' assignment-expression)*
    pub(crate) fn parse_expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_assign()?;
        while self.at_punct(Punct::Comma) {
            self.bump();
            let rhs = self.parse_assign()?;
            let span = lhs.span.to(rhs.span);
            lhs = Expr { kind: ExprKind::Comma(Box::new(lhs), Box::new(rhs)), span };
        }
        Ok(lhs)
    }

    pub(crate) fn parse_assign(&mut self) -> PResult<Expr> {
        let lhs = self.parse_cond()?;
        if let PKind::Punct(p) = self.kind() {
            if let Some(op) = assign_op(p) {
                let op_tok = self.bump();
                let rhs = self.parse_assign()?;
                let span = lhs.span.to(rhs.span);
                return Ok(Expr {
                    kind: ExprKind::Assign { op, lhs: Box::new(lhs), rhs: Box::new(rhs), op_span: op_tok.span },
                    span,
                });
            }
        }
        Ok(lhs)
    }

    /// conditional-expression (also the grammar for constant expressions)
    pub(crate) fn parse_cond(&mut self) -> PResult<Expr> {
        let cond = self.parse_binary(1)?;
        if !self.at_punct(Punct::Question) {
            return Ok(cond);
        }
        let q = self.bump();
        let then = self.parse_expr()?;
        if !self.eat_punct(Punct::Colon) {
            let at = self.prev_span().end();
            let d = Diagnostic::error(at, "expected ':'").with_note(q.span, "to match this '?'");
            return self.emit(d).map(|_| unreachable!());
        }
        let els = self.parse_cond()?;
        let span = cond.span.to(els.span);
        Ok(Expr { kind: ExprKind::Cond { cond: Box::new(cond), then: Box::new(then), els: Box::new(els) }, span })
    }

    fn parse_binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut lhs = self.parse_cast()?;
        while let PKind::Punct(p) = self.kind() {
            let Some((op, prec)) = binop(p) else { break };
            if prec < min_prec {
                break;
            }
            let op_tok = self.bump();
            let rhs = self.parse_binary(prec + 1)?;
            let span = lhs.span.to(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs), op_span: op_tok.span },
                span,
            };
        }
        Ok(lhs)
    }

    /// cast-expression: unary-expression | '(' type-name ')' cast-expression
    fn parse_cast(&mut self) -> PResult<Expr> {
        if self.at_punct(Punct::LParen) && self.type_start_at(1) {
            let open = self.bump();
            let ty = self.parse_type_name()?;
            self.expect_close(Punct::RParen, open.span)?;
            if self.at_punct(Punct::LBrace) {
                // compound literal: a postfix-expression
                let init = self.parse_init_list()?;
                let span = self.span_from(open.span);
                let lit = Expr { kind: ExprKind::CompoundLiteral { ty: Box::new(ty), init }, span };
                return self.parse_postfix(lit);
            }
            let operand = self.parse_cast()?;
            let span = open.span.to(operand.span);
            return Ok(Expr { kind: ExprKind::Cast { ty: Box::new(ty), operand: Box::new(operand) }, span });
        }
        self.parse_unary()
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        let tok = self.peek();
        let (op, via_cast) = match tok.kind {
            PKind::Punct(Punct::PlusPlus) => (Some(UnOp::PreInc), false),
            PKind::Punct(Punct::MinusMinus) => (Some(UnOp::PreDec), false),
            PKind::Punct(Punct::Amp) => (Some(UnOp::AddrOf), true),
            PKind::Punct(Punct::Star) => (Some(UnOp::Deref), true),
            PKind::Punct(Punct::Plus) => (Some(UnOp::Plus), true),
            PKind::Punct(Punct::Minus) => (Some(UnOp::Neg), true),
            PKind::Punct(Punct::Tilde) => (Some(UnOp::BitNot), true),
            PKind::Punct(Punct::Bang) => (Some(UnOp::LogNot), true),
            _ => (None, false),
        };
        if let Some(op) = op {
            self.bump();
            let operand = if via_cast { self.parse_cast()? } else { self.parse_unary()? };
            let span = tok.span.to(operand.span);
            return Ok(Expr { kind: ExprKind::Unary { op, operand: Box::new(operand), op_span: tok.span }, span });
        }
        match tok.kind {
            PKind::Kw(Kw::Sizeof) => {
                self.bump();
                if self.at_punct(Punct::LParen) && self.type_start_at(1) {
                    let open = self.bump();
                    let ty = self.parse_type_name()?;
                    self.expect_close(Punct::RParen, open.span)?;
                    let span = self.span_from(tok.span);
                    return Ok(Expr { kind: ExprKind::SizeofType(Box::new(ty)), span });
                }
                let operand = self.parse_unary()?;
                let span = tok.span.to(operand.span);
                Ok(Expr { kind: ExprKind::SizeofExpr(Box::new(operand)), span })
            }
            PKind::Kw(Kw::Alignof) => {
                self.bump();
                let open = self.span();
                if !self.eat_punct(Punct::LParen) {
                    self.error_here("expected '(' after '_Alignof'")?;
                }
                let ty = self.parse_type_name()?;
                self.expect_close(Punct::RParen, open)?;
                let span = self.span_from(tok.span);
                Ok(Expr { kind: ExprKind::AlignofType(Box::new(ty)), span })
            }
            PKind::Punct(Punct::AmpAmp) => {
                self.error_here("not yet supported: address-of-label (computed goto)")?;
                unreachable!()
            }
            _ => {
                let prim = self.parse_primary()?;
                self.parse_postfix(prim)
            }
        }
    }

    fn parse_postfix(&mut self, mut base: Expr) -> PResult<Expr> {
        loop {
            match self.kind() {
                PKind::Punct(Punct::LBracket) => {
                    let open = self.bump();
                    let index = self.parse_expr()?;
                    self.expect_close(Punct::RBracket, open.span)?;
                    let span = self.span_from(base.span);
                    base = Expr { kind: ExprKind::Index { base: Box::new(base), index: Box::new(index) }, span };
                }
                PKind::Punct(Punct::LParen) => {
                    let open = self.bump();
                    let mut args = Vec::new();
                    if !self.at_punct(Punct::RParen) {
                        loop {
                            args.push(self.parse_assign()?);
                            if !self.eat_punct(Punct::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect_close(Punct::RParen, open.span)?;
                    let span = self.span_from(base.span);
                    base = Expr { kind: ExprKind::Call { callee: Box::new(base), args }, span };
                }
                PKind::Punct(p @ (Punct::Dot | Punct::Arrow)) => {
                    self.bump();
                    let member = self.expect_ident("member name following '.' or '->'")?;
                    let span = self.span_from(base.span);
                    base = Expr {
                        kind: ExprKind::Member { base: Box::new(base), member, arrow: p == Punct::Arrow },
                        span,
                    };
                }
                PKind::Punct(p @ (Punct::PlusPlus | Punct::MinusMinus)) => {
                    let t = self.bump();
                    let op = if p == Punct::PlusPlus { UnOp::PostInc } else { UnOp::PostDec };
                    let span = self.span_from(base.span);
                    base = Expr { kind: ExprKind::Unary { op, operand: Box::new(base), op_span: t.span }, span };
                }
                _ => return Ok(base),
            }
        }
    }

    pub(crate) fn parse_primary(&mut self) -> PResult<Expr> {
        let tok = self.peek();
        match tok.kind {
            PKind::Int(s) => {
                self.bump();
                Ok(Expr { kind: ExprKind::IntLit(s), span: tok.span })
            }
            PKind::Float(s) => {
                self.bump();
                Ok(Expr { kind: ExprKind::FloatLit(s), span: tok.span })
            }
            PKind::Char(s) => {
                self.bump();
                Ok(Expr { kind: ExprKind::CharLit(s), span: tok.span })
            }
            PKind::Str(_) => self.parse_string_literal(),
            PKind::Ident(name) => {
                self.bump();
                Ok(Expr { kind: ExprKind::Ident(name), span: tok.span })
            }
            PKind::Punct(Punct::LParen) => {
                if self.peek_n(1).kind == PKind::Punct(Punct::LBrace) {
                    self.error_here("not yet supported: statement expressions")?;
                }
                let open = self.bump();
                let inner = self.parse_expr()?;
                self.expect_close(Punct::RParen, open.span)?;
                let span = self.span_from(open.span);
                Ok(Expr { kind: ExprKind::Paren(Box::new(inner)), span })
            }
            PKind::Kw(Kw::Generic) => self.parse_generic(),
            PKind::Kw(Kw::BuiltinVaArg) => {
                self.bump();
                let open = self.span();
                if !self.eat_punct(Punct::LParen) {
                    self.error_here("expected '(' after '__builtin_va_arg'")?;
                }
                let ap = self.parse_assign()?;
                if !self.eat_punct(Punct::Comma) {
                    self.error_here("expected ','")?;
                }
                let ty = self.parse_type_name()?;
                self.expect_close(Punct::RParen, open)?;
                let span = self.span_from(tok.span);
                Ok(Expr { kind: ExprKind::VaArg { ap: Box::new(ap), ty: Box::new(ty) }, span })
            }
            PKind::Kw(Kw::BuiltinOffsetof) => {
                self.bump();
                let open = self.span();
                if !self.eat_punct(Punct::LParen) {
                    self.error_here("expected '(' after '__builtin_offsetof'")?;
                }
                let ty = self.parse_type_name()?;
                if !self.eat_punct(Punct::Comma) {
                    self.error_here("expected ','")?;
                }
                let mut path = vec![OffsetofStep::Field(self.expect_ident("member designator")?)];
                loop {
                    if self.eat_punct(Punct::Dot) {
                        path.push(OffsetofStep::Field(self.expect_ident("member designator")?));
                    } else if self.at_punct(Punct::LBracket) {
                        let ob = self.bump().span;
                        let idx = self.parse_expr()?;
                        self.expect_close(Punct::RBracket, ob)?;
                        path.push(OffsetofStep::Index(idx));
                    } else {
                        break;
                    }
                }
                self.expect_close(Punct::RParen, open)?;
                let span = self.span_from(tok.span);
                Ok(Expr { kind: ExprKind::Offsetof { ty: Box::new(ty), path }, span })
            }
            _ => {
                self.error_here("expected expression")?;
                unreachable!()
            }
        }
    }

    fn parse_generic(&mut self) -> PResult<Expr> {
        let kw = self.bump();
        let open = self.span();
        if !self.eat_punct(Punct::LParen) {
            self.error_here("expected '(' after '_Generic'")?;
        }
        let controlling = self.parse_assign()?;
        let mut assocs = Vec::new();
        while self.eat_punct(Punct::Comma) {
            let ty = if self.eat_kw(Kw::Default) { None } else { Some(self.parse_type_name()?) };
            if !self.eat_punct(Punct::Colon) {
                self.error_here("expected ':' in _Generic association")?;
            }
            let expr = self.parse_assign()?;
            assocs.push(GenericAssoc { ty, expr });
        }
        self.expect_close(Punct::RParen, open)?;
        let span = self.span_from(kw.span);
        Ok(Expr { kind: ExprKind::Generic { controlling: Box::new(controlling), assocs }, span })
    }

    /// Adjacent string literals are concatenated (translation phase 6).
    fn parse_string_literal(&mut self) -> PResult<Expr> {
        let first = self.peek().span;
        let mut kind = StrKind::Plain;
        let mut units: Vec<u32> = Vec::new();
        while let PKind::Str(sym) = self.kind() {
            let tok = self.bump();
            let mut issues = Vec::new();
            let Some(lit) = literal::parse_string(sym.as_str(), &mut issues) else {
                let _ = self.error_at(tok.span, "invalid string literal");
                continue;
            };
            for i in issues {
                let lo = tok.span.lo.saturating_add(i.offset as u32);
                let sp = Span::new(tok.span.file, lo, (lo + i.len as u32).min(tok.span.hi.max(lo)));
                if i.is_error {
                    let _ = self.error_at(sp, i.message);
                } else {
                    self.sess.diags.warn(Warn::UnknownEscape, sp, i.message);
                }
            }
            if kind == StrKind::Plain {
                kind = lit.kind;
            } else if lit.kind != StrKind::Plain && lit.kind != kind {
                let _ = self.error_at(tok.span, "unsupported non-standard concatenation of string literals");
            }
            units.extend(lit.units);
        }
        let span = self.span_from(first);
        Ok(Expr { kind: ExprKind::StrLit { kind, units }, span })
    }
}
