//! Machine IR: x86-64 instructions over virtual and physical registers.
//!
//! Instruction selection produces MIR with virtual registers; register
//! allocation rewrites them to physical registers (inserting spill code);
//! emission prints AT&T syntax. Operand order follows AT&T: `src, dst`.

use crate::intern::Symbol;
use crate::ir::{IPred, Linkage, SymId, Type};
use std::collections::HashMap;

/// A register: virtual (to be allocated) or physical (0..16 general purpose,
/// 16..32 are `xmm0..xmm15`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Reg {
    V(u32),
    P(u8),
}

pub const RAX: u8 = 0;
pub const RCX: u8 = 1;
pub const RDX: u8 = 2;
pub const RBX: u8 = 3;
pub const RSP: u8 = 4;
pub const RBP: u8 = 5;
pub const RSI: u8 = 6;
pub const RDI: u8 = 7;
pub const R8: u8 = 8;
pub const R9: u8 = 9;
pub const R10: u8 = 10;
pub const R11: u8 = 11;
pub const R12: u8 = 12;
pub const R13: u8 = 13;
pub const R14: u8 = 14;
pub const R15: u8 = 15;
pub const XMM0: u8 = 16;

pub const ARG_GPRS: [u8; 6] = [RDI, RSI, RDX, RCX, R8, R9];

pub const fn xmm(n: u8) -> u8 {
    XMM0 + n
}

pub fn is_xmm(p: u8) -> bool {
    p >= XMM0
}

pub fn is_caller_saved(p: u8) -> bool {
    !matches!(p, RBX | RBP | RSP | R12 | R13 | R14 | R15)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Gpr,
    Xmm,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sz {
    B,
    W,
    L,
    Q,
}

impl Sz {
    pub fn of(t: Type) -> Sz {
        match t {
            Type::I8 => Sz::B,
            Type::I16 => Sz::W,
            Type::I32 | Type::F32 => Sz::L,
            Type::I64 | Type::Ptr | Type::F64 => Sz::Q,
        }
    }

    pub fn bytes(self) -> u32 {
        match self {
            Sz::B => 1,
            Sz::W => 2,
            Sz::L => 4,
            Sz::Q => 8,
        }
    }

    pub fn suffix(self) -> char {
        match self {
            Sz::B => 'b',
            Sz::W => 'w',
            Sz::L => 'l',
            Sz::Q => 'q',
        }
    }
}

/// Name of a physical register at the given operand size.
pub fn reg_name(p: u8, sz: Sz) -> String {
    const Q: [&str; 16] =
        ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];
    const L: [&str; 16] = [
        "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d", "r13d", "r14d",
        "r15d",
    ];
    const W: [&str; 16] =
        ["ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w", "r14w", "r15w"];
    const B: [&str; 16] = [
        "al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b", "r13b", "r14b",
        "r15b",
    ];
    if is_xmm(p) {
        return format!("%xmm{}", p - XMM0);
    }
    let i = p as usize;
    format!(
        "%{}",
        match sz {
            Sz::Q => Q[i],
            Sz::L => L[i],
            Sz::W => W[i],
            Sz::B => B[i],
        }
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Base {
    None,
    Reg(Reg),
    /// A frame slot (locals, spills); resolved to `-N(%rbp)` after layout.
    Slot(u32),
    /// Incoming stack arguments: `16 + disp(%rbp)`.
    Incoming,
    /// Outgoing argument area: `disp(%rsp)`.
    Outgoing,
    /// Just above the (16-byte rounded) outgoing area: where dynamically
    /// allocated stack memory starts. `disp + round16(outgoing)(%rsp)`.
    OutgoingTop,
    /// A symbol, addressed rip-relative.
    Sym(SymId),
    /// A floating-point/mask constant from the function's constant pool.
    Const(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mem {
    pub base: Base,
    pub index: Option<(Reg, u8)>,
    pub disp: i32,
}

impl Mem {
    pub fn reg(r: Reg) -> Mem {
        Mem { base: Base::Reg(r), index: None, disp: 0 }
    }

    pub fn slot(s: u32) -> Mem {
        Mem { base: Base::Slot(s), index: None, disp: 0 }
    }

    pub fn offset(self, d: i32) -> Mem {
        Mem { disp: self.disp.wrapping_add(d), ..self }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MOp {
    None,
    Reg(Reg),
    Imm(i64),
    Mem(Mem),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cc {
    E,
    Ne,
    L,
    Le,
    G,
    Ge,
    B,
    Be,
    A,
    Ae,
    S,
    Ns,
    P,
    Np,
}

impl Cc {
    pub fn from_ipred(p: IPred) -> Cc {
        match p {
            IPred::Eq => Cc::E,
            IPred::Ne => Cc::Ne,
            IPred::Slt => Cc::L,
            IPred::Sle => Cc::Le,
            IPred::Sgt => Cc::G,
            IPred::Sge => Cc::Ge,
            IPred::Ult => Cc::B,
            IPred::Ule => Cc::Be,
            IPred::Ugt => Cc::A,
            IPred::Uge => Cc::Ae,
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Cc::E => "e",
            Cc::Ne => "ne",
            Cc::L => "l",
            Cc::Le => "le",
            Cc::G => "g",
            Cc::Ge => "ge",
            Cc::B => "b",
            Cc::Be => "be",
            Cc::A => "a",
            Cc::Ae => "ae",
            Cc::S => "s",
            Cc::Ns => "ns",
            Cc::P => "p",
            Cc::Np => "np",
        }
    }

    pub fn negate(self) -> Cc {
        match self {
            Cc::E => Cc::Ne,
            Cc::Ne => Cc::E,
            Cc::L => Cc::Ge,
            Cc::Ge => Cc::L,
            Cc::Le => Cc::G,
            Cc::G => Cc::Le,
            Cc::B => Cc::Ae,
            Cc::Ae => Cc::B,
            Cc::Be => Cc::A,
            Cc::A => Cc::Be,
            Cc::S => Cc::Ns,
            Cc::Ns => Cc::S,
            Cc::P => Cc::Np,
            Cc::Np => Cc::P,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum Target {
    Sym(SymId),
    Reg(Reg),
}

#[derive(Clone, PartialEq, Debug)]
pub struct CallInfo {
    pub target: Target,
    /// Argument registers read by the call.
    pub uses: Vec<u8>,
    /// Result registers written by the call.
    pub defs: Vec<u8>,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Op {
    // data movement
    Mov,
    /// zero-extending move; `sz` is the destination size, the payload the source size
    MovZX(Sz),
    MovSX(Sz),
    Lea,
    // integer ALU, two-address: `dst = dst op src`
    Add,
    Sub,
    And,
    Or,
    Xor,
    Imul,
    Cmp,
    Test,
    Neg,
    Not,
    Shl,
    Shr,
    Sar,
    Idiv,
    Div,
    /// sign-extend `eax`/`rax` into `edx`/`rdx` (`cltd`/`cqto`)
    SignExtAccum,
    SetCC(Cc),
    CMov(Cc),
    // control flow
    Jmp(usize),
    Jcc(Cc, usize),
    Call(Box<CallInfo>),
    /// A call in tail position: restore the frame, then `jmp` to the target.
    TailCall(Box<CallInfo>),
    Ret,
    Ud2,
    // scalar floating point (`sz` = L for single, Q for double)
    MovF,
    MovAps,
    /// unaligned 128-bit move (`movups`)
    MovUps,
    /// set an XMM register to zero (`xorps r, r`); a pure definition
    ZeroF,
    FAdd,
    FSub,
    FMul,
    FDiv,
    Ucomi,
    Xorps,
    /// int -> float: `sz` is the integer source size
    CvtSi2F(Sz),
    /// float -> int (truncating): `sz` is the integer destination size
    CvtF2Si(Sz),
    /// float <-> float: `sz` is the destination precision
    CvtF2F,
    /// move 32/64 bits between a GPR and an XMM register
    MovGX,
    MovXG,
    // block moves
    RepMovsb,
    RepStosb,
    /// `.loc` source line marker
    Loc(u32),
}

#[derive(Clone, PartialEq, Debug)]
pub struct MInst {
    pub op: Op,
    pub sz: Sz,
    pub dst: MOp,
    pub src: MOp,
}

impl MInst {
    pub fn new(op: Op, sz: Sz, dst: MOp, src: MOp) -> MInst {
        MInst { op, sz, dst, src }
    }

    pub fn bare(op: Op) -> MInst {
        MInst { op, sz: Sz::Q, dst: MOp::None, src: MOp::None }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Acc {
    Use,
    Def,
    UseDef,
}

fn mem_regs(m: &Mem, f: &mut impl FnMut(Reg, Acc)) {
    if let Base::Reg(r) = m.base {
        f(r, Acc::Use);
    }
    if let Some((r, _)) = m.index {
        f(r, Acc::Use);
    }
}

fn mem_regs_mut(m: &mut Mem, f: &mut impl FnMut(&mut Reg, Acc)) {
    if let Base::Reg(r) = &mut m.base {
        f(r, Acc::Use);
    }
    if let Some((r, _)) = &mut m.index {
        f(r, Acc::Use);
    }
}

/// How the destination operand is accessed when it is a register.
fn dst_access(op: &Op) -> Option<Acc> {
    use Op::*;
    match op {
        Mov | MovZX(_) | MovSX(_) | Lea | MovF | MovAps | MovUps | ZeroF | CvtSi2F(_) | CvtF2Si(_) | CvtF2F | MovGX
        | MovXG | SetCC(_) => Some(Acc::Def),
        Add | Sub | And | Or | Xor | Imul | Neg | Not | Shl | Shr | Sar | CMov(_) | FAdd | FSub | FMul | FDiv
        | Xorps => Some(Acc::UseDef),
        Cmp | Test | Ucomi => Some(Acc::Use),
        Idiv | Div | SignExtAccum | Jmp(_) | Jcc(..) | Call(_) | TailCall(_) | Ret | Ud2 | RepMovsb | RepStosb
        | Loc(_) => None,
    }
}

impl MInst {
    pub fn for_each_reg(&self, f: &mut impl FnMut(Reg, Acc)) {
        if let Op::Call(info) | Op::TailCall(info) = &self.op {
            if let Target::Reg(r) = &info.target {
                f(*r, Acc::Use);
            }
        }
        match &self.dst {
            MOp::Reg(r) => {
                if let Some(a) = dst_access(&self.op) {
                    f(*r, a)
                }
            }
            MOp::Mem(m) => mem_regs(m, f),
            _ => {}
        }
        match &self.src {
            MOp::Reg(r) => f(*r, Acc::Use),
            MOp::Mem(m) => mem_regs(m, f),
            _ => {}
        }
    }

    pub fn for_each_reg_mut(&mut self, f: &mut impl FnMut(&mut Reg, Acc)) {
        if let Op::Call(info) | Op::TailCall(info) = &mut self.op {
            if let Target::Reg(r) = &mut info.target {
                f(r, Acc::Use);
            }
        }
        let acc = dst_access(&self.op);
        match &mut self.dst {
            MOp::Reg(r) => {
                if let Some(a) = acc {
                    f(r, a)
                }
            }
            MOp::Mem(m) => mem_regs_mut(m, f),
            _ => {}
        }
        match &mut self.src {
            MOp::Reg(r) => f(r, Acc::Use),
            MOp::Mem(m) => mem_regs_mut(m, f),
            _ => {}
        }
    }

    /// Physical registers read/written implicitly, and whether all
    /// caller-saved registers are clobbered.
    pub fn implicit(&self) -> (Vec<u8>, Vec<u8>, bool) {
        match &self.op {
            Op::Idiv | Op::Div => (vec![RAX, RDX], vec![RAX, RDX], false),
            Op::SignExtAccum => (vec![RAX], vec![RDX], false),
            Op::RepMovsb => (vec![RDI, RSI, RCX], vec![RDI, RSI, RCX], false),
            Op::RepStosb => (vec![RDI, RAX, RCX], vec![RDI, RCX], false),
            Op::Call(info) => (info.uses.clone(), info.defs.clone(), true),
            // control never comes back, so nothing is clobbered for later code
            Op::TailCall(info) => (info.uses.clone(), Vec::new(), false),
            _ => (Vec::new(), Vec::new(), false),
        }
    }

    pub fn is_terminator(&self) -> bool {
        matches!(self.op, Op::Jmp(_) | Op::Ret | Op::Ud2 | Op::TailCall(_))
    }
}

#[derive(Debug, Default)]
pub struct MBlock {
    pub insts: Vec<MInst>,
    pub succs: Vec<usize>,
}

#[derive(Clone, Debug)]
pub struct SlotInfo {
    pub size: u32,
    pub align: u32,
    /// Offset below `%rbp` (positive number; the slot is at `-offset(%rbp)`).
    pub offset: i32,
}

#[derive(Clone, Debug)]
pub struct ConstEntry {
    pub bytes: Vec<u8>,
    pub align: u32,
}

pub struct MFunc {
    pub name: Symbol,
    pub linkage: Linkage,
    pub blocks: Vec<MBlock>,
    /// Emission order of `blocks` (instruction selection may add blocks that
    /// belong in the middle of the function).
    pub layout: Vec<usize>,
    pub vclass: Vec<Class>,
    pub slots: Vec<SlotInfo>,
    pub consts: Vec<ConstEntry>,
    /// Bytes of outgoing stack-argument space needed by calls.
    pub outgoing: u32,
    pub variadic: bool,
    /// Slot of the register save area (variadic functions).
    pub regsave_slot: Option<u32>,
    /// Preferred registers for virtual registers (copy coalescing hints).
    pub hints: HashMap<u32, Reg>,
    pub used_callee_saved: Vec<u8>,
    /// Total bytes subtracted from `%rsp` in the prologue (after pushes).
    pub frame_size: u32,
    /// The function allocates stack memory at run time (VLAs): `%rsp` moves, so
    /// the frame reserves a 16-byte-rounded outgoing area below the locals.
    pub dyn_alloca: bool,
}

impl MFunc {
    pub fn new_vreg(&mut self, c: Class) -> Reg {
        let id = self.vclass.len() as u32;
        self.vclass.push(c);
        Reg::V(id)
    }

    pub fn new_slot(&mut self, size: u32, align: u32) -> u32 {
        let id = self.slots.len() as u32;
        self.slots.push(SlotInfo { size: size.max(1), align: align.max(1), offset: 0 });
        id
    }
}
