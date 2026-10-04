//! Expression checking: typing, implicit conversions, lvalue handling.

use super::consteval::{self, eval, eval_int};
use super::*;
use crate::literal;

#[derive(Clone, Copy, Debug)]
pub(crate) enum ConvCtx {
    Assign,
    Init,
    Arg,
    Return,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModKind {
    Assign,
    Inc,
    Dec,
}

/// Edit distance where swapping two adjacent characters counts as one edit
/// (optimal string alignment), so `cuont` is one edit from `count`.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

impl<'a> Sema<'a> {
    // ───────────────────────────── small constructors ─────────────────────────────

    pub(crate) fn err_expr(&self, span: Span) -> HExpr {
        HExpr::new(HExprKind::Error, self.types.p.int, span)
    }

    fn int_lit(&self, v: u64, ty: Ty, span: Span) -> HExpr {
        HExpr::new(HExprKind::Int(consteval::norm(&self.types, v, ty)), ty, span)
    }

    pub(crate) fn is_error(e: &HExpr) -> bool {
        matches!(e.kind, HExprKind::Error)
    }

    // ───────────────────────────── conversions ─────────────────────────────

    /// Lvalue-to-rvalue conversion and array/function decay.
    pub(crate) fn rvalue(&mut self, e: HExpr) -> HExpr {
        if Self::is_error(&e) {
            return e;
        }
        let span = e.span;
        match self.types.kind(e.ty).clone() {
            TyKind::Array(elem, _) if e.is_place() => {
                let pty = self.types.ptr(elem);
                HExpr::new(HExprKind::Cast(CastKind::ArrayToPointer, Box::new(e)), pty, span)
            }
            TyKind::Func(_) => {
                let pty = self.types.ptr(e.ty);
                HExpr::new(HExprKind::Cast(CastKind::FunctionToPointer, Box::new(e)), pty, span)
            }
            _ if e.is_place() => {
                let uty = self.types.unqual(e.ty);
                HExpr::new(HExprKind::Cast(CastKind::LValueToRValue, Box::new(e)), uty, span)
            }
            _ => e,
        }
    }

    pub(crate) fn rexpr(&mut self, e: &Expr) -> HExpr {
        let h = self.expr(e);
        self.rvalue(h)
    }

    /// Replace a constant-valued arithmetic expression by a literal.
    fn fold(&self, e: HExpr) -> HExpr {
        if matches!(e.kind, HExprKind::Int(_) | HExprKind::Float(_) | HExprKind::Error) {
            return e;
        }
        if !self.types.is_arithmetic(e.ty) {
            return e;
        }
        match eval(&self.types, &e) {
            Some(ConstVal::Int(v)) if self.types.is_integer(e.ty) => HExpr::new(HExprKind::Int(v), e.ty, e.span),
            Some(ConstVal::Float(f)) if self.types.is_floating(e.ty) => HExpr::new(HExprKind::Float(f), e.ty, e.span),
            _ => e,
        }
    }

    fn cast_kind(&self, from: Ty, to: Ty) -> CastKind {
        let t = &self.types;
        if t.is_bool(to) {
            return CastKind::ToBool;
        }
        match (
            t.is_integer(from),
            t.is_floating(from),
            t.is_pointer(from),
            t.is_integer(to),
            t.is_floating(to),
            t.is_pointer(to),
        ) {
            (true, _, _, true, _, _) => CastKind::IntToInt,
            (true, _, _, _, true, _) => CastKind::IntToFloat,
            (_, true, _, true, _, _) => CastKind::FloatToInt,
            (_, true, _, _, true, _) => CastKind::FloatToFloat,
            (_, _, true, true, _, _) => CastKind::PtrToInt,
            (true, _, _, _, _, true) => CastKind::IntToPtr,
            (_, _, true, _, _, true) => CastKind::NoOp,
            _ => CastKind::NoOp,
        }
    }

    /// Convert between scalar types without any diagnostics.
    pub(crate) fn cast_to(&mut self, e: HExpr, to: Ty) -> HExpr {
        if Self::is_error(&e) {
            return e;
        }
        let to = self.types.unqual(to);
        if e.ty == to {
            return e;
        }
        let kind = self.cast_kind(e.ty, to);
        let span = e.span;
        let r = HExpr::new(HExprKind::Cast(kind, Box::new(e)), to, span);
        self.fold(r)
    }

    fn is_null_ptr_const(&self, e: &HExpr) -> bool {
        let int_zero = || matches!(eval(&self.types, e), Some(ConstVal::Int(0)));
        if self.types.is_integer(e.ty) {
            return int_zero();
        }
        if let TyKind::Ptr(p) = self.types.kind(e.ty) {
            if self.types.is_void(*p) && self.types.quals(*p).is_empty() {
                return int_zero();
            }
        }
        false
    }

    fn conv_phrase(&self, ctx: ConvCtx, from: Ty, to: Ty) -> String {
        let (f, t) = (self.show(from), self.show(to));
        match ctx {
            ConvCtx::Assign => format!("assigning to '{}' from '{}'", t, f),
            ConvCtx::Init => format!("initializing '{}' with an expression of type '{}'", t, f),
            ConvCtx::Arg => format!("passing '{}' to parameter of type '{}'", f, t),
            ConvCtx::Return => format!("returning '{}' from a function with result type '{}'", f, t),
        }
    }

    fn conv_bad_phrase(&self, ctx: ConvCtx, from: Ty, to: Ty) -> String {
        let (f, t) = (self.show(from), self.show(to));
        match ctx {
            ConvCtx::Assign => format!("assigning to '{}' from incompatible type '{}'", t, f),
            ConvCtx::Init => format!("initializing '{}' with an expression of incompatible type '{}'", t, f),
            ConvCtx::Arg => format!("passing '{}' to parameter of incompatible type '{}'", f, t),
            ConvCtx::Return => format!("returning '{}' from a function with incompatible result type '{}'", f, t),
        }
    }

    /// Implicit conversion of an rvalue to `to` (assignment, initialization,
    /// argument passing, return), with the diagnostics C requires.
    pub(crate) fn convert(&mut self, e: HExpr, to: Ty, ctx: ConvCtx, span: Span) -> HExpr {
        if Self::is_error(&e) {
            return e;
        }
        let to = self.types.unqual(to);
        let from = e.ty;
        if from == to {
            return e;
        }
        let t = &self.types;
        // arithmetic <-> arithmetic
        if t.is_arithmetic(from) && t.is_arithmetic(to) {
            self.check_narrowing(&e, to, span);
            return self.cast_to(e, to);
        }
        // pointer -> pointer
        if t.is_pointer(from) && t.is_pointer(to) {
            let (pf, pt) = (t.pointee(from).unwrap(), t.pointee(to).unwrap());
            let (qf, qt) = (t.quals(pf), t.quals(pt));
            let ok_types = t.is_void(pf) || t.is_void(pt) || t.compatible_unqual(pf, pt);
            if !ok_types {
                let msg = format!("incompatible pointer types {}", self.conv_phrase(ctx, from, to));
                self.warn(Warn::IncompatiblePointerTypes, span, msg);
            } else if !qt.contains(qf) {
                let msg = format!("{} discards qualifiers", self.conv_phrase(ctx, from, to));
                self.warn(Warn::DiscardedQualifiers, span, msg);
            }
            return self.cast_to(e, to);
        }
        // integer -> pointer
        if t.is_integer(from) && t.is_pointer(to) {
            if !self.is_null_ptr_const(&e) {
                let msg = format!("incompatible integer to pointer conversion {}", self.conv_phrase(ctx, from, to));
                self.warn(Warn::IntConversion, span, msg);
            }
            return self.cast_to(e, to);
        }
        // pointer -> integer / bool
        if t.is_pointer(from) && t.is_integer(to) {
            if !t.is_bool(to) {
                let msg = format!("incompatible pointer to integer conversion {}", self.conv_phrase(ctx, from, to));
                self.warn(Warn::IntConversion, span, msg);
            }
            return self.cast_to(e, to);
        }
        // struct/union to the same struct/union
        if let (TyKind::Record(a), TyKind::Record(b)) = (t.kind(from), t.kind(to)) {
            if a == b {
                return HExpr { ty: to, ..e };
            }
        }
        let msg = self.conv_bad_phrase(ctx, from, to);
        self.error(span, msg);
        self.err_expr(span)
    }

    // ── -Wconversion support ──

    /// Conservative range of values an integer expression can take.
    fn expr_range(&self, e: &HExpr) -> (i128, i128) {
        let full = |t: Ty| self.types.int_range(t);
        let ty = e.ty;
        if !self.types.is_integer(ty) {
            return (i128::MIN / 4, i128::MAX / 4);
        }
        let (tmin, tmax) = full(ty);
        let clamp = |r: (i128, i128)| if r.0 >= tmin && r.1 <= tmax { r } else { (tmin, tmax) };
        match &e.kind {
            HExprKind::Int(v) => {
                let x = if self.types.is_signed(ty) { *v as i64 as i128 } else { *v as i128 };
                (x, x)
            }
            HExprKind::Cast(CastKind::ToBool, _) => (0, 1),
            HExprKind::Cast(CastKind::IntToInt, inner) => {
                let r = self.expr_range(inner);
                clamp(r)
            }
            HExprKind::Cast(CastKind::LValueToRValue, inner) => {
                if let HExprKind::Member(_, m) = &inner.kind {
                    if let Some(b) = m.bit {
                        let w = b.width;
                        if self.types.is_signed(ty) {
                            return (-(1i128 << (w - 1)), (1i128 << (w - 1)) - 1);
                        }
                        return (0, (1i128 << w) - 1);
                    }
                }
                (tmin, tmax)
            }
            HExprKind::Binary(op, l, r) => {
                if op.is_comparison() {
                    return (0, 1);
                }
                let (a, b) = (self.expr_range(l), self.expr_range(r));
                let res = match op {
                    BinKind::Add => (a.0 + b.0, a.1 + b.1),
                    BinKind::Sub => (a.0 - b.1, a.1 - b.0),
                    BinKind::Mul => {
                        let p = [a.0 * b.0, a.0 * b.1, a.1 * b.0, a.1 * b.1];
                        (*p.iter().min().unwrap(), *p.iter().max().unwrap())
                    }
                    BinKind::And => {
                        if a.0 >= 0 && b.0 >= 0 {
                            (0, a.1.min(b.1))
                        } else if a.0 >= 0 {
                            (0, a.1)
                        } else if b.0 >= 0 {
                            (0, b.1)
                        } else {
                            (tmin, tmax)
                        }
                    }
                    BinKind::Or | BinKind::Xor => {
                        if a.0 >= 0 && b.0 >= 0 {
                            let m = a.1.max(b.1);
                            (0, ((m + 1) as u128).next_power_of_two() as i128 - 1)
                        } else {
                            (tmin, tmax)
                        }
                    }
                    BinKind::Rem => {
                        let bm = b.0.abs().max(b.1.abs());
                        if a.0 >= 0 {
                            (0, a.1.min((bm - 1).max(0)))
                        } else {
                            (-(bm - 1).max(0), (bm - 1).max(0))
                        }
                    }
                    BinKind::Div => {
                        let m = a.0.abs().max(a.1.abs());
                        if a.0 >= 0 && b.0 > 0 {
                            (0, a.1)
                        } else {
                            (-m, m)
                        }
                    }
                    BinKind::Shr => match eval_int(&self.types, r) {
                        Some(k) if k < 64 => (a.0 >> k, a.1 >> k),
                        _ => (tmin, tmax),
                    },
                    BinKind::Shl => match eval_int(&self.types, r) {
                        Some(k) if k < 64 => (a.0 << k, a.1 << k),
                        _ => (tmin, tmax),
                    },
                    _ => (tmin, tmax),
                };
                clamp(res)
            }
            HExprKind::Unary(UnKind::LogNot, _) | HExprKind::LogAnd(..) | HExprKind::LogOr(..) => (0, 1),
            HExprKind::Unary(UnKind::Neg, x) => {
                let r = self.expr_range(x);
                clamp((-r.1, -r.0))
            }
            HExprKind::Cond(_, t, f) => {
                let (a, b) = (self.expr_range(t), self.expr_range(f));
                clamp((a.0.min(b.0), a.1.max(b.1)))
            }
            HExprKind::Comma(_, x) => self.expr_range(x),
            _ => (tmin, tmax),
        }
    }

    fn check_narrowing(&mut self, e: &HExpr, to: Ty, span: Span) {
        let from = e.ty;
        let t = &self.types;
        if t.is_bool(to) {
            return;
        }
        if t.is_integer(from) && t.is_integer(to) {
            let (tmin, tmax) = t.int_range(to);
            let (rmin, rmax) = self.expr_range(e);
            if rmin >= tmin && rmax <= tmax {
                return;
            }
            if let Some(v) = eval_int(&self.types, e) {
                let from_signed = self.types.is_signed(from);
                let val: i128 = if from_signed { v as i64 as i128 } else { v as i128 };
                let newv = consteval::norm(&self.types, v, to);
                let nv: i128 = if self.types.is_signed(to) { newv as i64 as i128 } else { newv as i128 };
                let msg = format!(
                    "implicit conversion from '{}' to '{}' changes value from {} to {}",
                    self.show(from),
                    self.show(to),
                    val,
                    nv
                );
                self.warn(Warn::ConstantConversion, span, msg);
            } else if self.types.size_of(to) < self.types.size_of(from) {
                let msg = format!(
                    "implicit conversion loses integer precision: '{}' to '{}'",
                    self.show(from),
                    self.show(to)
                );
                self.warn(Warn::Conversion, span, msg);
            }
        } else if t.is_floating(from) && t.is_integer(to) {
            if let Some(ConstVal::Float(f)) = eval(&self.types, e) {
                let (tmin, tmax) = t.int_range(to);
                if f.fract() == 0.0 && (f as i128) >= tmin && (f as i128) <= tmax {
                    return;
                }
                let msg = format!(
                    "implicit conversion from '{}' to '{}' changes value from {} to {}",
                    self.show(from),
                    self.show(to),
                    f,
                    f as i64
                );
                self.warn(Warn::Conversion, span, msg);
            } else {
                let msg = format!(
                    "implicit conversion turns floating-point number into integer: '{}' to '{}'",
                    self.show(from),
                    self.show(to)
                );
                self.warn(Warn::Conversion, span, msg);
            }
        } else if matches!(t.kind(from), TyKind::Double) && matches!(t.kind(to), TyKind::Float) {
            if let Some(ConstVal::Float(f)) = eval(&self.types, e) {
                if f.is_finite() && f.abs() <= f32::MAX as f64 {
                    return;
                }
            }
            let msg = format!(
                "implicit conversion loses floating-point precision: '{}' to '{}'",
                self.show(from),
                self.show(to)
            );
            self.warn(Warn::Conversion, span, msg);
        }
    }

    // ───────────────────────────── identifiers ─────────────────────────────

    fn suggestion(&self, name: &str) -> Option<String> {
        let max = (name.len() / 3).clamp(1, 3);
        let mut best: Option<(usize, String)> = None;
        for sc in self.scopes.iter().rev() {
            for (k, ent) in &sc.ordinary {
                if matches!(ent.kind, EntKind::Typedef(_)) {
                    continue;
                }
                let d = edit_distance(name, k.as_str());
                if d > 0 && d <= max && best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                    best = Some((d, k.to_string()));
                }
            }
        }
        best.map(|(_, s)| s)
    }

    fn ident_expr(&mut self, name: Symbol, span: Span) -> HExpr {
        match self.lookup(name).map(|e| e.kind.clone()) {
            Some(EntKind::Local(id)) => {
                let f = self.f.as_mut().expect("function context");
                f.locals[id.0 as usize].used = true;
                let ty = f.locals[id.0 as usize].ty;
                HExpr::new(HExprKind::Local(id), ty, span)
            }
            Some(EntKind::Global(id)) => {
                self.mark_used(id);
                let ty = self.syms[id.0 as usize].ty;
                HExpr::new(HExprKind::Global(id), ty, span)
            }
            Some(EntKind::EnumConst(v, ty)) => self.int_lit(v as u64, ty, span),
            Some(EntKind::Typedef(_)) => {
                self.error(span, format!("unexpected type name '{}': expected expression", name));
                self.err_expr(span)
            }
            None => {
                let s = name.as_str();
                if (s == "__func__" || s == "__FUNCTION__" || s == "__PRETTY_FUNCTION__") && self.in_function() {
                    let fname = self.f.as_ref().unwrap().name.to_string();
                    let units: Vec<u32> = fname.bytes().map(|b| b as u32).collect();
                    let n = units.len() as u64 + 1;
                    let id = self.intern_string(literal::StrKind::Plain, units);
                    let aty = self.types.array(self.types.p.char_, ArrayLen::Known(n));
                    return HExpr::new(HExprKind::Str(id), aty, span);
                }
                let mut d = Diagnostic::error(span, format!("use of undeclared identifier '{}'", name));
                if let Some(sug) = self.suggestion(s) {
                    d.message = format!("use of undeclared identifier '{}'; did you mean '{}'?", name, sug);
                    d = d.with_fixit(span, sug);
                }
                self.emit(d);
                self.err_expr(span)
            }
        }
    }

    // ───────────────────────────── main dispatch ─────────────────────────────

    pub(crate) fn expr(&mut self, e: &Expr) -> HExpr {
        let span = e.span;
        match &e.kind {
            ExprKind::IntLit(s) => self.int_literal(s.as_str(), span),
            ExprKind::FloatLit(s) => self.float_literal(s.as_str(), span),
            ExprKind::CharLit(s) => self.char_literal(s.as_str(), span),
            ExprKind::StrLit { kind, units } => {
                let p = self.types.p;
                let elem = match kind {
                    literal::StrKind::Plain | literal::StrKind::Utf8 => p.char_,
                    literal::StrKind::Wide => p.int,
                    literal::StrKind::Utf16 => p.ushort,
                    literal::StrKind::Utf32 => p.uint,
                };
                let aty = self.types.array(elem, ArrayLen::Known(units.len() as u64 + 1));
                let id = self.intern_string(*kind, units.clone());
                HExpr::new(HExprKind::Str(id), aty, span)
            }
            ExprKind::Ident(name) => self.ident_expr(*name, span),
            ExprKind::Paren(inner) => {
                let mut h = self.expr(inner);
                h.span = span;
                h
            }
            ExprKind::Unary { op, operand, op_span } => self.unary(*op, operand, *op_span, span),
            ExprKind::Binary { op, lhs, rhs, op_span } => self.binary(*op, lhs, rhs, *op_span, span),
            ExprKind::Assign { op, lhs, rhs, op_span } => self.assign(*op, lhs, rhs, *op_span, span),
            ExprKind::Cond { cond, then, els } => self.conditional(cond, then, els, span),
            ExprKind::Comma(a, b) => {
                let ha = self.rexpr(a);
                let hb = self.rexpr(b);
                let ty = hb.ty;
                HExpr::new(HExprKind::Comma(Box::new(ha), Box::new(hb)), ty, span)
            }
            ExprKind::Call { callee, args } => self.call(callee, args, span),
            ExprKind::Index { base, index } => self.index(base, index, span),
            ExprKind::Member { base, member, arrow } => self.member(base, *member, *arrow, span),
            ExprKind::Cast { ty, operand } => self.cast_expr(ty, operand, span),
            ExprKind::SizeofExpr(x) => self.sizeof_expr(x, span),
            ExprKind::SizeofType(t) => {
                let mark = self.pending_vla.len();
                let ty = self.type_name(t);
                let e = self.sizeof_type(ty, span);
                self.wrap_vla_inits(mark, e)
            }
            ExprKind::AlignofType(t) => {
                let ty = self.type_name(t);
                if !self.types.is_complete(ty) {
                    let s = self.show(ty);
                    self.error(span, format!("invalid application of '_Alignof' to an incomplete type '{}'", s));
                    return self.err_expr(span);
                }
                let a = self.types.align_of(ty);
                let ul = self.types.p.ulong;
                self.int_lit(a, ul, span)
            }
            ExprKind::CompoundLiteral { ty, init } => self.compound_literal(ty, init, span),
            ExprKind::Generic { controlling, assocs } => self.generic(controlling, assocs, span),
            ExprKind::VaArg { ap, ty } => {
                let hap = self.rexpr(ap);
                let to = self.type_name(ty);
                let tag_ptr = {
                    let tag = self.types.record_type(self.types.p.va_list_tag);
                    self.types.ptr(tag)
                };
                if Self::is_error(&hap) {
                    return hap;
                }
                if hap.ty != tag_ptr {
                    let s = self.show(hap.ty);
                    self.error(ap.span, format!("first argument to 'va_arg' is of type '{}' and not 'va_list'", s));
                    return self.err_expr(span);
                }
                if !self.types.is_complete(to) {
                    let s = self.show(to);
                    self.error(span, format!("va_arg with incomplete type '{}'", s));
                    return self.err_expr(span);
                }
                let uty = self.types.unqual(to);
                HExpr::new(HExprKind::VaArg(Box::new(hap)), uty, span)
            }
            ExprKind::Offsetof { ty, path } => self.offsetof(ty, path, span),
        }
    }

    // ───────────────────────────── literals ─────────────────────────────

    fn int_literal(&mut self, text: &str, span: Span) -> HExpr {
        let p = self.types.p;
        let lit = match literal::parse_int(text) {
            Ok(l) => l,
            Err(m) => {
                self.error(span, m);
                return self.err_expr(span);
            }
        };
        // C11 6.4.4.1 candidate lists
        let candidates: Vec<Ty> = match (lit.unsigned, lit.longs, lit.decimal) {
            (false, 0, true) => vec![p.int, p.long, p.llong],
            (false, 0, false) => vec![p.int, p.uint, p.long, p.ulong, p.llong, p.ullong],
            (true, 0, _) => vec![p.uint, p.ulong, p.ullong],
            (false, 1, true) => vec![p.long, p.llong],
            (false, 1, false) => vec![p.long, p.ulong, p.llong, p.ullong],
            (true, 1, _) => vec![p.ulong, p.ullong],
            (false, _, true) => vec![p.llong],
            (false, _, false) => vec![p.llong, p.ullong],
            (true, _, _) => vec![p.ullong],
        };
        for t in &candidates {
            let (_, max) = self.types.int_range(*t);
            if (lit.value as i128) <= max {
                return self.int_lit(lit.value, *t, span);
            }
        }
        self.warn(
            Warn::Overflow,
            span,
            "integer literal is too large to be represented in a signed integer type, interpreting as unsigned",
        );
        self.int_lit(lit.value, p.ullong, span)
    }

    fn float_literal(&mut self, text: &str, span: Span) -> HExpr {
        let p = self.types.p;
        match literal::parse_float(text) {
            Ok(f) => {
                if f.is_long_double {
                    self.error(span, "not yet supported: long double");
                    return self.err_expr(span);
                }
                if f.is_float {
                    HExpr::new(HExprKind::Float(f.value_f32() as f64), p.float, span)
                } else {
                    HExpr::new(HExprKind::Float(f.value_f64()), p.double, span)
                }
            }
            Err(m) => {
                self.error(span, m);
                self.err_expr(span)
            }
        }
    }

    fn char_literal(&mut self, text: &str, span: Span) -> HExpr {
        let mut issues = Vec::new();
        let Some(c) = literal::parse_char(text, &mut issues) else {
            self.error(span, "invalid character constant");
            return self.err_expr(span);
        };
        let mut bad = false;
        for i in issues {
            let lo = span.lo.saturating_add(i.offset as u32);
            let sp = Span::new(span.file, lo, (lo + i.len as u32).min(span.hi.max(lo)));
            if i.is_error {
                self.error(sp, i.message);
                bad = true;
            } else {
                self.warn(Warn::UnknownEscape, sp, i.message);
            }
        }
        if bad {
            return self.err_expr(span);
        }
        if c.multichar {
            self.warn(Warn::Multichar, span, "multi-character character constant");
        }
        let p = self.types.p;
        let ty = match c.kind {
            literal::StrKind::Plain | literal::StrKind::Utf8 | literal::StrKind::Wide => p.int,
            literal::StrKind::Utf16 => p.ushort,
            literal::StrKind::Utf32 => p.uint,
        };
        self.int_lit(c.value as u64, ty, span)
    }

    // ───────────────────────────── unary ─────────────────────────────

    fn invalid_unary(&mut self, op: &str, span: Span, operand_span: Span, ty: Ty) -> HExpr {
        let s = self.show(ty);
        self.emit(
            Diagnostic::error(span, format!("invalid argument type '{}' to unary expression", s))
                .with_label(operand_span),
        );
        let _ = op;
        self.err_expr(span)
    }

    fn unary(&mut self, op: UnOp, operand: &Expr, op_span: Span, span: Span) -> HExpr {
        match op {
            UnOp::AddrOf => {
                let h = self.expr(operand);
                if Self::is_error(&h) {
                    return h;
                }
                if self.types.is_function(h.ty) {
                    // &f is the same as f decayed
                    let pty = self.types.ptr(h.ty);
                    return HExpr::new(HExprKind::AddrOf(Box::new(h)), pty, span);
                }
                if !h.is_place() {
                    self.emit(
                        Diagnostic::error(op_span, "cannot take the address of an rvalue").with_label(operand.span),
                    );
                    return self.err_expr(span);
                }
                if let HExprKind::Member(_, m) = &h.kind {
                    if m.bit.is_some() {
                        self.error(op_span, "address of bit-field requested");
                        return self.err_expr(span);
                    }
                }
                let pty = self.types.ptr(h.ty);
                HExpr::new(HExprKind::AddrOf(Box::new(h)), pty, span)
            }
            UnOp::Deref => {
                let h = self.rexpr(operand);
                if Self::is_error(&h) {
                    return h;
                }
                let Some(pointee) = self.types.pointee(h.ty) else {
                    let s = self.show(h.ty);
                    self.emit(
                        Diagnostic::error(op_span, format!("indirection requires pointer operand ('{}' invalid)", s))
                            .with_label(operand.span),
                    );
                    return self.err_expr(span);
                };
                if self.types.is_void(pointee) {
                    self.emit(
                        Diagnostic::error(op_span, "cannot dereference a pointer to 'void'").with_label(operand.span),
                    );
                    return self.err_expr(span);
                }
                HExpr::new(HExprKind::Deref(Box::new(h)), pointee, span)
            }
            UnOp::Plus | UnOp::Neg => {
                let h = self.rexpr(operand);
                if Self::is_error(&h) {
                    return h;
                }
                if !self.types.is_arithmetic(h.ty) {
                    return self.invalid_unary(if op == UnOp::Neg { "-" } else { "+" }, op_span, operand.span, h.ty);
                }
                let pt = self.types.promote(h.ty);
                let h = self.cast_to(h, pt);
                if op == UnOp::Plus {
                    return HExpr { span, ..h };
                }
                let r = HExpr::new(HExprKind::Unary(UnKind::Neg, Box::new(h)), pt, span);
                self.fold(r)
            }
            UnOp::BitNot => {
                let h = self.rexpr(operand);
                if Self::is_error(&h) {
                    return h;
                }
                if !self.types.is_integer(h.ty) {
                    return self.invalid_unary("~", op_span, operand.span, h.ty);
                }
                let pt = self.types.promote(h.ty);
                let h = self.cast_to(h, pt);
                let r = HExpr::new(HExprKind::Unary(UnKind::BitNot, Box::new(h)), pt, span);
                self.fold(r)
            }
            UnOp::LogNot => {
                let h = self.rexpr(operand);
                if Self::is_error(&h) {
                    return h;
                }
                if !self.types.is_scalar(h.ty) {
                    return self.invalid_unary("!", op_span, operand.span, h.ty);
                }
                let int = self.types.p.int;
                let r = HExpr::new(HExprKind::Unary(UnKind::LogNot, Box::new(h)), int, span);
                self.fold(r)
            }
            UnOp::PreInc | UnOp::PreDec | UnOp::PostInc | UnOp::PostDec => {
                let h = self.expr(operand);
                if Self::is_error(&h) {
                    return h;
                }
                let is_inc = matches!(op, UnOp::PreInc | UnOp::PostInc);
                let is_prefix = matches!(op, UnOp::PreInc | UnOp::PreDec);
                if !self.check_modifiable(&h, op_span, operand.span, if is_inc { ModKind::Inc } else { ModKind::Dec }) {
                    return self.err_expr(span);
                }
                if !self.types.is_scalar(h.ty) {
                    let s = self.show(h.ty);
                    self.emit(
                        Diagnostic::error(
                            op_span,
                            format!("cannot {} value of type '{}'", if is_inc { "increment" } else { "decrement" }, s),
                        )
                        .with_label(operand.span),
                    );
                    return self.err_expr(span);
                }
                if let Some(p) = self.types.pointee(h.ty) {
                    if !self.types.is_complete(p) && !self.types.is_void(p) && !self.types.is_function(p) {
                        let s = self.show(p);
                        self.error(span, format!("arithmetic on a pointer to an incomplete type '{}'", s));
                        return self.err_expr(span);
                    }
                }
                let ty = self.types.unqual(h.ty);
                HExpr::new(HExprKind::IncDec { place: Box::new(h), is_inc, is_prefix }, ty, span)
            }
        }
    }

    /// Is `place` something that may be assigned to? Diagnoses when not.
    fn check_modifiable(&mut self, place: &HExpr, op_span: Span, operand_span: Span, kind: ModKind) -> bool {
        if !place.is_place() || self.types.is_function(place.ty) {
            self.emit(Diagnostic::error(op_span, "expression is not assignable").with_label(operand_span));
            return false;
        }
        if self.types.is_array(place.ty) {
            let s = self.show(place.ty);
            self.emit(
                Diagnostic::error(op_span, format!("array type '{}' is not assignable", s)).with_label(operand_span),
            );
            return false;
        }
        if !self.types.is_complete(place.ty) {
            let s = self.show(place.ty);
            self.error(op_span, format!("cannot assign to incomplete type '{}'", s));
            return false;
        }
        if self.types.quals(place.ty).is_const {
            let verb = match kind {
                ModKind::Assign => "assign to",
                ModKind::Inc => "increment",
                ModKind::Dec => "decrement",
            };
            let name = match &place.kind {
                HExprKind::Local(id) => self.f.as_ref().map(|f| f.locals[id.0 as usize].name.to_string()),
                HExprKind::Global(id) => Some(self.syms[id.0 as usize].name.to_string()),
                _ => None,
            };
            let s = self.show(place.ty);
            let msg = match name {
                Some(n) => format!("cannot {} variable '{}' with const-qualified type '{}'", verb, n, s),
                None => format!("cannot {} this expression with const-qualified type '{}'", verb, s),
            };
            self.emit(Diagnostic::error(op_span, msg).with_label(operand_span));
            return false;
        }
        true
    }

    // ───────────────────────────── binary ─────────────────────────────

    fn invalid_operands(
        &mut self,
        opstr: &str,
        op_span: Span,
        l: &HExpr,
        r: &HExpr,
        lspan: Span,
        rspan: Span,
    ) -> HExpr {
        let _ = opstr;
        let (a, b) = (self.show(l.ty), self.show(r.ty));
        self.emit(
            Diagnostic::error(op_span, format!("invalid operands to binary expression ('{}' and '{}')", a, b))
                .with_label(lspan)
                .with_label(rspan),
        );
        self.err_expr(op_span)
    }

    fn pointee_scale(&mut self, ptr_ty: Ty, span: Span) -> Option<u64> {
        let p = self.types.pointee(ptr_ty)?;
        if self.types.is_void(p) || self.types.is_function(p) {
            return Some(1); // GNU extension: sizeof(void) == 1
        }
        if self.types.is_vla(p) {
            return Some(0); // stride computed at run time
        }
        match self.types.size_of(p) {
            Some(s) => Some(s),
            None => {
                let t = self.show(p);
                self.error(span, format!("arithmetic on a pointer to an incomplete type '{}'", t));
                None
            }
        }
    }

    fn make_ptr_add(&mut self, p: HExpr, i: HExpr, negate: bool, span: Span) -> HExpr {
        let Some(scale) = self.pointee_scale(p.ty, span) else { return self.err_expr(span) };
        let long = self.types.p.long;
        let pt = self.types.promote(i.ty);
        let i = self.cast_to(i, pt);
        let idx = self.cast_to(i, long);
        let ty = p.ty;
        HExpr::new(HExprKind::PtrAdd { ptr: Box::new(p), idx: Box::new(idx), scale, negate }, ty, span)
    }

    /// Apply the usual arithmetic conversions to both operands.
    fn arith_pair(&mut self, l: HExpr, r: HExpr) -> (HExpr, HExpr, Ty) {
        let ty = self.types.usual_arith(l.ty, r.ty);
        let l = self.cast_to(l, ty);
        let r = self.cast_to(r, ty);
        (l, r, ty)
    }

    fn binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, op_span: Span, span: Span) -> HExpr {
        let l = self.rexpr(lhs);
        let r = self.rexpr(rhs);
        if Self::is_error(&l) || Self::is_error(&r) {
            return self.err_expr(span);
        }
        let int = self.types.p.int;
        let (lt, rt) = (l.ty, r.ty);
        let ops = op.spelling();
        match op {
            BinOp::LogAnd | BinOp::LogOr => {
                if !self.types.is_scalar(lt) || !self.types.is_scalar(rt) {
                    return self.invalid_operands(ops, op_span, &l, &r, lhs.span, rhs.span);
                }
                let k = if op == BinOp::LogAnd {
                    HExprKind::LogAnd(Box::new(l), Box::new(r))
                } else {
                    HExprKind::LogOr(Box::new(l), Box::new(r))
                };
                let e = HExpr::new(k, int, span);
                self.fold(e)
            }
            BinOp::Add | BinOp::Sub => {
                let (la, ra) = (self.types.is_arithmetic(lt), self.types.is_arithmetic(rt));
                let (lp, rp) = (self.types.is_pointer(lt), self.types.is_pointer(rt));
                if la && ra {
                    let (l, r, ty) = self.arith_pair(l, r);
                    let k = if op == BinOp::Add { BinKind::Add } else { BinKind::Sub };
                    let e = HExpr::new(HExprKind::Binary(k, Box::new(l), Box::new(r)), ty, span);
                    return self.fold(e);
                }
                if op == BinOp::Add && lp && self.types.is_integer(rt) {
                    return self.make_ptr_add(l, r, false, span);
                }
                if op == BinOp::Add && rp && self.types.is_integer(lt) {
                    return self.make_ptr_add(r, l, false, span);
                }
                if op == BinOp::Sub && lp && self.types.is_integer(rt) {
                    return self.make_ptr_add(l, r, true, span);
                }
                if op == BinOp::Sub && lp && rp {
                    let (pa, pb) = (self.types.pointee(lt).unwrap(), self.types.pointee(rt).unwrap());
                    if !self.types.compatible_unqual(pa, pb) {
                        let (a, b) = (self.show(lt), self.show(rt));
                        self.emit(
                            Diagnostic::error(
                                op_span,
                                format!("'{}' and '{}' are not pointers to compatible types", a, b),
                            )
                            .with_label(lhs.span)
                            .with_label(rhs.span),
                        );
                        return self.err_expr(span);
                    }
                    let Some(size) = self.pointee_scale(lt, span) else { return self.err_expr(span) };
                    let long = self.types.p.long;
                    return HExpr::new(
                        HExprKind::PtrDiff { l: Box::new(l), r: Box::new(r), elem_size: size },
                        long,
                        span,
                    );
                }
                self.invalid_operands(ops, op_span, &l, &r, lhs.span, rhs.span)
            }
            BinOp::Mul | BinOp::Div | BinOp::Rem => {
                let ok = if op == BinOp::Rem {
                    self.types.is_integer(lt) && self.types.is_integer(rt)
                } else {
                    self.types.is_arithmetic(lt) && self.types.is_arithmetic(rt)
                };
                if !ok {
                    return self.invalid_operands(ops, op_span, &l, &r, lhs.span, rhs.span);
                }
                let (l, r, ty) = self.arith_pair(l, r);
                if matches!(op, BinOp::Div | BinOp::Rem)
                    && self.types.is_integer(ty)
                    && matches!(eval(&self.types, &r), Some(ConstVal::Int(0)))
                {
                    let what = if op == BinOp::Div { "division" } else { "remainder" };
                    self.emit(
                        Diagnostic::warning(Warn::DivisionByZero, op_span, format!("{} by zero is undefined", what))
                            .with_label(rhs.span),
                    );
                    let e = HExpr::new(HExprKind::Binary(bin_kind(op), Box::new(l), Box::new(r)), ty, span);
                    return e;
                }
                let e = HExpr::new(HExprKind::Binary(bin_kind(op), Box::new(l), Box::new(r)), ty, span);
                self.fold(e)
            }
            BinOp::Shl | BinOp::Shr => {
                if !self.types.is_integer(lt) || !self.types.is_integer(rt) {
                    return self.invalid_operands(ops, op_span, &l, &r, lhs.span, rhs.span);
                }
                let pl = self.types.promote(lt);
                let pr = self.types.promote(rt);
                let l = self.cast_to(l, pl);
                let r = self.cast_to(r, pr);
                if let Some(v) = eval_int(&self.types, &r) {
                    let bits = self.types.size_of(pl).unwrap_or(4) * 8;
                    let neg = self.types.is_signed(pr) && (v as i64) < 0;
                    if neg {
                        self.emit(Diagnostic::warning(Warn::ShiftCount, rhs.span, "shift count is negative"));
                    } else if v >= bits {
                        self.emit(Diagnostic::warning(
                            Warn::ShiftCount,
                            rhs.span,
                            format!("shift count >= width of type ('{}' has {} bits)", self.show(pl), bits),
                        ));
                    }
                }
                let e = HExpr::new(HExprKind::Binary(bin_kind(op), Box::new(l), Box::new(r)), pl, span);
                self.fold(e)
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                if !self.types.is_integer(lt) || !self.types.is_integer(rt) {
                    return self.invalid_operands(ops, op_span, &l, &r, lhs.span, rhs.span);
                }
                let (l, r, ty) = self.arith_pair(l, r);
                let e = HExpr::new(HExprKind::Binary(bin_kind(op), Box::new(l), Box::new(r)), ty, span);
                self.fold(e)
            }
            BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne => {
                let is_eq = matches!(op, BinOp::Eq | BinOp::Ne);
                let (la, ra) = (self.types.is_arithmetic(lt), self.types.is_arithmetic(rt));
                let (lp, rp) = (self.types.is_pointer(lt), self.types.is_pointer(rt));
                if la && ra {
                    let (l, r, _) = self.arith_pair(l, r);
                    let e = HExpr::new(HExprKind::Binary(bin_kind(op), Box::new(l), Box::new(r)), int, span);
                    return self.fold(e);
                }
                if lp && rp {
                    let (pa, pb) = (self.types.pointee(lt).unwrap(), self.types.pointee(rt).unwrap());
                    let ok = self.types.compatible_unqual(pa, pb)
                        || self.types.is_void(pa)
                        || self.types.is_void(pb)
                        || (is_eq && (self.is_null_ptr_const(&l) || self.is_null_ptr_const(&r)));
                    if !ok {
                        let (a, b) = (self.show(lt), self.show(rt));
                        self.emit(
                            Diagnostic::warning(
                                Warn::IncompatiblePointerTypes,
                                op_span,
                                format!("comparison of distinct pointer types ('{}' and '{}')", a, b),
                            )
                            .with_label(lhs.span)
                            .with_label(rhs.span),
                        );
                    }
                    let e = HExpr::new(HExprKind::Binary(bin_kind(op), Box::new(l), Box::new(r)), int, span);
                    return e;
                }
                if (lp && self.types.is_integer(rt)) || (rp && self.types.is_integer(lt)) {
                    let (ptr, other, int_is_right) = if lp { (&l, &r, true) } else { (&r, &l, false) };
                    if !(is_eq && self.is_null_ptr_const(other)) {
                        let (a, b) = (self.show(lt), self.show(rt));
                        self.emit(
                            Diagnostic::warning(
                                Warn::IntConversion,
                                op_span,
                                format!("comparison between pointer and integer ('{}' and '{}')", a, b),
                            )
                            .with_label(lhs.span)
                            .with_label(rhs.span),
                        );
                    }
                    let pty = ptr.ty;
                    let (l, r) = if int_is_right {
                        let r = self.cast_to(r, pty);
                        (l, r)
                    } else {
                        let l = self.cast_to(l, pty);
                        (l, r)
                    };
                    return HExpr::new(HExprKind::Binary(bin_kind(op), Box::new(l), Box::new(r)), int, span);
                }
                self.invalid_operands(ops, op_span, &l, &r, lhs.span, rhs.span)
            }
        }
    }

    // ───────────────────────────── assignment ─────────────────────────────

    fn assign(&mut self, op: Option<BinOp>, lhs: &Expr, rhs: &Expr, op_span: Span, span: Span) -> HExpr {
        let place = self.expr(lhs);
        if Self::is_error(&place) {
            let _ = self.rexpr(rhs);
            return place;
        }
        if !self.check_modifiable(&place, op_span, lhs.span, ModKind::Assign) {
            let _ = self.rexpr(rhs);
            return self.err_expr(span);
        }
        let r = self.rexpr(rhs);
        if Self::is_error(&r) {
            return r;
        }
        let lty = self.types.unqual(place.ty);
        let Some(op) = op else {
            let v = self.convert(r, lty, ConvCtx::Assign, rhs.span);
            if Self::is_error(&v) {
                return v;
            }
            return HExpr::new(HExprKind::Assign(Box::new(place), Box::new(v)), lty, span);
        };
        // compound assignment
        let kind = bin_kind(op);
        let (lt, rt) = (lty, r.ty);
        let types = &self.types;
        match op {
            BinOp::Add | BinOp::Sub if types.is_pointer(lt) && types.is_integer(rt) => {
                let Some(_) = self.pointee_scale(lt, span) else { return self.err_expr(span) };
                let long = self.types.p.long;
                let pr = self.types.promote(rt);
                let v = self.cast_to(r, pr);
                let v = self.cast_to(v, long);
                HExpr::new(
                    HExprKind::CompoundAssign { op: kind, place: Box::new(place), value: Box::new(v), calc: lt },
                    lty,
                    span,
                )
            }
            BinOp::Shl | BinOp::Shr => {
                if !types.is_integer(lt) || !types.is_integer(rt) {
                    let (a, b) = (self.show(lt), self.show(rt));
                    self.emit(
                        Diagnostic::error(
                            op_span,
                            format!("invalid operands to binary expression ('{}' and '{}')", a, b),
                        )
                        .with_label(lhs.span)
                        .with_label(rhs.span),
                    );
                    return self.err_expr(span);
                }
                let calc = self.types.promote(lt);
                let pr = self.types.promote(rt);
                let v = self.cast_to(r, pr);
                HExpr::new(
                    HExprKind::CompoundAssign { op: kind, place: Box::new(place), value: Box::new(v), calc },
                    lty,
                    span,
                )
            }
            _ => {
                let ints_only = matches!(op, BinOp::Rem | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor);
                let ok = if ints_only {
                    types.is_integer(lt) && types.is_integer(rt)
                } else {
                    types.is_arithmetic(lt) && types.is_arithmetic(rt)
                };
                if !ok {
                    let (a, b) = (self.show(lt), self.show(rt));
                    self.emit(
                        Diagnostic::error(
                            op_span,
                            format!("invalid operands to binary expression ('{}' and '{}')", a, b),
                        )
                        .with_label(lhs.span)
                        .with_label(rhs.span),
                    );
                    return self.err_expr(span);
                }
                let calc = self.types.usual_arith(lt, rt);
                if matches!(op, BinOp::Div | BinOp::Rem)
                    && self.types.is_integer(calc)
                    && matches!(eval(&self.types, &r), Some(ConstVal::Int(0)))
                {
                    let what = if op == BinOp::Div { "division" } else { "remainder" };
                    self.emit(
                        Diagnostic::warning(Warn::DivisionByZero, op_span, format!("{} by zero is undefined", what))
                            .with_label(rhs.span),
                    );
                }
                let v = self.cast_to(r, calc);
                HExpr::new(
                    HExprKind::CompoundAssign { op: kind, place: Box::new(place), value: Box::new(v), calc },
                    lty,
                    span,
                )
            }
        }
    }

    // ───────────────────────────── conditional ─────────────────────────────

    fn conditional(&mut self, cond: &Expr, then: &Expr, els: &Expr, span: Span) -> HExpr {
        let c = self.rexpr(cond);
        let t = self.rexpr(then);
        let f = self.rexpr(els);
        if Self::is_error(&c) || Self::is_error(&t) || Self::is_error(&f) {
            return self.err_expr(span);
        }
        if !self.types.is_scalar(c.ty) {
            let s = self.show(c.ty);
            self.emit(Diagnostic::error(
                cond.span,
                format!("used type '{}' where arithmetic or pointer type is required", s),
            ));
            return self.err_expr(span);
        }
        let ty;
        let (t, f) = {
            let (tt, ft) = (t.ty, f.ty);
            let types = &self.types;
            if types.is_arithmetic(tt) && types.is_arithmetic(ft) {
                let (a, b, rty) = self.arith_pair(t, f);
                ty = rty;
                (a, b)
            } else if types.is_void(tt) && types.is_void(ft) {
                ty = tt;
                (t, f)
            } else if let (TyKind::Record(a), TyKind::Record(b)) = (types.kind(tt), types.kind(ft)) {
                if a != b {
                    let (x, y) = (self.show(tt), self.show(ft));
                    self.error(span, format!("incompatible operand types ('{}' and '{}')", x, y));
                    return self.err_expr(span);
                }
                ty = tt;
                (t, f)
            } else if types.is_pointer(tt) && types.is_pointer(ft) {
                let (pa, pb) = (types.pointee(tt).unwrap(), types.pointee(ft).unwrap());
                let q = types.quals(pa).union(types.quals(pb));
                if self.is_null_ptr_const(&t) {
                    ty = ft;
                } else if self.is_null_ptr_const(&f) {
                    ty = tt;
                } else if self.types.is_void(pa) || self.types.is_void(pb) {
                    let v = self.types.p.void;
                    let qv = self.types.qualified(v, q);
                    ty = self.types.ptr(qv);
                } else if self.types.compatible_unqual(pa, pb) {
                    let (ua, ub) = (self.types.unqual(pa), self.types.unqual(pb));
                    let comp = self.types.composite(ua, ub);
                    let qc = self.types.qualified(comp, q);
                    ty = self.types.ptr(qc);
                } else {
                    let (x, y) = (self.show(tt), self.show(ft));
                    self.warn(
                        Warn::IncompatiblePointerTypes,
                        span,
                        format!("pointer type mismatch ('{}' and '{}')", x, y),
                    );
                    ty = tt;
                }
                let a = self.cast_to(t, ty);
                let b = self.cast_to(f, ty);
                (a, b)
            } else if types.is_pointer(tt) && types.is_integer(ft) {
                if !self.is_null_ptr_const(&f) {
                    let (x, y) = (self.show(tt), self.show(ft));
                    self.warn(
                        Warn::IntConversion,
                        span,
                        format!("pointer/integer type mismatch in conditional expression ('{}' and '{}')", x, y),
                    );
                }
                ty = tt;
                let b = self.cast_to(f, ty);
                (t, b)
            } else if types.is_integer(tt) && types.is_pointer(ft) {
                if !self.is_null_ptr_const(&t) {
                    let (x, y) = (self.show(tt), self.show(ft));
                    self.warn(
                        Warn::IntConversion,
                        span,
                        format!("pointer/integer type mismatch in conditional expression ('{}' and '{}')", x, y),
                    );
                }
                ty = ft;
                let a = self.cast_to(t, ty);
                (a, f)
            } else {
                let (x, y) = (self.show(tt), self.show(ft));
                self.error(span, format!("incompatible operand types ('{}' and '{}')", x, y));
                return self.err_expr(span);
            }
        };
        let e = HExpr::new(HExprKind::Cond(Box::new(c), Box::new(t), Box::new(f)), ty, span);
        self.fold(e)
    }

    // ───────────────────────────── calls ─────────────────────────────

    fn call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> HExpr {
        // Unknown identifier in call position: a builtin or an implicit declaration.
        let mut callee_h: Option<HExpr> = None;
        if let ExprKind::Ident(name) = &unparen_expr(callee).kind {
            if self.lookup(*name).is_none() {
                if name.as_str().starts_with("__builtin_") {
                    return self.builtin_call(*name, args, span);
                }
                self.warn(
                    Warn::ImplicitFunctionDeclaration,
                    callee.span,
                    format!("call to undeclared function '{}'; ISO C99 and later do not support implicit function declarations", name),
                );
                let int = self.types.p.int;
                let fty = self.types.func(FuncSig { ret: int, params: Vec::new(), variadic: false, unspecified: true });
                let id = Ident { name: *name, span: callee.span };
                let sid = self.declare_global_func(id, fty, None, false, false);
                self.scopes[0].ordinary.insert(*name, Ent { kind: EntKind::Global(sid), span: callee.span });
                self.mark_used(sid);
                callee_h = Some(HExpr::new(HExprKind::Global(sid), fty, callee.span));
            }
        }
        let f = match callee_h {
            Some(h) => h,
            None => self.expr(callee),
        };
        let direct_sym = match &f.kind {
            HExprKind::Global(id) => Some(*id),
            _ => None,
        };
        let f = self.rvalue(f);
        // Evaluate arguments even if the callee is bad (to report their errors).
        let mut hargs: Vec<(HExpr, Span)> = Vec::with_capacity(args.len());
        for a in args {
            let h = self.rexpr(a);
            hargs.push((h, a.span));
        }
        if Self::is_error(&f) {
            return f;
        }
        let sig = match self.types.pointee(f.ty).and_then(|p| self.types.func_sig(p).cloned()) {
            Some(s) => s,
            None => {
                let s = self.show(f.ty);
                self.emit(
                    Diagnostic::error(
                        span,
                        format!("called object type '{}' is not a function or function pointer", s),
                    )
                    .with_label(callee.span),
                );
                return self.err_expr(span);
            }
        };
        let n = hargs.len();
        let np = sig.params.len();
        if !sig.unspecified && (n < np || (n > np && !sig.variadic)) {
            let msg = if n < np {
                format!("too few arguments to function call, expected {}, have {}", np, n)
            } else {
                format!("too many arguments to function call, expected {}, have {}", np, n)
            };
            let mut d = Diagnostic::error(if n > np { hargs[np].1 } else { span.end() }, msg).with_label(callee.span);
            if let Some(id) = direct_sym {
                let sp = self.sym_span_note(id);
                let nm = self.syms[id.0 as usize].name;
                d = d.with_note(sp, format!("'{}' declared here", nm));
            }
            self.emit(d);
            return self.err_expr(span);
        }
        let mut out = Vec::with_capacity(n);
        for (i, (h, asp)) in hargs.into_iter().enumerate() {
            if Self::is_error(&h) {
                out.push(h);
                continue;
            }
            if i < np {
                out.push(self.convert(h, sig.params[i], ConvCtx::Arg, asp));
            } else {
                out.push(self.default_promote(h));
            }
        }
        let ret = self.types.unqual(sig.ret);
        HExpr::new(HExprKind::Call { callee: Box::new(f), args: out }, ret, span)
    }

    /// Default argument promotions (C11 6.5.2.2p6).
    fn default_promote(&mut self, e: HExpr) -> HExpr {
        if self.types.is_integer(e.ty) {
            let pt = self.types.promote(e.ty);
            return self.cast_to(e, pt);
        }
        if matches!(self.types.kind(e.ty), TyKind::Float) {
            let d = self.types.p.double;
            return self.cast_to(e, d);
        }
        e
    }

    fn builtin_call(&mut self, name: Symbol, args: &[Expr], span: Span) -> HExpr {
        let p = self.types.p;
        let void = p.void;
        let want = |s: &mut Self, n: usize| -> bool {
            if args.len() != n {
                s.error(
                    span,
                    format!("incorrect number of arguments to '{}': expected {}, have {}", name, n, args.len()),
                );
                return false;
            }
            true
        };
        match name.as_str() {
            "__builtin_va_start" => {
                if args.is_empty() || args.len() > 2 {
                    self.error(span, "incorrect number of arguments to '__builtin_va_start'");
                    return self.err_expr(span);
                }
                let ap = self.rexpr(&args[0]);
                if args.len() == 2 {
                    let _ = self.expr(&args[1]);
                }
                if Self::is_error(&ap) {
                    return ap;
                }
                if !self.f_variadic() {
                    self.error(span, "'va_start' used in function with fixed args");
                    return self.err_expr(span);
                }
                HExpr::new(HExprKind::VaStart(Box::new(ap)), void, span)
            }
            "__builtin_va_end" => {
                if !want(self, 1) {
                    return self.err_expr(span);
                }
                let ap = self.rexpr(&args[0]);
                if Self::is_error(&ap) {
                    return ap;
                }
                HExpr::new(HExprKind::VaEnd(Box::new(ap)), void, span)
            }
            "__builtin_va_copy" => {
                if !want(self, 2) {
                    return self.err_expr(span);
                }
                let d = self.rexpr(&args[0]);
                let s = self.rexpr(&args[1]);
                if Self::is_error(&d) || Self::is_error(&s) {
                    return self.err_expr(span);
                }
                HExpr::new(HExprKind::VaCopy(Box::new(d), Box::new(s)), void, span)
            }
            "__builtin_expect" => {
                if !want(self, 2) {
                    return self.err_expr(span);
                }
                let v = self.rexpr(&args[0]);
                let _ = self.rexpr(&args[1]);
                v
            }
            "__builtin_unreachable" | "__builtin_trap" => {
                if !want(self, 0) {
                    return self.err_expr(span);
                }
                HExpr::new(HExprKind::Trap, void, span)
            }
            "__builtin_huge_val" | "__builtin_inf" => HExpr::new(HExprKind::Float(f64::INFINITY), p.double, span),
            "__builtin_inff" | "__builtin_huge_valf" => HExpr::new(HExprKind::Float(f64::INFINITY), p.float, span),
            "__builtin_nan" => HExpr::new(HExprKind::Float(f64::NAN), p.double, span),
            "__builtin_nanf" => HExpr::new(HExprKind::Float(f64::NAN), p.float, span),
            other => {
                self.error(span, format!("use of unknown builtin '{}'", other));
                self.err_expr(span)
            }
        }
    }

    fn f_variadic(&self) -> bool {
        // The enclosing function's variadic flag is recorded in its symbol's type.
        let Some(f) = &self.f else { return false };
        let Some(&id) = self.global_names.get(&f.name) else { return false };
        self.types.func_sig(self.syms[id.0 as usize].ty).is_some_and(|s| s.variadic)
    }

    // ───────────────────────────── postfix ─────────────────────────────

    fn index(&mut self, base: &Expr, index: &Expr, span: Span) -> HExpr {
        let b = self.rexpr(base);
        let i = self.rexpr(index);
        if Self::is_error(&b) || Self::is_error(&i) {
            return self.err_expr(span);
        }
        let (ptr, idx) = if self.types.is_pointer(b.ty) && self.types.is_integer(i.ty) {
            (b, i)
        } else if self.types.is_pointer(i.ty) && self.types.is_integer(b.ty) {
            (i, b)
        } else {
            let t = if self.types.is_pointer(b.ty) { self.show(i.ty) } else { self.show(b.ty) };
            let _ = t;
            self.emit(
                Diagnostic::error(span, "subscripted value is not an array, pointer, or vector").with_label(base.span),
            );
            return self.err_expr(span);
        };
        let pointee = self.types.pointee(ptr.ty).unwrap();
        if self.types.is_void(pointee) {
            self.emit(Diagnostic::error(span, "subscript of pointer to void is not allowed"));
            return self.err_expr(span);
        }
        let sum = self.make_ptr_add(ptr, idx, false, span);
        if Self::is_error(&sum) {
            return sum;
        }
        HExpr::new(HExprKind::Deref(Box::new(sum)), pointee, span)
    }

    fn member(&mut self, base: &Expr, member: Ident, arrow: bool, span: Span) -> HExpr {
        let h = if arrow { self.rexpr(base) } else { self.expr(base) };
        if Self::is_error(&h) {
            return h;
        }
        // Normalise to a record-typed base.
        let (b, rec_ty) = if arrow {
            match self.types.pointee(h.ty) {
                Some(p) if self.types.is_record(p) => {
                    let d = HExpr::new(HExprKind::Deref(Box::new(h)), p, span);
                    (d, p)
                }
                _ => {
                    let s = self.show(h.ty);
                    let hint = if self.types.is_record(h.ty) { "; did you mean to use '.'?" } else { "" };
                    self.emit(
                        Diagnostic::error(
                            member.span,
                            format!("member reference type '{}' is not a pointer to a structure or union{}", s, hint),
                        )
                        .with_label(base.span),
                    );
                    return self.err_expr(span);
                }
            }
        } else if self.types.is_record(h.ty) {
            let t = h.ty;
            (h, t)
        } else {
            let s = self.show(h.ty);
            let hint = if self.types.pointee(h.ty).is_some_and(|p| self.types.is_record(p)) {
                "; did you mean to use '->'?"
            } else {
                ""
            };
            self.emit(
                Diagnostic::error(
                    member.span,
                    format!("member reference base type '{}' is not a structure or union{}", s, hint),
                )
                .with_label(base.span),
            );
            return self.err_expr(span);
        };
        let rid = self.types.record_id(rec_ty).unwrap();
        if !self.types.record(rid).complete {
            let s = self.show(rec_ty);
            self.emit(
                Diagnostic::error(member.span, format!("incomplete definition of type '{}'", s)).with_label(base.span),
            );
            return self.err_expr(span);
        }
        let Some((field, offset)) = self.types.find_field(rid, member.name) else {
            let s = self.show(rec_ty);
            self.emit(Diagnostic::error(member.span, format!("no member named '{}' in '{}'", member.name, s)));
            return self.err_expr(span);
        };
        let quals = self.types.quals(b.ty);
        let fty = self.types.qualified(field.ty, quals);
        let mref = MemberRef { offset, bit: field.bit };
        HExpr::new(HExprKind::Member(Box::new(b), mref), fty, span)
    }

    // ───────────────────────────── casts / sizeof ─────────────────────────────

    /// Run-time size computations of variable length array types named inside
    /// an expression (`sizeof(int[n])`, `(int (*)[n])p`) are evaluated first.
    pub(crate) fn wrap_vla_inits(&mut self, mark: usize, e: HExpr) -> HExpr {
        if self.pending_vla.len() <= mark {
            return e;
        }
        let inits = self.pending_vla.split_off(mark);
        let (ty, span) = (e.ty, e.span);
        inits.into_iter().rev().fold(e, |acc, i| HExpr::new(HExprKind::Comma(Box::new(i), Box::new(acc)), ty, span))
    }

    fn cast_expr(&mut self, ty: &TypeName, operand: &Expr, span: Span) -> HExpr {
        let mark = self.pending_vla.len();
        let e = self.cast_expr_inner(ty, operand, span);
        self.wrap_vla_inits(mark, e)
    }

    fn cast_expr_inner(&mut self, ty: &TypeName, operand: &Expr, span: Span) -> HExpr {
        let to = self.type_name(ty);
        let e = self.rexpr(operand);
        if Self::is_error(&e) {
            return e;
        }
        let to_u = self.types.unqual(to);
        if self.types.is_void(to_u) {
            return HExpr::new(HExprKind::Cast(CastKind::ToVoid, Box::new(e)), to_u, span);
        }
        let t = &self.types;
        if t.is_record(to_u) {
            if t.record_id(e.ty) == t.record_id(to_u) {
                return HExpr { ty: to_u, ..e };
            }
            let (a, b) = (self.show(to), self.show(e.ty));
            self.error(span, format!("cannot cast '{}' to '{}'", b, a));
            return self.err_expr(span);
        }
        if !t.is_scalar(to_u) {
            let s = self.show(to);
            self.error(span, format!("used type '{}' where arithmetic or pointer type is required", s));
            return self.err_expr(span);
        }
        if !t.is_scalar(e.ty) {
            let s = self.show(e.ty);
            self.emit(Diagnostic::error(
                operand.span,
                format!("used type '{}' where arithmetic or pointer type is required", s),
            ));
            return self.err_expr(span);
        }
        if (t.is_pointer(to_u) && t.is_floating(e.ty)) || (t.is_floating(to_u) && t.is_pointer(e.ty)) {
            let (a, b) = (self.show(to), self.show(e.ty));
            self.error(span, format!("cannot cast '{}' to '{}'", b, a));
            return self.err_expr(span);
        }
        if t.is_pointer(to_u) && t.is_integer(e.ty) && t.size_of(e.ty) < Some(8) && eval(&self.types, &e).is_none() {
            let (a, b) = (self.show(to), self.show(e.ty));
            self.warn(Warn::IntToPointerCast, span, format!("cast to '{}' from smaller integer type '{}'", a, b));
        }
        let mut r = self.cast_to(e, to_u);
        r.span = span;
        r
    }

    fn sizeof_type(&mut self, ty: Ty, span: Span) -> HExpr {
        let ul = self.types.p.ulong;
        if self.types.is_function(ty) || self.types.is_void(ty) {
            return self.int_lit(1, ul, span); // GNU extension
        }
        if self.types.is_vla(ty) {
            return HExpr::new(HExprKind::VlaSizeof(ty), ul, span);
        }
        match self.types.size_of(ty) {
            Some(s) => self.int_lit(s, ul, span),
            None => {
                let s = self.show(ty);
                self.error(span, format!("invalid application of 'sizeof' to an incomplete type '{}'", s));
                self.err_expr(span)
            }
        }
    }

    fn sizeof_expr(&mut self, x: &Expr, span: Span) -> HExpr {
        let h = self.expr(x);
        if Self::is_error(&h) {
            return h;
        }
        if let HExprKind::Member(_, m) = &h.kind {
            if m.bit.is_some() {
                self.error(span, "invalid application of 'sizeof' to bit-field");
                return self.err_expr(span);
            }
        }
        self.sizeof_type(h.ty, span)
    }

    fn offsetof(&mut self, ty: &TypeName, path: &[OffsetofStep], span: Span) -> HExpr {
        let mut cur = self.type_name(ty);
        let mut off: u64 = 0;
        for step in path {
            match step {
                OffsetofStep::Field(f) => {
                    let Some(rid) = self.types.record_id(cur) else {
                        let s = self.show(cur);
                        self.error(f.span, format!("offsetof of non-struct type '{}'", s));
                        return self.err_expr(span);
                    };
                    if !self.types.record(rid).complete {
                        let s = self.show(cur);
                        self.error(f.span, format!("offsetof of incomplete type '{}'", s));
                        return self.err_expr(span);
                    }
                    match self.types.find_field(rid, f.name) {
                        Some((field, o)) => {
                            if field.bit.is_some() {
                                self.error(f.span, "cannot compute offset of bit-field");
                                return self.err_expr(span);
                            }
                            off += o;
                            cur = field.ty;
                        }
                        None => {
                            let s = self.show(cur);
                            self.error(f.span, format!("no member named '{}' in '{}'", f.name, s));
                            return self.err_expr(span);
                        }
                    }
                }
                OffsetofStep::Index(e) => {
                    let Some(elem) = self.types.array_elem(cur) else {
                        self.error(e.span, "offsetof subscript applied to a non-array");
                        return self.err_expr(span);
                    };
                    let h = self.rexpr(e);
                    match eval_int(&self.types, &h) {
                        Some(v) => off += v * self.types.size_of(elem).unwrap_or(0),
                        None => {
                            self.error(e.span, "offsetof index is not an integer constant expression");
                            return self.err_expr(span);
                        }
                    }
                    cur = elem;
                }
            }
        }
        let ul = self.types.p.ulong;
        self.int_lit(off, ul, span)
    }

    fn compound_literal(&mut self, ty: &TypeName, init: &InitList, span: Span) -> HExpr {
        let t = self.type_name(ty);
        if self.types.is_vla(t) {
            self.error(span, "not yet supported: variable length array compound literals");
            return self.err_expr(span);
        }
        let is_static = !self.in_function();
        let (ty2, plan) = self.initialize(t, &Initializer::List(init.clone()), is_static, span);
        let Some(plan) = plan else { return self.err_expr(span) };
        let align = self.types.align_of(ty2);
        if is_static {
            let sid = self.new_anon_global("compound".to_string(), ty2, span, align);
            self.syms[sid.0 as usize].init = Some(plan);
            return HExpr::new(HExprKind::Global(sid), ty2, span);
        }
        let lid = self.new_local(Symbol::new("<compound literal>"), ty2, span, false, align);
        self.f.as_mut().unwrap().locals[lid.0 as usize].used = true;
        HExpr::new(HExprKind::CompoundLit { local: lid, init: plan }, ty2, span)
    }

    fn generic(&mut self, controlling: &Expr, assocs: &[GenericAssoc], span: Span) -> HExpr {
        let c = self.rexpr(controlling);
        if Self::is_error(&c) {
            return c;
        }
        let cty = self.types.unqual(c.ty);
        let mut chosen: Option<&GenericAssoc> = None;
        let mut default: Option<&GenericAssoc> = None;
        for a in assocs {
            match &a.ty {
                None => {
                    if default.is_some() {
                        self.error(a.expr.span, "duplicate default generic association");
                    }
                    default = Some(a);
                }
                Some(tn) => {
                    let t = self.type_name(tn);
                    let tu = self.types.unqual(t);
                    if self.types.compatible(tu, cty) {
                        if chosen.is_some() {
                            self.error(
                                tn.span,
                                "type in generic association is compatible with a previously specified type",
                            );
                        }
                        chosen.get_or_insert(a);
                    }
                }
            }
        }
        match chosen.or(default) {
            Some(a) => {
                let h = self.expr(&a.expr);
                HExpr { span, ..h }
            }
            None => {
                let s = self.show(c.ty);
                self.error(
                    span,
                    format!("controlling expression type '{}' not compatible with any generic association type", s),
                );
                self.err_expr(span)
            }
        }
    }
}

fn unparen_expr(e: &Expr) -> &Expr {
    match &e.kind {
        ExprKind::Paren(i) => unparen_expr(i),
        _ => e,
    }
}

fn bin_kind(op: BinOp) -> BinKind {
    match op {
        BinOp::Add => BinKind::Add,
        BinOp::Sub => BinKind::Sub,
        BinOp::Mul => BinKind::Mul,
        BinOp::Div => BinKind::Div,
        BinOp::Rem => BinKind::Rem,
        BinOp::Shl => BinKind::Shl,
        BinOp::Shr => BinKind::Shr,
        BinOp::Lt => BinKind::Lt,
        BinOp::Gt => BinKind::Gt,
        BinOp::Le => BinKind::Le,
        BinOp::Ge => BinKind::Ge,
        BinOp::Eq => BinKind::Eq,
        BinOp::Ne => BinKind::Ne,
        BinOp::BitAnd => BinKind::And,
        BinOp::BitXor => BinKind::Xor,
        BinOp::BitOr => BinKind::Or,
        BinOp::LogAnd | BinOp::LogOr => unreachable!("logical ops have their own HIR nodes"),
    }
}
