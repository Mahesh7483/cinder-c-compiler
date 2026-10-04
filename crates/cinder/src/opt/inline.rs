//! Function inlining.
//!
//! Direct calls to small, non-recursive, non-variadic functions are replaced
//! by a copy of the callee's body. Functions are processed callees-first
//! (post-order of the call graph) so each body is already final when it is
//! copied. The copy follows these rules:
//!
//! * the call's block is split after the call; the callee's `ret`s become
//!   jumps to the continuation, merged with phis when there are several;
//! * the callee's stack slots move to the caller's entry block (they are
//!   static, so this does not change the frame size per call and lets
//!   `mem2reg` promote them);
//! * a by-value aggregate parameter gets a private copy (`memcpy` into a new
//!   slot), because the callee may modify its parameter;
//! * `tail` markers are cleared (the call site is no longer the callee's tail).
//!
//! Internal functions left without any reference are deleted afterwards.

use super::simplifycfg::rename_phi_pred;
use super::{apply_replacements, OptConfig};
use crate::ir::*;
use std::collections::{HashMap, HashSet};

const SMALL: usize = 40;
const SMALL_INLINE_KEYWORD: usize = 100;
const SINGLE_USE: usize = 400;
const CALLER_LIMIT: usize = 4000;

/// Inline across the module; returns the symbols of the functions that changed.
pub fn run(m: &mut Module, _cfg: &OptConfig) -> Vec<SymId> {
    let nf = m.funcs.len();
    let by_sym: HashMap<SymId, usize> = m.funcs.iter().enumerate().map(|(i, f)| (f.sym, i)).collect();
    let (callees, refs) = scan(m, &by_sym);
    let recursive: Vec<bool> = (0..nf).map(|i| reaches(&callees, i, i)).collect();

    // callees first
    let mut order: Vec<usize> = Vec::new();
    let mut seen = vec![false; nf];
    for i in 0..nf {
        postorder(i, &callees, &mut seen, &mut order);
    }

    let mut changed: Vec<SymId> = Vec::new();
    for &fi in &order {
        let n0 = m.funcs[fi].blocks.len();
        let mut conts: HashSet<BlockId> = HashSet::new();
        let mut repl: HashMap<ValueId, Operand> = HashMap::new();
        let mut did = false;
        let mut bi = 0;
        while bi < m.funcs[fi].blocks.len() {
            if bi >= n0 && !conts.contains(&BlockId(bi as u32)) {
                bi += 1;
                continue;
            }
            let mut pos = 0;
            while pos < m.funcs[fi].blocks[bi].insts.len() {
                let id = m.funcs[fi].blocks[bi].insts[pos];
                let caller = &m.funcs[fi];
                if let InstKind::Call { callee: Callee::Direct(s), variadic: false, .. } = &caller.insts[id.idx()].kind
                {
                    if let Some(&ci) = by_sym.get(s) {
                        if ci != fi
                            && !recursive[ci]
                            && caller.num_insts() < CALLER_LIMIT
                            && profitable(&m.funcs[ci], refs.get(s).copied().unwrap_or(0))
                            && compatible(caller, id, &m.funcs[ci])
                        {
                            let callee = m.funcs[ci].clone();
                            inline_site(&mut m.funcs[fi], BlockId(bi as u32), pos, &callee, &mut repl, &mut conts);
                            did = true;
                            break; // the rest of the block now lives in a continuation block
                        }
                    }
                }
                pos += 1;
            }
            bi += 1;
        }
        if did {
            let f = &mut m.funcs[fi];
            apply_replacements(f, &repl);
            f.sweep();
            changed.push(f.sym);
        }
    }
    remove_dead_functions(m);
    changed
}

/// Direct-call graph and a reference count per symbol (calls count 1, address uses 2).
fn scan(m: &Module, by_sym: &HashMap<SymId, usize>) -> (Vec<Vec<usize>>, HashMap<SymId, u32>) {
    let mut callees: Vec<Vec<usize>> = vec![Vec::new(); m.funcs.len()];
    let mut refs: HashMap<SymId, u32> = HashMap::new();
    for (i, f) in m.funcs.iter().enumerate() {
        for b in &f.blocks {
            for &id in &b.insts {
                let k = &f.insts[id.idx()].kind;
                if let InstKind::Call { callee: Callee::Direct(s), .. } = k {
                    *refs.entry(*s).or_default() += 1;
                    if let Some(&j) = by_sym.get(s) {
                        if !callees[i].contains(&j) {
                            callees[i].push(j);
                        }
                    }
                }
                k.for_each_operand(|o| {
                    if let Operand::Global(s) = o {
                        *refs.entry(s).or_default() += 2;
                    }
                });
            }
        }
    }
    for s in &m.syms {
        if let SymBody::Data(Some(d)) = &s.body {
            for it in &d.items {
                if let DataItem::Addr { sym, .. } = it {
                    *refs.entry(*sym).or_default() += 2;
                }
            }
        }
    }
    (callees, refs)
}

fn reaches(g: &[Vec<usize>], from: usize, target: usize) -> bool {
    let mut seen = vec![false; g.len()];
    let mut stack: Vec<usize> = g[from].clone();
    while let Some(n) = stack.pop() {
        if n == target {
            return true;
        }
        if !seen[n] {
            seen[n] = true;
            stack.extend(g[n].iter().copied());
        }
    }
    false
}

fn postorder(i: usize, g: &[Vec<usize>], seen: &mut Vec<bool>, out: &mut Vec<usize>) {
    if seen[i] {
        return;
    }
    seen[i] = true;
    for &j in &g[i] {
        postorder(j, g, seen, out);
    }
    out.push(i);
}

/// Is the callee worth copying, and is its body one we know how to copy?
fn profitable(g: &Func, refs: u32) -> bool {
    if g.variadic || g.blocks.is_empty() {
        return false;
    }
    let size = g.num_insts() + g.blocks.len();
    for b in &g.blocks {
        for &id in &b.insts {
            if matches!(
                g.insts[id.idx()].kind,
                InstKind::VaRegSave
                    | InstKind::VaStackArgs
                    | InstKind::DynAlloca { .. }
                    | InstKind::StackSave
                    | InstKind::StackRestore { .. }
            ) {
                return false;
            }
        }
    }
    let limit = if g.linkage == Linkage::Internal && refs == 1 {
        SINGLE_USE
    } else if g.is_inline {
        SMALL_INLINE_KEYWORD
    } else {
        SMALL
    };
    size <= limit
}

/// Do the call's arguments and results line up exactly with the callee's signature?
fn compatible(f: &Func, call: InstId, g: &Func) -> bool {
    let InstKind::Call { args, rets, .. } = &f.insts[call.idx()].kind else { return false };
    if args.len() != g.params.len() || *rets != g.rets {
        return false;
    }
    args.iter().zip(&g.params).all(|(a, p)| match (&a.kind, &p.kind) {
        (ArgKind::Value, ParamKind::Value(t)) => f.operand_ty(a.val) == *t,
        (ArgKind::ByVal { size: s1, align: a1 }, ParamKind::ByVal { size: s2, align: a2 }) => s1 == s2 && a1 == a2,
        _ => false,
    })
}

fn inline_site(
    f: &mut Func,
    b: BlockId,
    pos: usize,
    g: &Func,
    repl: &mut HashMap<ValueId, Operand>,
    conts: &mut HashSet<BlockId>,
) {
    let call_id = f.blocks[b.idx()].insts[pos];
    let call = f.insts[call_id.idx()].clone();
    let InstKind::Call { args, .. } = &call.kind else { unreachable!() };
    let line = call.line;

    // split the block after the call
    let cont = f.new_block("inline.cont");
    conts.insert(cont);
    let tail = f.blocks[b.idx()].insts.split_off(pos + 1);
    f.blocks[b.idx()].insts.pop();
    f.kill(call_id);
    f.blocks[cont.idx()].insts = tail;
    f.blocks[cont.idx()].term = std::mem::replace(&mut f.blocks[b.idx()].term, Term::None);
    f.blocks[cont.idx()].term_line = f.blocks[b.idx()].term_line;
    for s in f.blocks[cont.idx()].term.successors() {
        rename_phi_pred(f, s, b, cont);
    }

    // parameters
    let mut vmap: Vec<Option<Operand>> = vec![None; g.values.len()];
    for (i, pv) in g.param_values.iter().enumerate() {
        match &g.params[i].kind {
            ParamKind::Value(_) => vmap[pv.idx()] = Some(args[i].val),
            ParamKind::ByVal { size, align } => {
                let slot = f
                    .insert(BlockId(0), 0, InstKind::Alloca { size: *size, align: *align }, Some(Type::Ptr), None, line)
                    .unwrap();
                f.push(
                    b,
                    InstKind::MemCopy { dst: slot, src: args[i].val, size: *size as u64, align: *align },
                    None,
                    None,
                    line,
                );
                vmap[pv.idx()] = Some(slot);
            }
        }
    }

    // pass 1: allocate an instruction (and result values) for every callee instruction
    let bmap: Vec<BlockId> = g.blocks.iter().map(|cb| f.new_block(&cb.name)).collect();
    let mut imap: HashMap<InstId, InstId> = HashMap::new();
    for cb in &g.blocks {
        for &cid in &cb.insts {
            let ci = &g.insts[cid.idx()];
            let nid = InstId(f.insts.len() as u32);
            f.insts.push(Inst { kind: ci.kind.clone(), dst: None, dst2: None, line: ci.line });
            f.dead_insts.push(false);
            let mut dsts = [None, None];
            for (k, d) in [ci.dst, ci.dst2].into_iter().enumerate() {
                if let Some(d) = d {
                    let data = &g.values[d.idx()];
                    let nv = f.new_value(data.ty, data.name, ValueDef::Inst(nid));
                    vmap[d.idx()] = Some(Operand::Value(nv));
                    dsts[k] = Some(nv);
                }
            }
            f.insts[nid.idx()].dst = dsts[0];
            f.insts[nid.idx()].dst2 = dsts[1];
            imap.insert(cid, nid);
        }
    }

    // pass 2: rewrite operands, place instructions, convert terminators
    let map_op = |o: Operand| -> Operand {
        match o {
            Operand::Value(v) => vmap[v.idx()].unwrap_or(Operand::Undef(g.values[v.idx()].ty)),
            other => other,
        }
    };
    let mut rets: Vec<(BlockId, Vec<Operand>)> = Vec::new();
    let mut entry_allocas: Vec<InstId> = Vec::new();
    for (ci, cb) in g.blocks.iter().enumerate() {
        let nb = bmap[ci];
        for &cid in &cb.insts {
            let nid = imap[&cid];
            let mut kind = std::mem::replace(&mut f.insts[nid.idx()].kind, InstKind::Trap);
            kind.for_each_operand_mut(|o| *o = map_op(*o));
            match &mut kind {
                InstKind::Phi { incoming, .. } => {
                    for (p, _) in incoming.iter_mut() {
                        *p = bmap[p.idx()];
                    }
                }
                InstKind::Call { tail, .. } => *tail = false,
                _ => {}
            }
            let is_alloca = matches!(kind, InstKind::Alloca { .. });
            f.insts[nid.idx()].kind = kind;
            if is_alloca {
                entry_allocas.push(nid);
            } else {
                f.blocks[nb.idx()].insts.push(nid);
            }
        }
        let mut term = cb.term.clone();
        match &mut term {
            Term::Br(t) => *t = bmap[t.idx()],
            Term::CondBr { cond, then_bb, else_bb } => {
                *cond = map_op(*cond);
                *then_bb = bmap[then_bb.idx()];
                *else_bb = bmap[else_bb.idx()];
            }
            Term::Switch { val, cases, default, .. } => {
                *val = map_op(*val);
                for (_, t) in cases.iter_mut() {
                    *t = bmap[t.idx()];
                }
                *default = bmap[default.idx()];
            }
            Term::Ret(vals) => {
                let vals: Vec<Operand> = vals.iter().map(|v| map_op(*v)).collect();
                rets.push((nb, vals));
                term = Term::Br(cont);
            }
            Term::Unreachable | Term::None => {}
        }
        f.blocks[nb.idx()].term = term;
        f.blocks[nb.idx()].term_line = cb.term_line;
    }
    // the callee's slots go to the front of the caller's entry block
    for (k, id) in entry_allocas.into_iter().enumerate() {
        f.blocks[0].insts.insert(k, id);
    }
    f.blocks[b.idx()].term = Term::Br(bmap[0]);
    f.blocks[b.idx()].term_line = line;

    // results
    for (j, d) in [call.dst, call.dst2].into_iter().enumerate() {
        let Some(d) = d else { continue };
        let ty = f.values[d.idx()].ty;
        let val = match rets.len() {
            0 => Operand::Undef(ty),
            1 => rets[0].1[j],
            _ => {
                let incoming: Vec<(BlockId, Operand)> = rets.iter().map(|(blk, vals)| (*blk, vals[j])).collect();
                f.insert(cont, 0, InstKind::Phi { ty, incoming }, Some(ty), f.values[d.idx()].name, line).unwrap()
            }
        };
        repl.insert(d, val);
    }
}

/// Drop internal functions that nothing refers to any more.
fn remove_dead_functions(m: &mut Module) {
    let by_sym: HashMap<SymId, usize> = m.funcs.iter().enumerate().map(|(i, f)| (f.sym, i)).collect();
    let mut live: HashSet<SymId> = HashSet::new();
    let mut work: Vec<SymId> = Vec::new();
    for f in &m.funcs {
        if f.linkage == Linkage::External && live.insert(f.sym) {
            work.push(f.sym);
        }
    }
    for s in &m.syms {
        if let SymBody::Data(Some(d)) = &s.body {
            for it in &d.items {
                if let DataItem::Addr { sym, .. } = it {
                    if live.insert(*sym) {
                        work.push(*sym);
                    }
                }
            }
        }
    }
    while let Some(s) = work.pop() {
        let Some(&fi) = by_sym.get(&s) else { continue };
        let f = &m.funcs[fi];
        for b in &f.blocks {
            for &id in &b.insts {
                let k = &f.insts[id.idx()].kind;
                if let InstKind::Call { callee: Callee::Direct(t), .. } = k {
                    if live.insert(*t) {
                        work.push(*t);
                    }
                }
                k.for_each_operand(|o| {
                    if let Operand::Global(t) = o {
                        if live.insert(t) {
                            work.push(t);
                        }
                    }
                });
            }
        }
    }
    m.funcs.retain(|f| live.contains(&f.sym));
}
