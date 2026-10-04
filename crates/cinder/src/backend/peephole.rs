//! Small local clean-ups on allocated MIR.

use super::mir::*;

pub fn run(mf: &mut MFunc) {
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
