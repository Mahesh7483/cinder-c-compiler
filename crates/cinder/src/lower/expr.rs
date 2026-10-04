//! Expression lowering.

use super::*;
use crate::abi as callconv;
use crate::hir::{BinKind, CastKind, ConstVal, HExpr, HExprKind, InitPlan, InitValue, UnKind};

#[derive(Clone, Copy)]
pub(crate) struct BitLoc {
    pub shift: u32,
    pub width: u32,
    /// Type of the storage unit that is loaded and stored.
    pub unit: Type,
    pub signed: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct Place {
    pub ptr: Operand,
    pub bit: Option<BitLoc>,
    pub volatile: bool,
}

impl<'a> FnLower<'a> {
    // ───────────────────────────── IR-level helpers ─────────────────────────────

    pub fn cast(&mut self, op: CastOp, from: Type, to: Type, v: Operand) -> Operand {
        if from == to && matches!(op, CastOp::ZExt | CastOp::SExt | CastOp::Trunc) {
            return v;
        }
        // Fold integer-constant width changes right away.
        if let Operand::Int(c, _) = v {
            match op {
                CastOp::Trunc => return Operand::Int(canon(c as u64, to), to),
                CastOp::SExt => return Operand::Int(c, to),
                CastOp::ZExt => {
                    let masked = if from.bits() >= 64 { c as u64 } else { (c as u64) & ((1u64 << from.bits()) - 1) };
                    return Operand::Int(canon(masked, to), to);
                }
                _ => {}
            }
        }
        self.val(InstKind::Cast { op, from, to, val: v }, to)
    }

    /// Widen an integer value to at least 32 bits (by signedness).
    fn widen32(&mut self, v: Operand, from: Type, signed: bool) -> (Operand, Type) {
        if !from.is_int() || from.size() >= 4 {
            return (v, from);
        }
        let op = if signed { CastOp::SExt } else { CastOp::ZExt };
        (self.cast(op, from, Type::I32, v), Type::I32)
    }

    pub fn bin(&mut self, op: BinOp, ty: Type, l: Operand, r: Operand) -> Operand {
        self.val(InstKind::Bin { op, ty, lhs: l, rhs: r }, ty)
    }

    pub fn load(&mut self, ty: Type, ptr: Operand, volatile: bool) -> Operand {
        self.val(InstKind::Load { ty, ptr, volatile }, ty)
    }

    pub fn store(&mut self, ty: Type, val: Operand, ptr: Operand, volatile: bool) {
        self.emit(InstKind::Store { ty, val, ptr, volatile }, None);
    }

    /// `x != 0` as an `i32` 0/1 for any scalar IR type.
    fn is_nonzero(&mut self, v: Operand, ty: Type) -> Operand {
        match ty {
            Type::F32 | Type::F64 => {
                self.val(InstKind::FCmp { pred: FPred::Une, ty, lhs: v, rhs: Operand::float(0.0, ty) }, Type::I32)
            }
            _ => self.val(InstKind::ICmp { pred: IPred::Ne, ty, lhs: v, rhs: Operand::Int(0, ty) }, Type::I32),
        }
    }

    // ───────────────────────────── scalar conversions ─────────────────────────────

    /// Convert a scalar value between C types (used for casts and the implicit
    /// conversions that compound assignment and `++` perform internally).
    pub fn convert(&mut self, v: Operand, from: Ty, to: Ty) -> Operand {
        let t = &self.hir.types;
        let (fi, ti) = (t.is_integer(from), t.is_integer(to));
        let (ff, tf) = (t.is_floating(from), t.is_floating(to));
        let (fp, tp) = (t.is_pointer(from), t.is_pointer(to));
        let (fty, tty) = (self.ity(from), self.ity(to));
        let from_signed = self.is_signed(from);
        if t.is_bool(to) {
            if t.is_bool(from) {
                return v;
            }
            let nz = self.is_nonzero(v, fty);
            return self.cast(CastOp::Trunc, Type::I32, Type::I8, nz);
        }
        match (fi, ff, fp, ti, tf, tp) {
            (true, _, _, true, _, _) => self.int_to_int(v, fty, tty, from_signed),
            (true, _, _, _, true, _) => {
                let (w, wt) = self.widen32(v, fty, from_signed);
                let op = if from_signed { CastOp::SIToFP } else { CastOp::UIToFP };
                self.cast(op, wt, tty, w)
            }
            (_, true, _, true, _, _) => {
                // narrow results go through i32
                let signed = self.is_signed(to);
                let wide = if tty.size() < 4 { Type::I32 } else { tty };
                let op = if signed || tty.size() < 4 { CastOp::FPToSI } else { CastOp::FPToUI };
                let r = self.cast(op, fty, wide, v);
                self.int_to_int(r, wide, tty, signed)
            }
            (_, true, _, _, true, _) => {
                if fty == tty {
                    v
                } else if fty == Type::F32 {
                    self.cast(CastOp::FPExt, Type::F32, Type::F64, v)
                } else {
                    self.cast(CastOp::FPTrunc, Type::F64, Type::F32, v)
                }
            }
            (_, _, true, true, _, _) => {
                let r = self.cast(CastOp::PtrToInt, Type::Ptr, Type::I64, v);
                self.int_to_int(r, Type::I64, tty, false)
            }
            (true, _, _, _, _, true) => {
                let r = self.int_to_int(v, fty, Type::I64, from_signed);
                self.cast(CastOp::IntToPtr, Type::I64, Type::Ptr, r)
            }
            _ => v,
        }
    }

    fn int_to_int(&mut self, v: Operand, from: Type, to: Type, from_signed: bool) -> Operand {
        use std::cmp::Ordering::*;
        match from.size().cmp(&to.size()) {
            Equal => v,
            Greater => self.cast(CastOp::Trunc, from, to, v),
            Less => self.cast(if from_signed { CastOp::SExt } else { CastOp::ZExt }, from, to, v),
        }
    }

    // ───────────────────────────── places ─────────────────────────────

    /// Address of an aggregate-typed expression (a place, or an rvalue such
    /// as a call result, which is represented by the address of a temporary).
    pub fn address(&mut self, e: &'a HExpr) -> Operand {
        if e.is_place() {
            self.place(e).ptr
        } else {
            self.rv(e)
        }
    }

    pub fn place(&mut self, e: &'a HExpr) -> Place {
        let vol = self.hir.types.quals(e.ty).is_volatile;
        match &e.kind {
            HExprKind::Local(id) => Place { ptr: self.slots[id.0 as usize], bit: None, volatile: vol },
            HExprKind::Global(s) => {
                let sym = self.mb.sym(self.hir, *s);
                Place { ptr: Operand::Global(sym), bit: None, volatile: vol }
            }
            HExprKind::Str(id) => {
                let sym = self.mb.str_sym(self.hir, *id);
                Place { ptr: Operand::Global(sym), bit: None, volatile: false }
            }
            HExprKind::Deref(p) => Place { ptr: self.rv(p), bit: None, volatile: vol },
            HExprKind::Member(base, m) => {
                let b = self.address(base);
                match m.bit {
                    None => Place { ptr: self.ptr_add(b, m.offset), bit: None, volatile: vol },
                    Some(bi) => {
                        let tsize = self.size_of(e.ty).max(1);
                        let unit_bits = tsize * 8;
                        let rel_byte = bi.bit_offset / 8;
                        let outer = m.offset - rel_byte;
                        let (unit_off, shift, unit) = if bi.bit_offset % unit_bits + bi.width as u64 <= unit_bits {
                            let unit_start = (bi.bit_offset / unit_bits) * unit_bits;
                            (outer + unit_start / 8, (bi.bit_offset - unit_start) as u32, self.ity(e.ty))
                        } else {
                            // packed field straddling its unit: access it via the bytes it touches
                            let shift = (bi.bit_offset % 8) as u32;
                            let need = shift + bi.width;
                            (
                                m.offset,
                                shift,
                                Type::from_int_bits(if need <= 8 {
                                    8
                                } else if need <= 16 {
                                    16
                                } else if need <= 32 {
                                    32
                                } else {
                                    64
                                }),
                            )
                        };
                        let ptr = self.ptr_add(b, unit_off);
                        let loc = BitLoc { shift, width: bi.width, unit, signed: self.is_signed(e.ty) };
                        Place { ptr, bit: Some(loc), volatile: vol }
                    }
                }
            }
            HExprKind::CompoundLit { local, init } => {
                let slot = self.slots[local.0 as usize];
                self.emit_init(slot, init, e.ty);
                Place { ptr: slot, bit: None, volatile: false }
            }
            _ => {
                // an aggregate rvalue (e.g. a call result): its address
                Place { ptr: self.rv(e), bit: None, volatile: false }
            }
        }
    }

    /// Value of the bit-field at `p`, as the field's declared integer type.
    fn load_bits(&mut self, p: &Place, field_ty: Type) -> Operand {
        let b = p.bit.unwrap();
        let raw = self.load(b.unit, p.ptr, p.volatile);
        let w = if b.unit == Type::I64 { Type::I64 } else { Type::I32 };
        let wv = if b.unit.size() < w.size() {
            self.cast(if b.signed { CastOp::SExt } else { CastOp::ZExt }, b.unit, w, raw)
        } else {
            raw
        };
        let up = w.bits() - b.shift - b.width;
        let v = if up > 0 { self.bin(BinOp::Shl, w, wv, Operand::Int(up as i64, w)) } else { wv };
        let down = w.bits() - b.width;
        let v = if down > 0 {
            self.bin(if b.signed { BinOp::AShr } else { BinOp::LShr }, w, v, Operand::Int(down as i64, w))
        } else {
            v
        };
        if w.size() > field_ty.size() {
            self.cast(CastOp::Trunc, w, field_ty, v)
        } else {
            v
        }
    }

    /// Store `v` (of the field's declared type) into the bit-field at `p`.
    fn store_bits(&mut self, p: &Place, v: Operand, field_ty: Type) {
        let b = p.bit.unwrap();
        let w = if b.unit == Type::I64 { Type::I64 } else { Type::I32 };
        let old_raw = self.load(b.unit, p.ptr, p.volatile);
        let old = if b.unit.size() < w.size() { self.cast(CastOp::ZExt, b.unit, w, old_raw) } else { old_raw };
        let vw = if field_ty.size() < w.size() {
            self.cast(CastOp::ZExt, field_ty, w, v)
        } else if field_ty.size() > w.size() {
            self.cast(CastOp::Trunc, field_ty, w, v)
        } else {
            v
        };
        let field_mask: u64 = if b.width >= 64 { u64::MAX } else { (1u64 << b.width) - 1 };
        let mask = field_mask << b.shift;
        let keep = canon(!mask, w);
        let cleared = self.bin(BinOp::And, w, old, Operand::Int(keep, w));
        let masked = self.bin(BinOp::And, w, vw, Operand::Int(canon(field_mask, w), w));
        let shifted =
            if b.shift > 0 { self.bin(BinOp::Shl, w, masked, Operand::Int(b.shift as i64, w)) } else { masked };
        let new = self.bin(BinOp::Or, w, cleared, shifted);
        let new = if b.unit.size() < w.size() { self.cast(CastOp::Trunc, w, b.unit, new) } else { new };
        self.store(b.unit, new, p.ptr, p.volatile);
    }

    fn load_place(&mut self, p: &Place, ty: Ty) -> Operand {
        let t = self.ity(ty);
        if p.bit.is_some() {
            self.load_bits(p, t)
        } else {
            self.load(t, p.ptr, p.volatile)
        }
    }

    fn store_place(&mut self, p: &Place, v: Operand, ty: Ty) {
        let t = self.ity(ty);
        if p.bit.is_some() {
            self.store_bits(p, v, t);
        } else {
            self.store(t, v, p.ptr, p.volatile);
        }
    }

    pub fn copy_aggregate(&mut self, dst: Operand, src: Operand, ty: Ty) {
        let size = self.size_of(ty);
        if size > 0 {
            let align = self.align_of(ty);
            self.emit(InstKind::MemCopy { dst, src, size, align }, None);
        }
    }

    // ───────────────────────────── rvalues ─────────────────────────────

    /// Lower an expression to a value. Aggregates yield their address; `void`
    /// expressions yield a dummy.
    pub fn rv(&mut self, e: &'a HExpr) -> Operand {
        let saved = self.line;
        let l = self.line_of(e.span);
        if l != 0 {
            self.line = l;
        }
        let r = self.rv_inner(e);
        self.line = saved;
        r
    }

    fn dummy(&self) -> Operand {
        Operand::Undef(Type::I32)
    }

    fn rv_inner(&mut self, e: &'a HExpr) -> Operand {
        let ty = e.ty;
        match &e.kind {
            HExprKind::Int(v) => {
                let t = self.ity(ty);
                Operand::Int(canon(*v, t), t)
            }
            HExprKind::Float(f) => Operand::float(*f, self.ity(ty)),
            HExprKind::Str(_)
            | HExprKind::Local(_)
            | HExprKind::Global(_)
            | HExprKind::Deref(_)
            | HExprKind::Member(..)
            | HExprKind::CompoundLit { .. } => {
                // A place used directly as an rvalue is an aggregate (its address), or a
                // member of an rvalue struct such as `f().x`, which must be loaded.
                let p = self.place(e);
                if self.is_agg(ty) {
                    p.ptr
                } else {
                    self.load_place(&p, ty)
                }
            }
            HExprKind::Cast(kind, inner) => self.cast_expr(*kind, inner, ty),
            HExprKind::Unary(op, x) => self.unary(*op, x, ty),
            HExprKind::Binary(op, l, r) => self.binary(*op, l, r, ty),
            HExprKind::PtrAdd { ptr, idx, scale, negate } => {
                let p = self.rv(ptr);
                let i = self.rv(idx);
                if *scale == 0 {
                    let sz = self.dyn_pointee_size(ptr.ty);
                    self.ptr_index_dyn(p, i, sz, *negate)
                } else {
                    self.ptr_index(p, i, *scale, *negate)
                }
            }
            HExprKind::VlaSizeof(t) => self.runtime_size(*t),
            HExprKind::PtrDiff { l, r, elem_size } => {
                let a = self.rv(l);
                let b = self.rv(r);
                let ai = self.cast(CastOp::PtrToInt, Type::Ptr, Type::I64, a);
                let bi = self.cast(CastOp::PtrToInt, Type::Ptr, Type::I64, b);
                let d = self.bin(BinOp::Sub, Type::I64, ai, bi);
                if *elem_size == 0 {
                    let sz = self.dyn_pointee_size(l.ty);
                    self.bin(BinOp::SDiv, Type::I64, d, sz)
                } else if *elem_size > 1 {
                    self.bin(BinOp::SDiv, Type::I64, d, Operand::Int(*elem_size as i64, Type::I64))
                } else {
                    d
                }
            }
            HExprKind::LogAnd(..) | HExprKind::LogOr(..) => self.logical_value(e),
            HExprKind::Cond(c, t, f) => self.conditional(c, t, f, ty),
            HExprKind::Comma(a, b) => {
                self.rv(a);
                self.rv(b)
            }
            HExprKind::Assign(place, value) => self.assign(place, value),
            HExprKind::CompoundAssign { op, place, value, calc } => self.compound_assign(*op, place, value, *calc),
            HExprKind::IncDec { place, is_inc, is_prefix } => self.inc_dec(place, *is_inc, *is_prefix),
            HExprKind::Call { callee, args } => self.call(callee, args, ty),
            HExprKind::AddrOf(x) => self.place(x).ptr,
            HExprKind::VaStart(ap) => {
                let p = self.rv(ap);
                self.va_start(p);
                self.dummy()
            }
            HExprKind::VaEnd(ap) => {
                self.rv(ap);
                self.dummy()
            }
            HExprKind::VaCopy(d, s) => {
                let dp = self.rv(d);
                let sp = self.rv(s);
                self.emit(InstKind::MemCopy { dst: dp, src: sp, size: 24, align: 8 }, None);
                self.dummy()
            }
            HExprKind::VaArg(ap) => {
                let p = self.rv(ap);
                self.va_arg(p, ty)
            }
            HExprKind::Trap => {
                self.emit(InstKind::Trap, None);
                self.terminate(Term::Unreachable);
                self.start_dead_block();
                self.dummy()
            }
            HExprKind::Error => unreachable!("HIR with errors reached lowering"),
        }
    }

    fn cast_expr(&mut self, kind: CastKind, inner: &'a HExpr, to: Ty) -> Operand {
        match kind {
            CastKind::LValueToRValue => {
                if self.is_agg(inner.ty) {
                    return self.address(inner);
                }
                let p = self.place(inner);
                self.load_place(&p, to)
            }
            CastKind::ArrayToPointer | CastKind::FunctionToPointer => self.place(inner).ptr,
            CastKind::NoOp => self.rv(inner),
            CastKind::ToVoid => {
                self.rv(inner);
                self.dummy()
            }
            CastKind::ToBool
            | CastKind::IntToInt
            | CastKind::IntToFloat
            | CastKind::FloatToInt
            | CastKind::FloatToFloat
            | CastKind::PtrToInt
            | CastKind::IntToPtr => {
                let v = self.rv(inner);
                self.convert(v, inner.ty, to)
            }
        }
    }

    fn unary(&mut self, op: UnKind, x: &'a HExpr, ty: Ty) -> Operand {
        let v = self.rv(x);
        let t = self.ity(x.ty);
        match op {
            UnKind::Neg => {
                if t.is_float() {
                    self.val(InstKind::Un { op: UnOp::FNeg, ty: t, val: v }, t)
                } else {
                    self.bin(BinOp::Sub, t, Operand::Int(0, t), v)
                }
            }
            UnKind::BitNot => self.bin(BinOp::Xor, t, v, Operand::Int(-1, t)),
            UnKind::LogNot => {
                let _ = ty;
                match t {
                    Type::F32 | Type::F64 => self.val(
                        InstKind::FCmp { pred: FPred::Oeq, ty: t, lhs: v, rhs: Operand::float(0.0, t) },
                        Type::I32,
                    ),
                    _ => {
                        self.val(InstKind::ICmp { pred: IPred::Eq, ty: t, lhs: v, rhs: Operand::Int(0, t) }, Type::I32)
                    }
                }
            }
        }
    }

    /// Like [`Self::ptr_index`] with a stride that is only known at run time.
    pub fn ptr_index_dyn(&mut self, p: Operand, i: Operand, scale: Operand, negate: bool) -> Operand {
        let scaled = self.bin(BinOp::Mul, Type::I64, i, scale);
        let off = if negate { self.bin(BinOp::Sub, Type::I64, Operand::Int(0, Type::I64), scaled) } else { scaled };
        self.val(InstKind::PtrAdd { base: p, offset: off }, Type::Ptr)
    }

    pub fn ptr_index(&mut self, p: Operand, i: Operand, scale: u64, negate: bool) -> Operand {
        let off = match i {
            Operand::Int(v, _) => {
                let o = v.wrapping_mul(scale as i64);
                Operand::Int(if negate { o.wrapping_neg() } else { o }, Type::I64)
            }
            _ => {
                let scaled = if scale == 1 {
                    i
                } else {
                    self.bin(BinOp::Mul, Type::I64, i, Operand::Int(scale as i64, Type::I64))
                };
                if negate {
                    self.bin(BinOp::Sub, Type::I64, Operand::Int(0, Type::I64), scaled)
                } else {
                    scaled
                }
            }
        };
        if off == Operand::Int(0, Type::I64) {
            return p;
        }
        self.val(InstKind::PtrAdd { base: p, offset: off }, Type::Ptr)
    }

    fn binary(&mut self, op: BinKind, l: &'a HExpr, r: &'a HExpr, ty: Ty) -> Operand {
        let a = self.rv(l);
        let b = self.rv(r);
        let lt = self.ity(l.ty);
        let signed = self.is_signed(l.ty);
        if op.is_comparison() {
            return self.compare(op, lt, signed, l.ty, a, b);
        }
        let _ = ty;
        if lt.is_float() {
            let o = match op {
                BinKind::Add => BinOp::FAdd,
                BinKind::Sub => BinOp::FSub,
                BinKind::Mul => BinOp::FMul,
                BinKind::Div => BinOp::FDiv,
                _ => unreachable!("invalid floating-point operator"),
            };
            return self.bin(o, lt, a, b);
        }
        // shifts: the right operand has its own (promoted) type
        let b = if matches!(op, BinKind::Shl | BinKind::Shr) {
            let rt = self.ity(r.ty);
            self.int_to_int(b, rt, lt, self.is_signed(r.ty))
        } else {
            b
        };
        let o = match op {
            BinKind::Add => BinOp::Add,
            BinKind::Sub => BinOp::Sub,
            BinKind::Mul => BinOp::Mul,
            BinKind::Div => {
                if signed {
                    BinOp::SDiv
                } else {
                    BinOp::UDiv
                }
            }
            BinKind::Rem => {
                if signed {
                    BinOp::SRem
                } else {
                    BinOp::URem
                }
            }
            BinKind::And => BinOp::And,
            BinKind::Or => BinOp::Or,
            BinKind::Xor => BinOp::Xor,
            BinKind::Shl => BinOp::Shl,
            BinKind::Shr => {
                if signed {
                    BinOp::AShr
                } else {
                    BinOp::LShr
                }
            }
            _ => unreachable!(),
        };
        self.bin(o, lt, a, b)
    }

    fn compare(&mut self, op: BinKind, lt: Type, signed: bool, _hty: Ty, a: Operand, b: Operand) -> Operand {
        if lt.is_float() {
            let pred = match op {
                BinKind::Eq => FPred::Oeq,
                BinKind::Ne => FPred::Une,
                BinKind::Lt => FPred::Olt,
                BinKind::Le => FPred::Ole,
                BinKind::Gt => FPred::Ogt,
                _ => FPred::Oge,
            };
            return self.val(InstKind::FCmp { pred, ty: lt, lhs: a, rhs: b }, Type::I32);
        }
        let pred = match (op, signed) {
            (BinKind::Eq, _) => IPred::Eq,
            (BinKind::Ne, _) => IPred::Ne,
            (BinKind::Lt, true) => IPred::Slt,
            (BinKind::Le, true) => IPred::Sle,
            (BinKind::Gt, true) => IPred::Sgt,
            (BinKind::Ge, true) => IPred::Sge,
            (BinKind::Lt, false) => IPred::Ult,
            (BinKind::Le, false) => IPred::Ule,
            (BinKind::Gt, false) => IPred::Ugt,
            (_, false) => IPred::Uge,
            _ => unreachable!(),
        };
        // Pointers are compared as unsigned; make sure both sides are `ptr`.
        let lt = if lt == Type::Ptr || self.f.operand_ty(a) == Type::Ptr { Type::Ptr } else { lt };
        self.val(InstKind::ICmp { pred, ty: lt, lhs: a, rhs: b }, Type::I32)
    }

    // ───────────────────────────── control flow in expressions ─────────────────────────────

    /// Branch on `e`: true goes to `then_bb`, false to `else_bb`.
    pub fn cond_branch(&mut self, e: &'a HExpr, then_bb: BlockId, else_bb: BlockId) {
        match &e.kind {
            HExprKind::Int(v) => {
                self.br(if *v != 0 { then_bb } else { else_bb });
            }
            HExprKind::LogAnd(a, b) => {
                let mid = self.new_block("land.rhs");
                self.cond_branch(a, mid, else_bb);
                self.set_cur(mid);
                self.cond_branch(b, then_bb, else_bb);
            }
            HExprKind::LogOr(a, b) => {
                let mid = self.new_block("lor.rhs");
                self.cond_branch(a, then_bb, mid);
                self.set_cur(mid);
                self.cond_branch(b, then_bb, else_bb);
            }
            HExprKind::Unary(UnKind::LogNot, x) => self.cond_branch(x, else_bb, then_bb),
            _ => {
                let v = self.rv(e);
                let t = self.f.operand_ty(v);
                let cond = if t.is_int() { v } else { self.is_nonzero(v, t) };
                self.terminate(Term::CondBr { cond, then_bb, else_bb });
            }
        }
    }

    /// `a && b` / `a || b` as a 0/1 `int` value.
    fn logical_value(&mut self, e: &'a HExpr) -> Operand {
        let t_bb = self.new_block("lv.true");
        let f_bb = self.new_block("lv.false");
        let end = self.new_block("lv.end");
        self.cond_branch(e, t_bb, f_bb);
        self.set_cur(t_bb);
        self.br(end);
        self.set_cur(f_bb);
        self.br(end);
        self.set_cur(end);
        self.val(
            InstKind::Phi {
                ty: Type::I32,
                incoming: vec![(t_bb, Operand::Int(1, Type::I32)), (f_bb, Operand::Int(0, Type::I32))],
            },
            Type::I32,
        )
    }

    fn conditional(&mut self, c: &'a HExpr, t: &'a HExpr, f: &'a HExpr, ty: Ty) -> Operand {
        let then_bb = self.new_block("cond.true");
        let else_bb = self.new_block("cond.false");
        let end = self.new_block("cond.end");
        self.cond_branch(c, then_bb, else_bb);
        self.set_cur(then_bb);
        let tv = self.rv(t);
        let t_end = self.cur;
        self.br(end);
        self.set_cur(else_bb);
        let fv = self.rv(f);
        let f_end = self.cur;
        self.br(end);
        self.set_cur(end);
        if self.hir.types.is_void(ty) {
            return self.dummy();
        }
        let pty = if self.is_agg(ty) { Type::Ptr } else { self.ity(ty) };
        self.val(InstKind::Phi { ty: pty, incoming: vec![(t_end, tv), (f_end, fv)] }, pty)
    }

    // ───────────────────────────── assignment ─────────────────────────────

    fn assign(&mut self, place: &'a HExpr, value: &'a HExpr) -> Operand {
        if self.is_agg(place.ty) {
            let src = self.rv(value);
            let dst = self.place(place).ptr;
            self.copy_aggregate(dst, src, place.ty);
            return dst;
        }
        let v = self.rv(value);
        let p = self.place(place);
        self.store_place(&p, v, place.ty);
        if p.bit.is_some() {
            return self.load_place(&p, place.ty);
        }
        v
    }

    fn compound_assign(&mut self, op: BinKind, place: &'a HExpr, value: &'a HExpr, calc: Ty) -> Operand {
        let v = self.rv(value);
        let p = self.place(place);
        let old = self.load_place(&p, place.ty);
        let new = if self.hir.types.is_pointer(place.ty) {
            if self.is_vla_pointee(place.ty) {
                let sz = self.dyn_pointee_size(place.ty);
                self.ptr_index_dyn(old, v, sz, op == BinKind::Sub)
            } else {
                let scale = self.pointee_size(place.ty);
                self.ptr_index(old, v, scale, op == BinKind::Sub)
            }
        } else {
            let ct = self.ity(calc);
            let oldc = self.convert(old, place.ty, calc);
            let signed = self.is_signed(calc);
            let v = if matches!(op, BinKind::Shl | BinKind::Shr) {
                let vt = self.f.operand_ty(v);
                self.int_to_int(v, vt, ct, true)
            } else {
                v
            };
            let r = if ct.is_float() {
                let o = match op {
                    BinKind::Add => BinOp::FAdd,
                    BinKind::Sub => BinOp::FSub,
                    BinKind::Mul => BinOp::FMul,
                    _ => BinOp::FDiv,
                };
                self.bin(o, ct, oldc, v)
            } else {
                let o = match op {
                    BinKind::Add => BinOp::Add,
                    BinKind::Sub => BinOp::Sub,
                    BinKind::Mul => BinOp::Mul,
                    BinKind::Div => {
                        if signed {
                            BinOp::SDiv
                        } else {
                            BinOp::UDiv
                        }
                    }
                    BinKind::Rem => {
                        if signed {
                            BinOp::SRem
                        } else {
                            BinOp::URem
                        }
                    }
                    BinKind::And => BinOp::And,
                    BinKind::Or => BinOp::Or,
                    BinKind::Xor => BinOp::Xor,
                    BinKind::Shl => BinOp::Shl,
                    _ => {
                        if signed {
                            BinOp::AShr
                        } else {
                            BinOp::LShr
                        }
                    }
                };
                self.bin(o, ct, oldc, v)
            };
            self.convert(r, calc, place.ty)
        };
        self.store_place(&p, new, place.ty);
        if p.bit.is_some() {
            return self.load_place(&p, place.ty);
        }
        new
    }

    fn pointee_size(&self, ptr_ty: Ty) -> u64 {
        let t = &self.hir.types;
        match t.pointee(ptr_ty) {
            Some(p) if t.is_void(p) || t.is_function(p) => 1,
            Some(p) => t.size_of(p).unwrap_or(1),
            None => 1,
        }
    }

    fn inc_dec(&mut self, place: &'a HExpr, is_inc: bool, is_prefix: bool) -> Operand {
        let p = self.place(place);
        let old = self.load_place(&p, place.ty);
        let ty = place.ty;
        let new = if self.hir.types.is_pointer(ty) {
            if self.is_vla_pointee(ty) {
                let sz = self.dyn_pointee_size(ty);
                self.ptr_index_dyn(old, Operand::Int(1, Type::I64), sz, !is_inc)
            } else {
                let scale = self.pointee_size(ty);
                self.ptr_index(old, Operand::Int(1, Type::I64), scale, !is_inc)
            }
        } else if self.hir.types.is_floating(ty) {
            let t = self.ity(ty);
            self.bin(if is_inc { BinOp::FAdd } else { BinOp::FSub }, t, old, Operand::float(1.0, t))
        } else if self.hir.types.is_bool(ty) {
            // b++ sets the flag; b-- toggles through arithmetic
            let w = self.int_to_int(old, Type::I8, Type::I32, false);
            let r = self.bin(if is_inc { BinOp::Add } else { BinOp::Sub }, Type::I32, w, Operand::Int(1, Type::I32));
            let nz = self.is_nonzero(r, Type::I32);
            self.cast(CastOp::Trunc, Type::I32, Type::I8, nz)
        } else {
            let t = self.ity(ty);
            let signed = self.is_signed(ty);
            let (w, wt) = self.widen32(old, t, signed);
            let r = self.bin(if is_inc { BinOp::Add } else { BinOp::Sub }, wt, w, Operand::Int(1, wt));
            self.int_to_int(r, wt, t, signed)
        };
        self.store_place(&p, new, ty);
        if is_prefix {
            if p.bit.is_some() {
                return self.load_place(&p, ty);
            }
            new
        } else {
            old
        }
    }

    // ───────────────────────────── calls ─────────────────────────────

    fn call(&mut self, callee: &'a HExpr, args: &'a [HExpr], ret_ty: Ty) -> Operand {
        // Direct call: callee is a decayed function symbol.
        let (target, fn_ty) = match &callee.kind {
            HExprKind::Cast(CastKind::FunctionToPointer, inner) if matches!(inner.kind, HExprKind::Global(_)) => {
                let HExprKind::Global(s) = &inner.kind else { unreachable!() };
                (Callee::Direct(self.mb.sym(self.hir, *s)), self.hir.types.pointee(callee.ty))
            }
            _ => {
                let v = self.rv(callee);
                (Callee::Indirect(v), self.hir.types.pointee(callee.ty))
            }
        };
        let variadic = fn_ty.and_then(|t| self.hir.types.func_sig(t)).is_some_and(|s| s.variadic || s.unspecified);

        // Result passing.
        let mut ret_types: Vec<Type> = Vec::new();
        let mut ret_temp: Option<Operand> = None;
        let mut ret_pieces: Vec<Piece> = Vec::new();
        let mut sret_arg: Option<CallArg> = None;
        let mut scalar_ret: Option<Type> = None;
        if !self.hir.types.is_void(ret_ty) {
            match self.passing(ret_ty) {
                Passing::Scalar(t) => {
                    ret_types.push(t);
                    scalar_ret = Some(t);
                }
                Passing::Pieces(p) => {
                    ret_types = p.iter().map(|x| x.ty).collect();
                    ret_pieces = p;
                    ret_temp = Some(self.temp_for(ret_ty));
                }
                Passing::ByVal => {
                    let tmp = self.temp_for(ret_ty);
                    sret_arg = Some(CallArg { val: tmp, kind: ArgKind::Value, group: None });
                    ret_types.push(Type::Ptr);
                    ret_temp = Some(tmp);
                }
                Passing::Nothing => ret_temp = Some(self.temp_for(ret_ty)),
            }
        }

        // Arguments.
        let mut cargs: Vec<CallArg> = Vec::new();
        if let Some(a) = sret_arg {
            cargs.push(a);
        }
        for a in args {
            self.lower_arg(a, &mut cargs);
        }
        let line = self.line;
        let id_dst = {
            let tys = ret_types.clone();
            let kind = InstKind::Call { callee: target, args: cargs, rets: ret_types.clone(), variadic, tail: false };
            // Allocate result values.
            let inst_id = {
                let id = InstId(self.f.insts.len() as u32);
                self.f.insts.push(Inst { kind, dst: None, dst2: None, line });
                self.f.dead_insts.push(false);
                self.f.blocks[self.cur.idx()].insts.push(id);
                id
            };
            let mut dsts: Vec<ValueId> = Vec::new();
            for t in &tys {
                let v = self.f.new_value(*t, None, ValueDef::Inst(inst_id));
                dsts.push(v);
            }
            self.f.insts[inst_id.idx()].dst = dsts.first().copied();
            self.f.insts[inst_id.idx()].dst2 = dsts.get(1).copied();
            dsts
        };
        if let Some(t) = scalar_ret {
            let v = Operand::Value(id_dst[0]);
            let natural = self.ity(ret_ty);
            if t != natural && natural.is_int() {
                return self.cast(CastOp::Trunc, t, natural, v);
            }
            return v;
        }
        if !ret_pieces.is_empty() {
            let tmp = ret_temp.unwrap();
            for (piece, d) in ret_pieces.iter().zip(&id_dst) {
                let ptr = self.ptr_add(tmp, piece.offset as u64);
                self.store(piece.ty, Operand::Value(*d), ptr, false);
            }
            return tmp;
        }
        if let Some(t) = ret_temp {
            return t;
        }
        self.dummy()
    }

    /// A stack temporary big enough to hold a value of aggregate type `ty`.
    pub fn temp_for(&mut self, ty: Ty) -> Operand {
        let size = round_up(self.size_of(ty).max(1), 8);
        let want = self.align_of(ty).max(8);
        let over = want > MAX_FRAME_ALIGN;
        let (size, align) = if over { (size + want as u64 - 1, MAX_FRAME_ALIGN) } else { (size, want) };
        // allocas must live in the entry block
        let line = self.line;
        let entry = self.f.entry();
        let pos = self.f.blocks[entry.idx()]
            .insts
            .iter()
            .take_while(|i| matches!(self.f.insts[i.idx()].kind, InstKind::Alloca { .. }))
            .count();
        let slot = self
            .f
            .insert(
                entry,
                pos,
                InstKind::Alloca { size: size as u32, align },
                Some(Type::Ptr),
                Some(Symbol::new("tmp")),
                line,
            )
            .unwrap();
        if over {
            self.realign(slot, want)
        } else {
            slot
        }
    }

    fn lower_arg(&mut self, a: &'a HExpr, out: &mut Vec<CallArg>) {
        match self.passing(a.ty) {
            Passing::Scalar(abi_t) => {
                let v = self.rv(a);
                let nat = self.ity(a.ty);
                let v = if abi_t != nat {
                    self.cast(if self.is_signed(a.ty) { CastOp::SExt } else { CastOp::ZExt }, nat, abi_t, v)
                } else {
                    v
                };
                out.push(CallArg { val: v, kind: ArgKind::Value, group: None });
            }
            Passing::Pieces(pieces) => {
                let addr = self.address(a);
                let size = self.size_of(a.ty);
                let src = if super::abi::pieces_exact(&pieces, size) {
                    addr
                } else {
                    // copy into a zero-padded temporary so the last eightbyte never over-reads
                    let tmp = self.temp_for(a.ty);
                    self.emit(InstKind::MemSet { dst: tmp, byte: 0, size: round_up(size, 8), align: 8 }, None);
                    self.emit(InstKind::MemCopy { dst: tmp, src: addr, size, align: self.align_of(a.ty) }, None);
                    tmp
                };
                let g = self.next_group;
                self.next_group += 1;
                for p in pieces {
                    let ptr = self.ptr_add(src, p.offset as u64);
                    let v = self.load(p.ty, ptr, false);
                    out.push(CallArg { val: v, kind: ArgKind::Value, group: Some(g) });
                }
            }
            Passing::ByVal => {
                let addr = self.address(a);
                let size = self.size_of(a.ty) as u32;
                let align = self.align_of(a.ty);
                out.push(CallArg { val: addr, kind: ArgKind::ByVal { size, align }, group: None });
            }
            Passing::Nothing => {
                self.address(a);
            }
        }
    }

    // ───────────────────────────── varargs ─────────────────────────────

    /// (named GPRs, named XMMs, bytes of named stack arguments) of this function.
    fn va_named(&self) -> (u32, u32, u32) {
        let descs: Vec<callconv::ArgDesc> = self
            .f
            .params
            .iter()
            .map(|p| match &p.kind {
                ParamKind::Value(t) => callconv::ArgDesc { ty: Some(*t), byval: None, group: p.group },
                ParamKind::ByVal { size, align } => {
                    callconv::ArgDesc { ty: None, byval: Some((*size, *align)), group: None }
                }
            })
            .collect();
        let a = callconv::assign(&descs);
        (a.gp_used, a.fp_used, a.stack_size)
    }

    fn va_start(&mut self, ap: Operand) {
        let (gp, fp, stack) = self.va_named();
        self.store(Type::I32, Operand::Int(gp as i64 * 8, Type::I32), ap, false);
        let p4 = self.ptr_add(ap, 4);
        self.store(Type::I32, Operand::Int(48 + fp as i64 * 16, Type::I32), p4, false);
        let args = self.val(InstKind::VaStackArgs, Type::Ptr);
        let args = self.ptr_add(args, stack as u64);
        let p8 = self.ptr_add(ap, 8);
        self.store(Type::Ptr, args, p8, false);
        let rsa = self.val(InstKind::VaRegSave, Type::Ptr);
        let p16 = self.ptr_add(ap, 16);
        self.store(Type::Ptr, rsa, p16, false);
    }

    /// Pointer to the next variadic argument that lives in the overflow area, advancing it.
    fn va_from_stack(&mut self, ap: Operand, size: u64, align: u64) -> Operand {
        let p8 = self.ptr_add(ap, 8);
        let cur = self.load(Type::Ptr, p8, false);
        let aligned = if align > 8 {
            let i = self.cast(CastOp::PtrToInt, Type::Ptr, Type::I64, cur);
            let i = self.bin(BinOp::Add, Type::I64, i, Operand::Int(align as i64 - 1, Type::I64));
            let i = self.bin(BinOp::And, Type::I64, i, Operand::Int(-(align as i64), Type::I64));
            self.cast(CastOp::IntToPtr, Type::I64, Type::Ptr, i)
        } else {
            cur
        };
        let next = self.val(
            InstKind::PtrAdd { base: aligned, offset: Operand::Int(round_up(size, 8) as i64, Type::I64) },
            Type::Ptr,
        );
        self.store(Type::Ptr, next, p8, false);
        aligned
    }

    fn va_arg(&mut self, ap: Operand, ty: Ty) -> Operand {
        let reg_bb = self.new_block("va.reg");
        let stack_bb = self.new_block("va.stack");
        let end = self.new_block("va.end");
        let p4 = self.ptr_add(ap, 4);
        let p16 = self.ptr_add(ap, 16);
        match self.passing(ty) {
            Passing::Scalar(t) => {
                let is_fp = t.is_float();
                let off_ptr = if is_fp { p4 } else { ap };
                let off = self.load(Type::I32, off_ptr, false);
                let limit = if is_fp { 160 } else { 40 };
                let cond = self.val(
                    InstKind::ICmp { pred: IPred::Ule, ty: Type::I32, lhs: off, rhs: Operand::Int(limit, Type::I32) },
                    Type::I32,
                );
                self.terminate(Term::CondBr { cond, then_bb: reg_bb, else_bb: stack_bb });
                self.set_cur(reg_bb);
                let rsa = self.load(Type::Ptr, p16, false);
                let off64 = self.cast(CastOp::ZExt, Type::I32, Type::I64, off);
                let addr_r = self.val(InstKind::PtrAdd { base: rsa, offset: off64 }, Type::Ptr);
                let next = self.bin(BinOp::Add, Type::I32, off, Operand::Int(if is_fp { 16 } else { 8 }, Type::I32));
                self.store(Type::I32, next, off_ptr, false);
                let reg_end = self.cur;
                self.br(end);
                self.set_cur(stack_bb);
                let addr_s = self.va_from_stack(ap, 8, 8);
                let stack_end = self.cur;
                self.br(end);
                self.set_cur(end);
                let addr = self.val(
                    InstKind::Phi { ty: Type::Ptr, incoming: vec![(reg_end, addr_r), (stack_end, addr_s)] },
                    Type::Ptr,
                );
                // varargs integers narrower than int are passed as int
                let natural = self.ity(ty);
                if is_fp && natural == Type::F32 {
                    let d = self.load(Type::F64, addr, false);
                    return self.cast(CastOp::FPTrunc, Type::F64, Type::F32, d);
                }
                self.load(natural, addr, false)
            }
            Passing::Pieces(pieces) => {
                let need_gp = pieces.iter().filter(|p| p.ty.is_gpr()).count() as i64;
                let need_fp = pieces.iter().filter(|p| p.ty.is_float()).count() as i64;
                let gp = self.load(Type::I32, ap, false);
                let fp = self.load(Type::I32, p4, false);
                let c1 = self.val(
                    InstKind::ICmp {
                        pred: IPred::Ule,
                        ty: Type::I32,
                        lhs: gp,
                        rhs: Operand::Int(48 - need_gp * 8, Type::I32),
                    },
                    Type::I32,
                );
                let c2 = self.val(
                    InstKind::ICmp {
                        pred: IPred::Ule,
                        ty: Type::I32,
                        lhs: fp,
                        rhs: Operand::Int(176 - need_fp * 16, Type::I32),
                    },
                    Type::I32,
                );
                let both = self.bin(BinOp::And, Type::I32, c1, c2);
                self.terminate(Term::CondBr { cond: both, then_bb: reg_bb, else_bb: stack_bb });
                // registers: copy the pieces into a temporary
                self.set_cur(reg_bb);
                let tmp = self.temp_for(ty);
                let rsa = self.load(Type::Ptr, p16, false);
                let (mut gpo, mut fpo) = (gp, fp);
                for p in &pieces {
                    let is_fp = p.ty.is_float();
                    let off = if is_fp { fpo } else { gpo };
                    let off64 = self.cast(CastOp::ZExt, Type::I32, Type::I64, off);
                    let src = self.val(InstKind::PtrAdd { base: rsa, offset: off64 }, Type::Ptr);
                    let v = self.load(p.ty, src, false);
                    let dst = self.ptr_add(tmp, p.offset as u64);
                    self.store(p.ty, v, dst, false);
                    let next =
                        self.bin(BinOp::Add, Type::I32, off, Operand::Int(if is_fp { 16 } else { 8 }, Type::I32));
                    if is_fp {
                        fpo = next;
                    } else {
                        gpo = next;
                    }
                }
                self.store(Type::I32, gpo, ap, false);
                self.store(Type::I32, fpo, p4, false);
                let reg_end = self.cur;
                self.br(end);
                self.set_cur(stack_bb);
                let size = self.size_of(ty);
                let align = self.align_of(ty) as u64;
                let addr_s = self.va_from_stack(ap, size, align);
                let stack_end = self.cur;
                self.br(end);
                self.set_cur(end);
                self.val(
                    InstKind::Phi { ty: Type::Ptr, incoming: vec![(reg_end, tmp), (stack_end, addr_s)] },
                    Type::Ptr,
                )
            }
            Passing::ByVal | Passing::Nothing => {
                // always from the overflow area
                self.br(stack_bb);
                self.set_cur(reg_bb);
                self.terminate(Term::Unreachable);
                self.set_cur(stack_bb);
                let size = self.size_of(ty);
                let align = self.align_of(ty) as u64;
                let addr = self.va_from_stack(ap, size, align);
                self.br(end);
                self.set_cur(end);
                addr
            }
        }
    }

    // ───────────────────────────── initializers ─────────────────────────────

    /// Initialize the object at `slot` (type `ty`) from `plan`.
    pub fn emit_init(&mut self, slot: Operand, plan: &'a InitPlan, ty: Ty) {
        let total = self.size_of(ty);
        if plan.needs_zero && total > 0 {
            self.emit(InstKind::MemSet { dst: slot, byte: 0, size: total, align: self.align_of(ty) }, None);
        }
        for e in &plan.entries {
            let ptr = self.ptr_add(slot, e.offset);
            match &e.value {
                InitValue::Expr(x) => {
                    if self.is_agg(e.ty) {
                        let src = self.rv(x);
                        self.copy_aggregate(ptr, src, e.ty);
                    } else {
                        let v = self.rv(x);
                        self.store_init(ptr, v, e);
                    }
                }
                InitValue::Const(c) => {
                    let v = self.const_operand(c, e.ty);
                    self.store_init(ptr, v, e);
                }
                InitValue::Bytes(b) => self.store_bytes(ptr, b),
            }
        }
    }

    fn store_init(&mut self, ptr: Operand, v: Operand, e: &crate::hir::InitEntry) {
        let t = self.ity(e.ty);
        match e.bit {
            None => self.store(t, v, ptr, false),
            Some(b) => {
                // the entry offset is the containing byte; recompute the unit for this field
                let tsize = self.size_of(e.ty).max(1);
                let unit_bits = tsize * 8;
                let rel_byte = b.bit_offset / 8;
                let outer = e.offset - rel_byte;
                let unit_start = (b.bit_offset / unit_bits) * unit_bits;
                let base = self.ptr_add(ptr, 0);
                // `ptr` already includes `e.offset`; step back to the unit start
                let delta = (outer + unit_start / 8) as i64 - e.offset as i64;
                let up = if delta == 0 {
                    base
                } else {
                    self.val(InstKind::PtrAdd { base, offset: Operand::Int(delta, Type::I64) }, Type::Ptr)
                };
                let loc = BitLoc {
                    shift: (b.bit_offset - unit_start) as u32,
                    width: b.width,
                    unit: t,
                    signed: self.is_signed(e.ty),
                };
                let p = Place { ptr: up, bit: Some(loc), volatile: false };
                self.store_bits(&p, v, t);
            }
        }
    }

    fn const_operand(&mut self, c: &ConstVal, ty: Ty) -> Operand {
        let t = self.ity(ty);
        match c {
            ConstVal::Int(v) => Operand::Int(canon(*v, t), t),
            ConstVal::Float(f) => Operand::float(*f, t),
            ConstVal::Addr { base, offset } => {
                let sym = match base {
                    crate::hir::AddrBase::Global(s) => self.mb.sym(self.hir, *s),
                    crate::hir::AddrBase::Str(s) => self.mb.str_sym(self.hir, *s),
                };
                let g = Operand::Global(sym);
                if *offset == 0 {
                    g
                } else {
                    self.val(InstKind::PtrAdd { base: g, offset: Operand::Int(*offset, Type::I64) }, Type::Ptr)
                }
            }
        }
    }

    fn store_bytes(&mut self, ptr: Operand, bytes: &[u8]) {
        if bytes.len() > 32 {
            let sym = self.mb.const_blob(bytes.to_vec(), 1);
            self.emit(
                InstKind::MemCopy { dst: ptr, src: Operand::Global(sym), size: bytes.len() as u64, align: 1 },
                None,
            );
            return;
        }
        let mut off = 0usize;
        while off < bytes.len() {
            let rem = bytes.len() - off;
            let (t, n) = if rem >= 8 {
                (Type::I64, 8)
            } else if rem >= 4 {
                (Type::I32, 4)
            } else if rem >= 2 {
                (Type::I16, 2)
            } else {
                (Type::I8, 1)
            };
            let mut v: u64 = 0;
            for i in 0..n {
                v |= (bytes[off + i] as u64) << (8 * i);
            }
            let p = self.ptr_add(ptr, off as u64);
            self.store(t, Operand::Int(canon(v, t), t), p, false);
            off += n;
        }
    }
}
