//! Dead code elimination.
//!
//! * mark-and-sweep from the instructions with side effects and the
//!   terminator operands (this also removes dead phi cycles);
//! * stack slots that are only ever stored to, together with those stores;
//! * block-local dead-store elimination (a store overwritten by a later store
//!   to the same address with nothing reading memory in between).

use super::alias;
use crate::ir::*;
use std::collections::HashMap;

pub fn run(f: &mut Func) -> bool {
    let mut changed = dead_stores(f);
    loop {
        let mut c = mark_sweep(f);
        c |= dead_allocas(f);
        if !c {
            break;
        }
        changed = true;
    }
    f.sweep();
    changed
}

fn mark_sweep(f: &mut Func) -> bool {
    let n = f.insts.len();
    let mut live = vec![false; n];
    let mut work: Vec<InstId> = Vec::new();
    let mark = |o: Operand, live: &mut Vec<bool>, work: &mut Vec<InstId>, f: &Func| {
        if let Operand::Value(v) = o {
            if let Some(d) = f.def_inst(v) {
                if !live[d.idx()] {
                    live[d.idx()] = true;
                    work.push(d);
                }
            }
        }
    };
    for b in &f.blocks {
        for &id in &b.insts {
            if f.dead_insts[id.idx()] {
                continue;
            }
            if f.insts[id.idx()].kind.has_side_effects() && !live[id.idx()] {
                live[id.idx()] = true;
                work.push(id);
            }
        }
        for o in b.term.operands() {
            mark(o, &mut live, &mut work, f);
        }
    }
    while let Some(id) = work.pop() {
        f.insts[id.idx()].kind.for_each_operand(|o| mark(o, &mut live, &mut work, f));
    }
    let mut changed = false;
    for b in &f.blocks {
        for &id in &b.insts {
            if !live[id.idx()] && !f.dead_insts[id.idx()] {
                f.dead_insts[id.idx()] = true;
                changed = true;
            }
        }
    }
    f.sweep();
    changed
}

/// Stack slots that are only ever *written* (plain stores, `memset`, or the
/// destination of `memcpy`, possibly through `ptradd`) and never read or
/// passed anywhere: remove the slot together with those writes.
fn dead_allocas(f: &mut Func) -> bool {
    let mut allocas: HashMap<ValueId, InstId> = HashMap::new();
    for &id in &f.blocks[0].insts {
        if let InstKind::Alloca { .. } = f.insts[id.idx()].kind {
            allocas.insert(f.insts[id.idx()].dst.unwrap(), id);
        }
    }
    if allocas.is_empty() {
        return false;
    }
    // value -> the slot it points into (the slot itself, or a ptradd chain from it)
    let mut root: HashMap<ValueId, ValueId> = allocas.keys().map(|a| (*a, *a)).collect();
    loop {
        let mut grew = false;
        for b in &f.blocks {
            for &id in &b.insts {
                if let InstKind::PtrAdd { base: Operand::Value(bv), .. } = &f.insts[id.idx()].kind {
                    if let Some(&r) = root.get(bv) {
                        if root.insert(f.insts[id.idx()].dst.unwrap(), r).is_none() {
                            grew = true;
                        }
                    }
                }
            }
        }
        if !grew {
            break;
        }
    }
    let slot_of = |o: Operand| -> Option<ValueId> {
        match o {
            Operand::Value(v) => root.get(&v).copied(),
            _ => None,
        }
    };
    let mut writes: HashMap<ValueId, Vec<InstId>> = HashMap::new();
    let mut escaped: std::collections::HashSet<ValueId> = std::collections::HashSet::new();
    for b in &f.blocks {
        for &id in &b.insts {
            match &f.insts[id.idx()].kind {
                InstKind::Store { val, ptr, volatile, .. } => {
                    if let Some(s) = slot_of(*val) {
                        escaped.insert(s);
                    }
                    if let Some(s) = slot_of(*ptr) {
                        if *volatile {
                            escaped.insert(s);
                        } else {
                            writes.entry(s).or_default().push(id);
                        }
                    }
                }
                InstKind::MemSet { dst, .. } => {
                    if let Some(s) = slot_of(*dst) {
                        writes.entry(s).or_default().push(id);
                    }
                }
                InstKind::MemCopy { dst, src, .. } => {
                    if let Some(s) = slot_of(*src) {
                        escaped.insert(s); // read from
                    }
                    if let Some(s) = slot_of(*dst) {
                        writes.entry(s).or_default().push(id);
                    }
                }
                // extending the chain is not an escape (a dead ptradd is swept later)
                InstKind::PtrAdd { offset, .. } => {
                    if let Some(s) = slot_of(*offset) {
                        escaped.insert(s);
                    }
                }
                other => other.for_each_operand(|o| {
                    if let Some(s) = slot_of(o) {
                        escaped.insert(s);
                    }
                }),
            }
        }
        for o in b.term.operands() {
            if let Some(s) = slot_of(o) {
                escaped.insert(s);
            }
        }
    }
    let mut changed = false;
    for (v, aid) in allocas {
        if escaped.contains(&v) {
            continue;
        }
        if let Some(ws) = writes.get(&v) {
            for &w in ws {
                f.kill(w);
            }
        }
        f.kill(aid);
        changed = true;
    }
    f.sweep();
    changed
}

/// Within a block, drop a store that a later store to the same address
/// completely overwrites before anything can observe it.
fn dead_stores(f: &mut Func) -> bool {
    let mut changed = false;
    for bi in 0..f.blocks.len() {
        // address -> (store, size) of the latest un-observed store
        let mut pending: Vec<(Operand, u32, InstId)> = Vec::new();
        let ids: Vec<InstId> = f.blocks[bi].insts.clone();
        for id in ids {
            match f.insts[id.idx()].kind.clone() {
                InstKind::Store { ty, ptr, volatile: false, .. } => {
                    let size = ty.size();
                    // an earlier store to the same address that this one covers is dead
                    if let Some(k) = pending.iter().position(|(p, s, _)| *p == ptr && *s <= size) {
                        let (_, _, old) = pending.remove(k);
                        f.kill(old);
                        changed = true;
                    }
                    // a store through an unknown pointer may be overwriting what a pending
                    // store wrote, but cannot make it observable: keep the entries
                    pending.push((ptr, size, id));
                }
                InstKind::Load { ty, ptr, volatile: false } => {
                    let sz = ty.size();
                    pending.retain(|(p, s, _)| !alias::may_alias(f, *p, *s as u64, ptr, sz as u64));
                }
                InstKind::MemSet { .. } | InstKind::Alloca { .. } => {}
                InstKind::MemCopy { src, size, .. } => {
                    pending.retain(|(p, s, _)| !alias::may_alias(f, *p, *s as u64, src, size));
                }
                k if k.is_pure() || matches!(k, InstKind::Phi { .. }) => {}
                _ => {
                    // calls, volatile accesses, traps...: everything may be observed
                    // (except stack slots whose address never escapes, for calls)
                    pending.clear();
                }
            }
        }
    }
    if changed {
        f.sweep();
    }
    changed
}
