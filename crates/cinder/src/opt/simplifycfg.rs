//! Control-flow graph clean-up.
//!
//! * conditional branches on constants (or with identical targets) and
//!   switches on constants become plain jumps;
//! * unreachable blocks are deleted;
//! * a block that is the only successor of a block that jumps only to it is
//!   merged into that block;
//! * empty blocks that just forward to another block are bypassed;
//! * finally blocks are laid out in reverse post-order, visiting the first
//!   successor first so the likely fall-through edge is adjacent.

use super::sccp::remove_phi_edge;
use super::{apply_replacements, canon};
use crate::ir::cfg;
use crate::ir::*;
use std::collections::HashMap;

pub fn run(f: &mut Func) -> bool {
    let mut any = false;
    for _ in 0..64 {
        let mut changed = fold_terms(f);
        changed |= cfg::remove_unreachable(f);
        changed |= thread_returns(f);
        changed |= merge_blocks(f);
        changed |= thread_empty(f);
        changed |= cfg::remove_unreachable(f);
        f.sweep();
        if !changed {
            break;
        }
        any = true;
    }
    layout(f);
    any
}

/// Replace every edge to `from` in a terminator by an edge to `to`.
pub fn retarget(t: &mut Term, from: BlockId, to: BlockId) {
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

fn fold_terms(f: &mut Func) -> bool {
    let mut changed = false;
    for bi in 0..f.blocks.len() {
        let bid = BlockId(bi as u32);
        let new = match &f.blocks[bi].term {
            Term::CondBr { cond, then_bb, else_bb } => {
                if then_bb == else_bb {
                    Some(Term::Br(*then_bb))
                } else if let Operand::Int(c, _) = cond {
                    Some(Term::Br(if *c != 0 { *then_bb } else { *else_bb }))
                } else {
                    None
                }
            }
            Term::Switch { ty, val, cases, default } => {
                if let Operand::Int(c, _) = val {
                    let c = canon(*c, *ty);
                    let target = cases.iter().find(|(v, _)| canon(*v, *ty) == c).map(|(_, b)| *b).unwrap_or(*default);
                    Some(Term::Br(target))
                } else if cases.iter().all(|(_, b)| b == default) {
                    Some(Term::Br(*default))
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(t) = new {
            let new_succs = t.successors();
            for s in f.blocks[bi].term.successors() {
                if !new_succs.contains(&s) {
                    remove_phi_edge(f, s, bid);
                }
            }
            f.blocks[bi].term = t;
            changed = true;
        }
    }
    changed
}

/// A block that only merges values with phis and returns them
/// (`m: %r = phi ...; ret %r`) is copied into each predecessor that jumps to
/// it, which turns `return c ? a : f(x)` into a call directly followed by
/// `ret` (a tail call) and saves a jump on every path.
fn thread_returns(f: &mut Func) -> bool {
    let g = cfg::build(f);
    let mut changed = false;
    for mi in 1..f.blocks.len() {
        let m = BlockId(mi as u32);
        let Term::Ret(vals) = f.blocks[mi].term.clone() else { continue };
        let insts = f.blocks[mi].insts.clone();
        if insts.len() > 4 || !insts.iter().all(|&i| f.is_phi(i)) {
            continue;
        }
        let phi_dst: Vec<(ValueId, InstId)> = insts.iter().map(|&i| (f.insts[i.idx()].dst.unwrap(), i)).collect();
        for &p in &g.preds[mi] {
            if !matches!(f.blocks[p.idx()].term, Term::Br(t) if t == m) {
                continue;
            }
            let mut mapped: Vec<Operand> = Vec::with_capacity(vals.len());
            for v in &vals {
                let from_phi = match v {
                    Operand::Value(x) => phi_dst.iter().find(|(d, _)| d == x).map(|(_, i)| *i),
                    _ => None,
                };
                match from_phi {
                    Some(i) => {
                        let InstKind::Phi { incoming, .. } = &f.insts[i.idx()].kind else { unreachable!() };
                        match incoming.iter().find(|(q, _)| *q == p) {
                            Some((_, o)) => mapped.push(*o),
                            None => break,
                        }
                    }
                    None => mapped.push(*v),
                }
            }
            if mapped.len() != vals.len() {
                continue;
            }
            f.blocks[p.idx()].term = Term::Ret(mapped);
            remove_phi_edge(f, m, p);
            changed = true;
        }
    }
    changed
}

/// Merge `t` into `b` when `b` ends in `br t` and `b` is `t`'s only predecessor.
fn merge_blocks(f: &mut Func) -> bool {
    let mut preds = cfg::build(f).preds;
    let mut repl: HashMap<ValueId, Operand> = HashMap::new();
    let mut changed = false;
    for bi in 0..f.blocks.len() {
        let b = BlockId(bi as u32);
        while let Term::Br(t) = f.blocks[bi].term {
            if t == b || t.idx() == 0 || preds[t.idx()].len() != 1 || preds[t.idx()][0] != b {
                break;
            }
            let t_insts = std::mem::take(&mut f.blocks[t.idx()].insts);
            let mut moved = Vec::new();
            for id in t_insts {
                if let InstKind::Phi { incoming, .. } = &f.insts[id.idx()].kind {
                    let dst = f.insts[id.idx()].dst.unwrap();
                    let v = incoming
                        .iter()
                        .find(|(p, _)| *p == b)
                        .map(|(_, v)| *v)
                        .unwrap_or(Operand::Undef(f.values[dst.idx()].ty));
                    repl.insert(dst, v);
                    f.kill(id);
                } else {
                    moved.push(id);
                }
            }
            f.blocks[bi].insts.extend(moved);
            let term = std::mem::replace(&mut f.blocks[t.idx()].term, Term::Unreachable);
            let term_line = f.blocks[t.idx()].term_line;
            for s in term.successors() {
                rename_phi_pred(f, s, t, b);
                for p in preds[s.idx()].iter_mut() {
                    if *p == t {
                        *p = b;
                    }
                }
            }
            f.blocks[bi].term = term;
            f.blocks[bi].term_line = term_line;
            preds[t.idx()].clear();
            changed = true;
        }
    }
    apply_replacements(f, &repl);
    changed
}

pub fn rename_phi_pred(f: &mut Func, s: BlockId, from: BlockId, to: BlockId) {
    let ids: Vec<InstId> = f.blocks[s.idx()].insts.clone();
    for id in ids {
        if let InstKind::Phi { incoming, .. } = &mut f.insts[id.idx()].kind {
            for (p, _) in incoming.iter_mut() {
                if *p == from {
                    *p = to;
                }
            }
        } else {
            break;
        }
    }
}

/// Bypass blocks that contain nothing but `br t`.
fn thread_empty(f: &mut Func) -> bool {
    let mut changed = false;
    let mut g = cfg::build(f);
    for ei in 1..f.blocks.len() {
        let e = BlockId(ei as u32);
        if !f.blocks[ei].insts.is_empty() {
            continue;
        }
        let Term::Br(t) = f.blocks[ei].term else { continue };
        if t == e || t.idx() == 0 || g.preds[ei].is_empty() {
            continue;
        }
        let preds = g.preds[ei].clone();
        let t_phis: Vec<InstId> = f.blocks[t.idx()].insts.iter().copied().take_while(|&i| f.is_phi(i)).collect();
        let mut ok = true;
        for &p in &preds {
            if p == e {
                ok = false;
                break;
            }
            // `p` already reaches `t` directly: a phi cannot take two values from it
            if g.preds[t.idx()].contains(&p) {
                for &ph in &t_phis {
                    if let InstKind::Phi { incoming, .. } = &f.insts[ph.idx()].kind {
                        let ve = incoming.iter().find(|(q, _)| *q == e).map(|x| x.1);
                        let vp = incoming.iter().find(|(q, _)| *q == p).map(|x| x.1);
                        if ve != vp {
                            ok = false;
                        }
                    }
                }
            }
        }
        if !ok {
            continue;
        }
        for &p in &preds {
            retarget(&mut f.blocks[p.idx()].term, e, t);
        }
        for &ph in &t_phis {
            if let InstKind::Phi { incoming, .. } = &mut f.insts[ph.idx()].kind {
                let ve = incoming.iter().find(|(q, _)| *q == e).map(|x| x.1);
                incoming.retain(|(q, _)| *q != e);
                if let Some(ve) = ve {
                    for &p in &preds {
                        if !incoming.iter().any(|(q, _)| *q == p) {
                            incoming.push((p, ve));
                        }
                    }
                }
            }
        }
        changed = true;
        g = cfg::build(f);
    }
    changed
}

/// Reorder blocks in reverse post-order, visiting successors last-to-first so
/// the first successor (usually the "then" or loop-body edge) follows its predecessor.
fn layout(f: &mut Func) {
    let n = f.blocks.len();
    if n <= 2 {
        return;
    }
    let mut seen = vec![false; n];
    let mut post: Vec<BlockId> = Vec::with_capacity(n);
    let mut stack: Vec<(BlockId, Vec<BlockId>, usize)> = Vec::new();
    let succs_rev = |b: BlockId| {
        let mut s = f_succs(&f.blocks[b.idx()].term);
        s.reverse();
        s
    };
    seen[0] = true;
    stack.push((BlockId(0), succs_rev(BlockId(0)), 0));
    while let Some((b, ss, i)) = stack.pop() {
        if i < ss.len() {
            let s = ss[i];
            stack.push((b, ss, i + 1));
            if !seen[s.idx()] {
                seen[s.idx()] = true;
                let sr = succs_rev(s);
                stack.push((s, sr, 0));
            }
        } else {
            post.push(b);
        }
    }
    post.reverse();
    if post.len() != n || post.iter().enumerate().all(|(i, b)| b.idx() == i) {
        return; // unreachable blocks remain (cannot happen after clean-up) or already in order
    }
    let mut map: Vec<Option<BlockId>> = vec![None; n];
    for (i, b) in post.iter().enumerate() {
        map[b.idx()] = Some(BlockId(i as u32));
    }
    let mut old: Vec<Option<Block>> = std::mem::take(&mut f.blocks).into_iter().map(Some).collect();
    let mut new_blocks: Vec<Block> = Vec::with_capacity(n);
    for b in &post {
        let mut blk = old[b.idx()].take().unwrap();
        for &id in &blk.insts {
            if let InstKind::Phi { incoming, .. } = &mut f.insts[id.idx()].kind {
                for (p, _) in incoming.iter_mut() {
                    *p = map[p.idx()].unwrap();
                }
            }
        }
        remap(&mut blk.term, &map);
        new_blocks.push(blk);
    }
    f.blocks = new_blocks;
}

fn f_succs(t: &Term) -> Vec<BlockId> {
    let mut v = t.successors();
    // keep first occurrence order, drop duplicates
    let mut seen: Vec<BlockId> = Vec::new();
    v.retain(|b| {
        if seen.contains(b) {
            false
        } else {
            seen.push(*b);
            true
        }
    });
    v
}

fn remap(t: &mut Term, map: &[Option<BlockId>]) {
    let m = |b: &mut BlockId| *b = map[b.idx()].unwrap();
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
