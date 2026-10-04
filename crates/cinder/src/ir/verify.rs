//! IR verifier. Run after lowering and (in tests / debug builds) after every
//! optimization pass: it checks SSA dominance, phi shape, operand types and
//! structural invariants so a buggy pass is caught at the pass that broke it.

use super::cfg;
use super::*;
use std::collections::HashSet;

pub fn verify_module(m: &Module) -> Result<(), Vec<String>> {
    let mut errs = Vec::new();
    for f in &m.funcs {
        if let Err(e) = verify_func(m, f) {
            for x in e {
                errs.push(format!("in @{}: {}", f.name, x));
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

pub fn verify_func(m: &Module, f: &Func) -> Result<(), Vec<String>> {
    let mut errs: Vec<String> = Vec::new();
    let nblocks = f.blocks.len();
    if nblocks == 0 {
        return Err(vec!["function has no blocks".into()]);
    }
    let cfg = cfg::build(f);
    let dom = cfg::dominators(f, &cfg);

    // position of every live instruction
    let mut pos: Vec<Option<(BlockId, usize)>> = vec![None; f.insts.len()];
    for (bi, b) in f.blocks.iter().enumerate() {
        for (ii, id) in b.insts.iter().enumerate() {
            if f.dead_insts[id.idx()] {
                errs.push(format!("block {} lists dead instruction {}", b.name, id.0));
                continue;
            }
            if pos[id.idx()].is_some() {
                errs.push(format!("instruction {} appears twice", id.0));
            }
            pos[id.idx()] = Some((BlockId(bi as u32), ii));
        }
    }

    // each value has exactly one definition
    let mut defined: HashSet<u32> = HashSet::new();
    for (i, inst) in f.insts.iter().enumerate() {
        if f.dead_insts[i] || pos[i].is_none() {
            continue;
        }
        for d in [inst.dst, inst.dst2].into_iter().flatten() {
            if !defined.insert(d.0) {
                errs.push(format!("value %{} defined twice", d.0));
            }
            if f.values[d.idx()].def != ValueDef::Inst(InstId(i as u32)) {
                errs.push(format!("value %{} has an inconsistent definition record", d.0));
            }
        }
    }
    for v in &f.param_values {
        defined.insert(v.0);
    }

    let check_use = |errs: &mut Vec<String>, o: Operand, use_block: BlockId, use_idx: Option<usize>, what: &str| {
        let Operand::Value(v) = o else { return };
        if v.idx() >= f.values.len() {
            errs.push(format!("{}: use of out-of-range value %{}", what, v.0));
            return;
        }
        if !defined.contains(&v.0) {
            errs.push(format!("{}: use of undefined value %{}", what, v.0));
            return;
        }
        if !dom.is_reachable(use_block) {
            return;
        }
        match f.values[v.idx()].def {
            ValueDef::Param(_) => {}
            ValueDef::Inst(def) => {
                let Some((db, di)) = pos[def.idx()] else {
                    errs.push(format!("{}: use of value %{} whose definition was removed", what, v.0));
                    return;
                };
                if db == use_block {
                    if let Some(ui) = use_idx {
                        if di >= ui {
                            errs.push(format!("{}: %{} used before its definition", what, v.0));
                        }
                    }
                } else if !dom.dominates(db, use_block) {
                    errs.push(format!(
                        "{}: definition of %{} (block {}) does not dominate its use (block {})",
                        what,
                        v.0,
                        f.blocks[db.idx()].name,
                        f.blocks[use_block.idx()].name
                    ));
                }
            }
        }
    };

    for (bi, b) in f.blocks.iter().enumerate() {
        let bid = BlockId(bi as u32);
        let bname = format!("{}.{}", b.name, bi);
        if !b.term.is_terminated() {
            errs.push(format!("block {} has no terminator", bname));
        }
        for s in b.term.successors() {
            if s.idx() >= nblocks {
                errs.push(format!("block {} branches to invalid block {}", bname, s.0));
            }
        }
        if let Term::Switch { cases, .. } = &b.term {
            let mut seen = HashSet::new();
            for (v, _) in cases {
                if !seen.insert(*v) {
                    errs.push(format!("block {} has duplicate switch case {}", bname, v));
                }
            }
        }
        // phis first
        let mut seen_non_phi = false;
        for (ii, &id) in b.insts.iter().enumerate() {
            if f.dead_insts[id.idx()] {
                continue;
            }
            let inst = &f.insts[id.idx()];
            let what = format!("block {} inst {}", bname, ii);
            match &inst.kind {
                InstKind::Phi { ty, incoming } => {
                    if seen_non_phi {
                        errs.push(format!("{}: phi after a non-phi instruction", what));
                    }
                    if dom.is_reachable(bid) {
                        // One entry per (reachable) predecessor, and no duplicates.
                        let mut want: Vec<BlockId> =
                            cfg.preds[bi].iter().copied().filter(|p| dom.is_reachable(*p)).collect();
                        want.sort();
                        let mut inc: Vec<BlockId> = incoming.iter().map(|(p, _)| *p).collect();
                        inc.sort();
                        let has_dup = inc.windows(2).any(|w| w[0] == w[1]);
                        let inc_reach: Vec<BlockId> = inc.iter().copied().filter(|p| dom.is_reachable(*p)).collect();
                        if has_dup || inc_reach != want {
                            errs.push(format!(
                                "{}: phi incoming blocks {:?} do not match predecessors {:?}",
                                what, inc, want
                            ));
                        }
                    }
                    for (p, o) in incoming {
                        if f.operand_ty(*o) != *ty {
                            errs.push(format!(
                                "{}: phi operand type mismatch ({:?} vs {:?})",
                                what,
                                f.operand_ty(*o),
                                ty
                            ));
                        }
                        if p.idx() < nblocks {
                            let end = f.blocks[p.idx()].insts.len();
                            check_use(&mut errs, *o, *p, Some(end), &what);
                        }
                    }
                }
                other => {
                    seen_non_phi = true;
                    inst_type_check(f, other, inst, &what, &mut errs);
                    other.for_each_operand(|o| check_use(&mut errs, o, bid, Some(ii), &what));
                }
            }
            if let InstKind::Alloca { .. } = &inst.kind {
                if bi != 0 {
                    errs.push(format!("{}: alloca outside the entry block", what));
                }
            }
            if let InstKind::Call { callee: Callee::Direct(s), .. } = &inst.kind {
                if s.idx() >= m.syms.len() {
                    errs.push(format!("{}: call to invalid symbol", what));
                }
            }
        }
        let what = format!("block {} terminator", bname);
        let n = b.insts.len();
        for o in b.term.operands() {
            check_use(&mut errs, o, bid, Some(n), &what);
        }
        match &b.term {
            Term::Ret(vs) => {
                let tys: Vec<Type> = vs.iter().map(|v| f.operand_ty(*v)).collect();
                if tys != f.rets {
                    errs.push(format!("{}: returns {:?} but the function returns {:?}", what, tys, f.rets));
                }
            }
            Term::CondBr { cond, .. } => {
                if !f.operand_ty(*cond).is_int() {
                    errs.push(format!("{}: branch condition is not an integer", what));
                }
            }
            Term::Switch { ty, val, .. } if f.operand_ty(*val) != *ty || !ty.is_int() => {
                errs.push(format!("{}: switch value type mismatch", what));
            }
            _ => {}
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

fn inst_type_check(f: &Func, k: &InstKind, inst: &Inst, what: &str, errs: &mut Vec<String>) {
    let ty = |o: Operand| f.operand_ty(o);
    let mut bad = |msg: String| errs.push(format!("{}: {}", what, msg));
    let dst_ty = inst.dst.map(|d| f.values[d.idx()].ty);
    match k {
        InstKind::Alloca { .. } | InstKind::StackSave | InstKind::VaRegSave | InstKind::VaStackArgs => {
            if dst_ty != Some(Type::Ptr) {
                bad("result must be ptr".into());
            }
        }
        InstKind::DynAlloca { size, .. } => {
            if ty(*size) != Type::I64 {
                bad("dynalloca size must be i64".into());
            }
            if dst_ty != Some(Type::Ptr) {
                bad("result must be ptr".into());
            }
        }
        InstKind::StackRestore { ptr } => {
            if ty(*ptr) != Type::Ptr {
                bad("stackrestore operand must be ptr".into());
            }
        }
        InstKind::Load { ty: t, ptr, .. } => {
            if ty(*ptr) != Type::Ptr {
                bad("load address is not a ptr".into());
            }
            if dst_ty != Some(*t) {
                bad("load result type mismatch".into());
            }
        }
        InstKind::Store { ty: t, val, ptr, .. } => {
            if ty(*ptr) != Type::Ptr {
                bad("store address is not a ptr".into());
            }
            if ty(*val) != *t {
                bad(format!("store value is {:?}, expected {:?}", ty(*val), t));
            }
        }
        InstKind::MemCopy { dst, src, .. } => {
            if ty(*dst) != Type::Ptr || ty(*src) != Type::Ptr {
                bad("memcpy operands must be ptr".into());
            }
        }
        InstKind::MemSet { dst, .. } => {
            if ty(*dst) != Type::Ptr {
                bad("memset destination must be ptr".into());
            }
        }
        InstKind::Bin { op, ty: t, lhs, rhs } => {
            if ty(*lhs) != *t || ty(*rhs) != *t {
                bad(format!("{} operands are {:?}/{:?}, expected {:?}", op.name(), ty(*lhs), ty(*rhs), t));
            }
            if op.is_float() != t.is_float() {
                bad(format!("{} used with type {:?}", op.name(), t));
            }
            if dst_ty != Some(*t) {
                bad("binary result type mismatch".into());
            }
        }
        InstKind::Un { ty: t, val, .. } => {
            if ty(*val) != *t || dst_ty != Some(*t) {
                bad("unary type mismatch".into());
            }
        }
        InstKind::ICmp { ty: t, lhs, rhs, .. } => {
            if ty(*lhs) != *t || ty(*rhs) != *t || t.is_float() {
                bad("icmp operand type mismatch".into());
            }
            if dst_ty != Some(Type::I32) {
                bad("icmp result must be i32".into());
            }
        }
        InstKind::FCmp { ty: t, lhs, rhs, .. } => {
            if ty(*lhs) != *t || ty(*rhs) != *t || !t.is_float() {
                bad("fcmp operand type mismatch".into());
            }
            if dst_ty != Some(Type::I32) {
                bad("fcmp result must be i32".into());
            }
        }
        InstKind::Cast { op, from, to, val } => {
            if ty(*val) != *from || dst_ty != Some(*to) {
                bad(format!("{} type mismatch", op.name()));
            }
            let ok = match op {
                CastOp::ZExt | CastOp::SExt => from.is_int() && to.is_int() && from.size() < to.size(),
                CastOp::Trunc => from.is_int() && to.is_int() && from.size() > to.size(),
                CastOp::SIToFP | CastOp::UIToFP => from.is_int() && to.is_float(),
                CastOp::FPToSI | CastOp::FPToUI => from.is_float() && to.is_int(),
                CastOp::FPExt => *from == Type::F32 && *to == Type::F64,
                CastOp::FPTrunc => *from == Type::F64 && *to == Type::F32,
                CastOp::PtrToInt => *from == Type::Ptr && to.is_int(),
                CastOp::IntToPtr => from.is_int() && *to == Type::Ptr,
            };
            if !ok {
                bad(format!("invalid {} from {:?} to {:?}", op.name(), from, to));
            }
        }
        InstKind::PtrAdd { base, offset } => {
            if ty(*base) != Type::Ptr || ty(*offset) != Type::I64 {
                bad(format!("ptradd operands are {:?}/{:?}, expected ptr/i64", ty(*base), ty(*offset)));
            }
            if dst_ty != Some(Type::Ptr) {
                bad("ptradd result must be ptr".into());
            }
        }
        InstKind::Select { ty: t, cond, a, b } => {
            if !ty(*cond).is_int() || ty(*a) != *t || ty(*b) != *t || dst_ty != Some(*t) {
                bad("select type mismatch".into());
            }
        }
        InstKind::Phi { .. } => {}
        InstKind::Call { callee, rets, .. } => {
            if let Callee::Indirect(o) = callee {
                if ty(*o) != Type::Ptr {
                    bad("indirect callee must be ptr".into());
                }
            }
            if rets.len() > 2 {
                bad("calls return at most two values".into());
            }
            let got: Vec<Type> = [inst.dst, inst.dst2].into_iter().flatten().map(|d| f.values[d.idx()].ty).collect();
            if got != *rets {
                bad(format!("call result types {:?} do not match {:?}", got, rets));
            }
        }
        InstKind::Trap => {}
    }
}
