//! Assembly text emission (AT&T syntax, GNU assembler dialect).

use super::mir::*;
use crate::ir::{DataDef, DataItem, Linkage, Module, SymBody, SymId};
use std::fmt::Write;

pub struct Emitter<'a> {
    pub m: &'a Module,
    pub out: String,
    pub emit_loc: bool,
}

fn escape_ascii(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        match b {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            b'\n' => s.push_str("\\n"),
            b'\t' => s.push_str("\\t"),
            32..=126 => s.push(b as char),
            _ => {
                let _ = write!(s, "\\{:03o}", b);
            }
        }
    }
    s
}

fn log2(a: u64) -> u32 {
    a.max(1).trailing_zeros()
}

impl<'a> Emitter<'a> {
    pub fn new(m: &'a Module, emit_loc: bool) -> Emitter<'a> {
        Emitter { m, out: String::new(), emit_loc }
    }

    fn sym_name(&self, s: SymId) -> String {
        self.m.syms[s.idx()].name.to_string()
    }

    pub fn header(&mut self, source: &str) {
        let _ = writeln!(self.out, "\t.file \"{}\"", escape_ascii(source.as_bytes()));
        if self.emit_loc {
            let _ = writeln!(self.out, "\t.file 1 \"{}\"", escape_ascii(source.as_bytes()));
        }
    }

    pub fn footer(&mut self) {
        self.out.push_str("\t.section .note.GNU-stack,\"\",@progbits\n");
    }

    // ───────────────────────────── functions ─────────────────────────────

    pub fn function(&mut self, mf: &MFunc) {
        let name = mf.name.to_string();
        self.out.push_str("\t.text\n");
        if mf.linkage == Linkage::External {
            let _ = writeln!(self.out, "\t.globl {}", name);
        }
        let _ = writeln!(self.out, "\t.type {}, @function", name);
        self.out.push_str("\t.p2align 4\n");
        let _ = writeln!(self.out, "{}:", name);
        self.prologue(mf);
        let layout = &mf.layout;
        for &b in layout {
            let _ = writeln!(self.out, ".L{}_{}:", name, b);
            for inst in &mf.blocks[b].insts {
                self.inst(mf, &name, inst);
            }
        }
        let _ = writeln!(self.out, "\t.size {}, .-{}", name, name);
        if !mf.consts.is_empty() {
            self.out.push_str("\t.section .rodata\n");
            for (i, c) in mf.consts.iter().enumerate() {
                let _ = writeln!(self.out, "\t.p2align {}", log2(c.align as u64));
                let _ = writeln!(self.out, ".LC{}_{}:", name, i);
                let bytes: Vec<String> = c.bytes.iter().map(|b| b.to_string()).collect();
                let _ = writeln!(self.out, "\t.byte {}", bytes.join(","));
            }
        }
        self.out.push('\n');
    }

    fn prologue(&mut self, mf: &MFunc) {
        self.out.push_str("\tpushq %rbp\n\tmovq %rsp, %rbp\n");
        for &r in &mf.used_callee_saved {
            let _ = writeln!(self.out, "\tpushq {}", reg_name(r, Sz::Q));
        }
        if mf.frame_size > 0 {
            let _ = writeln!(self.out, "\tsubq ${}, %rsp", mf.frame_size);
        }
        if let Some(slot) = mf.regsave_slot {
            let base = -mf.slots[slot as usize].offset;
            for (i, r) in ARG_GPRS.iter().enumerate() {
                let _ = writeln!(self.out, "\tmovq {}, {}(%rbp)", reg_name(*r, Sz::Q), base + 8 * i as i32);
            }
            let name = mf.name.to_string();
            let _ = writeln!(self.out, "\ttestb %al, %al");
            let _ = writeln!(self.out, "\tje .L{}_novec", name);
            for i in 0..8 {
                let _ = writeln!(self.out, "\tmovaps %xmm{}, {}(%rbp)", i, base + 48 + 16 * i);
            }
            let _ = writeln!(self.out, ".L{}_novec:", name);
        }
    }

    /// Restore callee-saved registers and the caller's frame (everything but the `ret`).
    fn leave_frame(&mut self, mf: &MFunc) {
        let k = mf.used_callee_saved.len() as i32;
        if k == 0 {
            self.out.push_str("\tleave\n");
        } else {
            let _ = writeln!(self.out, "\tleaq {}(%rbp), %rsp", -8 * k);
            for &r in mf.used_callee_saved.iter().rev() {
                let _ = writeln!(self.out, "\tpopq {}", reg_name(r, Sz::Q));
            }
            self.out.push_str("\tpopq %rbp\n");
        }
    }

    fn epilogue(&mut self, mf: &MFunc) {
        self.leave_frame(mf);
        self.out.push_str("\tret\n");
    }

    // ───────────────────────────── operands ─────────────────────────────

    fn reg(&self, r: &Reg, sz: Sz) -> String {
        match r {
            Reg::P(p) => reg_name(*p, sz),
            Reg::V(v) => format!("%v{}", v), // only seen if allocation was skipped
        }
    }

    fn mem(&self, mf: &MFunc, fname: &str, m: &Mem) -> String {
        let mut base = String::new();
        let mut disp = m.disp as i64;
        let mut prefix = String::new();
        match m.base {
            Base::None => {}
            Base::Reg(r) => base = self.reg(&r, Sz::Q),
            Base::Slot(s) => {
                disp -= mf.slots[s as usize].offset as i64;
                base = "%rbp".to_string();
            }
            Base::Incoming => {
                disp += 16;
                base = "%rbp".to_string();
            }
            Base::Outgoing => base = "%rsp".to_string(),
            Base::Sym(s) => {
                prefix = self.sym_name(s);
                if m.index.is_none() {
                    base = "%rip".to_string();
                }
            }
            Base::Const(i) => {
                prefix = format!(".LC{}_{}", fname, i);
                if m.index.is_none() {
                    base = "%rip".to_string();
                }
            }
        }
        let mut s = String::new();
        if !prefix.is_empty() {
            s.push_str(&prefix);
            if disp != 0 {
                let _ = write!(s, "{:+}", disp);
            }
        } else if disp != 0 || (base.is_empty() && m.index.is_none()) {
            let _ = write!(s, "{}", disp);
        }
        match (base.is_empty(), m.index) {
            (true, None) => {}
            (_, None) => {
                let _ = write!(s, "({})", base);
            }
            (_, Some((idx, scale))) => {
                let _ = write!(s, "({},{},{})", base, self.reg(&idx, Sz::Q), scale);
            }
        }
        s
    }

    fn operand(&self, mf: &MFunc, fname: &str, o: &MOp, sz: Sz) -> String {
        match o {
            MOp::None => String::new(),
            MOp::Reg(r) => self.reg(r, sz),
            MOp::Imm(c) => format!("${}", c),
            MOp::Mem(m) => self.mem(mf, fname, m),
        }
    }

    // ───────────────────────────── instructions ─────────────────────────────

    fn inst(&mut self, mf: &MFunc, fname: &str, i: &MInst) {
        let sx = i.sz.suffix();
        let sz = i.sz;
        let d = self.operand(mf, fname, &i.dst, sz);
        let s = self.operand(mf, fname, &i.src, sz);
        let two = |name: &str| format!("\t{}{} {}, {}\n", name, sx, s, d);
        let fsfx = if sz == Sz::L { "ss" } else { "sd" };
        let line = match &i.op {
            Op::Mov => {
                if let (MOp::Reg(r), MOp::Imm(c)) = (&i.dst, &i.src) {
                    // 64-bit immediates that do not fit a sign-extended imm32
                    if sz == Sz::Q && i32::try_from(*c).is_err() {
                        if u32::try_from(*c).is_ok() {
                            format!("\tmovl ${}, {}\n", c, self.reg(r, Sz::L))
                        } else {
                            format!("\tmovabsq ${}, {}\n", c, self.reg(r, Sz::Q))
                        }
                    } else {
                        two("mov")
                    }
                } else {
                    two("mov")
                }
            }
            Op::MovZX(from) => {
                let sfx = if sz == Sz::Q { 'q' } else { 'l' };
                let src = self.operand(mf, fname, &i.src, *from);
                let dst = self.operand(mf, fname, &i.dst, sz);
                format!("\tmovz{}{} {}, {}\n", from.suffix(), sfx, src, dst)
            }
            Op::MovSX(from) => {
                let sfx = if sz == Sz::Q { 'q' } else { 'l' };
                let src = self.operand(mf, fname, &i.src, *from);
                let dst = self.operand(mf, fname, &i.dst, sz);
                format!("\tmovs{}{} {}, {}\n", from.suffix(), sfx, src, dst)
            }
            Op::Lea => format!("\tlea{} {}, {}\n", sx, s, d),
            Op::Add => two("add"),
            Op::Sub => two("sub"),
            Op::And => two("and"),
            Op::Or => two("or"),
            Op::Xor => two("xor"),
            Op::Imul => two("imul"),
            Op::Cmp => two("cmp"),
            Op::Test => two("test"),
            Op::Neg => format!("\tneg{} {}\n", sx, d),
            Op::Not => format!("\tnot{} {}\n", sx, d),
            Op::Shl | Op::Shr | Op::Sar => {
                let name = match i.op {
                    Op::Shl => "shl",
                    Op::Shr => "shr",
                    _ => "sar",
                };
                let count = match &i.src {
                    MOp::Imm(c) => format!("${}", c),
                    _ => "%cl".to_string(),
                };
                format!("\t{}{} {}, {}\n", name, sx, count, d)
            }
            Op::Idiv => format!("\tidiv{} {}\n", sx, s),
            Op::Div => format!("\tdiv{} {}\n", sx, s),
            Op::SignExtAccum => (if sz == Sz::Q { "\tcqto\n" } else { "\tcltd\n" }).to_string(),
            Op::SetCC(cc) => format!("\tset{} {}\n", cc.suffix(), self.operand(mf, fname, &i.dst, Sz::B)),
            Op::CMov(cc) => format!("\tcmov{} {}, {}\n", cc.suffix(), s, d),
            Op::Jmp(t) => format!("\tjmp .L{}_{}\n", fname, t),
            Op::Jcc(cc, t) => format!("\tj{} .L{}_{}\n", cc.suffix(), fname, t),
            Op::Call(info) => {
                let target = match &info.target {
                    Target::Sym(s) => {
                        let name = self.sym_name(*s);
                        let defined = matches!(self.m.syms[s.idx()].body, SymBody::Func { defined: true });
                        if defined {
                            name
                        } else {
                            format!("{}@PLT", name)
                        }
                    }
                    Target::Reg(r) => format!("*{}", self.reg(r, Sz::Q)),
                };
                format!("\tcall {}\n", target)
            }
            Op::TailCall(info) => {
                let target = match &info.target {
                    Target::Sym(s) => {
                        let name = self.sym_name(*s);
                        let defined = matches!(self.m.syms[s.idx()].body, SymBody::Func { defined: true });
                        if defined {
                            name
                        } else {
                            format!("{}@PLT", name)
                        }
                    }
                    Target::Reg(r) => format!("*{}", self.reg(r, Sz::Q)),
                };
                self.leave_frame(mf);
                format!("\tjmp {}\n", target)
            }
            Op::Ret => {
                self.epilogue(mf);
                return;
            }
            Op::Ud2 => "\tud2\n".to_string(),
            Op::MovF => format!("\tmov{} {}, {}\n", fsfx, s, d),
            Op::MovAps => format!("\tmovaps {}, {}\n", s, d),
            Op::MovUps => format!("\tmovups {}, {}\n", s, d),
            Op::ZeroF => format!("\txorps {}, {}\n", d, d),
            Op::FAdd => format!("\tadd{} {}, {}\n", fsfx, s, d),
            Op::FSub => format!("\tsub{} {}, {}\n", fsfx, s, d),
            Op::FMul => format!("\tmul{} {}, {}\n", fsfx, s, d),
            Op::FDiv => format!("\tdiv{} {}\n", fsfx, format_args!("{}, {}", s, d)),
            Op::Ucomi => format!("\tucomi{} {}, {}\n", fsfx, s, d),
            Op::Xorps => format!("\txorps {}, {}\n", s, d),
            Op::CvtSi2F(src_sz) => {
                let src = self.operand(mf, fname, &i.src, *src_sz);
                format!("\tcvtsi2{}{} {}, {}\n", fsfx, src_sz.suffix(), src, d)
            }
            Op::CvtF2Si(dst_sz) => {
                let dst = self.operand(mf, fname, &i.dst, *dst_sz);
                format!("\tcvtt{}2si {}, {}\n", fsfx, s, dst)
            }
            Op::CvtF2F => {
                if sz == Sz::Q {
                    format!("\tcvtss2sd {}, {}\n", s, d)
                } else {
                    format!("\tcvtsd2ss {}, {}\n", s, d)
                }
            }
            Op::MovGX => format!("\tmov{} {}, {}\n", if sz == Sz::Q { 'q' } else { 'd' }, s, d),
            Op::MovXG => format!("\tmov{} {}, {}\n", if sz == Sz::Q { 'q' } else { 'd' }, s, d),
            Op::RepMovsb => "\trep movsb\n".to_string(),
            Op::RepStosb => "\trep stosb\n".to_string(),
            Op::Loc(l) => {
                if self.emit_loc {
                    format!("\t.loc 1 {}\n", l)
                } else {
                    String::new()
                }
            }
        };
        self.out.push_str(&line);
    }

    // ───────────────────────────── data ─────────────────────────────

    pub fn data(&mut self) {
        let m = self.m;
        for sym in &m.syms {
            let SymBody::Data(Some(def)) = &sym.body else { continue };
            self.data_symbol(&sym.name.to_string(), sym.linkage, def);
        }
    }

    fn data_symbol(&mut self, name: &str, linkage: Linkage, def: &DataDef) {
        let section = if def.zero {
            ".bss"
        } else if def.readonly {
            ".rodata"
        } else {
            ".data"
        };
        let _ = writeln!(self.out, "\t.section {}", section);
        if linkage == Linkage::External {
            let _ = writeln!(self.out, "\t.globl {}", name);
        }
        let _ = writeln!(self.out, "\t.type {}, @object", name);
        let _ = writeln!(self.out, "\t.size {}, {}", name, def.size);
        let _ = writeln!(self.out, "\t.p2align {}", log2(def.align));
        let _ = writeln!(self.out, "{}:", name);
        if def.zero {
            let _ = writeln!(self.out, "\t.zero {}", def.size);
        } else {
            let mut written = 0u64;
            for item in &def.items {
                match item {
                    DataItem::Bytes(b) => {
                        written += b.len() as u64;
                        let printable = b.iter().filter(|&&c| (32..127).contains(&c) || c == b'\n' || c == 0).count();
                        if b.len() >= 4 && printable * 4 >= b.len() * 3 {
                            let _ = writeln!(self.out, "\t.ascii \"{}\"", escape_ascii(b));
                        } else {
                            for chunk in b.chunks(16) {
                                let v: Vec<String> = chunk.iter().map(|x| x.to_string()).collect();
                                let _ = writeln!(self.out, "\t.byte {}", v.join(","));
                            }
                        }
                    }
                    DataItem::Zero(n) => {
                        written += n;
                        let _ = writeln!(self.out, "\t.zero {}", n);
                    }
                    DataItem::Addr { sym, offset } => {
                        written += 8;
                        let target = self.sym_name(*sym);
                        if *offset == 0 {
                            let _ = writeln!(self.out, "\t.quad {}", target);
                        } else {
                            let _ = writeln!(self.out, "\t.quad {}{:+}", target, offset);
                        }
                    }
                }
            }
            if written < def.size {
                let _ = writeln!(self.out, "\t.zero {}", def.size - written);
            }
        }
        self.out.push('\n');
    }
}
