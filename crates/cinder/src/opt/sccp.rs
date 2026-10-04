//! Sparse conditional constant propagation (Wegman–Zadeck).
//!
//! Values move down the lattice `Top` (not yet known) → `Const` →
//! `Bottom` (varying). Only blocks reachable through *executable* edges are
//! evaluated, so a branch on a constant condition makes the dead arm vanish
//! even if it would otherwise feed a phi. Afterwards constants replace
//! their uses, constant branches become jumps, and dead blocks are dropped.

use super::{apply_replacements, canon, fold_icmp, fold_int_bin, unsigned};
use crate::ir::cfg;
use crate::ir::*;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Lat {
    Top,
    Const(Operand),
    Bottom,
}

fn meet(a: Lat, b: Lat) -> Lat {
    match (a, b) {
        (Lat::Top, x) | (x, Lat::Top) => x,
        (Lat::Const(x), Lat::Const(y)) if x == y => Lat::Const(x),
        _ => Lat::Bottom,
    }
}

#[derive(Clone, Copy)]
enum User {
    Inst(InstId),
    Term(BlockId),
}

struct Sccp<'a> {
    f: &'a Func,
    lat: Vec<Lat>,
    exec_block: Vec<bool>,
    exec_edge: HashSet<(BlockId, BlockId)>,
    users: Vec<Vec<User>>,
    block_of: Vec<Option<BlockId>>,
    work: Vec<User>,
}

pub fn run(f: &mut Func) -> bool {
    cfg::remove_unreachable(f);
    let nv = f.values.len();
    let mut users: Vec<Vec<User>> = vec![Vec::new(); nv];
    let mut block_of: Vec<Option<BlockId>> = vec![None; f.insts.len()];
    for (bi, b) in f.blocks.iter().enumerate() {
        let bid = BlockId(bi as u32);
        for &id in &b.insts {
            block_of[id.idx()] = Some(bid);
            f.insts[id.idx()].kind.for_each_operand(|o| {
                if let Operand::Value(v) = o {
                    users[v.idx()].push(User::Inst(id));
                }
            });
        }
        for o in b.term.operands() {
            if let Operand::Value(v) = o {
                users[v.idx()].push(User::Term(bid));
            }
        }
    }
    let (lat, exec_block, block_of) = {
        let mut s = Sccp {
            f: &*f,
            lat: vec![Lat::Top; nv],
            exec_block: vec![false; f.blocks.len()],
            exec_edge: HashSet::new(),
            users,
            block_of,
            work: Vec::new(),
        };
        for &pv in &f.param_values {
            s.lat[pv.idx()] = Lat::Bottom;
        }
        s.solve();
        (s.lat, s.exec_block, s.block_of)
    };
    apply(f, &lat, &exec_block, &block_of)
}

impl<'a> Sccp<'a> {
    fn solve(&mut self) {
        self.mark_block(BlockId(0));
        loop {
            while let Some(u) = self.work.pop() {
                match u {
                    User::Inst(id) => {
                        if let Some(b) = self.block_of[id.idx()] {
                            if self.exec_block[b.idx()] {
                                self.visit_inst(id);
                            }
                        }
                    }
                    User::Term(b) => {
                        if self.exec_block[b.idx()] {
                            self.visit_term(b);
                        }
                    }
                }
            }
            // Branches on a value that is still Top (e.g. undef) are resolved by
            // taking every successor, then propagation resumes.
            let mut progressed = false;
            for bi in 0..self.f.blocks.len() {
                if !self.exec_block[bi] {
                    continue;
                }
                let cond = match &self.f.blocks[bi].term {
                    Term::CondBr { cond, .. } => Some(*cond),
                    Term::Switch { val, .. } => Some(*val),
                    _ => None,
                };
                if let Some(c) = cond {
                    if self.val(c) == Lat::Top {
                        for s in self.f.blocks[bi].term.successors() {
                            progressed |= self.mark_edge(BlockId(bi as u32), s);
                        }
                    }
                }
            }
            if !progressed && self.work.is_empty() {
                break;
            }
        }
    }

    fn val(&self, o: Operand) -> Lat {
        match o {
            Operand::Int(..) | Operand::Float(..) => Lat::Const(o),
            Operand::Value(v) => self.lat[v.idx()],
            Operand::Global(_) => Lat::Bottom,
            Operand::Undef(_) => Lat::Top,
        }
    }

    fn mark_block(&mut self, b: BlockId) {
        if self.exec_block[b.idx()] {
            return;
        }
        self.exec_block[b.idx()] = true;
        for k in 0..self.f.blocks[b.idx()].insts.len() {
            let id = self.f.blocks[b.idx()].insts[k];
            self.visit_inst(id);
        }
        self.visit_term(b);
    }

    /// Returns true if the edge was newly marked.
    fn mark_edge(&mut self, from: BlockId, to: BlockId) -> bool {
        if !self.exec_edge.insert((from, to)) {
            return false;
        }
        if self.exec_block[to.idx()] {
            // a new way in: re-evaluate the phis
            for k in 0..self.f.blocks[to.idx()].insts.len() {
                let id = self.f.blocks[to.idx()].insts[k];
                if self.f.is_phi(id) {
                    self.visit_inst(id);
                } else {
                    break;
                }
            }
        } else {
            self.mark_block(to);
        }
        true
    }

    fn set(&mut self, v: ValueId, l: Lat) {
        let old = self.lat[v.idx()];
        let new = if old == Lat::Bottom { Lat::Bottom } else { l };
        // lattice values only move down
        let new = match (old, new) {
            (Lat::Const(a), Lat::Const(b)) if a != b => Lat::Bottom,
            (Lat::Const(_), Lat::Top) => old,
            _ => new,
        };
        if new != old {
            self.lat[v.idx()] = new;
            for u in self.users[v.idx()].clone() {
                self.work.push(u);
            }
        }
    }

    fn visit_term(&mut self, b: BlockId) {
        let term = self.f.blocks[b.idx()].term.clone();
        match term {
            Term::Br(t) => {
                self.mark_edge(b, t);
            }
            Term::CondBr { cond, then_bb, else_bb } => match self.val(cond) {
                Lat::Const(Operand::Int(c, _)) => {
                    self.mark_edge(b, if c != 0 { then_bb } else { else_bb });
                }
                Lat::Top => {}
                _ => {
                    self.mark_edge(b, then_bb);
                    self.mark_edge(b, else_bb);
                }
            },
            Term::Switch { val, cases, default, ty } => match self.val(val) {
                Lat::Const(Operand::Int(c, _)) => {
                    let c = canon(c, ty);
                    let t = cases.iter().find(|(v, _)| canon(*v, ty) == c).map(|(_, b)| *b).unwrap_or(default);
                    self.mark_edge(b, t);
                }
                Lat::Top => {}
                _ => {
                    let targets: Vec<BlockId> = cases.iter().map(|c| c.1).chain([default]).collect();
                    for s in targets {
                        self.mark_edge(b, s);
                    }
                }
            },
            _ => {}
        }
    }

    fn visit_inst(&mut self, id: InstId) {
        let f = self.f;
        let inst = &f.insts[id.idx()];
        let Some(dst) = inst.dst else { return };
        let b = self.block_of[id.idx()].unwrap();
        let new = match &inst.kind {
            InstKind::Phi { incoming, .. } => {
                let mut acc = Lat::Top;
                for (p, o) in incoming {
                    if self.exec_edge.contains(&(*p, b)) {
                        acc = meet(acc, self.val(*o));
                    }
                }
                acc
            }
            InstKind::Bin { op, ty, lhs, rhs } => {
                let (l, r) = (self.val(*lhs), self.val(*rhs));
                match (l, r) {
                    (Lat::Const(x), Lat::Const(y)) => match fold_bin(*op, *ty, x, y) {
                        Some(c) => Lat::Const(c),
                        None => Lat::Bottom,
                    },
                    (Lat::Bottom, _) | (_, Lat::Bottom) => Lat::Bottom,
                    _ => Lat::Top,
                }
            }
            InstKind::Un { op, ty, val } => match self.val(*val) {
                Lat::Const(x) => match fold_un(*op, *ty, x) {
                    Some(c) => Lat::Const(c),
                    None => Lat::Bottom,
                },
                other => other,
            },
            InstKind::ICmp { pred, ty, lhs, rhs } => {
                let (l, r) = (self.val(*lhs), self.val(*rhs));
                match (l, r) {
                    (Lat::Const(Operand::Int(x, _)), Lat::Const(Operand::Int(y, _))) => {
                        Lat::Const(Operand::Int(fold_icmp(*pred, *ty, x, y) as i64, Type::I32))
                    }
                    (Lat::Bottom, _) | (_, Lat::Bottom) => Lat::Bottom,
                    (Lat::Const(_), Lat::Const(_)) => Lat::Bottom,
                    _ => Lat::Top,
                }
            }
            InstKind::FCmp { pred, lhs, rhs, .. } => {
                let (l, r) = (self.val(*lhs), self.val(*rhs));
                match (l, r) {
                    (Lat::Const(Operand::Float(x, _)), Lat::Const(Operand::Float(y, _))) => {
                        let (x, y) = (f64::from_bits(x), f64::from_bits(y));
                        let r = match pred {
                            FPred::Oeq => x == y,
                            FPred::Une => x != y,
                            FPred::Olt => x < y,
                            FPred::Ole => x <= y,
                            FPred::Ogt => x > y,
                            FPred::Oge => x >= y,
                        };
                        Lat::Const(Operand::Int(r as i64, Type::I32))
                    }
                    (Lat::Bottom, _) | (_, Lat::Bottom) => Lat::Bottom,
                    (Lat::Const(_), Lat::Const(_)) => Lat::Bottom,
                    _ => Lat::Top,
                }
            }
            InstKind::Cast { op, from, to, val } => match self.val(*val) {
                Lat::Const(x) => match fold_cast(*op, *from, *to, x) {
                    Some(c) => Lat::Const(c),
                    None => Lat::Bottom,
                },
                other => other,
            },
            InstKind::Select { cond, a, b, .. } => match self.val(*cond) {
                Lat::Const(Operand::Int(c, _)) => self.val(if c != 0 { *a } else { *b }),
                Lat::Top => Lat::Top,
                _ => meet(self.val(*a), self.val(*b)),
            },
            InstKind::PtrAdd { base, offset } => match (self.val(*base), self.val(*offset)) {
                (Lat::Const(Operand::Int(x, _)), Lat::Const(Operand::Int(y, _))) => {
                    Lat::Const(Operand::Int(x.wrapping_add(y), Type::Ptr))
                }
                (Lat::Top, _) | (_, Lat::Top) => {
                    if matches!(self.val(*base), Lat::Bottom) || matches!(self.val(*offset), Lat::Bottom) {
                        Lat::Bottom
                    } else {
                        Lat::Top
                    }
                }
                _ => Lat::Bottom,
            },
            // loads, calls, allocas, intrinsics: not constant
            _ => Lat::Bottom,
        };
        self.set(dst, new);
        if let Some(d2) = inst.dst2 {
            self.set(d2, Lat::Bottom);
        }
    }
}

// ───────────────────────────── applying the result ─────────────────────────────

fn apply(f: &mut Func, lat: &[Lat], exec_block: &[bool], block_of: &[Option<BlockId>]) -> bool {
    let mut changed = false;
    let mut repl: HashMap<ValueId, Operand> = HashMap::new();
    for (v, l) in lat.iter().enumerate() {
        if let Lat::Const(c) = l {
            // only values defined by instructions in reachable blocks
            if let Some(id) = f.def_inst(ValueId(v as u32)) {
                if let Some(b) = block_of[id.idx()] {
                    if exec_block[b.idx()] && !f.dead_insts[id.idx()] {
                        let ty = f.values[v].ty;
                        let c = match c {
                            Operand::Int(x, _) => Operand::Int(*x, ty),
                            Operand::Float(x, _) => Operand::Float(*x, ty),
                            o => *o,
                        };
                        repl.insert(ValueId(v as u32), c);
                        f.dead_insts[id.idx()] = true;
                        changed = true;
                    }
                }
            }
        }
    }
    apply_replacements(f, &repl);
    // constant branches
    for (bi, &executed) in exec_block.iter().enumerate() {
        if !executed {
            continue;
        }
        let bid = BlockId(bi as u32);
        let new_term = match f.blocks[bi].term.clone() {
            Term::CondBr { cond, then_bb, else_bb } => {
                let c = match cond {
                    Operand::Int(c, _) => Some(c),
                    Operand::Value(v) => match lat[v.idx()] {
                        Lat::Const(Operand::Int(c, _)) => Some(c),
                        _ => None,
                    },
                    _ => None,
                };
                match c {
                    Some(c) => Some(Term::Br(if c != 0 { then_bb } else { else_bb })),
                    None if then_bb == else_bb => Some(Term::Br(then_bb)),
                    None => None,
                }
            }
            Term::Switch { ty, val, cases, default } => {
                let c = match val {
                    Operand::Int(c, _) => Some(c),
                    Operand::Value(v) => match lat[v.idx()] {
                        Lat::Const(Operand::Int(c, _)) => Some(c),
                        _ => None,
                    },
                    _ => None,
                };
                c.map(|c| {
                    let c = canon(c, ty);
                    Term::Br(cases.iter().find(|(v, _)| canon(*v, ty) == c).map(|(_, b)| *b).unwrap_or(default))
                })
            }
            _ => None,
        };
        if let Some(t) = new_term {
            let old_succs = f.blocks[bi].term.successors();
            let new_succs = t.successors();
            for s in old_succs {
                if !new_succs.contains(&s) {
                    remove_phi_edge(f, s, bid);
                }
            }
            f.blocks[bi].term = t;
            changed = true;
        }
    }
    if cfg::remove_unreachable(f) {
        changed = true;
    }
    f.sweep();
    changed
}

/// Drop the phi inputs that came from `pred` in block `s`.
pub fn remove_phi_edge(f: &mut Func, s: BlockId, pred: BlockId) {
    let ids: Vec<InstId> = f.blocks[s.idx()].insts.clone();
    for id in ids {
        if let InstKind::Phi { incoming, .. } = &mut f.insts[id.idx()].kind {
            incoming.retain(|(p, _)| *p != pred);
        } else {
            break;
        }
    }
}

fn int_of(o: Operand) -> Option<i64> {
    match o {
        Operand::Int(v, _) => Some(v),
        _ => None,
    }
}

fn float_of(o: Operand) -> Option<f64> {
    match o {
        Operand::Float(b, _) => Some(f64::from_bits(b)),
        _ => None,
    }
}

fn fold_bin(op: BinOp, ty: Type, x: Operand, y: Operand) -> Option<Operand> {
    if op.is_float() {
        let (a, b) = (float_of(x)?, float_of(y)?);
        let r = if ty == Type::F32 {
            let (a, b) = (a as f32, b as f32);
            (match op {
                BinOp::FAdd => a + b,
                BinOp::FSub => a - b,
                BinOp::FMul => a * b,
                _ => a / b,
            }) as f64
        } else {
            match op {
                BinOp::FAdd => a + b,
                BinOp::FSub => a - b,
                BinOp::FMul => a * b,
                _ => a / b,
            }
        };
        return Some(Operand::Float(r.to_bits(), ty));
    }
    let (a, b) = (int_of(x)?, int_of(y)?);
    fold_int_bin(op, ty, a, b).map(|v| Operand::Int(v, ty))
}

fn fold_un(op: UnOp, ty: Type, x: Operand) -> Option<Operand> {
    match op {
        UnOp::Neg => Some(Operand::Int(canon(int_of(x)?.wrapping_neg(), ty), ty)),
        UnOp::Not => Some(Operand::Int(canon(!int_of(x)?, ty), ty)),
        UnOp::FNeg => Some(Operand::Float((-float_of(x)?).to_bits(), ty)),
    }
}

fn fold_cast(op: CastOp, from: Type, to: Type, x: Operand) -> Option<Operand> {
    match op {
        CastOp::ZExt => Some(Operand::Int(canon(unsigned(int_of(x)?, from) as i64, to), to)),
        CastOp::SExt => Some(Operand::Int(int_of(x)?, to)),
        CastOp::Trunc => Some(Operand::Int(canon(int_of(x)?, to), to)),
        CastOp::PtrToInt | CastOp::IntToPtr => Some(Operand::Int(canon(int_of(x)?, to), to)),
        // converted straight to the destination width: going through f64 first would round twice
        CastOp::SIToFP => {
            let v = int_of(x)?;
            let r = if to == Type::F32 { v as f32 as f64 } else { v as f64 };
            Some(Operand::Float(r.to_bits(), to))
        }
        CastOp::UIToFP => {
            let v = unsigned(int_of(x)?, from);
            let r = if to == Type::F32 { v as f32 as f64 } else { v as f64 };
            Some(Operand::Float(r.to_bits(), to))
        }
        CastOp::FPToSI | CastOp::FPToUI if !matches!(to, Type::I32 | Type::I64) => None,
        CastOp::FPToSI => {
            let f = float_of(x)?;
            let bits = to.bits();
            let (lo, hi) = if bits >= 64 {
                (-9223372036854775808.0, 9223372036854775808.0)
            } else {
                (-((1u64 << (bits - 1)) as f64), (1u64 << (bits - 1)) as f64)
            };
            if f.is_nan() || f < lo || f >= hi {
                return None; // the hardware result ("integer indefinite") is left to run time
            }
            Some(Operand::Int(canon(f.trunc() as i64, to), to))
        }
        CastOp::FPToUI => {
            let f = float_of(x)?;
            let hi = if to.bits() >= 64 { 18446744073709551616.0 } else { (1u64 << to.bits()) as f64 };
            if f.is_nan() || f <= -1.0 || f >= hi {
                return None;
            }
            Some(Operand::Int(canon(f.trunc() as u64 as i64, to), to))
        }
        CastOp::FPExt => Some(Operand::Float(float_of(x)?.to_bits(), to)),
        CastOp::FPTrunc => Some(Operand::float(float_of(x)?, to)),
    }
}
