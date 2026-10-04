//! Promote scalar stack slots to SSA values (Cytron et al.): place phis at
//! the iterated dominance frontier of the blocks that store to a slot, then
//! rename loads/stores along the dominator tree.
//!
//! A slot is promotable when its address is only ever used as the pointer
//! operand of non-volatile loads and stores, all with one scalar type. Reads
//! before any store yield `undef`.

use super::{apply_replacements, resolve};
use crate::ir::cfg;
use crate::ir::*;
use std::collections::{HashMap, HashSet};

struct Cand {
    alloca: InstId,
    ty: Option<Type>,
    ok: bool,
    store_blocks: Vec<BlockId>,
    /// Some load can see the value from a previous block (needs phis).
    global: bool,
}

pub fn run(f: &mut Func) -> bool {
    cfg::remove_unreachable(f);
    // candidates: allocas in the entry block
    let mut cands: HashMap<ValueId, Cand> = HashMap::new();
    for &id in &f.blocks[0].insts {
        if let InstKind::Alloca { .. } = f.insts[id.idx()].kind {
            let v = f.insts[id.idx()].dst.unwrap();
            cands.insert(v, Cand { alloca: id, ty: None, ok: true, store_blocks: Vec::new(), global: false });
        }
    }
    if cands.is_empty() {
        return false;
    }
    let alloca_size = |f: &Func, id: InstId| match f.insts[id.idx()].kind {
        InstKind::Alloca { size, .. } => size,
        _ => 0,
    };

    // classify every use
    for (bi, b) in f.blocks.iter().enumerate() {
        let bid = BlockId(bi as u32);
        let mut stored_here: HashSet<ValueId> = HashSet::new();
        for &id in &b.insts {
            let inst = &f.insts[id.idx()];
            match &inst.kind {
                InstKind::Load { ty, ptr: Operand::Value(p), volatile } if cands.contains_key(p) => {
                    let c = cands.get_mut(p).unwrap();
                    if *volatile || c.ty.is_some_and(|t| t != *ty) || ty.size() > alloca_size_of(f, c.alloca) {
                        c.ok = false;
                    }
                    c.ty = c.ty.or(Some(*ty));
                    if !stored_here.contains(p) {
                        c.global = true;
                    }
                }
                InstKind::Store { ty, val, ptr: Operand::Value(p), volatile } if cands.contains_key(p) => {
                    if let Operand::Value(vv) = val {
                        if let Some(c) = cands.get_mut(vv) {
                            c.ok = false; // the address escapes into memory
                        }
                    }
                    let c = cands.get_mut(p).unwrap();
                    if *volatile
                        || c.ty.is_some_and(|t| t != *ty)
                        || *val == Operand::Value(*p)
                        || ty.size() > alloca_size_of(f, c.alloca)
                    {
                        c.ok = false;
                    }
                    c.ty = c.ty.or(Some(*ty));
                    if !c.store_blocks.contains(&bid) {
                        c.store_blocks.push(bid);
                    }
                    stored_here.insert(*p);
                }
                other => {
                    other.for_each_operand(|o| {
                        if let Operand::Value(v) = o {
                            if let Some(c) = cands.get_mut(&v) {
                                c.ok = false;
                            }
                        }
                    });
                }
            }
        }
        for o in b.term.operands() {
            if let Operand::Value(v) = o {
                if let Some(c) = cands.get_mut(&v) {
                    c.ok = false;
                }
            }
        }
    }
    let _ = alloca_size;
    let promotable: Vec<ValueId> = {
        let mut v: Vec<ValueId> = cands.iter().filter(|(_, c)| c.ok && c.ty.is_some()).map(|(k, _)| *k).collect();
        v.sort();
        v
    };
    // allocas that are never loaded or stored are simply dead (left to dce)
    if promotable.is_empty() {
        return false;
    }

    let g = cfg::build(f);
    let dom = cfg::dominators(f, &g);
    let df = dom.frontiers(&g);

    // phi placement
    let mut phi_of: HashMap<(BlockId, ValueId), InstId> = HashMap::new();
    for &a in &promotable {
        let c = &cands[&a];
        if !c.global {
            continue;
        }
        let ty = c.ty.unwrap();
        let name = f.values[a.idx()].name;
        let mut work: Vec<BlockId> = c.store_blocks.clone();
        let mut has_phi: HashSet<BlockId> = HashSet::new();
        let mut queued: HashSet<BlockId> = work.iter().copied().collect();
        while let Some(b) = work.pop() {
            for &d in &df[b.idx()] {
                if has_phi.insert(d) {
                    let inc = Vec::new();
                    let line = 0;
                    let op = f.insert(d, 0, InstKind::Phi { ty, incoming: inc }, Some(ty), name, line).unwrap();
                    let id = f.def_inst(op.value().unwrap()).unwrap();
                    phi_of.insert((d, a), id);
                    if queued.insert(d) {
                        work.push(d);
                    }
                }
            }
        }
    }

    // rename along the dominator tree
    let mut repl: HashMap<ValueId, Operand> = HashMap::new();
    let mut stacks: HashMap<ValueId, Vec<Operand>> = promotable.iter().map(|a| (*a, Vec::new())).collect();
    let mut dead: Vec<InstId> = Vec::new();
    struct Frame {
        block: BlockId,
        child: usize,
        pushed: Vec<ValueId>,
    }
    let cur = |stacks: &HashMap<ValueId, Vec<Operand>>, a: ValueId, ty: Type| {
        stacks[&a].last().copied().unwrap_or(Operand::Undef(ty))
    };
    let mut frames: Vec<Frame> = Vec::new();
    // enter a block: process phis, loads, stores, then fill successor phis
    macro_rules! enter {
        ($b:expr) => {{
            let b: BlockId = $b;
            let mut pushed: Vec<ValueId> = Vec::new();
            for &a in &promotable {
                if let Some(&pid) = phi_of.get(&(b, a)) {
                    let pv = f.insts[pid.idx()].dst.unwrap();
                    stacks.get_mut(&a).unwrap().push(Operand::Value(pv));
                    pushed.push(a);
                }
            }
            let ids: Vec<InstId> = f.blocks[b.idx()].insts.clone();
            for id in ids {
                let kind = f.insts[id.idx()].kind.clone();
                match kind {
                    InstKind::Load { ty, ptr: Operand::Value(p), .. } if stacks.contains_key(&p) => {
                        let v = cur(&stacks, p, ty);
                        let dst = f.insts[id.idx()].dst.unwrap();
                        repl.insert(dst, resolve(&repl, v));
                        dead.push(id);
                    }
                    InstKind::Store { val, ptr: Operand::Value(p), .. } if stacks.contains_key(&p) => {
                        let v = resolve(&repl, val);
                        stacks.get_mut(&p).unwrap().push(v);
                        pushed.push(p);
                        dead.push(id);
                    }
                    _ => {}
                }
            }
            // feed the phis of successors
            for s in f.blocks[b.idx()].term.successors() {
                for &a in &promotable {
                    if let Some(&pid) = phi_of.get(&(s, a)) {
                        let ty = cands[&a].ty.unwrap();
                        let v = cur(&stacks, a, ty);
                        if let InstKind::Phi { incoming, .. } = &mut f.insts[pid.idx()].kind {
                            if !incoming.iter().any(|(pb, _)| *pb == b) {
                                incoming.push((b, v));
                            }
                        }
                    }
                }
            }
            frames.push(Frame { block: b, child: 0, pushed });
        }};
    }
    enter!(BlockId(0));
    while let Some(top) = frames.last_mut() {
        let children = &dom.children[top.block.idx()];
        if top.child < children.len() {
            let c = children[top.child];
            top.child += 1;
            enter!(c);
        } else {
            let fr = frames.pop().unwrap();
            for a in fr.pushed.iter().rev() {
                stacks.get_mut(a).unwrap().pop();
            }
        }
    }

    for id in dead {
        f.kill(id);
    }
    for a in &promotable {
        f.kill(cands[a].alloca);
    }
    // phi inputs recorded before later replacements: resolve them too
    apply_replacements(f, &repl);
    f.sweep();
    true
}

fn alloca_size_of(f: &Func, id: InstId) -> u32 {
    match f.insts[id.idx()].kind {
        InstKind::Alloca { size, .. } => size,
        _ => 0,
    }
}
