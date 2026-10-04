//! Tail-call optimization.
//!
//! Two transformations, both only for functions whose stack frame is private
//! (no stack slot's address escapes, so nothing can observe the frame after
//! the call):
//!
//! * **self tail recursion** (`return f(a, b);` inside `f`) becomes a loop:
//!   the entry is split, every parameter becomes a phi at the new loop
//!   header, and the recursive call turns into a jump that feeds the phis;
//! * **sibling tail calls** (`return g(x);`) are marked `tail`; the backend
//!   emits them as `jmp` after restoring the frame. Only calls whose arguments
//!   all travel in registers qualify (the caller's incoming stack area is not
//!   large enough in general), and variadic callees are left alone.
//!
//! A call is in tail position when it is the last instruction of a block that
//! returns exactly the call's results.

use super::simplifycfg::rename_phi_pred;
use super::{alias, apply_replacements};
use crate::abi::{self, ArgDesc};
use crate::ir::cfg;
use crate::ir::*;
use std::collections::HashMap;

/// The call instruction of block `b` if it is in tail position.
fn tail_position(f: &Func, b: BlockId) -> Option<InstId> {
    let blk = &f.blocks[b.idx()];
    let Term::Ret(vals) = &blk.term else { return None };
    let &last = blk.insts.last()?;
    let inst = &f.insts[last.idx()];
    let InstKind::Call { rets, .. } = &inst.kind else { return None };
    if *rets != f.rets {
        return None;
    }
    let results: Vec<Operand> = [inst.dst, inst.dst2].into_iter().flatten().map(Operand::Value).collect();
    if results != *vals {
        return None;
    }
    Some(last)
}

/// No stack slot has its address observed outside plain loads and stores.
fn frame_is_private(f: &Func) -> bool {
    let total = f.blocks[0].insts.iter().filter(|id| matches!(f.insts[id.idx()].kind, InstKind::Alloca { .. })).count();
    alias::non_escaping_allocas(f).len() == total
}

fn self_sites(f: &Func) -> Vec<(BlockId, InstId)> {
    let mut sites = Vec::new();
    for bi in 0..f.blocks.len() {
        let b = BlockId(bi as u32);
        let Some(id) = tail_position(f, b) else { continue };
        let InstKind::Call { callee: Callee::Direct(s), args, variadic: false, .. } = &f.insts[id.idx()].kind else {
            continue;
        };
        let same_shape = args.len() == f.params.len()
            && args.iter().zip(&f.params).all(|(a, p)| {
                a.kind == ArgKind::Value && matches!(&p.kind, ParamKind::Value(t) if f.operand_ty(a.val) == *t)
            });
        if *s == f.sym && same_shape {
            sites.push((b, id));
        }
    }
    sites
}

/// Turn self tail recursion into loops; returns the symbols of the functions changed.
pub fn loops(m: &mut Module) -> Vec<SymId> {
    let mut changed = Vec::new();
    for f in &mut m.funcs {
        if convert_self_recursion(f) {
            changed.push(f.sym);
        }
    }
    changed
}

fn convert_self_recursion(f: &mut Func) -> bool {
    if f.variadic || f.blocks.is_empty() || f.params.iter().any(|p| matches!(p.kind, ParamKind::ByVal { .. })) {
        return false;
    }
    if self_sites(f).is_empty() || !frame_is_private(f) || !cfg::build(f).preds[0].is_empty() {
        return false;
    }
    let entry = BlockId(0);

    // split the entry: stack slots stay, everything else moves to the loop header
    let header = f.new_block("tail.loop");
    let all = std::mem::take(&mut f.blocks[0].insts);
    let (allocas, rest): (Vec<InstId>, Vec<InstId>) =
        all.into_iter().partition(|&id| matches!(f.insts[id.idx()].kind, InstKind::Alloca { .. }));
    f.blocks[0].insts = allocas;
    f.blocks[header.idx()].insts = rest;
    let term = std::mem::replace(&mut f.blocks[0].term, Term::Br(header));
    f.blocks[header.idx()].term_line = f.blocks[0].term_line;
    for s in term.successors() {
        rename_phi_pred(f, s, entry, header);
    }
    f.blocks[header.idx()].term = term;

    // every parameter becomes a phi at the header
    let params: Vec<ValueId> = f.param_values.clone();
    let mut map: HashMap<ValueId, Operand> = HashMap::new();
    let mut phis: Vec<InstId> = Vec::new();
    for (i, &pv) in params.iter().enumerate() {
        let (ty, name) = (f.values[pv.idx()].ty, f.values[pv.idx()].name);
        let line = f.line;
        let op = f.insert(header, i, InstKind::Phi { ty, incoming: Vec::new() }, Some(ty), name, line).unwrap();
        phis.push(f.def_inst(op.value().unwrap()).unwrap());
        map.insert(pv, op);
    }
    apply_replacements(f, &map);

    // feed the phis: parameters from the entry, call arguments from each site
    let sites = self_sites(f);
    for (i, &pid) in phis.iter().enumerate() {
        let mut inc: Vec<(BlockId, Operand)> = vec![(entry, Operand::Value(params[i]))];
        for &(b, cid) in &sites {
            let InstKind::Call { args, .. } = &f.insts[cid.idx()].kind else { unreachable!() };
            inc.push((b, args[i].val));
        }
        if let InstKind::Phi { incoming, .. } = &mut f.insts[pid.idx()].kind {
            *incoming = inc;
        }
    }
    for &(b, cid) in &sites {
        f.kill(cid);
        f.blocks[b.idx()].insts.pop();
        f.blocks[b.idx()].term = Term::Br(header);
    }
    f.sweep();
    true
}

/// Mark calls in tail position that the backend can emit as a jump.
pub fn mark_sibling_calls(m: &mut Module) {
    for f in &mut m.funcs {
        if f.variadic || f.blocks.is_empty() || !frame_is_private(f) {
            continue;
        }
        for bi in 0..f.blocks.len() {
            let Some(id) = tail_position(f, BlockId(bi as u32)) else { continue };
            let InstKind::Call { args, variadic, .. } = &f.insts[id.idx()].kind else { continue };
            if *variadic || args.iter().any(|a| a.kind != ArgKind::Value) {
                continue;
            }
            let descs: Vec<ArgDesc> =
                args.iter().map(|a| ArgDesc { ty: Some(f.operand_ty(a.val)), byval: None, group: a.group }).collect();
            if abi::assign(&descs).stack_size != 0 {
                continue;
            }
            if let InstKind::Call { tail, .. } = &mut f.insts[id.idx()].kind {
                *tail = true;
            }
        }
    }
}
