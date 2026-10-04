//! Statement checking.

use super::consteval::{self, eval_int};
use super::expr::ConvCtx;
use super::*;

/// Does evaluating `e` do anything besides produce a value?
fn has_side_effects(e: &HExpr) -> bool {
    use HExprKind::*;
    match &e.kind {
        Assign(..)
        | CompoundAssign { .. }
        | IncDec { .. }
        | Call { .. }
        | VaStart(_)
        | VaEnd(_)
        | VaCopy(..)
        | VaArg(_)
        | Trap
        | Error => true,
        CompoundLit { .. } => true,
        Int(_) | Float(_) | Str(_) | Local(_) | Global(_) | VlaSizeof(_) => false,
        Deref(x) | AddrOf(x) | Unary(_, x) | Member(x, _) => has_side_effects(x),
        Cast(_, x) => has_side_effects(x),
        Binary(_, a, b) | LogAnd(a, b) | LogOr(a, b) => has_side_effects(a) || has_side_effects(b),
        PtrAdd { ptr, idx, .. } => has_side_effects(ptr) || has_side_effects(idx),
        PtrDiff { l, r, .. } => has_side_effects(l) || has_side_effects(r),
        Cond(c, t, f) => has_side_effects(c) || has_side_effects(t) || has_side_effects(f),
        Comma(a, b) => has_side_effects(a) || has_side_effects(b),
    }
}

impl<'a> Sema<'a> {
    pub(crate) fn block_items(&mut self, items: &[BlockItem]) -> Vec<HStmt> {
        let mut out = Vec::new();
        for it in items {
            match it {
                BlockItem::Decl(d) => out.extend(self.block_declaration(d)),
                BlockItem::Stmt(s) => out.push(self.stmt(s)),
            }
        }
        out
    }

    fn mk(&self, kind: HStmtKind, span: Span) -> HStmt {
        HStmt { kind, span }
    }

    /// A controlling expression: must be scalar.
    fn condition(&mut self, e: &Expr, what: &str) -> HExpr {
        // `if (a = b)` is almost always a typo.
        if matches!(e.kind, ExprKind::Assign { op: None, .. }) {
            if let ExprKind::Assign { op_span, .. } = &e.kind {
                self.emit(
                    Diagnostic::warning(
                        Warn::Parentheses,
                        *op_span,
                        "using the result of an assignment as a condition without parentheses",
                    )
                    .with_note(e.span, "place parentheses around the assignment to silence this warning"),
                );
            }
        }
        let h = self.rexpr(e);
        if matches!(h.kind, HExprKind::Error) {
            return h;
        }
        if !self.types.is_scalar(h.ty) {
            let s = self.show(h.ty);
            self.error(e.span, format!("statement requires expression of scalar type ('{}' invalid)", s));
            let _ = what;
            return self.err_expr(e.span);
        }
        h
    }

    fn get_label(&mut self, name: Symbol, span: Span) -> usize {
        let f = self.f.as_mut().expect("function");
        if let Some(&i) = f.label_map.get(&name) {
            return i;
        }
        let i = f.labels.len();
        f.labels.push(LabelInfo { id: LabelId(i as u32), name, defined: None, used: false, first_use: span });
        f.label_map.insert(name, i);
        i
    }

    pub(crate) fn stmt(&mut self, s: &Stmt) -> HStmt {
        let span = s.span;
        match &s.kind {
            StmtKind::Empty => self.mk(HStmtKind::Empty, span),
            StmtKind::Expr(e) => {
                let h = self.expr(e);
                let h = if self.types.is_aggregate(h.ty) || self.types.is_function(h.ty) { h } else { self.rvalue(h) };
                if !matches!(h.kind, HExprKind::Error)
                    && !has_side_effects(&h)
                    && !matches!(h.kind, HExprKind::Cast(CastKind::ToVoid, _))
                    && !matches!(unparen_ast(e).kind, ExprKind::Cast { .. })
                {
                    self.emit(Diagnostic::warning(Warn::UnusedValue, e.span, "expression result unused"));
                }
                self.mk(HStmtKind::Expr(h), span)
            }
            StmtKind::Compound(items) => {
                self.push_scope();
                // only a block that declares a VLA itself saves and restores the stack: a
                // `switch` body, say, can be entered through its case labels
                let outer = self.f.as_mut().map(|f| std::mem::replace(&mut f.vla_direct, false));
                let stmts = self.block_items(items);
                self.pop_scope();
                let has_vla = match self.f.as_mut() {
                    Some(f) => std::mem::replace(&mut f.vla_direct, outer.unwrap_or(false)),
                    None => false,
                };
                if has_vla {
                    self.mk(HStmtKind::VlaScope(stmts), span)
                } else {
                    self.mk(HStmtKind::Block(stmts), span)
                }
            }
            StmtKind::If { cond, then, els } => {
                if matches!(then.kind, StmtKind::Empty) {
                    self.emit(
                        Diagnostic::warning(Warn::EmptyBody, then.span, "if statement has empty body")
                            .with_note(then.span, "put the semicolon on a separate line to silence this warning"),
                    );
                }
                let c = self.condition(cond, "if");
                let t = self.scoped_stmt(then);
                let e = els.as_ref().map(|x| Box::new(self.scoped_stmt(x)));
                self.mk(HStmtKind::If(c, Box::new(t), e), span)
            }
            StmtKind::While { cond, body } => {
                let c = self.condition(cond, "while");
                let b = self.loop_body(body);
                self.mk(HStmtKind::While(c, Box::new(b)), span)
            }
            StmtKind::DoWhile { body, cond } => {
                let b = self.loop_body(body);
                let c = self.condition(cond, "while");
                self.mk(HStmtKind::DoWhile(Box::new(b), c), span)
            }
            StmtKind::For { init, cond, step, body } => {
                self.push_scope();
                let mut init_stmts = Vec::new();
                match init {
                    ForInit::None => {}
                    ForInit::Expr(e) => {
                        let h = self.rexpr(e);
                        init_stmts.push(self.mk(HStmtKind::Expr(h), e.span));
                    }
                    ForInit::Decl(d) => init_stmts.extend(self.block_declaration(d)),
                }
                let c = cond.as_ref().map(|c| self.condition(c, "for"));
                let st = step.as_ref().map(|e| self.rexpr(e));
                let b = self.loop_body(body);
                self.pop_scope();
                self.mk(HStmtKind::For { init: init_stmts, cond: c, step: st, body: Box::new(b) }, span)
            }
            StmtKind::Switch { cond, body } => self.switch_stmt(cond, body, span),
            StmtKind::Case { value, body } => self.case_stmt(value, body, span),
            StmtKind::Default { body } => {
                let f = self.f.as_mut().expect("function");
                let id = CaseId(f.next_case);
                let Some(sw) = f.switches.last_mut() else {
                    self.error(span, "'default' statement not in switch statement");
                    return self.stmt(body);
                };
                f.next_case += 1;
                let prev = sw.default;
                if prev.is_none() {
                    sw.default = Some((id, span));
                }
                if let Some((_, psp)) = prev {
                    self.emit(
                        Diagnostic::error(span, "multiple default labels in one switch")
                            .with_note(psp, "previous default label is here"),
                    );
                }
                let b = self.stmt(body);
                self.mk(HStmtKind::Block(vec![HStmt { kind: HStmtKind::CaseLabel(id), span }, b]), span)
            }
            StmtKind::Break => {
                if self.f.as_ref().is_none_or(|f| f.breakables == 0) {
                    self.error(span, "'break' statement not in loop or switch statement");
                }
                self.mk(HStmtKind::Break, span)
            }
            StmtKind::Continue => {
                if self.f.as_ref().is_none_or(|f| f.loops == 0) {
                    self.error(span, "'continue' statement not in loop statement");
                }
                self.mk(HStmtKind::Continue, span)
            }
            StmtKind::Return(v) => self.return_stmt(v.as_ref(), span),
            StmtKind::Goto(l) => {
                let i = self.get_label(l.name, l.span);
                let f = self.f.as_mut().unwrap();
                f.labels[i].used = true;
                let id = f.labels[i].id;
                self.mk(HStmtKind::Goto(id), span)
            }
            StmtKind::Label { name, body } => {
                let i = self.get_label(name.name, name.span);
                let f = self.f.as_mut().unwrap();
                let id = f.labels[i].id;
                if let Some(prev) = f.labels[i].defined {
                    self.emit(
                        Diagnostic::error(name.span, format!("redefinition of label '{}'", name.name))
                            .with_note(prev, "previous definition is here"),
                    );
                } else {
                    f.labels[i].defined = Some(name.span);
                }
                let b = self.stmt(body);
                self.mk(HStmtKind::Block(vec![HStmt { kind: HStmtKind::Label(id), span }, b]), span)
            }
            StmtKind::StaticAssert(sa) => {
                self.static_assert(sa);
                self.mk(HStmtKind::Empty, span)
            }
        }
    }

    /// The body of an `if`/`else`: its own scope.
    fn scoped_stmt(&mut self, s: &Stmt) -> HStmt {
        self.push_scope();
        let r = self.stmt(s);
        self.pop_scope();
        r
    }

    fn loop_body(&mut self, body: &Stmt) -> HStmt {
        {
            let f = self.f.as_mut().unwrap();
            f.loops += 1;
            f.breakables += 1;
        }
        let b = self.scoped_stmt(body);
        let f = self.f.as_mut().unwrap();
        f.loops -= 1;
        f.breakables -= 1;
        b
    }

    fn switch_stmt(&mut self, cond: &Expr, body: &Stmt, span: Span) -> HStmt {
        let c = self.rexpr(cond);
        let mut c = c;
        let mut cty = self.types.p.int;
        if !matches!(c.kind, HExprKind::Error) {
            if !self.types.is_integer(c.ty) {
                let s = self.show(c.ty);
                self.error(cond.span, format!("statement requires expression of integer type ('{}' invalid)", s));
                c = self.err_expr(cond.span);
            } else {
                cty = self.types.promote(c.ty);
                c = self.cast_to(c, cty);
            }
        }
        {
            let f = self.f.as_mut().unwrap();
            f.switches.push(SwitchCtx { ty: cty, cases: Vec::new(), default: None });
            f.breakables += 1;
        }
        let b = self.scoped_stmt(body);
        let f = self.f.as_mut().unwrap();
        f.breakables -= 1;
        let sw = f.switches.pop().unwrap();
        let cases: Vec<(i64, CaseId)> = sw.cases.iter().map(|(v, id, _)| (*v, *id)).collect();
        self.mk(HStmtKind::Switch { cond: c, body: Box::new(b), cases, default: sw.default.map(|(id, _)| id) }, span)
    }

    fn case_stmt(&mut self, value: &Expr, body: &Stmt, span: Span) -> HStmt {
        let h = self.rexpr(value);
        if self.f.as_ref().unwrap().switches.is_empty() {
            self.error(span, "'case' statement not in switch statement");
            return self.stmt(body);
        }
        let cty = self.f.as_ref().unwrap().switches.last().unwrap().ty;
        let id;
        if matches!(h.kind, HExprKind::Error) {
            let f = self.f.as_mut().unwrap();
            id = CaseId(f.next_case);
            f.next_case += 1;
        } else {
            match eval_int(&self.types, &h) {
                Some(v) => {
                    // Convert the case value to the promoted type of the switch.
                    let v = consteval::norm(&self.types, v, cty) as i64;
                    let f = self.f.as_mut().unwrap();
                    id = CaseId(f.next_case);
                    f.next_case += 1;
                    let sw = f.switches.last_mut().unwrap();
                    if let Some((_, _, prev)) = sw.cases.iter().find(|(pv, _, _)| *pv == v) {
                        let prev = *prev;
                        self.emit(
                            Diagnostic::error(value.span, format!("duplicate case value '{}'", v))
                                .with_note(prev, "previous case is here"),
                        );
                    } else {
                        sw.cases.push((v, id, value.span));
                    }
                }
                None => {
                    self.error(value.span, "expression is not an integer constant expression");
                    let f = self.f.as_mut().unwrap();
                    id = CaseId(f.next_case);
                    f.next_case += 1;
                }
            }
        }
        let b = self.stmt(body);
        self.mk(HStmtKind::Block(vec![HStmt { kind: HStmtKind::CaseLabel(id), span }, b]), span)
    }

    fn return_stmt(&mut self, v: Option<&Expr>, span: Span) -> HStmt {
        let (ret, fname) = {
            let f = self.f.as_ref().unwrap();
            (f.ret, f.name)
        };
        let is_void = self.types.is_void(ret);
        match v {
            None => {
                if !is_void {
                    self.warn(Warn::ReturnType, span, format!("non-void function '{}' should return a value", fname));
                }
                self.mk(HStmtKind::Return(None), span)
            }
            Some(e) => {
                let h = self.rexpr(e);
                if is_void {
                    if !matches!(h.kind, HExprKind::Error) && !self.types.is_void(h.ty) {
                        self.warn(
                            Warn::ReturnType,
                            e.span,
                            format!("void function '{}' should not return a value", fname),
                        );
                    }
                    return self.mk(HStmtKind::Return(Some(h)), span);
                }
                if matches!(h.kind, HExprKind::Error) {
                    return self.mk(HStmtKind::Return(None), span);
                }
                if self.types.is_void(h.ty) {
                    self.error(e.span, "returning void expression from a function with non-void result type");
                    return self.mk(HStmtKind::Return(None), span);
                }
                // Returning the address of a local variable is almost always a bug.
                if let Some(name) = self.local_address_name(&h) {
                    self.warn(
                        Warn::ReturnStackAddress,
                        e.span,
                        format!("address of stack memory associated with local variable '{}' returned", name),
                    );
                }
                let v = self.convert(h, ret, ConvCtx::Return, e.span);
                self.mk(HStmtKind::Return(Some(v)), span)
            }
        }
    }

    /// `&local` or a local array decayed to a pointer, looking through no-op casts.
    fn local_address_name(&self, e: &HExpr) -> Option<Symbol> {
        match &e.kind {
            HExprKind::AddrOf(p) | HExprKind::Cast(CastKind::ArrayToPointer, p) => match &p.kind {
                HExprKind::Local(id) => self.f.as_ref().map(|f| f.locals[id.0 as usize].name),
                _ => None,
            },
            HExprKind::Cast(CastKind::NoOp, inner) => self.local_address_name(inner),
            _ => None,
        }
    }
}

fn unparen_ast(e: &Expr) -> &Expr {
    match &e.kind {
        ExprKind::Paren(i) => unparen_ast(i),
        _ => e,
    }
}
