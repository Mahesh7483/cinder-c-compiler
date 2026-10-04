//! Loop-invariant code motion.
//!
//! Every natural loop gets a *preheader* (a block that only jumps to the
//! header and is the loop's sole entry). Instructions whose operands are all
//! defined outside the loop are then moved there, innermost loops first so
//! that code can climb out of several levels.
//!
//! Only instructions that cannot fault are speculated: arithmetic that never
//! traps (divisions only by a constant other than 0 and -1), comparisons,
//! casts and address arithmetic. A load is hoisted only when nothing in the
//! loop can write the location *and* it cannot fault (a stack slot or global
//! accessed within bounds), or it sits in the header, which runs whenever the
//! preheader does.

use super::alias;
use super::simplifycfg::retarget;
use crate::ir::cfg::{self, Loop};
use crate::ir::*;
use std::collections::HashSet;

pub fn run(m: &Module, f: &mut Func) -> bool {
    let mut changed = false;
    {
        let g = cfg::build(f);
        let dom = cfg::dominators(f, &g);
        let loops = cfg::find_loops(f, &g, &dom);
        if loops.is_empty() {
            return false;
        }
        for l in &loops {
            changed |= ensure_preheader(f, l);
        }
    }
    let g = cfg::build(f);
    let dom = cfg::dominators(f, &g);
    let loops = cfg::find_loops(f, &g, &dom);
    let local = alias::non_escaping_allocas(f);
    for l in &loops {
        changed |= hoist(m, f, l, &g, &dom, &local);
    }
    changed
}

fn ensure_preheader(f: &mut Func, l: &Loop) -> bool {
    let header = l.header;
    if header.idx() == 0 {
        return false;
    }
    let g = cfg::build(f);
    let outside: Vec<BlockId> = g.preds[header.idx()].iter().copied().filter(|p| !l.blocks.contains(p)).collect();
    if outside.is_empty() {
        return false;
    }
    if outside.len() == 1 && matches!(f.blocks[outside[0].idx()].term, Term::Br(_)) {
        return false;
    }
    let ph = f.new_block("loop.ph");
    f.blocks[ph.idx()].term = Term::Br(header);
    for &p in &outside {
        retarget(&mut f.blocks[p.idx()].term, header, ph);
    }
    let phis: Vec<InstId> = f.blocks[header.idx()].insts.iter().copied().take_while(|&i| f.is_phi(i)).collect();
    for id in phis {
        let InstKind::Phi { ty, incoming } = f.insts[id.idx()].kind.clone() else { continue };
        let name = f.insts[id.idx()].dst.and_then(|d| f.values[d.idx()].name);
        let outs: Vec<(BlockId, Operand)> = incoming.iter().filter(|(p, _)| outside.contains(p)).copied().collect();
        let first = outs.first().map(|x| x.1);
        let entering = if outs.iter().all(|(_, v)| Some(*v) == first) {
            first.unwrap_or(Operand::Undef(ty))
        } else {
            f.insert(ph, 0, InstKind::Phi { ty, incoming: outs }, Some(ty), name, 0).unwrap()
        };
        if let InstKind::Phi { incoming, .. } = &mut f.insts[id.idx()].kind {
            incoming.retain(|(p, _)| !outside.contains(p));
            incoming.push((ph, entering));
        }
    }
    true
}

fn hoist(m: &Module, f: &mut Func, l: &Loop, g: &cfg::Cfg, dom: &cfg::DomTree, local: &HashSet<ValueId>) -> bool {
    let header = l.header;
    let mut in_loop = vec![false; f.blocks.len()];
    for b in &l.blocks {
        in_loop[b.idx()] = true;
    }
    let outside: Vec<BlockId> = g.preds[header.idx()].iter().copied().filter(|p| !in_loop[p.idx()]).collect();
    if outside.len() != 1 {
        return false;
    }
    let ph = outside[0];
    if !matches!(f.blocks[ph.idx()].term, Term::Br(_)) || in_loop[ph.idx()] {
        return false;
    }

    // values defined inside the loop, and what the loop does to memory
    let mut loop_def = vec![false; f.values.len()];
    let mut writes: Vec<(Operand, u64)> = Vec::new();
    let mut has_call = false;
    let mut clobbers_all = false;
    for b in &l.blocks {
        for &id in &f.blocks[b.idx()].insts {
            let inst = &f.insts[id.idx()];
            for d in [inst.dst, inst.dst2].into_iter().flatten() {
                loop_def[d.idx()] = true;
            }
            match &inst.kind {
                InstKind::Store { ty, ptr, volatile, .. } => {
                    if *volatile {
                        clobbers_all = true;
                    }
                    writes.push((*ptr, ty.size() as u64));
                }
                InstKind::MemCopy { dst, size, .. } | InstKind::MemSet { dst, size, .. } => writes.push((*dst, *size)),
                InstKind::Call { .. } => has_call = true,
                _ => {}
            }
        }
    }

    let mut moved = false;
    for &b in &dom.order {
        if !in_loop[b.idx()] {
            continue;
        }
        let ids: Vec<InstId> = f.blocks[b.idx()].insts.clone();
        for id in ids {
            let inst = &f.insts[id.idx()];
            let invariant = |o: Operand| match o {
                Operand::Value(v) => !loop_def[v.idx()],
                _ => true,
            };
            let ok = match &inst.kind {
                InstKind::Bin { op, lhs, rhs, .. } => {
                    let div = matches!(op, BinOp::SDiv | BinOp::UDiv | BinOp::SRem | BinOp::URem);
                    invariant(*lhs)
                        && invariant(*rhs)
                        && (!div || matches!(rhs, Operand::Int(c, _) if *c != 0 && *c != -1))
                }
                InstKind::Un { val, .. } | InstKind::Cast { val, .. } => invariant(*val),
                InstKind::ICmp { lhs, rhs, .. } | InstKind::FCmp { lhs, rhs, .. } => invariant(*lhs) && invariant(*rhs),
                InstKind::PtrAdd { base, offset } => invariant(*base) && invariant(*offset),
                InstKind::Select { cond, a, b, .. } => invariant(*cond) && invariant(*a) && invariant(*b),
                InstKind::Load { ty, ptr, volatile: false } => {
                    invariant(*ptr)
                        && !clobbers_all
                        && (!has_call || alias::based_on_local(f, local, *ptr))
                        && !writes.iter().any(|(w, sz)| alias::may_alias(f, *ptr, ty.size() as u64, *w, *sz))
                        && (b == header || safe_to_speculate(m, f, *ptr, *ty))
                }
                _ => false,
            };
            if !ok {
                continue;
            }
            f.blocks[b.idx()].insts.retain(|&i| i != id);
            f.blocks[ph.idx()].insts.push(id);
            for d in [f.insts[id.idx()].dst, f.insts[id.idx()].dst2].into_iter().flatten() {
                loop_def[d.idx()] = false;
            }
            moved = true;
        }
    }
    moved
}

/// Can a load of `ty` at `ptr` never fault? True for in-bounds accesses to a
/// stack slot or a global with a known size.
fn safe_to_speculate(m: &Module, f: &Func, ptr: Operand, ty: Type) -> bool {
    let loc = alias::decompose(f, ptr);
    let Some(off) = loc.off else { return false };
    if off < 0 {
        return false;
    }
    let end = off as u64 + ty.size() as u64;
    match loc.base {
        Operand::Global(s) => matches!(&m.syms[s.idx()].body, SymBody::Data(Some(d)) if end <= d.size),
        Operand::Value(v) => match f.def_inst(v).map(|id| &f.insts[id.idx()].kind) {
            Some(InstKind::Alloca { size, .. }) => end <= *size as u64,
            _ => false,
        },
        _ => false,
    }
}
