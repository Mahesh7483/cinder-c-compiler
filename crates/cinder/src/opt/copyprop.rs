//! Copy propagation. The IR has no explicit copy instruction, so the copies
//! that remain after mem2reg are *trivial phis*: a phi whose inputs are all
//! one value (ignoring references to itself) is that value.

use super::{apply_replacements, resolve};
use crate::ir::*;
use std::collections::HashMap;

pub fn run(f: &mut Func) -> bool {
    let mut changed = false;
    loop {
        let mut repl: HashMap<ValueId, Operand> = HashMap::new();
        let mut kill: Vec<InstId> = Vec::new();
        for b in &f.blocks {
            for &id in &b.insts {
                let inst = &f.insts[id.idx()];
                let InstKind::Phi { incoming, .. } = &inst.kind else { break };
                let dst = inst.dst.unwrap();
                let mut uniq: Option<Operand> = None;
                let mut ok = true;
                for (_, o) in incoming {
                    let o = resolve(&repl, *o);
                    if o == Operand::Value(dst) {
                        continue;
                    }
                    match uniq {
                        None => uniq = Some(o),
                        Some(u) if u == o => {}
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    // all inputs were the phi itself: the value is never defined on any path
                    repl.insert(dst, uniq.unwrap_or(Operand::Undef(f.values[dst.idx()].ty)));
                    kill.push(id);
                }
            }
        }
        if repl.is_empty() {
            break;
        }
        for id in kill {
            f.kill(id);
        }
        apply_replacements(f, &repl);
        f.sweep();
        changed = true;
    }
    changed
}
