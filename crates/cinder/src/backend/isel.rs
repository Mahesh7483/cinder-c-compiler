//! Instruction selection: IR -> MIR over virtual registers.
//!
//! * address expressions (`alloca`, `ptradd`, symbols) are folded into x86
//!   addressing modes at their load/store/memcpy uses;
//! * a compare whose only use is the block's conditional branch is fused
//!   into `cmp` + `jcc`;
//! * phis are eliminated with copies on the incoming edges (critical edges
//!   are split first);
//! * calls follow the SysV ABI using the shared argument assignment.

use super::mir::*;
use crate::abi::{self, ArgDesc, Loc};
use crate::ir::*;
use std::collections::HashMap;

/// Select instructions for one function.
pub fn select(_m: &Module, f: &Func) -> MFunc {
    let mut f = f.clone();
    split_critical_edges(&mut f);
    let mut isel = Isel::new(&f);
    isel.run();
    isel.mf
}

// ───────────────────────────── critical edges ─────────────────────────────

fn retarget(t: &mut Term, from: BlockId, to: BlockId) {
    let m = |b: &mut BlockId| {
        if *b == from {
            *b = to;
        }
    };
    match t {
        Term::Br(b) => m(b),
        Term::CondBr { then_bb, else_bb, .. } => {
            m(then_bb);
            m(else_bb);
        }
        Term::Switch { cases, default, .. } => {
            for (_, b) in cases.iter_mut() {
                m(b);
            }
            m(default);
        }
        _ => {}
    }
}

/// Insert an empty block on every edge from a multi-successor block to a
/// multi-predecessor block that starts with phis, so phi copies can always
/// be placed on the edge.
pub fn split_critical_edges(f: &mut Func) {
    let cfg = crate::ir::cfg::build(f);
    let n = f.blocks.len();
    for s in 0..n {
        let has_phi = f.blocks[s].insts.first().is_some_and(|&i| f.is_phi(i));
        if !has_phi || cfg.preds[s].len() < 2 {
            continue;
        }
        let sid = BlockId(s as u32);
        for &p in &cfg.preds[s] {
            if cfg.succs[p.idx()].len() < 2 {
                continue;
            }
            let nb = f.new_block("split");
            f.blocks[nb.idx()].term = Term::Br(sid);
            retarget(&mut f.blocks[p.idx()].term, sid, nb);
            let ids: Vec<InstId> = f.blocks[s].insts.clone();
            for id in ids {
                if let InstKind::Phi { incoming, .. } = &mut f.insts[id.idx()].kind {
                    for (b, _) in incoming.iter_mut() {
                        if *b == p {
                            *b = nb;
                        }
                    }
                }
            }
        }
    }
}

// ───────────────────────────── selector ─────────────────────────────

pub(super) struct Isel<'a> {
    pub(super) f: &'a Func,
    pub(super) mf: MFunc,
    pub(super) vmap: Vec<Option<Reg>>,
    pub(super) uses: Vec<u32>,
    pub(super) cur: usize,
    /// Operands that are plain aliases of others (trunc, ptrtoint, inttoptr).
    pub(super) alias: Vec<Option<Operand>>,
    /// Compares fused into their block's terminator.
    pub(super) fused: Vec<bool>,
    /// Instructions folded into addressing modes (skipped).
    pub(super) skip: Vec<bool>,
    /// Address-expression values (alloca/ptradd) that need a register.
    pub(super) needs_reg: Vec<bool>,
    pub(super) alloca_slot: Vec<Option<u32>>,
    /// For `ptradd base, mul/shl x` offsets: the index register and scale.
    pub(super) fold_index: Vec<Option<(Operand, u8)>>,
    pub(super) last_line: u32,
    pub(super) const_map: HashMap<(Vec<u8>, u32), u32>,
}

fn is_addr_value(f: &Func, v: ValueId) -> Option<&InstKind> {
    let id = f.def_inst(v)?;
    match &f.insts[id.idx()].kind {
        k @ (InstKind::PtrAdd { .. } | InstKind::Alloca { .. }) => Some(k),
        _ => None,
    }
}

impl<'a> Isel<'a> {
    pub(super) fn new(f: &'a Func) -> Isel<'a> {
        let mf = MFunc {
            name: f.name,
            linkage: f.linkage,
            blocks: Vec::new(),
            layout: Vec::new(),
            vclass: Vec::new(),
            slots: Vec::new(),
            consts: Vec::new(),
            outgoing: 0,
            variadic: f.variadic,
            regsave_slot: None,
            hints: HashMap::new(),
            used_callee_saved: Vec::new(),
            frame_size: 0,
        };
        let nv = f.values.len();
        let mut s = Isel {
            f,
            mf,
            vmap: vec![None; nv],
            uses: crate::ir::cfg::use_counts(f),
            cur: 0,
            alias: vec![None; nv],
            fused: vec![false; f.insts.len()],
            skip: vec![false; f.insts.len()],
            needs_reg: vec![false; nv],
            alloca_slot: vec![None; nv],
            fold_index: vec![None; nv],
            last_line: 0,
            const_map: HashMap::new(),
        };
        s.analyze();
        s
    }

    // ───────────────────────────── pre-analysis ─────────────────────────────

    pub(super) fn analyze(&mut self) {
        let f = self.f;
        // aliases (no-op casts)
        for b in &f.blocks {
            for &id in &b.insts {
                let i = &f.insts[id.idx()];
                if let (InstKind::Cast { op, from, to, val }, Some(d)) = (&i.kind, i.dst) {
                    let alias = match op {
                        CastOp::Trunc => true,
                        CastOp::PtrToInt => *to == Type::I64,
                        CastOp::IntToPtr => *from == Type::I64,
                        _ => false,
                    };
                    if alias {
                        let src = match val {
                            Operand::Int(c, _) if *op == CastOp::Trunc => crate::lower::canon(*c as u64, *to),
                            Operand::Int(c, _) => *c,
                            _ => 0,
                        };
                        let target = match val {
                            Operand::Int(..) => Operand::Int(src, *to),
                            other => *other,
                        };
                        self.alias[d.idx()] = Some(target);
                        self.skip[id.idx()] = true;
                    }
                }
            }
        }
        // allocas get frame slots
        for &id in &f.blocks[0].insts {
            if let InstKind::Alloca { size, align } = &f.insts[id.idx()].kind {
                let slot = self.mf.new_slot(*size, *align);
                self.alloca_slot[f.insts[id.idx()].dst.unwrap().idx()] = Some(slot);
            }
        }
        // compare/branch fusion
        for b in &f.blocks {
            if let Term::CondBr { cond: Operand::Value(v), .. } = &b.term {
                if let Some(id) = f.def_inst(*v) {
                    let in_block = b.insts.contains(&id);
                    if in_block
                        && self.uses[v.idx()] == 1
                        && matches!(f.insts[id.idx()].kind, InstKind::ICmp { .. } | InstKind::FCmp { .. })
                    {
                        self.fused[id.idx()] = true;
                        self.skip[id.idx()] = true;
                    }
                }
            }
        }
        // index folding: ptradd base, (x * {1,2,4,8}) / (x << {0..3})
        for b in &f.blocks {
            for &id in &b.insts {
                let i = &f.insts[id.idx()];
                let (InstKind::PtrAdd { offset: Operand::Value(ov), .. }, Some(d)) = (&i.kind, i.dst) else { continue };
                let Some(oid) = f.def_inst(*ov) else { continue };
                let found = match &f.insts[oid.idx()].kind {
                    InstKind::Bin { op: BinOp::Mul, ty: Type::I64, lhs, rhs: Operand::Int(c, _) }
                        if matches!(c, 1 | 2 | 4 | 8) =>
                    {
                        Some((*lhs, *c as u8))
                    }
                    InstKind::Bin { op: BinOp::Shl, ty: Type::I64, lhs, rhs: Operand::Int(c, _) }
                        if (0..=3).contains(c) =>
                    {
                        Some((*lhs, 1u8 << *c))
                    }
                    _ => None,
                };
                if let Some((x, scale)) = found {
                    if matches!(x, Operand::Value(_)) {
                        self.fold_index[d.idx()] = Some((x, scale));
                        if self.uses[ov.idx()] == 1 {
                            self.skip[oid.idx()] = true;
                        }
                    }
                }
            }
        }
        // which address values need a register of their own
        for b in &f.blocks {
            for &id in &b.insts {
                if self.skip[id.idx()] {
                    continue;
                }
                let i = &f.insts[id.idx()];
                let mark = |s: &mut Isel, o: Operand| {
                    if let Operand::Value(v) = o {
                        s.needs_reg[v.idx()] = true;
                    }
                };
                match &i.kind {
                    InstKind::Load { .. } | InstKind::MemSet { .. } => {
                        // the pointer operand folds
                    }
                    InstKind::Store { val, .. } => mark(self, *val),
                    InstKind::MemCopy { .. } => {}
                    InstKind::PtrAdd { offset, .. } => {
                        // base folds; the offset is a plain value
                        if let Some(d) = i.dst {
                            if self.fold_index[d.idx()].is_none() {
                                mark(self, *offset);
                            }
                        }
                    }
                    other => other.for_each_operand(|o| mark(self, o)),
                }
            }
            for o in b.term.operands() {
                if let Operand::Value(v) = o {
                    self.needs_reg[v.idx()] = true;
                }
            }
        }
        // Operands that feed an index fold need registers (the index itself).
        for b in &f.blocks {
            for &id in &b.insts {
                if let (InstKind::PtrAdd { .. }, Some(d)) = (&f.insts[id.idx()].kind, f.insts[id.idx()].dst) {
                    if let Some((Operand::Value(x), _)) = self.fold_index[d.idx()] {
                        self.needs_reg[x.idx()] = true;
                    }
                }
            }
        }
        // Aliases: whoever needs the alias target needs it as a register too.
        for v in 0..self.alias.len() {
            if self.needs_reg[v] {
                if let Some(Operand::Value(t)) = self.alias[v] {
                    self.needs_reg[t.idx()] = true;
                }
            }
        }
    }

    // ───────────────────────────── helpers ─────────────────────────────

    pub(super) fn resolve(&self, mut o: Operand) -> Operand {
        while let Operand::Value(v) = o {
            match self.alias[v.idx()] {
                Some(a) => o = a,
                None => break,
            }
        }
        o
    }

    pub(super) fn emit(&mut self, op: Op, sz: Sz, dst: MOp, src: MOp) {
        let cur = self.cur;
        self.mf.blocks[cur].insts.push(MInst::new(op, sz, dst, src));
    }

    pub(super) fn emit_bare(&mut self, op: Op) {
        let cur = self.cur;
        self.mf.blocks[cur].insts.push(MInst::bare(op));
    }

    pub(super) fn vr(&mut self, ty: Type) -> Reg {
        self.mf.new_vreg(if ty.is_float() { Class::Xmm } else { Class::Gpr })
    }

    pub(super) fn gpr_v(&mut self) -> Reg {
        self.mf.new_vreg(Class::Gpr)
    }

    pub(super) fn xmm_v(&mut self) -> Reg {
        self.mf.new_vreg(Class::Xmm)
    }

    pub(super) fn value_reg(&mut self, v: ValueId) -> Reg {
        if let Some(r) = self.vmap[v.idx()] {
            return r;
        }
        let ty = self.f.values[v.idx()].ty;
        let r = self.vr(ty);
        self.vmap[v.idx()] = Some(r);
        r
    }

    pub(super) fn add_block(&mut self) -> usize {
        let idx = self.mf.blocks.len();
        self.mf.blocks.push(MBlock::default());
        self.mf.layout.push(idx);
        idx
    }

    pub(super) fn set_cur(&mut self, b: usize) {
        self.cur = b;
    }

    pub(super) fn set_hint(&mut self, dst: Reg, src: Reg) {
        if let Reg::V(d) = dst {
            self.mf.hints.entry(d).or_insert(src);
        }
        if let Reg::V(s) = src {
            self.mf.hints.entry(s).or_insert(dst);
        }
    }

    pub(super) fn mov_rr(&mut self, ty: Type, dst: Reg, src: Reg) {
        if ty.is_float() {
            self.emit(Op::MovAps, Sz::Q, MOp::Reg(dst), MOp::Reg(src));
        } else {
            let sz = match Sz::of(ty) {
                Sz::B | Sz::W => Sz::L,
                s => s,
            };
            self.emit(Op::Mov, sz, MOp::Reg(dst), MOp::Reg(src));
        }
        self.set_hint(dst, src);
    }

    pub(super) fn const_mem(&mut self, bytes: Vec<u8>, align: u32) -> Mem {
        let key = (bytes.clone(), align);
        let idx = match self.const_map.get(&key) {
            Some(&i) => i,
            None => {
                let i = self.mf.consts.len() as u32;
                self.mf.consts.push(ConstEntry { bytes, align });
                self.const_map.insert(key, i);
                i
            }
        };
        Mem { base: Base::Const(idx), index: None, disp: 0 }
    }

    /// Materialize an operand in a register of the right class.
    pub(super) fn reg(&mut self, o: Operand) -> Reg {
        let o = self.resolve(o);
        match o {
            Operand::Value(v) => self.value_reg(v),
            Operand::Int(c, ty) => {
                let d = self.gpr_v();
                let sz = match Sz::of(ty) {
                    Sz::B | Sz::W => Sz::L,
                    s => s,
                };
                self.emit(Op::Mov, sz, MOp::Reg(d), MOp::Imm(c));
                d
            }
            Operand::Float(bits, ty) => {
                let d = self.xmm_v();
                self.load_float_const(d, bits, ty);
                d
            }
            Operand::Global(s) => {
                let d = self.gpr_v();
                self.emit(Op::Lea, Sz::Q, MOp::Reg(d), MOp::Mem(Mem { base: Base::Sym(s), index: None, disp: 0 }));
                d
            }
            Operand::Undef(ty) => {
                let d = self.vr(ty);
                if ty.is_float() {
                    self.emit(Op::ZeroF, Sz::Q, MOp::Reg(d), MOp::None);
                } else {
                    self.emit(Op::Mov, Sz::L, MOp::Reg(d), MOp::Imm(0));
                }
                d
            }
        }
    }

    pub(super) fn load_float_const(&mut self, d: Reg, bits: u64, ty: Type) {
        if bits == 0 {
            self.emit(Op::ZeroF, Sz::Q, MOp::Reg(d), MOp::None);
            return;
        }
        let f = f64::from_bits(bits);
        let (bytes, sz) = if ty == Type::F32 {
            ((f as f32).to_bits().to_le_bytes().to_vec(), Sz::L)
        } else {
            (bits.to_le_bytes().to_vec(), Sz::Q)
        };
        let mem = self.const_mem(bytes, sz.bytes());
        self.emit(Op::MovF, sz, MOp::Reg(d), MOp::Mem(mem));
    }

    /// An immediate if it fits a sign-extended imm32, else a register.
    pub(super) fn rmi(&mut self, o: Operand) -> MOp {
        let o = self.resolve(o);
        if let Operand::Int(c, _) = o {
            if c >= i32::MIN as i64 && c <= i32::MAX as i64 {
                return MOp::Imm(c);
            }
        }
        MOp::Reg(self.reg(o))
    }

    pub(super) fn lea_reg(&mut self, m: Mem) -> Reg {
        let d = self.gpr_v();
        self.emit(Op::Lea, Sz::Q, MOp::Reg(d), MOp::Mem(m));
        d
    }

    // ───────────────────────────── addressing ─────────────────────────────

    pub(super) fn addr(&mut self, ptr: Operand) -> Mem {
        let ptr = self.resolve(ptr);
        match ptr {
            Operand::Global(s) => Mem { base: Base::Sym(s), index: None, disp: 0 },
            Operand::Value(v) => {
                if let Some(slot) = self.alloca_slot[v.idx()] {
                    return Mem::slot(slot);
                }
                if !self.needs_reg[v.idx()] {
                    if let Some(InstKind::PtrAdd { base, offset }) = is_addr_value(self.f, v) {
                        let (base, offset) = (*base, *offset);
                        return self.addr_ptradd(v, base, offset);
                    }
                }
                Mem::reg(self.value_reg(v))
            }
            other => Mem::reg(self.reg(other)),
        }
    }

    pub(super) fn addr_ptradd(&mut self, v: ValueId, base: Operand, offset: Operand) -> Mem {
        let mut m = self.addr(base);
        if let Some((x, scale)) = self.fold_index[v.idx()] {
            let xr = self.reg(x);
            if m.index.is_some() {
                let r = self.lea_reg(m);
                m = Mem::reg(r);
            }
            m.index = Some((xr, scale));
            return m;
        }
        match self.resolve(offset) {
            Operand::Int(c, _) => {
                if let Some(d) = i32::try_from(c).ok().and_then(|c| m.disp.checked_add(c)) {
                    m.disp = d;
                    return m;
                }
                let r = self.reg(Operand::Int(c, Type::I64));
                self.with_index(m, r)
            }
            o => {
                let r = self.reg(o);
                self.with_index(m, r)
            }
        }
    }

    pub(super) fn with_index(&mut self, mut m: Mem, idx: Reg) -> Mem {
        if m.index.is_some() {
            let r = self.lea_reg(m);
            m = Mem::reg(r);
        }
        m.index = Some((idx, 1));
        m
    }

    // ───────────────────────────── driver ─────────────────────────────

    pub(super) fn run(&mut self) {
        let f = self.f;
        // one MIR block per IR block, in IR order
        for _ in &f.blocks {
            self.mf.blocks.push(MBlock::default());
        }
        for bi in 0..f.blocks.len() {
            self.mf.layout.push(bi);
            self.cur = bi;
            self.last_line = 0;
            if bi == 0 {
                self.prologue();
            }
            self.start_of_block(bi);
            for k in 0..f.blocks[bi].insts.len() {
                let id = f.blocks[bi].insts[k];
                if self.skip[id.idx()] {
                    continue;
                }
                let line = f.insts[id.idx()].line;
                self.mark_line(line);
                self.inst(id);
            }
            self.mark_line(f.blocks[bi].term_line);
            self.terminator(bi);
        }
    }

    pub(super) fn mark_line(&mut self, line: u32) {
        if line != 0 && line != self.last_line {
            self.last_line = line;
            self.emit_bare(Op::Loc(line));
        }
    }

    /// Move incoming arguments into virtual registers.
    pub(super) fn prologue(&mut self) {
        let f = self.f;
        let descs: Vec<ArgDesc> = f
            .params
            .iter()
            .map(|p| match &p.kind {
                ParamKind::Value(t) => ArgDesc { ty: Some(*t), byval: None, group: p.group },
                ParamKind::ByVal { size, align } => ArgDesc { ty: None, byval: Some((*size, *align)), group: None },
            })
            .collect();
        let a = abi::assign(&descs);
        // Register moves first, in order, so no later move can clobber a pending source.
        for (i, loc) in a.locs.iter().enumerate() {
            let v = f.param_values[i];
            let ty = f.values[v.idx()].ty;
            let dst = self.value_reg(v);
            match loc {
                Loc::Gpr(n) => {
                    let sz = match Sz::of(ty) {
                        Sz::B | Sz::W => Sz::L,
                        s => s,
                    };
                    self.emit(Op::Mov, sz, MOp::Reg(dst), MOp::Reg(Reg::P(ARG_GPRS[*n as usize])));
                    self.set_hint(dst, Reg::P(ARG_GPRS[*n as usize]));
                }
                Loc::Xmm(n) => {
                    self.emit(Op::MovAps, Sz::Q, MOp::Reg(dst), MOp::Reg(Reg::P(xmm(*n))));
                    self.set_hint(dst, Reg::P(xmm(*n)));
                }
                Loc::Stack(_) => {}
            }
        }
        for (i, loc) in a.locs.iter().enumerate() {
            let v = f.param_values[i];
            let ty = f.values[v.idx()].ty;
            let dst = self.value_reg(v);
            if let Loc::Stack(off) = loc {
                let mem = Mem { base: Base::Incoming, index: None, disp: *off as i32 };
                match &f.params[i].kind {
                    ParamKind::ByVal { .. } => self.emit(Op::Lea, Sz::Q, MOp::Reg(dst), MOp::Mem(mem)),
                    ParamKind::Value(_) if ty.is_float() => {
                        self.emit(Op::MovF, Sz::of(ty), MOp::Reg(dst), MOp::Mem(mem))
                    }
                    ParamKind::Value(_) => {
                        let sz = match Sz::of(ty) {
                            Sz::B | Sz::W => Sz::L,
                            s => s,
                        };
                        self.emit(Op::Mov, sz, MOp::Reg(dst), MOp::Mem(mem))
                    }
                }
            }
        }
        if f.variadic {
            let slot = self.mf.new_slot(176, 16);
            self.mf.regsave_slot = Some(slot);
        }
    }

    /// Phi copies for a single-predecessor successor of a multi-successor block.
    pub(super) fn start_of_block(&mut self, bi: usize) {
        let f = self.f;
        let has_phi = f.blocks[bi].insts.first().is_some_and(|&i| f.is_phi(i));
        if !has_phi {
            return;
        }
        let cfg_preds: Vec<usize> =
            (0..f.blocks.len()).filter(|&p| f.blocks[p].term.successors().contains(&BlockId(bi as u32))).collect();
        if cfg_preds.len() == 1 && f.blocks[cfg_preds[0]].term.successors().len() > 1 {
            self.emit_phi_copies(BlockId(cfg_preds[0] as u32), BlockId(bi as u32));
        }
    }

    pub(super) fn emit_phi_copies(&mut self, pred: BlockId, succ: BlockId) {
        enum Src {
            Reg(Reg),
            Op(Operand),
        }
        let f = self.f;
        let mut copies: Vec<(Reg, Operand, Type)> = Vec::new();
        for &id in &f.blocks[succ.idx()].insts {
            let InstKind::Phi { ty, incoming } = &f.insts[id.idx()].kind else { break };
            if let Some((_, op)) = incoming.iter().find(|(b, _)| *b == pred) {
                let d = f.insts[id.idx()].dst.unwrap();
                let dreg = self.value_reg(d);
                let op = self.resolve(*op);
                copies.push((dreg, op, *ty));
            }
        }
        // A source that is also the destination of another copy in this group is
        // saved to a temporary before anything is overwritten.
        let dsts: Vec<Reg> = copies.iter().map(|c| c.0).collect();
        let mut staged: Vec<(Reg, Src, Type)> = Vec::new();
        for (d, op, ty) in copies {
            let src_reg = match op {
                Operand::Value(v) => Some(self.value_reg(v)),
                _ => None,
            };
            if src_reg == Some(d) {
                continue; // no-op copy
            }
            match src_reg {
                Some(s) if dsts.contains(&s) => {
                    let tmp = self.vr(ty);
                    self.mov_rr(ty, tmp, s);
                    staged.push((d, Src::Reg(tmp), ty));
                }
                _ => staged.push((d, Src::Op(op), ty)),
            }
        }
        for (d, src, ty) in staged {
            match src {
                Src::Reg(r) => self.mov_rr(ty, d, r),
                Src::Op(op) => self.copy_operand(d, op, ty),
            }
        }
    }

    pub(super) fn copy_operand(&mut self, d: Reg, op: Operand, ty: Type) {
        match self.resolve(op) {
            Operand::Value(v) => {
                let s = self.value_reg(v);
                self.mov_rr(ty, d, s);
            }
            Operand::Int(c, _) => {
                let sz = match Sz::of(ty) {
                    Sz::B | Sz::W => Sz::L,
                    s => s,
                };
                self.emit(Op::Mov, sz, MOp::Reg(d), MOp::Imm(c));
            }
            Operand::Float(bits, t) => self.load_float_const(d, bits, t),
            Operand::Global(s) => {
                self.emit(Op::Lea, Sz::Q, MOp::Reg(d), MOp::Mem(Mem { base: Base::Sym(s), index: None, disp: 0 }))
            }
            Operand::Undef(_) => {}
        }
    }
}
