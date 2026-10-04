//! Linear-scan register allocation with spilling.
//!
//! 1. number the instructions in layout order and compute block successors;
//! 2. compute liveness of virtual registers by iterative dataflow;
//! 3. build one live interval per virtual register (no holes);
//! 4. compute *busy ranges* for physical registers that appear in the code
//!    (ABI argument/result registers, `rax`/`rdx` for division, `rcx` for
//!    shifts, call clobbers). A virtual register may not take a physical
//!    register whose busy range overlaps its interval — this is how all the
//!    fixed-register constraints are honoured;
//! 5. scan intervals by start position, preferring a copy-coalescing hint,
//!    spilling the interval that ends furthest away when registers run out;
//! 6. rewrite the code, replacing virtual registers and wrapping spilled
//!    ones in loads/stores through reserved scratch registers
//!    (`r10`/`r11`, `xmm14`/`xmm15`).

#![allow(clippy::explicit_counter_loop, clippy::needless_range_loop)]

use super::mir::*;
use std::collections::HashMap;

const GPR_ORDER: [u8; 12] = [R8, R9, RSI, RDI, RCX, RDX, RAX, RBX, R12, R13, R14, R15];
const XMM_ORDER: [u8; 14] = [18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 16, 17];
const GPR_SCRATCH: [u8; 2] = [R10, R11];
const XMM_SCRATCH: [u8; 2] = [30, 31];

struct Bits {
    w: Vec<u64>,
}

impl Bits {
    fn new(n: usize) -> Bits {
        Bits { w: vec![0; n.div_ceil(64)] }
    }

    fn set(&mut self, i: usize) {
        self.w[i / 64] |= 1 << (i % 64);
    }

    fn clear(&mut self, i: usize) {
        self.w[i / 64] &= !(1 << (i % 64));
    }

    fn get(&self, i: usize) -> bool {
        self.w[i / 64] >> (i % 64) & 1 == 1
    }

    fn or_with(&mut self, o: &Bits) -> bool {
        let mut changed = false;
        for (a, b) in self.w.iter_mut().zip(&o.w) {
            let n = *a | *b;
            if n != *a {
                *a = n;
                changed = true;
            }
        }
        changed
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.w.iter().enumerate().flat_map(|(i, &w)| (0..64).filter(move |b| w >> b & 1 == 1).map(move |b| i * 64 + b))
    }
}

pub fn compute_succs(mf: &mut MFunc) {
    let n = mf.layout.len();
    for k in 0..n {
        let b = mf.layout[k];
        let mut s: Vec<usize> = Vec::new();
        let mut falls = true;
        for inst in &mf.blocks[b].insts {
            match inst.op {
                Op::Jmp(t) => {
                    s.push(t);
                    falls = false;
                }
                Op::Jcc(_, t) => {
                    s.push(t);
                    falls = true;
                }
                Op::Ret | Op::Ud2 | Op::TailCall(_) => falls = false,
                Op::JmpTable(ref jt) => {
                    s.extend(jt.targets.iter().copied());
                    falls = false;
                }
                _ => {}
            }
        }
        // `falls` reflects the last control-flow instruction in the block
        if let Some(last) = mf.blocks[b].insts.iter().rev().find(|i| !matches!(i.op, Op::Loc(_))) {
            falls = !matches!(last.op, Op::Jmp(_) | Op::Ret | Op::Ud2 | Op::TailCall(_) | Op::JmpTable(_));
        }
        if falls && k + 1 < n {
            s.push(mf.layout[k + 1]);
        }
        s.sort();
        s.dedup();
        mf.blocks[b].succs = s;
    }
}

struct Interval {
    v: u32,
    start: u32,
    end: u32,
}

pub fn allocate(mf: &mut MFunc) {
    compute_succs(mf);
    let nb = mf.blocks.len();
    let nv = mf.vclass.len();
    let layout = mf.layout.clone();

    // ── numbering ──
    let mut bstart = vec![0u32; nb];
    let mut bend = vec![0u32; nb];
    let mut pos = 0u32;
    for &b in &layout {
        bstart[b] = 2 * pos;
        pos += mf.blocks[b].insts.len() as u32;
        bend[b] = (2 * pos).saturating_sub(1).max(bstart[b]);
    }

    // ── liveness ──
    let mut use_set: Vec<Bits> = (0..nb).map(|_| Bits::new(nv)).collect();
    let mut def_set: Vec<Bits> = (0..nb).map(|_| Bits::new(nv)).collect();
    for &b in &layout {
        for inst in &mf.blocks[b].insts {
            inst.for_each_reg(&mut |r, acc| {
                if let Reg::V(v) = r {
                    let v = v as usize;
                    if matches!(acc, Acc::Use | Acc::UseDef) && !def_set[b].get(v) {
                        use_set[b].set(v);
                    }
                    if matches!(acc, Acc::Def | Acc::UseDef) {
                        def_set[b].set(v);
                    }
                }
            });
        }
    }
    let mut live_in: Vec<Bits> = (0..nb).map(|_| Bits::new(nv)).collect();
    let mut live_out: Vec<Bits> = (0..nb).map(|_| Bits::new(nv)).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for &b in layout.iter().rev() {
            for &s in &mf.blocks[b].succs.clone() {
                let src = Bits { w: live_in[s].w.clone() };
                if live_out[b].or_with(&src) {
                    changed = true;
                }
            }
            let mut new_in = Bits { w: live_out[b].w.clone() };
            for i in def_set[b].iter() {
                new_in.clear(i);
            }
            new_in.or_with(&use_set[b]);
            if live_in[b].w != new_in.w {
                live_in[b] = new_in;
                changed = true;
            }
        }
    }

    // ── intervals ──
    let mut lo = vec![u32::MAX; nv];
    let mut hi = vec![0u32; nv];
    let extend = |lo: &mut Vec<u32>, hi: &mut Vec<u32>, v: usize, p: u32| {
        lo[v] = lo[v].min(p);
        hi[v] = hi[v].max(p);
    };
    for &b in &layout {
        for v in live_in[b].iter() {
            extend(&mut lo, &mut hi, v, bstart[b]);
        }
        for v in live_out[b].iter() {
            extend(&mut lo, &mut hi, v, bend[b]);
        }
        let mut n = bstart[b] / 2;
        for inst in &mf.blocks[b].insts {
            inst.for_each_reg(&mut |r, acc| {
                if let Reg::V(v) = r {
                    let v = v as usize;
                    if matches!(acc, Acc::Use | Acc::UseDef) {
                        extend(&mut lo, &mut hi, v, 2 * n);
                    }
                    if matches!(acc, Acc::Def | Acc::UseDef) {
                        extend(&mut lo, &mut hi, v, 2 * n + 1);
                    }
                }
            });
            n += 1;
        }
    }
    let mut intervals: Vec<Interval> =
        (0..nv).filter(|&v| lo[v] != u32::MAX).map(|v| Interval { v: v as u32, start: lo[v], end: hi[v] }).collect();
    intervals.sort_by_key(|i| (i.start, i.end));

    // ── physical busy ranges ──
    let mut busy: Vec<Vec<(u32, u32)>> = vec![Vec::new(); 32];
    for &b in &layout {
        let mut cur: [Option<(u32, u32)>; 32] = [None; 32];
        let mut n = bstart[b] / 2;
        for inst in &mf.blocks[b].insts {
            let mut uses: Vec<u8> = Vec::new();
            let mut defs: Vec<u8> = Vec::new();
            inst.for_each_reg(&mut |r, acc| {
                if let Reg::P(p) = r {
                    if matches!(acc, Acc::Use | Acc::UseDef) {
                        uses.push(p);
                    }
                    if matches!(acc, Acc::Def | Acc::UseDef) {
                        defs.push(p);
                    }
                }
            });
            let (iu, id, clobber) = inst.implicit();
            uses.extend(iu);
            defs.extend(id.iter().copied());
            for p in uses {
                let p = p as usize;
                cur[p] = Some(match cur[p] {
                    Some((s, _)) => (s, 2 * n),
                    None => (bstart[b], 2 * n),
                });
            }
            if clobber {
                for p in 0..32u8 {
                    if is_caller_saved(p) && !defs.contains(&p) {
                        busy[p as usize].push((2 * n, 2 * n + 1));
                    }
                }
            }
            for p in defs {
                let p = p as usize;
                if let Some(r) = cur[p].take() {
                    busy[p].push(r);
                }
                cur[p] = Some((2 * n + 1, 2 * n + 1));
            }
            n += 1;
        }
        for (p, c) in cur.iter().enumerate() {
            if let Some(r) = c {
                busy[p].push(*r);
            }
        }
    }
    for r in &mut busy {
        r.sort();
    }

    // ── scan ──
    let overlaps = |p: u8, s: u32, e: u32, busy: &Vec<Vec<(u32, u32)>>| -> bool {
        busy[p as usize].iter().any(|&(a, b)| a <= e && s <= b)
    };
    let mut assign: Vec<Option<u8>> = vec![None; nv];
    let mut spilled: Vec<bool> = vec![false; nv];
    let mut active: Vec<(u32, u32, u8)> = Vec::new(); // (end, vreg, preg)
    let mut used_callee: Vec<u8> = Vec::new();
    for iv in &intervals {
        active.retain(|&(end, _, _)| end >= iv.start);
        let class = mf.vclass[iv.v as usize];
        let order: &[u8] = if class == Class::Gpr { &GPR_ORDER } else { &XMM_ORDER };
        let is_free = |p: u8, active: &Vec<(u32, u32, u8)>| {
            !active.iter().any(|&(_, _, ap)| ap == p) && !overlaps(p, iv.start, iv.end, &busy)
        };
        // hint
        let mut choice: Option<u8> = None;
        if let Some(h) = mf.hints.get(&iv.v) {
            let hp = match h {
                Reg::P(p) => Some(*p),
                Reg::V(u) => assign[*u as usize],
            };
            if let Some(p) = hp {
                if order.contains(&p) && is_free(p, &active) {
                    choice = Some(p);
                }
            }
        }
        if choice.is_none() {
            choice = order.iter().copied().find(|&p| is_free(p, &active));
        }
        if let Some(p) = choice {
            assign[iv.v as usize] = Some(p);
            active.push((iv.end, iv.v, p));
            continue;
        }
        // no register: spill the interval that lives longest
        let victim = active
            .iter()
            .enumerate()
            .filter(|(_, &(_, v, _))| mf.vclass[v as usize] == class)
            .max_by_key(|(_, &(end, _, _))| end)
            .map(|(i, &(end, v, p))| (i, end, v, p));
        match victim {
            Some((idx, end, v, p)) if end > iv.end && !overlaps(p, iv.start, iv.end, &busy) => {
                spilled[v as usize] = true;
                assign[v as usize] = None;
                active.remove(idx);
                assign[iv.v as usize] = Some(p);
                active.push((iv.end, iv.v, p));
            }
            _ => spilled[iv.v as usize] = true,
        }
    }
    for v in 0..nv {
        if let Some(p) = assign[v] {
            if matches!(p, RBX | R12 | R13 | R14 | R15) && !used_callee.contains(&p) {
                used_callee.push(p);
            }
        }
    }
    used_callee.sort();
    mf.used_callee_saved = used_callee;

    // ── spill slots ──
    let mut spill_slot: HashMap<u32, u32> = HashMap::new();
    for v in 0..nv {
        if spilled[v] && lo[v] != u32::MAX {
            let s = mf.new_slot(8, 8);
            spill_slot.insert(v as u32, s);
        }
    }

    // ── rewrite ──
    for &b in &layout.clone() {
        let old = std::mem::take(&mut mf.blocks[b].insts);
        let mut out: Vec<MInst> = Vec::with_capacity(old.len());
        for mut inst in old {
            rewrite_inst(&mut inst, &assign, &spill_slot, mf, &mut out);
        }
        mf.blocks[b].insts = out;
    }
}

fn phys(assign: &[Option<u8>], v: u32) -> Option<Reg> {
    assign[v as usize].map(Reg::P)
}

fn rewrite_inst(inst: &mut MInst, assign: &[Option<u8>], spill: &HashMap<u32, u32>, mf: &MFunc, out: &mut Vec<MInst>) {
    // Distinct spilled virtual registers in this instruction, in order of appearance.
    struct Sp {
        v: u32,
        load: bool,
        store: bool,
        class: Class,
        scratch: Option<u8>,
    }
    let mut sps: Vec<Sp> = Vec::new();
    inst.for_each_reg(&mut |r, acc| {
        if let Reg::V(v) = r {
            if assign[v as usize].is_none() {
                let load = matches!(acc, Acc::Use | Acc::UseDef);
                let store = matches!(acc, Acc::Def | Acc::UseDef);
                match sps.iter_mut().find(|s| s.v == v) {
                    Some(s) => {
                        s.load |= load;
                        s.store |= store;
                    }
                    None => sps.push(Sp { v, load, store, class: mf.vclass[v as usize], scratch: None }),
                }
            }
        }
    });
    if sps.is_empty() {
        inst.for_each_reg_mut(&mut |r, _| {
            if let Reg::V(v) = *r {
                *r = phys(assign, v).expect("assigned register");
            }
        });
        out.push(inst.clone());
        return;
    }
    // A plain 64-bit move to/from a spill slot can use the slot directly.
    if inst.op == Op::Mov && inst.sz == Sz::Q {
        if let (MOp::Reg(Reg::V(d)), src) = (inst.dst, inst.src) {
            if assign[d as usize].is_none() {
                let direct = match src {
                    MOp::Imm(c) => i32::try_from(c).is_ok(),
                    MOp::Reg(Reg::V(s)) => assign[s as usize].is_some(),
                    MOp::Reg(Reg::P(_)) => true,
                    _ => false,
                };
                if direct {
                    let mut src = src;
                    if let MOp::Reg(Reg::V(s)) = src {
                        src = MOp::Reg(phys(assign, s).unwrap());
                    }
                    out.push(MInst::new(Op::Mov, Sz::Q, MOp::Mem(Mem::slot(spill[&d])), src));
                    return;
                }
            }
        }
        if let (MOp::Reg(Reg::V(d)), MOp::Reg(Reg::V(s))) = (inst.dst, inst.src) {
            if assign[d as usize].is_some() && assign[s as usize].is_none() {
                out.push(MInst::new(
                    Op::Mov,
                    Sz::Q,
                    MOp::Reg(phys(assign, d).unwrap()),
                    MOp::Mem(Mem::slot(spill[&s])),
                ));
                return;
            }
        }
    }
    // Assign scratch registers.
    let (mut gi, mut xi) = (0usize, 0usize);
    let gpr_needed = sps.iter().filter(|s| s.class == Class::Gpr).count();
    let mut pre: Vec<MInst> = Vec::new();
    if gpr_needed > GPR_SCRATCH.len() {
        // base + index + a third register: fold the address into one scratch first
        let mem_regs: Vec<(u32, bool)> = {
            let mut v = Vec::new();
            let m = match (&inst.dst, &inst.src) {
                (MOp::Mem(m), _) | (_, MOp::Mem(m)) => Some(*m),
                _ => None,
            };
            if let Some(m) = m {
                if let Base::Reg(Reg::V(b)) = m.base {
                    v.push((b, true));
                }
                if let Some((Reg::V(i), _)) = m.index {
                    v.push((i, false));
                }
            }
            v
        };
        // load base into r10, index into r11, lea into r10; the memory operand becomes (r10)
        let m = match (&mut inst.dst, &mut inst.src) {
            (MOp::Mem(m), _) | (_, MOp::Mem(m)) => m,
            _ => unreachable!("three spilled registers without a memory operand"),
        };
        let mut folded = *m;
        for (v, is_base) in &mem_regs {
            if assign[*v as usize].is_none() {
                let scratch = if *is_base { R10 } else { R11 };
                pre.push(MInst::new(Op::Mov, Sz::Q, MOp::Reg(Reg::P(scratch)), MOp::Mem(Mem::slot(spill[v]))));
                if *is_base {
                    folded.base = Base::Reg(Reg::P(scratch));
                } else {
                    folded.index = folded.index.map(|(_, s)| (Reg::P(scratch), s));
                }
            }
        }
        pre.push(MInst::new(Op::Lea, Sz::Q, MOp::Reg(Reg::P(R10)), MOp::Mem(folded)));
        *m = Mem { base: Base::Reg(Reg::P(R10)), index: None, disp: 0 };
        out.append(&mut pre);
        // the memory registers are now resolved; drop them from the spill list
        sps.retain(|s| !mem_regs.iter().any(|(v, _)| *v == s.v));
        gi = 1; // r10 is taken by the folded address
    }
    for s in &mut sps {
        s.scratch = Some(match s.class {
            Class::Gpr => {
                let r = GPR_SCRATCH[gi.min(1)];
                gi += 1;
                r
            }
            Class::Xmm => {
                let r = XMM_SCRATCH[xi.min(1)];
                xi += 1;
                r
            }
        });
    }
    // loads
    for s in &sps {
        if s.load {
            let slot = Mem::slot(spill[&s.v]);
            let r = Reg::P(s.scratch.unwrap());
            match s.class {
                Class::Gpr => out.push(MInst::new(Op::Mov, Sz::Q, MOp::Reg(r), MOp::Mem(slot))),
                Class::Xmm => out.push(MInst::new(Op::MovF, Sz::Q, MOp::Reg(r), MOp::Mem(slot))),
            }
        }
    }
    inst.for_each_reg_mut(&mut |r, _| {
        if let Reg::V(v) = *r {
            *r = match phys(assign, v) {
                Some(p) => p,
                None => Reg::P(sps.iter().find(|s| s.v == v).and_then(|s| s.scratch).expect("scratch register")),
            };
        }
    });
    out.push(inst.clone());
    // stores
    for s in &sps {
        if s.store {
            let slot = Mem::slot(spill[&s.v]);
            let r = Reg::P(s.scratch.unwrap());
            match s.class {
                Class::Gpr => out.push(MInst::new(Op::Mov, Sz::Q, MOp::Mem(slot), MOp::Reg(r))),
                Class::Xmm => out.push(MInst::new(Op::MovF, Sz::Q, MOp::Mem(slot), MOp::Reg(r))),
            }
        }
    }
}
