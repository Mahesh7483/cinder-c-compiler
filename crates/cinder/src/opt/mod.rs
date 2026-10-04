//! IR optimizations.
//!
//! Every pass is an independent, toggleable transformation of one function
//! (or, for inlining and tail calls, of the module). The pass manager runs
//! them in a fixed order at `-O1`/`-O2`, repeating the scalar passes until
//! nothing changes, and — when asked — runs the IR verifier after every
//! pass so a miscompiling transformation is caught at the pass that broke
//! the invariant.
//!
//! | pass         | what it does                                              | level |
//! |--------------|-----------------------------------------------------------|-------|
//! | `mem2reg`    | promote scalar stack slots to SSA values (phi insertion)  | O1    |
//! | `sccp`       | constant folding and propagation, constant branches       | O1    |
//! | `strength`   | algebraic simplification, mul/div/rem by powers of two    | O1    |
//! | `copyprop`   | trivial-phi elimination                                   | O1    |
//! | `cse`        | common subexpression + redundant load elimination         | O1    |
//! | `dce`        | dead code and dead store elimination                      | O1    |
//! | `simplifycfg`| merge/thread blocks, fold branches, remove dead blocks    | O1    |
//! | `licm`       | hoist loop-invariant computations                         | O2    |
//! | `inline`     | inline small non-recursive functions                      | O2    |
//! | `tailcall`   | self tail recursion -> loops; mark sibling tail calls     | O2    |

pub mod alias;
pub mod copyprop;
pub mod cse;
pub mod dce;
pub mod inline;
pub mod licm;
pub mod mem2reg;
pub mod sccp;
pub mod simplifycfg;
pub mod strength;
pub mod tailcall;

use crate::ir::*;
use std::collections::{HashMap, HashSet};

pub const PASS_NAMES: &[&str] =
    &["mem2reg", "sccp", "strength", "copyprop", "cse", "dce", "simplifycfg", "licm", "inline", "tailcall", "peephole"];

#[derive(Clone, Debug)]
pub struct OptConfig {
    pub level: u8,
    enabled: HashSet<&'static str>,
    /// Run the verifier after every pass (and panic with the offending pass name).
    pub verify: bool,
}

impl OptConfig {
    /// Configuration for an `-O` level with `-f<pass>` / `-fno-<pass>` overrides
    /// applied in order. Returns an error naming an unknown pass.
    pub fn new(level: u8, flags: &[(String, bool)]) -> Result<OptConfig, String> {
        let mut enabled: HashSet<&'static str> = HashSet::new();
        let o1 = ["mem2reg", "sccp", "strength", "copyprop", "cse", "dce", "simplifycfg", "peephole"];
        let o2 = ["licm", "inline", "tailcall"];
        if level >= 1 {
            enabled.extend(o1);
        }
        if level >= 2 {
            enabled.extend(o2);
        }
        // the backend peephole is on even at -O0 unless disabled
        enabled.insert("peephole");
        for (name, on) in flags {
            let Some(&known) = PASS_NAMES.iter().find(|n| **n == name.as_str()) else {
                // not an optimizer flag: other -f options are accepted and ignored
                continue;
            };
            if *on {
                enabled.insert(known);
            } else {
                enabled.remove(known);
            }
        }
        Ok(OptConfig { level, enabled, verify: cfg!(debug_assertions) })
    }

    pub fn is_enabled(&self, pass: &str) -> bool {
        self.enabled.contains(pass)
    }

    pub fn with_verify(mut self, v: bool) -> OptConfig {
        self.verify = v;
        self
    }
}

fn check(m: &Module, f: &Func, pass: &str, cfg: &OptConfig) {
    if cfg.verify {
        if let Err(errs) = verify::verify_func(m, f) {
            panic!(
                "internal compiler error: pass '{}' produced invalid IR in @{}:\n  {}\n\n{}",
                pass,
                f.name,
                errs.join("\n  "),
                print::print_func(m, f)
            );
        }
    }
}

/// Run the optimization pipeline over a whole module.
///
/// 1. the scalar pipeline on every function;
/// 2. self tail recursion to loops (so such functions stop being recursive
///    and can be inlined), then re-run on the functions that changed;
/// 3. inlining, then the scalar pipeline again on the functions that grew;
/// 4. marking of sibling tail calls for the backend (last, because nothing
///    may be inserted after a marked call).
pub fn optimize(m: &mut Module, cfg: &OptConfig) {
    let any = PASS_NAMES.iter().any(|p| *p != "peephole" && cfg.is_enabled(p));
    if !any {
        return;
    }
    for fi in 0..m.funcs.len() {
        reoptimize(m, fi, cfg);
    }
    if cfg.is_enabled("tailcall") {
        let changed = tailcall::loops(m);
        reoptimize_syms(m, &changed, "tailcall", cfg);
    }
    if cfg.is_enabled("inline") {
        let changed = inline::run(m, cfg);
        reoptimize_syms(m, &changed, "inline", cfg);
    }
    if cfg.is_enabled("tailcall") {
        tailcall::mark_sibling_calls(m);
        for fi in 0..m.funcs.len() {
            check(m, &m.funcs[fi], "tailcall", cfg);
        }
    }
}

/// Verify (after `pass`) and re-optimize the functions with these symbols.
fn reoptimize_syms(m: &mut Module, syms: &[SymId], pass: &str, cfg: &OptConfig) {
    for sym in syms {
        if let Some(fi) = m.funcs.iter().position(|f| f.sym == *sym) {
            check(m, &m.funcs[fi], pass, cfg);
            reoptimize(m, fi, cfg);
        }
    }
}

/// Run the scalar pipeline on `m.funcs[fi]` (the body is moved out while the
/// passes read the rest of the module).
fn reoptimize(m: &mut Module, fi: usize, cfg: &OptConfig) {
    let mut f =
        std::mem::replace(&mut m.funcs[fi], Func::new(crate::intern::Symbol::new(""), SymId(0), Linkage::External));
    optimize_function(m, &mut f, cfg);
    m.funcs[fi] = f;
}

/// The scalar pipeline for one function.
pub fn optimize_function(m: &Module, f: &mut Func, cfg: &OptConfig) {
    macro_rules! run_pass {
        ($name:literal, $call:expr) => {
            if cfg.is_enabled($name) {
                let changed: bool = $call;
                check(m, f, $name, cfg);
                changed
            } else {
                false
            }
        };
    }
    cfg::remove_unreachable(f);
    run_pass!("simplifycfg", simplifycfg::run(f));
    run_pass!("mem2reg", mem2reg::run(f));
    for _round in 0..6 {
        let mut changed = false;
        changed |= run_pass!("sccp", sccp::run(f));
        changed |= run_pass!("strength", strength::run(f));
        changed |= run_pass!("copyprop", copyprop::run(f));
        changed |= run_pass!("cse", cse::run(f));
        changed |= run_pass!("licm", licm::run(m, f));
        changed |= run_pass!("dce", dce::run(f));
        changed |= run_pass!("simplifycfg", simplifycfg::run(f));
        if !changed {
            break;
        }
    }
    f.sweep();
}

// ───────────────────────────── shared utilities ─────────────────────────────

/// Resolve chains in a replacement map.
pub fn resolve(map: &HashMap<ValueId, Operand>, mut o: Operand) -> Operand {
    let mut guard = 0;
    while let Operand::Value(v) = o {
        match map.get(&v) {
            Some(&n) if n != o => {
                o = n;
                guard += 1;
                if guard > 10_000 {
                    break;
                }
            }
            _ => break,
        }
    }
    o
}

/// Apply a value replacement map to every live use in the function.
pub fn apply_replacements(f: &mut Func, map: &HashMap<ValueId, Operand>) {
    if map.is_empty() {
        return;
    }
    for (i, inst) in f.insts.iter_mut().enumerate() {
        if f.dead_insts[i] {
            continue;
        }
        inst.kind.for_each_operand_mut(|o| *o = resolve(map, *o));
    }
    for b in &mut f.blocks {
        match &mut b.term {
            Term::CondBr { cond, .. } => *cond = resolve(map, *cond),
            Term::Switch { val, .. } => *val = resolve(map, *val),
            Term::Ret(vs) => {
                for v in vs {
                    *v = resolve(map, *v);
                }
            }
            _ => {}
        }
    }
}

/// Canonical integer constant: sign-extend `v` from the width of `ty`.
pub fn canon(v: i64, ty: Type) -> i64 {
    match ty {
        Type::I8 => v as i8 as i64,
        Type::I16 => v as i16 as i64,
        Type::I32 => v as i32 as i64,
        _ => v,
    }
}

/// The value of a canonical constant as an unsigned number of `ty`'s width.
pub fn unsigned(v: i64, ty: Type) -> u64 {
    match ty {
        Type::I8 => v as u8 as u64,
        Type::I16 => v as u16 as u64,
        Type::I32 => v as u32 as u64,
        _ => v as u64,
    }
}

/// Fold an integer binary operation on canonical constants; `None` when the
/// operation must be left to run time (division by zero, oversized shifts, ...).
pub fn fold_int_bin(op: BinOp, ty: Type, a: i64, b: i64) -> Option<i64> {
    let bits = ty.bits() as i64;
    let (ua, ub) = (unsigned(a, ty), unsigned(b, ty));
    let r = match op {
        BinOp::Add => a.wrapping_add(b),
        BinOp::Sub => a.wrapping_sub(b),
        BinOp::Mul => a.wrapping_mul(b),
        BinOp::SDiv => {
            let min = if bits >= 64 { i64::MIN } else { -(1i64 << (bits - 1)) };
            if b == 0 || (b == -1 && a == min) {
                return None; // traps at run time
            }
            a.wrapping_div(b)
        }
        BinOp::UDiv => {
            if ub == 0 {
                return None;
            }
            (ua / ub) as i64
        }
        BinOp::SRem => {
            let min = if bits >= 64 { i64::MIN } else { -(1i64 << (bits - 1)) };
            if b == 0 || (b == -1 && a == min) {
                return None; // traps at run time
            }
            if b == -1 {
                0
            } else {
                a.wrapping_rem(b)
            }
        }
        BinOp::URem => {
            if ub == 0 {
                return None;
            }
            (ua % ub) as i64
        }
        BinOp::And => a & b,
        BinOp::Or => a | b,
        BinOp::Xor => a ^ b,
        BinOp::Shl => {
            if ub >= bits as u64 {
                return None;
            }
            (ua << ub) as i64
        }
        BinOp::LShr => {
            if ub >= bits as u64 {
                return None;
            }
            (ua >> ub) as i64
        }
        BinOp::AShr => {
            if ub >= bits as u64 {
                return None;
            }
            a >> ub
        }
        _ => return None,
    };
    Some(canon(r, ty))
}

pub fn fold_icmp(pred: IPred, ty: Type, a: i64, b: i64) -> bool {
    let (ua, ub) = (unsigned(a, ty), unsigned(b, ty));
    match pred {
        IPred::Eq => a == b,
        IPred::Ne => a != b,
        IPred::Slt => a < b,
        IPred::Sle => a <= b,
        IPred::Sgt => a > b,
        IPred::Sge => a >= b,
        IPred::Ult => ua < ub,
        IPred::Ule => ua <= ub,
        IPred::Ugt => ua > ub,
        IPred::Uge => ua >= ub,
    }
}

#[cfg(test)]
mod tests;
