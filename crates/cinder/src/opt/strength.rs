//! Algebraic simplification and strength reduction.
//!
//! Every rewrite is local to one instruction and exact for two's-complement
//! wrap-around arithmetic: identities (`x+0`, `x*1`, `x^x`), constant
//! canonicalization (constants to the right, `x-c` to `x+(-c)`), strength
//! reduction (`x*2^k` to a shift, unsigned `/` and `%` by `2^k` to a shift and
//! a mask, signed `/` and `%` by `2^k` to a bias-and-shift sequence), cast
//! chains, and comparison clean-up.

use super::{apply_replacements, canon, fold_int_bin, resolve, unsigned};
use crate::ir::*;
use std::collections::HashMap;

enum Rw {
    None,
    /// The instruction's result is this operand.
    Value(Operand),
    /// Replace the instruction's operation (same result type).
    Kind(InstKind),
    /// Signed division / remainder by `2^k`.
    SDivPow2 {
        x: Operand,
        ty: Type,
        k: u32,
    },
    SRemPow2 {
        x: Operand,
        ty: Type,
        k: u32,
    },
}

pub fn run(f: &mut Func) -> bool {
    let mut repl: HashMap<ValueId, Operand> = HashMap::new();
    let mut changed = false;
    for bi in 0..f.blocks.len() {
        let b = BlockId(bi as u32);
        let mut pos = 0;
        while pos < f.blocks[bi].insts.len() {
            let id = f.blocks[bi].insts[pos];
            if f.dead_insts[id.idx()] {
                pos += 1;
                continue;
            }
            let kind = f.insts[id.idx()].kind.clone();
            let line = f.insts[id.idx()].line;
            match simplify(f, &repl, &kind) {
                Rw::None => {}
                Rw::Value(o) => {
                    repl.insert(f.insts[id.idx()].dst.unwrap(), o);
                    f.kill(id);
                    changed = true;
                }
                Rw::Kind(k) => {
                    f.insts[id.idx()].kind = k;
                    changed = true;
                }
                Rw::SDivPow2 { x, ty, k } => {
                    expand_signed_div(f, b, &mut pos, id, line, x, ty, k, true);
                    changed = true;
                }
                Rw::SRemPow2 { x, ty, k } => {
                    expand_signed_div(f, b, &mut pos, id, line, x, ty, k, false);
                    changed = true;
                }
            }
            pos += 1;
        }
    }
    if changed {
        apply_replacements(f, &repl);
        f.sweep();
    }
    changed
}

/// Rewrite the division/remainder at `*pos` (instruction `id`) by `2^k` into a
/// bias-and-shift sequence; `*pos` ends up on the rewritten instruction.
#[allow(clippy::too_many_arguments)]
fn expand_signed_div(
    f: &mut Func,
    b: BlockId,
    pos: &mut usize,
    id: InstId,
    line: u32,
    x: Operand,
    ty: Type,
    k: u32,
    is_div: bool,
) {
    let bits = ty.bits() as i64;
    let mut emit = |f: &mut Func, op: BinOp, l: Operand, r: Operand| -> Operand {
        let o = f.insert(b, *pos, InstKind::Bin { op, ty, lhs: l, rhs: r }, Some(ty), None, line).unwrap();
        *pos += 1;
        o
    };
    // bias = (x < 0) ? 2^k - 1 : 0, so the arithmetic shift rounds toward zero
    let sign = emit(f, BinOp::AShr, x, Operand::Int(bits - 1, ty));
    let bias = emit(f, BinOp::LShr, sign, Operand::Int(bits - k as i64, ty));
    let biased = emit(f, BinOp::Add, x, bias);
    let kind = if is_div {
        InstKind::Bin { op: BinOp::AShr, ty, lhs: biased, rhs: Operand::Int(k as i64, ty) }
    } else {
        let low = emit(f, BinOp::And, biased, Operand::Int(canon(-(1i64 << k), ty), ty));
        InstKind::Bin { op: BinOp::Sub, ty, lhs: x, rhs: low }
    };
    f.insts[id.idx()].kind = kind;
}

fn def_kind(f: &Func, o: Operand) -> Option<&InstKind> {
    let Operand::Value(v) = o else { return None };
    let id = f.def_inst(v)?;
    if f.dead_insts[id.idx()] {
        return None;
    }
    Some(&f.insts[id.idx()].kind)
}

fn pow2_exp(c: i64, ty: Type) -> Option<u32> {
    let u = unsigned(c, ty);
    if u > 1 && u.is_power_of_two() {
        Some(u.trailing_zeros())
    } else {
        None
    }
}

fn simplify(f: &Func, repl: &HashMap<ValueId, Operand>, kind: &InstKind) -> Rw {
    let r = |o: Operand| resolve(repl, o);
    match kind {
        InstKind::Bin { op, ty, lhs, rhs } if !op.is_float() => {
            let (mut a, mut b) = (r(*lhs), r(*rhs));
            if op.is_commutative() && matches!(a, Operand::Int(..)) && !matches!(b, Operand::Int(..)) {
                std::mem::swap(&mut a, &mut b);
            }
            let (op, ty) = (*op, *ty);
            let zero = Operand::Int(0, ty);
            if let (Operand::Int(x, _), Operand::Int(y, _)) = (a, b) {
                return match fold_int_bin(op, ty, x, y) {
                    Some(v) => Rw::Value(Operand::Int(v, ty)),
                    None => Rw::None,
                };
            }
            if a == b && !matches!(a, Operand::Undef(_)) {
                match op {
                    BinOp::Sub | BinOp::Xor => return Rw::Value(zero),
                    BinOp::And | BinOp::Or => return Rw::Value(a),
                    _ => {}
                }
            }
            if let Operand::Int(c, _) = b {
                match (op, c) {
                    (BinOp::Add | BinOp::Sub | BinOp::Or | BinOp::Xor | BinOp::Shl | BinOp::LShr | BinOp::AShr, 0) => {
                        return Rw::Value(a)
                    }
                    (BinOp::Mul | BinOp::SDiv | BinOp::UDiv, 1) => return Rw::Value(a),
                    (BinOp::Mul | BinOp::And, 0) => return Rw::Value(zero),
                    (BinOp::SRem | BinOp::URem, 1) => return Rw::Value(zero),
                    (BinOp::And, -1) => return Rw::Value(a),
                    (BinOp::Or, -1) => return Rw::Value(Operand::Int(-1, ty)),
                    (BinOp::Mul, -1) => return Rw::Kind(InstKind::Un { op: UnOp::Neg, ty, val: a }),
                    _ => {}
                }
                match op {
                    BinOp::Sub => {
                        return Rw::Kind(InstKind::Bin {
                            op: BinOp::Add,
                            ty,
                            lhs: a,
                            rhs: Operand::Int(canon(c.wrapping_neg(), ty), ty),
                        })
                    }
                    BinOp::Mul => {
                        if let Some(k) = pow2_exp(c, ty) {
                            return Rw::Kind(InstKind::Bin {
                                op: BinOp::Shl,
                                ty,
                                lhs: a,
                                rhs: Operand::Int(k as i64, ty),
                            });
                        }
                    }
                    BinOp::UDiv => {
                        if let Some(k) = pow2_exp(c, ty) {
                            return Rw::Kind(InstKind::Bin {
                                op: BinOp::LShr,
                                ty,
                                lhs: a,
                                rhs: Operand::Int(k as i64, ty),
                            });
                        }
                    }
                    BinOp::URem => {
                        if let Some(k) = pow2_exp(c, ty) {
                            let mask = canon(((1u64 << k) - 1) as i64, ty);
                            return Rw::Kind(InstKind::Bin { op: BinOp::And, ty, lhs: a, rhs: Operand::Int(mask, ty) });
                        }
                    }
                    BinOp::SDiv if c > 0 => {
                        if let Some(k) = pow2_exp(c, ty) {
                            return Rw::SDivPow2 { x: a, ty, k };
                        }
                    }
                    BinOp::SRem if c > 0 => {
                        if let Some(k) = pow2_exp(c, ty) {
                            return Rw::SRemPow2 { x: a, ty, k };
                        }
                    }
                    BinOp::Add => {
                        // (x + c1) + c2  =>  x + (c1 + c2)
                        if let Some(InstKind::Bin { op: BinOp::Add, lhs: x, rhs: Operand::Int(c1, _), .. }) =
                            def_kind(f, a)
                        {
                            return Rw::Kind(InstKind::Bin {
                                op: BinOp::Add,
                                ty,
                                lhs: r(*x),
                                rhs: Operand::Int(canon(c1.wrapping_add(c), ty), ty),
                            });
                        }
                    }
                    _ => {}
                }
            }
            // canonical operand order for commutative operations
            if (a, b) != (r(*lhs), r(*rhs)) {
                return Rw::Kind(InstKind::Bin { op, ty, lhs: a, rhs: b });
            }
            Rw::None
        }
        // negation, complement and fneg are all involutions
        InstKind::Un { op, val, .. } => match def_kind(f, r(*val)) {
            Some(InstKind::Un { op: inner_op, val: inner, .. }) if inner_op == op => Rw::Value(r(*inner)),
            _ => Rw::None,
        },
        InstKind::ICmp { pred, ty, lhs, rhs } => {
            let (mut p, mut a, mut b) = (*pred, r(*lhs), r(*rhs));
            if matches!(a, Operand::Int(..)) && !matches!(b, Operand::Int(..)) {
                std::mem::swap(&mut a, &mut b);
                p = p.swapped();
            }
            let ty = *ty;
            if let (Operand::Int(x, _), Operand::Int(y, _)) = (a, b) {
                return Rw::Value(Operand::Int(super::fold_icmp(p, ty, x, y) as i64, Type::I32));
            }
            if a == b && !matches!(a, Operand::Undef(_)) {
                let v = matches!(p, IPred::Eq | IPred::Sle | IPred::Sge | IPred::Ule | IPred::Uge);
                return Rw::Value(Operand::Int(v as i64, Type::I32));
            }
            if let Operand::Int(c, _) = b {
                // unsigned comparisons against zero
                if c == 0 {
                    match p {
                        IPred::Ult => return Rw::Value(Operand::Int(0, Type::I32)),
                        IPred::Uge => return Rw::Value(Operand::Int(1, Type::I32)),
                        IPred::Ule => return Rw::Kind(InstKind::ICmp { pred: IPred::Eq, ty, lhs: a, rhs: b }),
                        IPred::Ugt => return Rw::Kind(InstKind::ICmp { pred: IPred::Ne, ty, lhs: a, rhs: b }),
                        _ => {}
                    }
                    // comparing a 0/1 comparison result against zero
                    if matches!(p, IPred::Ne | IPred::Eq) && ty == Type::I32 {
                        match def_kind(f, a) {
                            Some(InstKind::ICmp { pred: ip, ty: it, lhs: il, rhs: ir }) => {
                                return if p == IPred::Ne {
                                    Rw::Value(a)
                                } else {
                                    Rw::Kind(InstKind::ICmp { pred: ip.negated(), ty: *it, lhs: r(*il), rhs: r(*ir) })
                                };
                            }
                            Some(InstKind::FCmp { pred: fp, ty: ft, lhs: fl, rhs: fr }) => {
                                if p == IPred::Ne {
                                    return Rw::Value(a);
                                }
                                // NaN-correct negation exists only for == and !=
                                let neg = match fp {
                                    FPred::Oeq => Some(FPred::Une),
                                    FPred::Une => Some(FPred::Oeq),
                                    _ => None,
                                };
                                if let Some(np) = neg {
                                    return Rw::Kind(InstKind::FCmp { pred: np, ty: *ft, lhs: r(*fl), rhs: r(*fr) });
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            if (p, a, b) != (*pred, r(*lhs), r(*rhs)) {
                return Rw::Kind(InstKind::ICmp { pred: p, ty, lhs: a, rhs: b });
            }
            Rw::None
        }
        InstKind::Cast { op, from, to, val } => {
            let v = r(*val);
            let (op, from, to) = (*op, *from, *to);
            let Some(InstKind::Cast { op: iop, from: ifrom, val: ival, .. }) = def_kind(f, v) else { return Rw::None };
            let (iop, ifrom, ival) = (*iop, *ifrom, r(*ival));
            match (op, iop) {
                (CastOp::Trunc, CastOp::ZExt | CastOp::SExt) => {
                    if to == ifrom {
                        Rw::Value(ival)
                    } else if to.size() < ifrom.size() {
                        Rw::Kind(InstKind::Cast { op: CastOp::Trunc, from: ifrom, to, val: ival })
                    } else {
                        Rw::Kind(InstKind::Cast { op: iop, from: ifrom, to, val: ival })
                    }
                }
                (CastOp::Trunc, CastOp::Trunc) | (CastOp::ZExt, CastOp::ZExt) | (CastOp::SExt, CastOp::SExt) => {
                    Rw::Kind(InstKind::Cast { op, from: ifrom, to, val: ival })
                }
                (CastOp::SExt, CastOp::ZExt) => {
                    Rw::Kind(InstKind::Cast { op: CastOp::ZExt, from: ifrom, to, val: ival })
                }
                (CastOp::PtrToInt, CastOp::IntToPtr) if to == ifrom => Rw::Value(ival),
                (CastOp::IntToPtr, CastOp::PtrToInt) if from.size() == 8 && ifrom == to => Rw::Value(ival),
                _ => Rw::None,
            }
        }
        InstKind::PtrAdd { base, offset } => {
            let (bs, off) = (r(*base), r(*offset));
            let Operand::Int(c2, _) = off else { return Rw::None };
            if c2 == 0 {
                return Rw::Value(bs);
            }
            if let Some(InstKind::PtrAdd { base: b2, offset: Operand::Int(c1, _) }) = def_kind(f, bs) {
                return Rw::Kind(InstKind::PtrAdd {
                    base: r(*b2),
                    offset: Operand::Int(c1.wrapping_add(c2), Type::I64),
                });
            }
            Rw::None
        }
        InstKind::Select { cond, a, b, .. } => {
            let (c, a, b) = (r(*cond), r(*a), r(*b));
            if let Operand::Int(c, _) = c {
                return Rw::Value(if c != 0 { a } else { b });
            }
            if a == b {
                return Rw::Value(a);
            }
            Rw::None
        }
        _ => Rw::None,
    }
}
