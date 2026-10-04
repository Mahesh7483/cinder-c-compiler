//! Per-instruction selection rules (continuation of `isel`).

use super::isel::Isel;
use super::mir::*;
use crate::abi::{self, ArgDesc, Loc};
use crate::ir::*;

enum FCond {
    /// a single condition code after `ucomis`
    Simple(Cc),
    /// equal and ordered (ZF=1 and PF=0)
    Eq,
    /// not equal or unordered (ZF=0 or PF=1)
    Ne,
}

fn widen(sz: Sz) -> Sz {
    match sz {
        Sz::B | Sz::W => Sz::L,
        s => s,
    }
}

impl<'a> Isel<'a> {
    pub(super) fn inst(&mut self, id: InstId) {
        let f = self.f;
        let i = &f.insts[id.idx()];
        match &i.kind {
            InstKind::Alloca { .. } => {
                let d = i.dst.unwrap();
                if self.needs_reg[d.idx()] {
                    let slot = self.alloca_slot[d.idx()].expect("alloca slot");
                    let r = self.value_reg(d);
                    self.emit(Op::Lea, Sz::Q, MOp::Reg(r), MOp::Mem(Mem::slot(slot)));
                }
            }
            InstKind::VaRegSave => {
                let d = i.dst.unwrap();
                let slot = self.mf.regsave_slot.expect("variadic function has a register save area");
                let r = self.value_reg(d);
                self.emit(Op::Lea, Sz::Q, MOp::Reg(r), MOp::Mem(Mem::slot(slot)));
            }
            InstKind::VaStackArgs => {
                let d = i.dst.unwrap();
                let r = self.value_reg(d);
                self.emit(Op::Lea, Sz::Q, MOp::Reg(r), MOp::Mem(Mem { base: Base::Incoming, index: None, disp: 0 }));
            }
            InstKind::Load { ty, ptr, .. } => {
                let d = self.value_reg(i.dst.unwrap());
                let m = self.addr(*ptr);
                match ty {
                    Type::I8 => self.emit(Op::MovZX(Sz::B), Sz::L, MOp::Reg(d), MOp::Mem(m)),
                    Type::I16 => self.emit(Op::MovZX(Sz::W), Sz::L, MOp::Reg(d), MOp::Mem(m)),
                    Type::I32 => self.emit(Op::Mov, Sz::L, MOp::Reg(d), MOp::Mem(m)),
                    Type::I64 | Type::Ptr => self.emit(Op::Mov, Sz::Q, MOp::Reg(d), MOp::Mem(m)),
                    Type::F32 => self.emit(Op::MovF, Sz::L, MOp::Reg(d), MOp::Mem(m)),
                    Type::F64 => self.emit(Op::MovF, Sz::Q, MOp::Reg(d), MOp::Mem(m)),
                }
            }
            InstKind::Store { ty, val, ptr, .. } => {
                let m = self.addr(*ptr);
                self.store_value(*ty, *val, m);
            }
            InstKind::MemCopy { dst, src, size, .. } => {
                let (d, s) = (self.addr(*dst), self.addr(*src));
                self.memcpy(d, s, *size);
            }
            InstKind::MemSet { dst, byte, size, .. } => {
                let d = self.addr(*dst);
                self.memset(d, *byte, *size);
            }
            InstKind::Bin { op, ty, lhs, rhs } => self.bin(i.dst.unwrap(), *op, *ty, *lhs, *rhs),
            InstKind::Un { op, ty, val } => {
                let d = self.value_reg(i.dst.unwrap());
                match op {
                    UnOp::Neg | UnOp::Not => {
                        self.copy_operand(d, *val, *ty);
                        self.emit(
                            if *op == UnOp::Neg { Op::Neg } else { Op::Not },
                            Sz::of(*ty),
                            MOp::Reg(d),
                            MOp::None,
                        );
                    }
                    UnOp::FNeg => {
                        let a = self.reg(*val);
                        self.emit(Op::MovAps, Sz::Q, MOp::Reg(d), MOp::Reg(a));
                        let mask: Vec<u8> = if *ty == Type::F32 {
                            let mut v = vec![0u8; 16];
                            v[3] = 0x80;
                            v
                        } else {
                            let mut v = vec![0u8; 16];
                            v[7] = 0x80;
                            v
                        };
                        let m = self.const_mem(mask, 16);
                        self.emit(Op::Xorps, Sz::Q, MOp::Reg(d), MOp::Mem(m));
                    }
                }
            }
            InstKind::ICmp { pred, ty, lhs, rhs } => {
                let d = self.value_reg(i.dst.unwrap());
                let cc = self.cmp_flags(*pred, *ty, *lhs, *rhs);
                let t = self.gpr_v();
                self.emit(Op::SetCC(cc), Sz::B, MOp::Reg(t), MOp::None);
                self.emit(Op::MovZX(Sz::B), Sz::L, MOp::Reg(d), MOp::Reg(t));
            }
            InstKind::FCmp { pred, ty, lhs, rhs } => {
                let d = self.value_reg(i.dst.unwrap());
                let fc = self.fcmp_flags(*pred, *ty, *lhs, *rhs);
                let t = self.gpr_v();
                match fc {
                    FCond::Simple(cc) => self.emit(Op::SetCC(cc), Sz::B, MOp::Reg(t), MOp::None),
                    FCond::Eq => {
                        let u = self.gpr_v();
                        self.emit(Op::SetCC(Cc::E), Sz::B, MOp::Reg(t), MOp::None);
                        self.emit(Op::SetCC(Cc::Np), Sz::B, MOp::Reg(u), MOp::None);
                        self.emit(Op::And, Sz::B, MOp::Reg(t), MOp::Reg(u));
                    }
                    FCond::Ne => {
                        let u = self.gpr_v();
                        self.emit(Op::SetCC(Cc::Ne), Sz::B, MOp::Reg(t), MOp::None);
                        self.emit(Op::SetCC(Cc::P), Sz::B, MOp::Reg(u), MOp::None);
                        self.emit(Op::Or, Sz::B, MOp::Reg(t), MOp::Reg(u));
                    }
                }
                self.emit(Op::MovZX(Sz::B), Sz::L, MOp::Reg(d), MOp::Reg(t));
            }
            InstKind::Cast { op, from, to, val } => {
                if self.skip[id.idx()] {
                    return;
                }
                self.cast(i.dst.unwrap(), *op, *from, *to, *val);
            }
            InstKind::PtrAdd { base, offset } => {
                let d = i.dst.unwrap();
                if self.needs_reg[d.idx()] {
                    let m = self.addr_ptradd(d, *base, *offset);
                    let r = self.value_reg(d);
                    self.emit(Op::Lea, Sz::Q, MOp::Reg(r), MOp::Mem(m));
                }
            }
            InstKind::Select { ty, cond, a, b } => {
                assert!(!ty.is_float(), "floating-point select is not supported by the backend");
                let d = self.value_reg(i.dst.unwrap());
                self.copy_operand(d, *b, *ty);
                let c = self.reg(*cond);
                let csz = Sz::of(self.f.operand_ty(*cond));
                self.emit(Op::Test, csz, MOp::Reg(c), MOp::Reg(c));
                let ar = self.reg(*a);
                self.emit(Op::CMov(Cc::Ne), widen(Sz::of(*ty)), MOp::Reg(d), MOp::Reg(ar));
            }
            InstKind::Phi { .. } => {}
            InstKind::Call { callee, args, rets, variadic, .. } => self.call(i, callee, args, rets, *variadic),
            InstKind::Trap => self.emit_bare(Op::Ud2),
        }
    }

    fn store_value(&mut self, ty: Type, val: Operand, m: Mem) {
        let val = self.resolve(val);
        if ty.is_float() {
            if let Operand::Float(0, _) = val {
                // storing +0.0: an integer zero store has the same bits
                self.emit(Op::Mov, Sz::of(ty), MOp::Mem(m), MOp::Imm(0));
                return;
            }
            let r = self.reg(val);
            self.emit(Op::MovF, Sz::of(ty), MOp::Mem(m), MOp::Reg(r));
            return;
        }
        let sz = Sz::of(ty);
        let src = self.rmi(val);
        self.emit(Op::Mov, sz, MOp::Mem(m), src);
    }

    // ───────────────────────────── arithmetic ─────────────────────────────

    fn bin(&mut self, dst: ValueId, op: BinOp, ty: Type, lhs: Operand, rhs: Operand) {
        let d = self.value_reg(dst);
        if ty.is_float() {
            let a = self.reg(lhs);
            self.emit(Op::MovAps, Sz::Q, MOp::Reg(d), MOp::Reg(a));
            let b = self.reg(rhs);
            let o = match op {
                BinOp::FAdd => Op::FAdd,
                BinOp::FSub => Op::FSub,
                BinOp::FMul => Op::FMul,
                _ => Op::FDiv,
            };
            self.emit(o, Sz::of(ty), MOp::Reg(d), MOp::Reg(b));
            return;
        }
        let sz = Sz::of(ty);
        let (l0, r0) = (self.resolve(lhs), self.resolve(rhs));
        match op {
            BinOp::Add | BinOp::Sub | BinOp::And | BinOp::Or | BinOp::Xor | BinOp::Mul => {
                // `lea` gives three-address adds without disturbing the operands
                if matches!(sz, Sz::L | Sz::Q) {
                    match (op, l0, r0) {
                        (BinOp::Add, Operand::Value(_), Operand::Int(c, _)) if i32::try_from(c).is_ok() => {
                            let b = self.reg(l0);
                            self.emit(
                                Op::Lea,
                                sz,
                                MOp::Reg(d),
                                MOp::Mem(Mem { base: Base::Reg(b), index: None, disp: c as i32 }),
                            );
                            return;
                        }
                        (BinOp::Sub, Operand::Value(_), Operand::Int(c, _)) if i32::try_from(-c).is_ok() => {
                            let b = self.reg(l0);
                            self.emit(
                                Op::Lea,
                                sz,
                                MOp::Reg(d),
                                MOp::Mem(Mem { base: Base::Reg(b), index: None, disp: (-c) as i32 }),
                            );
                            return;
                        }
                        (BinOp::Add, Operand::Value(_), Operand::Value(_)) => {
                            let (a, b) = (self.reg(l0), self.reg(r0));
                            self.emit(
                                Op::Lea,
                                sz,
                                MOp::Reg(d),
                                MOp::Mem(Mem { base: Base::Reg(a), index: Some((b, 1)), disp: 0 }),
                            );
                            return;
                        }
                        _ => {}
                    }
                }
                let (l, r) = if op.is_commutative() && l0.is_const() && !r0.is_const() { (r0, l0) } else { (l0, r0) };
                self.copy_operand(d, l, ty);
                let src = self.rmi(r);
                let o = match op {
                    BinOp::Add => Op::Add,
                    BinOp::Sub => Op::Sub,
                    BinOp::And => Op::And,
                    BinOp::Or => Op::Or,
                    BinOp::Xor => Op::Xor,
                    _ => Op::Imul,
                };
                let sz = if o == Op::Imul { widen(sz) } else { sz };
                self.emit(o, sz, MOp::Reg(d), src);
            }
            BinOp::Shl | BinOp::LShr | BinOp::AShr => {
                self.copy_operand(d, l0, ty);
                let o = match op {
                    BinOp::Shl => Op::Shl,
                    BinOp::LShr => Op::Shr,
                    _ => Op::Sar,
                };
                match r0 {
                    Operand::Int(c, _) => self.emit(o, sz, MOp::Reg(d), MOp::Imm(c & (ty.bits() as i64 - 1))),
                    other => {
                        let c = self.reg(other);
                        self.emit(Op::Mov, Sz::L, MOp::Reg(Reg::P(RCX)), MOp::Reg(c));
                        self.emit(o, sz, MOp::Reg(d), MOp::Reg(Reg::P(RCX)));
                    }
                }
            }
            BinOp::SDiv | BinOp::UDiv | BinOp::SRem | BinOp::URem => {
                let sz = widen(sz);
                let signed = matches!(op, BinOp::SDiv | BinOp::SRem);
                let divisor = self.reg(r0);
                self.copy_operand(Reg::P(RAX), l0, ty);
                if signed {
                    self.emit(Op::SignExtAccum, sz, MOp::None, MOp::None);
                } else {
                    self.emit(Op::Mov, Sz::L, MOp::Reg(Reg::P(RDX)), MOp::Imm(0));
                }
                self.emit(if signed { Op::Idiv } else { Op::Div }, sz, MOp::None, MOp::Reg(divisor));
                let result = if matches!(op, BinOp::SDiv | BinOp::UDiv) { RAX } else { RDX };
                self.mov_rr(ty, d, Reg::P(result));
            }
            _ => unreachable!("float op handled above"),
        }
    }

    fn cmp_flags(&mut self, pred: IPred, ty: Type, lhs: Operand, rhs: Operand) -> Cc {
        let sz = Sz::of(ty);
        let (mut l, mut r, mut p) = (self.resolve(lhs), self.resolve(rhs), pred);
        if l.is_const() && !r.is_const() {
            std::mem::swap(&mut l, &mut r);
            p = p.swapped();
        }
        let lr = self.reg(l);
        let src = self.rmi(r);
        self.emit(Op::Cmp, sz, MOp::Reg(lr), src);
        Cc::from_ipred(p)
    }

    fn fcmp_flags(&mut self, pred: FPred, ty: Type, lhs: Operand, rhs: Operand) -> FCond {
        let sz = Sz::of(ty);
        let (a, b) = (self.reg(lhs), self.reg(rhs));
        let (x, y, fc) = match pred {
            FPred::Oeq => (a, b, FCond::Eq),
            FPred::Une => (a, b, FCond::Ne),
            FPred::Olt => (b, a, FCond::Simple(Cc::A)),
            FPred::Ole => (b, a, FCond::Simple(Cc::Ae)),
            FPred::Ogt => (a, b, FCond::Simple(Cc::A)),
            FPred::Oge => (a, b, FCond::Simple(Cc::Ae)),
        };
        self.emit(Op::Ucomi, sz, MOp::Reg(x), MOp::Reg(y));
        fc
    }

    // ───────────────────────────── casts ─────────────────────────────

    fn cast(&mut self, dst: ValueId, op: CastOp, from: Type, to: Type, val: Operand) {
        let d = self.value_reg(dst);
        let val = self.resolve(val);
        match op {
            CastOp::ZExt => {
                if let Operand::Int(c, _) = val {
                    let masked = if from.bits() >= 64 { c } else { c & ((1i64 << from.bits()) - 1) };
                    self.emit(Op::Mov, widen(Sz::of(to)), MOp::Reg(d), MOp::Imm(masked));
                    return;
                }
                let s = self.reg(val);
                match from {
                    Type::I8 | Type::I16 => self.emit(Op::MovZX(Sz::of(from)), Sz::L, MOp::Reg(d), MOp::Reg(s)),
                    _ => self.emit(Op::Mov, Sz::L, MOp::Reg(d), MOp::Reg(s)),
                }
            }
            CastOp::SExt => {
                if let Operand::Int(c, _) = val {
                    self.emit(Op::Mov, widen(Sz::of(to)), MOp::Reg(d), MOp::Imm(c));
                    return;
                }
                let s = self.reg(val);
                self.emit(Op::MovSX(Sz::of(from)), widen(Sz::of(to)), MOp::Reg(d), MOp::Reg(s));
            }
            CastOp::SIToFP => {
                let s = self.reg(val);
                self.emit(Op::CvtSi2F(Sz::of(from)), Sz::of(to), MOp::Reg(d), MOp::Reg(s));
            }
            CastOp::UIToFP => {
                let s = self.reg(val);
                if from == Type::I32 {
                    let t = self.gpr_v();
                    self.emit(Op::Mov, Sz::L, MOp::Reg(t), MOp::Reg(s));
                    self.emit(Op::CvtSi2F(Sz::Q), Sz::of(to), MOp::Reg(d), MOp::Reg(t));
                } else {
                    // values with the top bit set: halve (keeping the low bit sticky), convert, double
                    let pos = self.add_block();
                    let neg = self.add_block();
                    let end = self.add_block();
                    self.emit(Op::Test, Sz::Q, MOp::Reg(s), MOp::Reg(s));
                    self.emit(Op::Jcc(Cc::S, neg), Sz::Q, MOp::None, MOp::None);
                    self.set_cur(pos);
                    self.emit(Op::CvtSi2F(Sz::Q), Sz::of(to), MOp::Reg(d), MOp::Reg(s));
                    self.emit(Op::Jmp(end), Sz::Q, MOp::None, MOp::None);
                    self.set_cur(neg);
                    let (t, u) = (self.gpr_v(), self.gpr_v());
                    self.emit(Op::Mov, Sz::Q, MOp::Reg(t), MOp::Reg(s));
                    self.emit(Op::Shr, Sz::Q, MOp::Reg(t), MOp::Imm(1));
                    self.emit(Op::Mov, Sz::Q, MOp::Reg(u), MOp::Reg(s));
                    self.emit(Op::And, Sz::Q, MOp::Reg(u), MOp::Imm(1));
                    self.emit(Op::Or, Sz::Q, MOp::Reg(t), MOp::Reg(u));
                    self.emit(Op::CvtSi2F(Sz::Q), Sz::of(to), MOp::Reg(d), MOp::Reg(t));
                    self.emit(Op::FAdd, Sz::of(to), MOp::Reg(d), MOp::Reg(d));
                    self.set_cur(end);
                }
            }
            CastOp::FPToSI => {
                let s = self.reg(val);
                self.emit(Op::CvtF2Si(Sz::of(to)), Sz::of(from), MOp::Reg(d), MOp::Reg(s));
            }
            CastOp::FPToUI => {
                let s = self.reg(val);
                if to == Type::I32 {
                    // go through a 64-bit conversion; the low 32 bits are the answer
                    self.emit(Op::CvtF2Si(Sz::Q), Sz::of(from), MOp::Reg(d), MOp::Reg(s));
                } else {
                    let small = self.add_block();
                    let big = self.add_block();
                    let end = self.add_block();
                    let limit = self.xmm_v();
                    self.load_float_const(limit, 9223372036854775808.0f64.to_bits(), from);
                    self.emit(Op::Ucomi, Sz::of(from), MOp::Reg(s), MOp::Reg(limit));
                    self.emit(Op::Jcc(Cc::Ae, big), Sz::Q, MOp::None, MOp::None);
                    self.set_cur(small);
                    self.emit(Op::CvtF2Si(Sz::Q), Sz::of(from), MOp::Reg(d), MOp::Reg(s));
                    self.emit(Op::Jmp(end), Sz::Q, MOp::None, MOp::None);
                    self.set_cur(big);
                    let t = self.xmm_v();
                    self.emit(Op::MovAps, Sz::Q, MOp::Reg(t), MOp::Reg(s));
                    self.emit(Op::FSub, Sz::of(from), MOp::Reg(t), MOp::Reg(limit));
                    self.emit(Op::CvtF2Si(Sz::Q), Sz::of(from), MOp::Reg(d), MOp::Reg(t));
                    let top = self.gpr_v();
                    self.emit(Op::Mov, Sz::Q, MOp::Reg(top), MOp::Imm(i64::MIN));
                    self.emit(Op::Xor, Sz::Q, MOp::Reg(d), MOp::Reg(top));
                    self.set_cur(end);
                }
            }
            CastOp::FPExt => {
                let s = self.reg(val);
                self.emit(Op::CvtF2F, Sz::Q, MOp::Reg(d), MOp::Reg(s));
            }
            CastOp::FPTrunc => {
                let s = self.reg(val);
                self.emit(Op::CvtF2F, Sz::L, MOp::Reg(d), MOp::Reg(s));
            }
            CastOp::Trunc | CastOp::PtrToInt | CastOp::IntToPtr => {
                // not aliased (e.g. narrowing ptrtoint): a plain register move
                let s = self.reg(val);
                self.emit(Op::Mov, widen(Sz::of(to)), MOp::Reg(d), MOp::Reg(s));
            }
        }
    }

    // ───────────────────────────── block memory operations ─────────────────────────────

    fn memcpy(&mut self, dst: Mem, src: Mem, size: u64) {
        if size == 0 {
            return;
        }
        if size <= 128 {
            let mut off: u64 = 0;
            while size - off >= 16 {
                let t = self.xmm_v();
                self.emit(Op::MovUps, Sz::Q, MOp::Reg(t), MOp::Mem(src.offset(off as i32)));
                self.emit(Op::MovUps, Sz::Q, MOp::Mem(dst.offset(off as i32)), MOp::Reg(t));
                off += 16;
            }
            for (chunk, sz) in [(8u64, Sz::Q), (4, Sz::L), (2, Sz::W), (1, Sz::B)] {
                while size - off >= chunk {
                    let t = self.gpr_v();
                    self.emit(Op::Mov, sz, MOp::Reg(t), MOp::Mem(src.offset(off as i32)));
                    self.emit(Op::Mov, sz, MOp::Mem(dst.offset(off as i32)), MOp::Reg(t));
                    off += chunk;
                }
            }
        } else {
            self.emit(Op::Lea, Sz::Q, MOp::Reg(Reg::P(RDI)), MOp::Mem(dst));
            self.emit(Op::Lea, Sz::Q, MOp::Reg(Reg::P(RSI)), MOp::Mem(src));
            self.emit(Op::Mov, Sz::Q, MOp::Reg(Reg::P(RCX)), MOp::Imm(size as i64));
            self.emit_bare(Op::RepMovsb);
        }
    }

    fn memset(&mut self, dst: Mem, byte: u8, size: u64) {
        if size == 0 {
            return;
        }
        if size <= 128 {
            let mut off: u64 = 0;
            if size >= 16 {
                let t = self.xmm_v();
                if byte == 0 {
                    self.emit(Op::ZeroF, Sz::Q, MOp::Reg(t), MOp::None);
                } else {
                    let pattern = vec![byte; 16];
                    let m = self.const_mem(pattern, 16);
                    self.emit(Op::MovUps, Sz::Q, MOp::Reg(t), MOp::Mem(m));
                }
                while size - off >= 16 {
                    self.emit(Op::MovUps, Sz::Q, MOp::Mem(dst.offset(off as i32)), MOp::Reg(t));
                    off += 16;
                }
            }
            let pat = u64::from_le_bytes([byte; 8]);
            for (chunk, sz) in [(8u64, Sz::Q), (4, Sz::L), (2, Sz::W), (1, Sz::B)] {
                while size - off >= chunk {
                    let val = match sz {
                        Sz::Q => pat as i64,
                        Sz::L => pat as u32 as i32 as i64,
                        Sz::W => pat as u16 as i16 as i64,
                        Sz::B => byte as i8 as i64,
                    };
                    if sz == Sz::Q && i32::try_from(val).is_err() {
                        let t = self.gpr_v();
                        self.emit(Op::Mov, Sz::Q, MOp::Reg(t), MOp::Imm(val));
                        self.emit(Op::Mov, sz, MOp::Mem(dst.offset(off as i32)), MOp::Reg(t));
                    } else {
                        self.emit(Op::Mov, sz, MOp::Mem(dst.offset(off as i32)), MOp::Imm(val));
                    }
                    off += chunk;
                }
            }
        } else {
            self.emit(Op::Lea, Sz::Q, MOp::Reg(Reg::P(RDI)), MOp::Mem(dst));
            self.emit(Op::Mov, Sz::L, MOp::Reg(Reg::P(RAX)), MOp::Imm(byte as i64));
            self.emit(Op::Mov, Sz::Q, MOp::Reg(Reg::P(RCX)), MOp::Imm(size as i64));
            self.emit_bare(Op::RepStosb);
        }
    }

    // ───────────────────────────── calls ─────────────────────────────

    fn call(&mut self, i: &Inst, callee: &Callee, args: &[CallArg], rets: &[Type], variadic: bool) {
        let f = self.f;
        let descs: Vec<ArgDesc> = args
            .iter()
            .map(|a| match &a.kind {
                ArgKind::Value => ArgDesc { ty: Some(f.operand_ty(a.val)), byval: None, group: a.group },
                ArgKind::ByVal { size, align } => ArgDesc { ty: None, byval: Some((*size, *align)), group: None },
            })
            .collect();
        let asg = abi::assign(&descs);
        self.mf.outgoing = self.mf.outgoing.max(asg.stack_size);

        // arguments that live on the stack
        for (a, loc) in args.iter().zip(&asg.locs) {
            let Loc::Stack(off) = loc else { continue };
            let m = Mem { base: Base::Outgoing, index: None, disp: *off as i32 };
            match &a.kind {
                ArgKind::Value => {
                    let ty = f.operand_ty(a.val);
                    self.store_value(ty, a.val, m);
                }
                ArgKind::ByVal { size, .. } => {
                    let src = self.addr(a.val);
                    self.memcpy(m, src, *size as u64);
                }
            }
        }
        // register arguments
        let mut uses: Vec<u8> = Vec::new();
        let mut xmm_used = 0u8;
        for (a, loc) in args.iter().zip(&asg.locs) {
            let ty = f.operand_ty(a.val);
            match loc {
                Loc::Gpr(n) => {
                    let p = ARG_GPRS[*n as usize];
                    self.copy_operand(Reg::P(p), a.val, ty);
                    uses.push(p);
                }
                Loc::Xmm(n) => {
                    let p = xmm(*n);
                    self.copy_operand(Reg::P(p), a.val, ty);
                    uses.push(p);
                    xmm_used = xmm_used.max(n + 1);
                }
                Loc::Stack(_) => {}
            }
        }
        if variadic {
            self.emit(Op::Mov, Sz::L, MOp::Reg(Reg::P(RAX)), MOp::Imm(xmm_used as i64));
            uses.push(RAX);
        }
        let target = match callee {
            Callee::Direct(s) => Target::Sym(*s),
            Callee::Indirect(o) => Target::Reg(self.reg(*o)),
        };
        let mut defs: Vec<u8> = Vec::new();
        let (mut gi, mut fi) = (0usize, 0u8);
        for t in rets {
            if t.is_float() {
                defs.push(xmm(fi));
                fi += 1;
            } else {
                defs.push([RAX, RDX][gi]);
                gi += 1;
            }
        }
        let info = CallInfo { target, uses, defs: defs.clone() };
        self.emit(Op::Call(Box::new(info)), Sz::Q, MOp::None, MOp::None);
        for (k, t) in rets.iter().enumerate() {
            let dv = if k == 0 { i.dst } else { i.dst2 };
            if let Some(v) = dv {
                if self.uses[v.idx()] == 0 {
                    continue; // result is never read
                }
                let r = self.value_reg(v);
                self.mov_rr(*t, r, Reg::P(defs[k]));
            }
        }
    }

    // ───────────────────────────── terminators ─────────────────────────────

    pub(super) fn terminator(&mut self, bi: usize) {
        let f = self.f;
        let b = &f.blocks[bi];
        match &b.term {
            Term::Br(t) => {
                self.emit_phi_copies(BlockId(bi as u32), *t);
                self.emit(Op::Jmp(t.idx()), Sz::Q, MOp::None, MOp::None);
            }
            Term::CondBr { cond, then_bb, else_bb } => self.cond_branch(*cond, then_bb.idx(), else_bb.idx()),
            Term::Switch { ty, val, cases, default } => {
                let sz = Sz::of(*ty);
                let v = self.reg(*val);
                for (c, target) in cases {
                    if i32::try_from(*c).is_ok() {
                        self.emit(Op::Cmp, sz, MOp::Reg(v), MOp::Imm(*c));
                    } else {
                        let t = self.reg(Operand::Int(*c, Type::I64));
                        self.emit(Op::Cmp, sz, MOp::Reg(v), MOp::Reg(t));
                    }
                    self.emit(Op::Jcc(Cc::E, target.idx()), Sz::Q, MOp::None, MOp::None);
                }
                self.emit(Op::Jmp(default.idx()), Sz::Q, MOp::None, MOp::None);
            }
            Term::Ret(vals) => {
                let (mut gi, mut fi) = (0usize, 0u8);
                for v in vals {
                    let ty = f.operand_ty(*v);
                    let p = if ty.is_float() {
                        fi += 1;
                        xmm(fi - 1)
                    } else {
                        gi += 1;
                        [RAX, RDX][gi - 1]
                    };
                    self.copy_operand(Reg::P(p), *v, ty);
                }
                self.emit_bare(Op::Ret);
            }
            Term::Unreachable => self.emit_bare(Op::Ud2),
            Term::None => unreachable!("block without terminator"),
        }
    }

    fn cond_branch(&mut self, cond: Operand, t: usize, e: usize) {
        let f = self.f;
        let cond = self.resolve(cond);
        if let Operand::Int(c, _) = cond {
            self.emit(Op::Jmp(if c != 0 { t } else { e }), Sz::Q, MOp::None, MOp::None);
            return;
        }
        if let Operand::Value(v) = cond {
            if let Some(id) = f.def_inst(v) {
                if self.fused[id.idx()] {
                    match &f.insts[id.idx()].kind {
                        InstKind::ICmp { pred, ty, lhs, rhs } => {
                            let cc = self.cmp_flags(*pred, *ty, *lhs, *rhs);
                            self.emit(Op::Jcc(cc, t), Sz::Q, MOp::None, MOp::None);
                            self.emit(Op::Jmp(e), Sz::Q, MOp::None, MOp::None);
                            return;
                        }
                        InstKind::FCmp { pred, ty, lhs, rhs } => {
                            match self.fcmp_flags(*pred, *ty, *lhs, *rhs) {
                                FCond::Simple(cc) => {
                                    self.emit(Op::Jcc(cc, t), Sz::Q, MOp::None, MOp::None);
                                    self.emit(Op::Jmp(e), Sz::Q, MOp::None, MOp::None);
                                }
                                FCond::Eq => {
                                    self.emit(Op::Jcc(Cc::Ne, e), Sz::Q, MOp::None, MOp::None);
                                    self.emit(Op::Jcc(Cc::P, e), Sz::Q, MOp::None, MOp::None);
                                    self.emit(Op::Jmp(t), Sz::Q, MOp::None, MOp::None);
                                }
                                FCond::Ne => {
                                    self.emit(Op::Jcc(Cc::Ne, t), Sz::Q, MOp::None, MOp::None);
                                    self.emit(Op::Jcc(Cc::P, t), Sz::Q, MOp::None, MOp::None);
                                    self.emit(Op::Jmp(e), Sz::Q, MOp::None, MOp::None);
                                }
                            }
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
        let ty = f.operand_ty(cond);
        let c = self.reg(cond);
        self.emit(Op::Test, Sz::of(ty), MOp::Reg(c), MOp::Reg(c));
        self.emit(Op::Jcc(Cc::Ne, t), Sz::Q, MOp::None, MOp::None);
        self.emit(Op::Jmp(e), Sz::Q, MOp::None, MOp::None);
    }
}
