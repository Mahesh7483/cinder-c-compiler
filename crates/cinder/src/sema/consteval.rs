//! Compile-time evaluation of typed expressions: integer constant
//! expressions (array sizes, case labels, enumerators, `_Static_assert`) and
//! the address/arithmetic constants allowed in static initializers.

use crate::hir::*;
use crate::types::{Ty, TyKind, TypeTable};

/// Normalize `v` to the width/signedness of `ty`: unsigned values are
/// zero-extended, signed values sign-extended, `_Bool` is 0/1.
pub fn norm(types: &TypeTable, v: u64, ty: Ty) -> u64 {
    let r = types.int_repr(ty);
    if types.is_bool(r) {
        return (v != 0) as u64;
    }
    if types.is_pointer(r) {
        return v;
    }
    let bits = types.size_of(r).unwrap_or(8) * 8;
    if bits >= 64 {
        return v;
    }
    let mask = (1u64 << bits) - 1;
    let low = v & mask;
    if types.is_signed(r) && (low >> (bits - 1)) & 1 == 1 {
        low | !mask
    } else {
        low
    }
}

fn as_float(types: &TypeTable, v: f64, ty: Ty) -> f64 {
    if matches!(types.kind(ty), TyKind::Float) {
        (v as f32) as f64
    } else {
        v
    }
}

pub fn eval(types: &TypeTable, e: &HExpr) -> Option<ConstVal> {
    match &e.kind {
        HExprKind::Int(v) => Some(ConstVal::Int(norm(types, *v, e.ty))),
        HExprKind::Float(f) => Some(ConstVal::Float(as_float(types, *f, e.ty))),
        HExprKind::Cast(kind, inner) => eval_cast(types, *kind, inner, e.ty),
        HExprKind::Unary(op, x) => {
            let v = eval(types, x)?;
            match (op, v) {
                (UnKind::Neg, ConstVal::Int(i)) => Some(ConstVal::Int(norm(types, i.wrapping_neg(), e.ty))),
                (UnKind::Neg, ConstVal::Float(f)) => Some(ConstVal::Float(-f)),
                (UnKind::BitNot, ConstVal::Int(i)) => Some(ConstVal::Int(norm(types, !i, e.ty))),
                (UnKind::LogNot, ConstVal::Int(i)) => Some(ConstVal::Int((i == 0) as u64)),
                (UnKind::LogNot, ConstVal::Float(f)) => Some(ConstVal::Int((f == 0.0) as u64)),
                (UnKind::LogNot, ConstVal::Addr { .. }) => Some(ConstVal::Int(0)),
                _ => None,
            }
        }
        HExprKind::Binary(op, l, r) => eval_binary(types, *op, l, r, e.ty),
        HExprKind::LogAnd(l, r) => {
            let lv = truth(&eval(types, l)?);
            if !lv {
                return Some(ConstVal::Int(0));
            }
            Some(ConstVal::Int(truth(&eval(types, r)?) as u64))
        }
        HExprKind::LogOr(l, r) => {
            let lv = truth(&eval(types, l)?);
            if lv {
                return Some(ConstVal::Int(1));
            }
            Some(ConstVal::Int(truth(&eval(types, r)?) as u64))
        }
        HExprKind::Cond(c, t, f) => {
            if truth(&eval(types, c)?) {
                eval(types, t)
            } else {
                eval(types, f)
            }
        }
        HExprKind::PtrAdd { ptr, idx, scale, negate } => {
            let p = eval(types, ptr)?;
            let ConstVal::Int(i) = eval(types, idx)? else { return None };
            let delta = (i as i64).wrapping_mul(*scale as i64);
            let delta = if *negate { delta.wrapping_neg() } else { delta };
            match p {
                ConstVal::Addr { base, offset } => Some(ConstVal::Addr { base, offset: offset.wrapping_add(delta) }),
                ConstVal::Int(a) => Some(ConstVal::Int(a.wrapping_add(delta as u64))),
                ConstVal::Float(_) => None,
            }
        }
        HExprKind::PtrDiff { l, r, elem_size } => {
            let (a, b) = (eval(types, l)?, eval(types, r)?);
            match (a, b) {
                (ConstVal::Addr { base: b1, offset: o1 }, ConstVal::Addr { base: b2, offset: o2 }) if b1 == b2 => {
                    Some(ConstVal::Int(((o1 - o2) / (*elem_size).max(1) as i64) as u64))
                }
                (ConstVal::Int(x), ConstVal::Int(y)) => {
                    Some(ConstVal::Int(((x as i64 - y as i64) / (*elem_size).max(1) as i64) as u64))
                }
                _ => None,
            }
        }
        HExprKind::AddrOf(place) => addr_of_place(types, place),
        _ => None,
    }
}

fn truth(v: &ConstVal) -> bool {
    match v {
        ConstVal::Int(i) => *i != 0,
        ConstVal::Float(f) => *f != 0.0,
        ConstVal::Addr { .. } => true,
    }
}

fn eval_cast(types: &TypeTable, kind: CastKind, inner: &HExpr, to: Ty) -> Option<ConstVal> {
    match kind {
        CastKind::ArrayToPointer | CastKind::FunctionToPointer => addr_of_place(types, inner),
        CastKind::LValueToRValue | CastKind::ToVoid => None,
        CastKind::NoOp => eval(types, inner),
        CastKind::ToBool => Some(ConstVal::Int(truth(&eval(types, inner)?) as u64)),
        CastKind::IntToInt | CastKind::IntToPtr => match eval(types, inner)? {
            ConstVal::Int(i) => Some(ConstVal::Int(norm(types, i, to))),
            _ => None,
        },
        CastKind::PtrToInt => match eval(types, inner)? {
            ConstVal::Int(i) => Some(ConstVal::Int(norm(types, i, to))),
            a @ ConstVal::Addr { .. } if types.size_of(to) == Some(8) => Some(a),
            _ => None,
        },
        CastKind::IntToFloat => match eval(types, inner)? {
            ConstVal::Int(i) => {
                let f = if types.is_signed(inner.ty) { i as i64 as f64 } else { i as f64 };
                Some(ConstVal::Float(as_float(types, f, to)))
            }
            _ => None,
        },
        CastKind::FloatToInt => match eval(types, inner)? {
            ConstVal::Float(f) => {
                let v = if types.is_signed(to) || types.is_bool(to) { f as i64 as u64 } else { f as u64 };
                Some(ConstVal::Int(norm(types, v, to)))
            }
            _ => None,
        },
        CastKind::FloatToFloat => match eval(types, inner)? {
            ConstVal::Float(f) => Some(ConstVal::Float(as_float(types, f, to))),
            _ => None,
        },
    }
}

fn eval_binary(types: &TypeTable, op: BinKind, l: &HExpr, r: &HExpr, ty: Ty) -> Option<ConstVal> {
    let (a, b) = (eval(types, l)?, eval(types, r)?);
    match (a, b) {
        (ConstVal::Int(x), ConstVal::Int(y)) => {
            let signed = types.is_signed(l.ty) && !types.is_pointer(l.ty);
            let bits = types.size_of(types.int_repr(l.ty)).unwrap_or(8) * 8;
            let (sx, sy) = (x as i64, y as i64);
            let v = match op {
                BinKind::Add => x.wrapping_add(y),
                BinKind::Sub => x.wrapping_sub(y),
                BinKind::Mul => x.wrapping_mul(y),
                BinKind::Div | BinKind::Rem => {
                    if y == 0 {
                        return None;
                    }
                    if signed {
                        if sx == i64::MIN && sy == -1 {
                            return None;
                        }
                        let (min, _) = types.int_range(l.ty);
                        if bits < 64 && sx as i128 == min && sy == -1 {
                            return None;
                        }
                        (if op == BinKind::Div { sx / sy } else { sx % sy }) as u64
                    } else if op == BinKind::Div {
                        x / y
                    } else {
                        x % y
                    }
                }
                BinKind::And => x & y,
                BinKind::Or => x | y,
                BinKind::Xor => x ^ y,
                BinKind::Shl | BinKind::Shr => {
                    if y >= bits || (sy < 0 && types.is_signed(r.ty)) {
                        return None;
                    }
                    if op == BinKind::Shl {
                        x << y
                    } else if signed {
                        (sx >> y) as u64
                    } else {
                        x >> y
                    }
                }
                BinKind::Eq => return Some(ConstVal::Int((x == y) as u64)),
                BinKind::Ne => return Some(ConstVal::Int((x != y) as u64)),
                BinKind::Lt => return Some(ConstVal::Int(if signed { sx < sy } else { x < y } as u64)),
                BinKind::Gt => return Some(ConstVal::Int(if signed { sx > sy } else { x > y } as u64)),
                BinKind::Le => return Some(ConstVal::Int(if signed { sx <= sy } else { x <= y } as u64)),
                BinKind::Ge => return Some(ConstVal::Int(if signed { sx >= sy } else { x >= y } as u64)),
            };
            Some(ConstVal::Int(norm(types, v, ty)))
        }
        (ConstVal::Float(x), ConstVal::Float(y)) => {
            let f = match op {
                BinKind::Add => x + y,
                BinKind::Sub => x - y,
                BinKind::Mul => x * y,
                BinKind::Div => x / y,
                BinKind::Eq => return Some(ConstVal::Int((x == y) as u64)),
                BinKind::Ne => return Some(ConstVal::Int((x != y) as u64)),
                BinKind::Lt => return Some(ConstVal::Int((x < y) as u64)),
                BinKind::Gt => return Some(ConstVal::Int((x > y) as u64)),
                BinKind::Le => return Some(ConstVal::Int((x <= y) as u64)),
                BinKind::Ge => return Some(ConstVal::Int((x >= y) as u64)),
                _ => return None,
            };
            Some(ConstVal::Float(as_float(types, f, ty)))
        }
        // Comparing two addresses of the same object is a constant too.
        (ConstVal::Addr { base: b1, offset: o1 }, ConstVal::Addr { base: b2, offset: o2 }) if b1 == b2 => match op {
            BinKind::Eq => Some(ConstVal::Int((o1 == o2) as u64)),
            BinKind::Ne => Some(ConstVal::Int((o1 != o2) as u64)),
            _ => None,
        },
        _ => None,
    }
}

/// Address constant of a place expression, if it has one.
pub fn addr_of_place(types: &TypeTable, place: &HExpr) -> Option<ConstVal> {
    match &place.kind {
        HExprKind::Global(s) => Some(ConstVal::Addr { base: AddrBase::Global(*s), offset: 0 }),
        HExprKind::Str(s) => Some(ConstVal::Addr { base: AddrBase::Str(*s), offset: 0 }),
        HExprKind::Deref(ptr) => eval(types, ptr),
        HExprKind::Member(base, m) => {
            if m.bit.is_some() {
                return None;
            }
            match addr_of_place(types, base)? {
                ConstVal::Addr { base, offset } => Some(ConstVal::Addr { base, offset: offset + m.offset as i64 }),
                ConstVal::Int(a) => Some(ConstVal::Int(a.wrapping_add(m.offset))),
                ConstVal::Float(_) => None,
            }
        }
        _ => None,
    }
}

/// An integer constant expression's value, as a sign-/zero-extended 64-bit pattern.
pub fn eval_int(types: &TypeTable, e: &HExpr) -> Option<u64> {
    if !types.is_integer(e.ty) {
        return None;
    }
    match eval(types, e)? {
        ConstVal::Int(v) => Some(v),
        _ => None,
    }
}
