//! Small local clean-ups on allocated MIR.

use super::mir::*;

fn last_real(insts: &[MInst]) -> Option<&MInst> {
    insts.iter().rev().find(|i| !matches!(i.op, Op::Loc(_)))
}

fn ends_flow(insts: &[MInst]) -> bool {
    matches!(last_real(insts).map(|i| &i.op), Some(Op::Jmp(_) | Op::Ret | Op::Ud2 | Op::TailCall(_) | Op::JmpTable(_)))
}

/// Loop rotation by layout: a loop whose test sits at the top (`H: if (!c) goto exit; body;
/// jmp H`) is laid out as `jmp H; body; H: if (c) goto body`, so each iteration takes one
/// conditional branch instead of a conditional plus an unconditional one. Only the order
/// of blocks changes (plus an explicit `jmp` where a block used to fall into the header).
fn rotate_loops(mf: &mut MFunc) {
    let mut i = 1; // the entry block must stay first
    let mut budget = 4 * mf.layout.len() + 16;
    while i < mf.layout.len() && budget > 0 {
        budget -= 1;
        let h = mf.layout[i];
        let insts = &mf.blocks[h].insts;
        let is_test = matches!(last_real(insts).map(|x| &x.op), Some(Op::Jmp(_)))
            && insts.iter().rev().filter(|x| !matches!(x.op, Op::Loc(_))).take(2).any(|x| matches!(x.op, Op::Jcc(..)));
        if !is_test {
            i += 1;
            continue;
        }
        let latch = (i + 1..mf.layout.len())
            .rev()
            .find(|&j| matches!(last_real(&mf.blocks[mf.layout[j]].insts).map(|x| &x.op), Some(Op::Jmp(t)) if *t == h));
        let Some(j) = latch else {
            i += 1;
            continue;
        };
        let prev = mf.layout[i - 1];
        if !ends_flow(&mf.blocks[prev].insts) {
            mf.blocks[prev].insts.push(MInst::bare(Op::Jmp(h)));
        }
        let hb = mf.layout.remove(i);
        mf.layout.insert(j, hb);
        // the block now at position i is a new candidate
    }
}

pub fn run(mf: &mut MFunc) {
    rotate_loops(mf);
    let layout = mf.layout.clone();
    for (k, &b) in layout.iter().enumerate() {
        let next = layout.get(k + 1).copied();
        let insts = std::mem::take(&mut mf.blocks[b].insts);
        let mut out: Vec<MInst> = Vec::with_capacity(insts.len());
        for mut i in insts {
            // a 64-bit register move onto itself, or a vector move onto itself
            if let (MOp::Reg(a), MOp::Reg(c)) = (i.dst, i.src) {
                let same = a == c;
                if same && ((i.op == Op::Mov && i.sz == Sz::Q) || matches!(i.op, Op::MovAps)) {
                    continue;
                }
            }
            // adding/shifting by zero
            if matches!(i.op, Op::Add | Op::Sub | Op::Shl | Op::Shr | Op::Sar) && i.src == MOp::Imm(0) {
                continue;
            }
            // cmp $0, reg  ->  test reg, reg
            if i.op == Op::Cmp && i.src == MOp::Imm(0) {
                if let MOp::Reg(r) = i.dst {
                    i = MInst::new(Op::Test, i.sz, MOp::Reg(r), MOp::Reg(r));
                }
            }
            // lea (reg) -> mov reg
            if i.op == Op::Lea {
                if let (MOp::Reg(d), MOp::Mem(m)) = (i.dst, i.src) {
                    if let (Base::Reg(r), None, 0) = (m.base, m.index, m.disp) {
                        // a 32-bit lea zero-extends, exactly like a 32-bit mov
                        let sz = i.sz;
                        i = MInst::new(Op::Mov, sz, MOp::Reg(d), MOp::Reg(r));
                        if d == r && sz == Sz::Q {
                            continue;
                        }
                    }
                }
            }
            out.push(i);
        }
        // branches to the block that follows
        while let Some(last) = out.last() {
            match last.op {
                Op::Jmp(t) if Some(t) == next => {
                    out.pop();
                }
                _ => break,
            }
        }
        // jcc L1; jmp L2 with L1 next: jcc !cc L2
        let n = out.len();
        if n >= 2 {
            if let (Op::Jcc(cc, t1), Op::Jmp(t2)) = (out[n - 2].op.clone(), out[n - 1].op.clone()) {
                if Some(t1) == next {
                    out.pop();
                    out.pop();
                    out.push(MInst::bare(Op::Jcc(cc.negate(), t2)));
                }
            }
        }
        mf.blocks[b].insts = out;
    }
}
