//! Common subexpression elimination.
//!
//! *Pure instructions* are value-numbered over the dominator tree: an
//! instruction identical to one in a dominating position (operands of
//! commutative operations in canonical order) is replaced by it.
//!
//! *Loads* are forwarded within a basic block: a load of an address that was
//! just loaded or stored returns that value, until something that may write
//! the location intervenes (decided by the alias analysis in [`super::alias`]).

use super::alias;
use super::{apply_replacements, resolve};
use crate::ir::cfg;
use crate::ir::*;
use std::collections::{HashMap, HashSet};

#[derive(PartialEq, Eq, Hash, Clone)]
enum Key {
    Bin(BinOp, Type, Operand, Operand),
    Un(UnOp, Type, Operand),
    ICmp(IPred, Type, Operand, Operand),
    FCmp(FPred, Type, Operand, Operand),
    Cast(CastOp, Type, Type, Operand),
    PtrAdd(Operand, Operand),
    Select(Type, Operand, Operand, Operand),
}

fn okey(o: Operand) -> (u8, u64, u64) {
    match o {
        Operand::Value(v) => (0, v.0 as u64, 0),
        Operand::Int(c, t) => (1, c as u64, t as u64),
        Operand::Float(b, t) => (2, b, t as u64),
        Operand::Global(s) => (3, s.0 as u64, 0),
        Operand::Undef(t) => (4, t as u64, 0),
    }
}

fn key_of(k: &InstKind, r: &dyn Fn(Operand) -> Operand) -> Option<Key> {
    Some(match k {
        InstKind::Bin { op, ty, lhs, rhs } => {
            let (a, b) = (r(*lhs), r(*rhs));
            let (a, b) = if op.is_commutative() && okey(a) > okey(b) { (b, a) } else { (a, b) };
            Key::Bin(*op, *ty, a, b)
        }
        InstKind::Un { op, ty, val } => Key::Un(*op, *ty, r(*val)),
        InstKind::ICmp { pred, ty, lhs, rhs } => {
            let (mut p, mut a, mut b) = (*pred, r(*lhs), r(*rhs));
            if okey(a) > okey(b) {
                std::mem::swap(&mut a, &mut b);
                p = p.swapped();
            }
            Key::ICmp(p, *ty, a, b)
        }
        InstKind::FCmp { pred, ty, lhs, rhs } => Key::FCmp(*pred, *ty, r(*lhs), r(*rhs)),
        InstKind::Cast { op, from, to, val } => Key::Cast(*op, *from, *to, r(*val)),
        InstKind::PtrAdd { base, offset } => Key::PtrAdd(r(*base), r(*offset)),
        InstKind::Select { ty, cond, a, b } => Key::Select(*ty, r(*cond), r(*a), r(*b)),
        _ => return None,
    })
}

enum Step {
    Enter(BlockId),
    Exit(Vec<Key>),
}

pub fn run(f: &mut Func) -> bool {
    let g = cfg::build(f);
    let dom = cfg::dominators(f, &g);
    let local = alias::non_escaping_allocas(f);
    let mut repl: HashMap<ValueId, Operand> = HashMap::new();
    let mut table: HashMap<Key, Operand> = HashMap::new();
    let mut dead: Vec<InstId> = Vec::new();

    let mut stack = vec![Step::Enter(BlockId(0))];
    while let Some(step) = stack.pop() {
        match step {
            Step::Exit(keys) => {
                for k in keys {
                    table.remove(&k);
                }
            }
            Step::Enter(b) => {
                let mut added: Vec<Key> = Vec::new();
                process_block(f, b, &local, &mut repl, &mut table, &mut added, &mut dead);
                stack.push(Step::Exit(added));
                for &c in dom.children[b.idx()].iter().rev() {
                    stack.push(Step::Enter(c));
                }
            }
        }
    }
    if repl.is_empty() {
        return false;
    }
    for id in dead {
        f.kill(id);
    }
    apply_replacements(f, &repl);
    f.sweep();
    true
}

fn process_block(
    f: &Func,
    b: BlockId,
    local: &HashSet<ValueId>,
    repl: &mut HashMap<ValueId, Operand>,
    table: &mut HashMap<Key, Operand>,
    added: &mut Vec<Key>,
    dead: &mut Vec<InstId>,
) {
    // (address, type, value) of memory contents known at this point of the block
    let mut avail: Vec<(Operand, Type, Operand)> = Vec::new();
    for &id in &f.blocks[b.idx()].insts {
        let inst = &f.insts[id.idx()];
        match &inst.kind {
            k if k.is_pure() => {
                let key = {
                    let r = |o: Operand| resolve(repl, o);
                    key_of(k, &r)
                };
                let Some(key) = key else { continue };
                let dst = inst.dst.unwrap();
                if let Some(&prev) = table.get(&key) {
                    repl.insert(dst, prev);
                    dead.push(id);
                } else {
                    table.insert(key.clone(), Operand::Value(dst));
                    added.push(key);
                }
            }
            InstKind::Load { ty, ptr, volatile: false } => {
                let ptr = resolve(repl, *ptr);
                let dst = inst.dst.unwrap();
                if let Some(e) = avail.iter().find(|(p, t, _)| *p == ptr && *t == *ty) {
                    repl.insert(dst, e.2);
                    dead.push(id);
                } else {
                    avail.push((ptr, *ty, Operand::Value(dst)));
                    if avail.len() > 64 {
                        avail.remove(0);
                    }
                }
            }
            InstKind::Store { ty, val, ptr, volatile: false } => {
                let (ptr, val) = (resolve(repl, *ptr), resolve(repl, *val));
                avail.retain(|(p, t, _)| !alias::may_alias(f, *p, t.size() as u64, ptr, ty.size() as u64));
                avail.push((ptr, *ty, val));
                if avail.len() > 64 {
                    avail.remove(0);
                }
            }
            InstKind::MemCopy { dst, size, .. } | InstKind::MemSet { dst, size, .. } => {
                let dst = resolve(repl, *dst);
                avail.retain(|(p, t, _)| !alias::may_alias(f, *p, t.size() as u64, dst, *size));
            }
            InstKind::Call { .. } => {
                // a callee can only touch stack slots whose address escaped
                avail.retain(|(p, _, _)| alias::based_on_local(f, local, *p));
            }
            InstKind::Phi { .. } | InstKind::Alloca { .. } => {}
            _ => avail.clear(), // volatile accesses, traps, va intrinsics
        }
    }
}
