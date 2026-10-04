//! Statements and compound blocks.

use super::decl::SpecCtx;
use super::*;

impl<'a> Parser<'a> {
    /// `{ block-item* }` — always opens a typedef scope.
    pub(crate) fn parse_compound(&mut self) -> PResult<Stmt> {
        let open = self.span();
        if !self.at_punct(Punct::LBrace) {
            self.error_here("expected '{'")?;
        }
        self.bump();
        self.push_scope();
        let mut items = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            let before = self.pos;
            match self.parse_block_item() {
                Ok(item) => items.push(item),
                Err(()) => self.sync_stmt(),
            }
            if self.pos == before {
                self.bump();
            }
        }
        self.pop_scope();
        self.expect_close(Punct::RBrace, open)?;
        Ok(Stmt { kind: StmtKind::Compound(items), span: self.span_from(open) })
    }

    fn parse_block_item(&mut self) -> PResult<BlockItem> {
        if self.at_kw(Kw::StaticAssert) {
            let sa = self.parse_static_assert()?;
            let span = sa.span;
            return Ok(BlockItem::Stmt(Stmt { kind: StmtKind::StaticAssert(sa), span }));
        }
        if self.is_declaration_start() {
            return Ok(BlockItem::Decl(self.parse_declaration(SpecCtx::Block)?));
        }
        Ok(BlockItem::Stmt(self.parse_stmt()?))
    }

    /// Parse the "body" of a control statement. A declaration there is not
    /// valid C, but is diagnosed clearly instead of cascading.
    fn parse_sub_stmt(&mut self) -> PResult<Stmt> {
        self.push_scope();
        let r = self.parse_stmt();
        self.pop_scope();
        r
    }

    pub(crate) fn parse_stmt(&mut self) -> PResult<Stmt> {
        let tok = self.peek();
        match tok.kind {
            PKind::Punct(Punct::LBrace) => self.parse_compound(),
            PKind::Punct(Punct::Semi) => {
                self.bump();
                Ok(Stmt { kind: StmtKind::Empty, span: tok.span })
            }
            PKind::Kw(Kw::If) => {
                self.bump();
                let cond = self.parse_paren_cond("if")?;
                let then = Box::new(self.parse_sub_stmt()?);
                let els = if self.eat_kw(Kw::Else) { Some(Box::new(self.parse_sub_stmt()?)) } else { None };
                Ok(Stmt { kind: StmtKind::If { cond, then, els }, span: self.span_from(tok.span) })
            }
            PKind::Kw(Kw::While) => {
                self.bump();
                let cond = self.parse_paren_cond("while")?;
                let body = Box::new(self.parse_sub_stmt()?);
                Ok(Stmt { kind: StmtKind::While { cond, body }, span: self.span_from(tok.span) })
            }
            PKind::Kw(Kw::Do) => {
                self.bump();
                let body = Box::new(self.parse_sub_stmt()?);
                if !self.eat_kw(Kw::While) {
                    self.error_here("expected 'while' in do/while statement")?;
                }
                let cond = self.parse_paren_cond("while")?;
                self.expect_semi("after do/while statement");
                Ok(Stmt { kind: StmtKind::DoWhile { body, cond }, span: self.span_from(tok.span) })
            }
            PKind::Kw(Kw::For) => self.parse_for(),
            PKind::Kw(Kw::Switch) => {
                self.bump();
                let cond = self.parse_paren_cond("switch")?;
                let body = Box::new(self.parse_sub_stmt()?);
                Ok(Stmt { kind: StmtKind::Switch { cond, body }, span: self.span_from(tok.span) })
            }
            PKind::Kw(Kw::Case) => {
                self.bump();
                let value = self.parse_cond()?;
                if self.at_punct(Punct::Ellipsis) {
                    self.error_here("not yet supported: case ranges")?;
                }
                if !self.eat_punct(Punct::Colon) {
                    self.error_after_prev("expected ':' after 'case'", ":")?;
                }
                let body = Box::new(self.parse_label_body()?);
                Ok(Stmt { kind: StmtKind::Case { value, body }, span: self.span_from(tok.span) })
            }
            PKind::Kw(Kw::Default) => {
                self.bump();
                if !self.eat_punct(Punct::Colon) {
                    self.error_after_prev("expected ':' after 'default'", ":")?;
                }
                let body = Box::new(self.parse_label_body()?);
                Ok(Stmt { kind: StmtKind::Default { body }, span: self.span_from(tok.span) })
            }
            PKind::Kw(Kw::Break) => {
                self.bump();
                self.expect_semi("after break statement");
                Ok(Stmt { kind: StmtKind::Break, span: self.span_from(tok.span) })
            }
            PKind::Kw(Kw::Continue) => {
                self.bump();
                self.expect_semi("after continue statement");
                Ok(Stmt { kind: StmtKind::Continue, span: self.span_from(tok.span) })
            }
            PKind::Kw(Kw::Return) => {
                self.bump();
                let value = if self.at_punct(Punct::Semi) { None } else { Some(self.parse_expr()?) };
                self.expect_semi("after return statement");
                Ok(Stmt { kind: StmtKind::Return(value), span: self.span_from(tok.span) })
            }
            PKind::Kw(Kw::Goto) => {
                self.bump();
                if self.at_punct(Punct::Star) {
                    self.error_here("not yet supported: computed goto")?;
                }
                let label = self.expect_ident("identifier")?;
                self.expect_semi("after goto statement");
                Ok(Stmt { kind: StmtKind::Goto(label), span: self.span_from(tok.span) })
            }
            PKind::Kw(Kw::Asm) => {
                self.error_here("not yet supported: inline assembly")?;
                unreachable!()
            }
            PKind::Ident(name) if self.peek_n(1).kind == PKind::Punct(Punct::Colon) => {
                self.bump();
                self.bump();
                let body = Box::new(self.parse_label_body()?);
                Ok(Stmt {
                    kind: StmtKind::Label { name: Ident { name, span: tok.span }, body },
                    span: self.span_from(tok.span),
                })
            }
            _ => {
                let e = self.parse_expr()?;
                self.expect_semi("after expression");
                let span = self.span_from(tok.span);
                Ok(Stmt { kind: StmtKind::Expr(e), span })
            }
        }
    }

    /// The statement after `label:` / `case x:` / `default:`. C11 requires a
    /// statement, so a label directly before `}` is diagnosed by sema as an
    /// extension; here an empty statement stands in for it.
    fn parse_label_body(&mut self) -> PResult<Stmt> {
        if self.at_punct(Punct::RBrace) {
            let at = self.span().lo;
            return Ok(Stmt { kind: StmtKind::Empty, span: Span::new(self.span().file, at, at) });
        }
        if self.is_declaration_start() {
            // `label: int x;` is not valid before C23; parse it anyway so scoping stays sane.
            let d = self.parse_declaration(SpecCtx::Block)?;
            let span = d.span;
            let _ = self.error_at(span, "a label can only be part of a statement and a declaration is not a statement");
            return Ok(Stmt { kind: StmtKind::Empty, span });
        }
        self.parse_stmt()
    }

    /// `( expression )` after `if` / `while` / `switch`.
    fn parse_paren_cond(&mut self, kw: &str) -> PResult<Expr> {
        let open = self.span();
        if !self.eat_punct(Punct::LParen) {
            self.error_here(format!("expected '(' after '{}'", kw))?;
        }
        let e = self.parse_expr()?;
        self.expect_close(Punct::RParen, open)?;
        Ok(e)
    }

    fn parse_for(&mut self) -> PResult<Stmt> {
        let kw = self.bump();
        let open = self.span();
        if !self.eat_punct(Punct::LParen) {
            self.error_here("expected '(' after 'for'")?;
        }
        // The init declaration's scope covers the whole statement.
        self.push_scope();
        let r = self.parse_for_rest(kw.span, open);
        self.pop_scope();
        r
    }

    fn parse_for_rest(&mut self, kw_span: Span, open: Span) -> PResult<Stmt> {
        let init = if self.at_punct(Punct::Semi) {
            self.bump();
            ForInit::None
        } else if self.is_declaration_start() {
            ForInit::Decl(self.parse_declaration(SpecCtx::Block)?)
        } else {
            let e = self.parse_expr()?;
            self.expect_semi("in 'for' statement specifier");
            ForInit::Expr(e)
        };
        let cond = if self.at_punct(Punct::Semi) { None } else { Some(self.parse_expr()?) };
        self.expect_semi("in 'for' statement specifier");
        let step = if self.at_punct(Punct::RParen) { None } else { Some(self.parse_expr()?) };
        self.expect_close(Punct::RParen, open)?;
        let body = Box::new(self.parse_sub_stmt()?);
        Ok(Stmt { kind: StmtKind::For { init, cond, step, body }, span: self.span_from(kw_span) })
    }
}
