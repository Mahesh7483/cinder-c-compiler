//! A small, conservative alias analysis shared by CSE, DSE and LICM.
//!
//! Pointers are decomposed into `base + constant offset` by looking through
//! `ptradd`. Two accesses cannot overlap when they hit different *objects*
//! (distinct allocas or globals) or disjoint ranges of the same object.
//! Everything else may alias.

use crate::ir::*;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Loc {
    pub base: Operand,
    /// `None` when the offset is not a compile-time constant.
    pub off: Option<i64>,
}

pub fn decompose(f: &Func, mut p: Operand) -> Loc {
    let mut off: Option<i64> = Some(0);
    for _ in 0..64 {
        let Operand::Value(v) = p else { break };
        let Some(id) = f.def_inst(v) else { break };
        match &f.insts[id.idx()].kind {
            InstKind::PtrAdd { base, offset } => {
                off = match (off, offset) {
                    (Some(o), Operand::Int(c, _)) => Some(o.wrapping_add(*c)),
                    _ => None,
                };
                p = *base;
            }
            _ => break,
        }
    }
    Loc { base: p, off }
}

/// Is this the address of a distinct, statically known object (a stack slot or a global)?
pub fn is_object(f: &Func, base: Operand) -> bool {
    match base {
        Operand::Global(_) => true,
        Operand::Value(v) => f.def_inst(v).is_some_and(|id| matches!(f.insts[id.idx()].kind, InstKind::Alloca { .. })),
        _ => false,
    }
}

/// Could an access of `psize` bytes at `p` overlap one of `qsize` bytes at `q`?
pub fn may_alias(f: &Func, p: Operand, psize: u64, q: Operand, qsize: u64) -> bool {
    if p == q {
        return true;
    }
    let (a, b) = (decompose(f, p), decompose(f, q));
    if a.base == b.base {
        return match (a.off, b.off) {
            (Some(x), Some(y)) => {
                let (x, y) = (x as i128, y as i128);
                !(x + psize as i128 <= y || y + qsize as i128 <= x)
            }
            _ => true,
        };
    }
    !(is_object(f, a.base) && is_object(f, b.base))
}

/// Stack slots whose address is never observed outside loads, stores and
/// `memcpy`/`memset` operands (so no callee or stored pointer can reach them).
pub fn non_escaping_allocas(f: &Func) -> HashSet<ValueId> {
    let mut slots: HashSet<ValueId> = HashSet::new();
    for &id in &f.blocks[0].insts {
        if let InstKind::Alloca { .. } = f.insts[id.idx()].kind {
            slots.insert(f.insts[id.idx()].dst.unwrap());
        }
    }
    if slots.is_empty() {
        return slots;
    }
    // derived[v] = the slot a ptradd chain value points into
    let mut derived: std::collections::HashMap<ValueId, ValueId> = slots.iter().map(|s| (*s, *s)).collect();
    // block order need not follow dominance, so iterate to a fixed point
    loop {
        let mut grew = false;
        for b in &f.blocks {
            for &id in &b.insts {
                if let InstKind::PtrAdd { base: Operand::Value(bv), .. } = &f.insts[id.idx()].kind {
                    let dst = f.insts[id.idx()].dst.unwrap();
                    if let Some(&root) = derived.get(bv) {
                        if derived.insert(dst, root).is_none() {
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
    let mut escaped: HashSet<ValueId> = HashSet::new();
    let esc = |o: Operand, escaped: &mut HashSet<ValueId>| {
        if let Operand::Value(v) = o {
            if let Some(&root) = derived.get(&v) {
                escaped.insert(root);
            }
        }
    };
    for b in &f.blocks {
        for &id in &b.insts {
            match &f.insts[id.idx()].kind {
                // the address operand is fine; a stored *value* escapes
                InstKind::Load { .. } | InstKind::MemSet { .. } => {}
                InstKind::Store { val, .. } => esc(*val, &mut escaped),
                InstKind::MemCopy { .. } => {}
                InstKind::PtrAdd { offset, .. } => esc(*offset, &mut escaped),
                other => other.for_each_operand(|o| esc(o, &mut escaped)),
            }
        }
        for o in b.term.operands() {
            esc(o, &mut escaped);
        }
    }
    slots.retain(|s| !escaped.contains(s));
    slots
}

/// Is `p` based on a non-escaping stack slot?
pub fn based_on_local(f: &Func, local: &HashSet<ValueId>, p: Operand) -> bool {
    matches!(decompose(f, p).base, Operand::Value(v) if local.contains(&v))
}
