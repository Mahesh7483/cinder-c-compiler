//! The SSA intermediate representation.
//!
//! A [`Module`] holds symbols (functions and data) and function bodies. A
//! [`Func`] is a CFG of basic blocks; instructions live in an arena
//! (`Func::insts`) and blocks list them by id. Every value is defined exactly
//! once, either by an instruction or as a parameter, and phi instructions
//! merge values at control-flow joins.
//!
//! Lowering emits locals as `alloca` + `load`/`store`; the `mem2reg` pass
//! turns promotable allocas into SSA values. Aggregates (structs, unions,
//! arrays) never appear as SSA values: they live in memory and are moved
//! with `memcpy`.

pub mod cfg;
pub mod print;
pub mod verify;

use crate::intern::Symbol;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        pub struct $name(pub u32);
        impl $name {
            pub fn idx(self) -> usize {
                self.0 as usize
            }
        }
    };
}

id_type!(BlockId);
id_type!(InstId);
id_type!(ValueId);
id_type!(SymId);

/// Scalar IR types. `Ptr` is 64-bit; aggregates have no IR type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Type {
    I8,
    I16,
    I32,
    I64,
    Ptr,
    F32,
    F64,
}

impl Type {
    pub fn size(self) -> u32 {
        match self {
            Type::I8 => 1,
            Type::I16 => 2,
            Type::I32 | Type::F32 => 4,
            Type::I64 | Type::Ptr | Type::F64 => 8,
        }
    }

    pub fn bits(self) -> u32 {
        self.size() * 8
    }

    pub fn is_int(self) -> bool {
        matches!(self, Type::I8 | Type::I16 | Type::I32 | Type::I64)
    }

    pub fn is_float(self) -> bool {
        matches!(self, Type::F32 | Type::F64)
    }

    /// Integer or pointer (lives in a general-purpose register).
    pub fn is_gpr(self) -> bool {
        !self.is_float()
    }

    pub fn from_int_bits(bits: u32) -> Type {
        match bits {
            8 => Type::I8,
            16 => Type::I16,
            32 => Type::I32,
            _ => Type::I64,
        }
    }
}

/// An instruction operand.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Operand {
    Value(ValueId),
    /// Integer constant of the given type (value sign-extended to 64 bits).
    Int(i64, Type),
    /// Floating constant (f64 bit pattern; `F32` values are pre-rounded).
    Float(u64, Type),
    /// Address of a symbol (a `Ptr`).
    Global(SymId),
    /// An unspecified value.
    Undef(Type),
}

impl Operand {
    pub fn value(self) -> Option<ValueId> {
        match self {
            Operand::Value(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_const(self) -> bool {
        !matches!(self, Operand::Value(_))
    }

    pub fn int(v: i64, ty: Type) -> Operand {
        Operand::Int(v, ty)
    }

    pub fn float(v: f64, ty: Type) -> Operand {
        let v = if ty == Type::F32 { (v as f32) as f64 } else { v };
        Operand::Float(v.to_bits(), ty)
    }

    pub fn as_int(self) -> Option<i64> {
        match self {
            Operand::Int(v, _) => Some(v),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    SDiv,
    UDiv,
    SRem,
    URem,
    And,
    Or,
    Xor,
    Shl,
    LShr,
    AShr,
    FAdd,
    FSub,
    FMul,
    FDiv,
}

impl BinOp {
    pub fn is_float(self) -> bool {
        matches!(self, BinOp::FAdd | BinOp::FSub | BinOp::FMul | BinOp::FDiv)
    }

    pub fn is_commutative(self) -> bool {
        matches!(self, BinOp::Add | BinOp::Mul | BinOp::And | BinOp::Or | BinOp::Xor | BinOp::FAdd | BinOp::FMul)
    }

    pub fn name(self) -> &'static str {
        match self {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::SDiv => "sdiv",
            BinOp::UDiv => "udiv",
            BinOp::SRem => "srem",
            BinOp::URem => "urem",
            BinOp::And => "and",
            BinOp::Or => "or",
            BinOp::Xor => "xor",
            BinOp::Shl => "shl",
            BinOp::LShr => "lshr",
            BinOp::AShr => "ashr",
            BinOp::FAdd => "fadd",
            BinOp::FSub => "fsub",
            BinOp::FMul => "fmul",
            BinOp::FDiv => "fdiv",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum UnOp {
    Neg,
    Not,
    FNeg,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum IPred {
    Eq,
    Ne,
    Slt,
    Sle,
    Sgt,
    Sge,
    Ult,
    Ule,
    Ugt,
    Uge,
}

impl IPred {
    pub fn name(self) -> &'static str {
        match self {
            IPred::Eq => "eq",
            IPred::Ne => "ne",
            IPred::Slt => "slt",
            IPred::Sle => "sle",
            IPred::Sgt => "sgt",
            IPred::Sge => "sge",
            IPred::Ult => "ult",
            IPred::Ule => "ule",
            IPred::Ugt => "ugt",
            IPred::Uge => "uge",
        }
    }

    pub fn swapped(self) -> IPred {
        match self {
            IPred::Eq | IPred::Ne => self,
            IPred::Slt => IPred::Sgt,
            IPred::Sle => IPred::Sge,
            IPred::Sgt => IPred::Slt,
            IPred::Sge => IPred::Sle,
            IPred::Ult => IPred::Ugt,
            IPred::Ule => IPred::Uge,
            IPred::Ugt => IPred::Ult,
            IPred::Uge => IPred::Ule,
        }
    }

    pub fn negated(self) -> IPred {
        match self {
            IPred::Eq => IPred::Ne,
            IPred::Ne => IPred::Eq,
            IPred::Slt => IPred::Sge,
            IPred::Sle => IPred::Sgt,
            IPred::Sgt => IPred::Sle,
            IPred::Sge => IPred::Slt,
            IPred::Ult => IPred::Uge,
            IPred::Ule => IPred::Ugt,
            IPred::Ugt => IPred::Ule,
            IPred::Uge => IPred::Ult,
        }
    }
}

/// Floating-point predicates with C semantics (`==` is false and `!=` true on NaN).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FPred {
    /// ordered equal
    Oeq,
    /// unordered or not equal
    Une,
    Olt,
    Ole,
    Ogt,
    Oge,
}

impl FPred {
    pub fn name(self) -> &'static str {
        match self {
            FPred::Oeq => "oeq",
            FPred::Une => "une",
            FPred::Olt => "olt",
            FPred::Ole => "ole",
            FPred::Ogt => "ogt",
            FPred::Oge => "oge",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CastOp {
    ZExt,
    SExt,
    Trunc,
    SIToFP,
    UIToFP,
    FPToSI,
    FPToUI,
    FPExt,
    FPTrunc,
    PtrToInt,
    IntToPtr,
}

impl CastOp {
    pub fn name(self) -> &'static str {
        match self {
            CastOp::ZExt => "zext",
            CastOp::SExt => "sext",
            CastOp::Trunc => "trunc",
            CastOp::SIToFP => "sitofp",
            CastOp::UIToFP => "uitofp",
            CastOp::FPToSI => "fptosi",
            CastOp::FPToUI => "fptoui",
            CastOp::FPExt => "fpext",
            CastOp::FPTrunc => "fptrunc",
            CastOp::PtrToInt => "ptrtoint",
            CastOp::IntToPtr => "inttoptr",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Callee {
    Direct(SymId),
    Indirect(Operand),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArgKind {
    /// A scalar (or one eightbyte of an aggregate) passed per its class.
    Value,
    /// An aggregate copied onto the stack; `val` is a pointer to it.
    ByVal { size: u32, align: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CallArg {
    pub val: Operand,
    pub kind: ArgKind,
    /// Pieces of one aggregate share a group: if the group does not fit in
    /// the remaining registers, all of its pieces go to the stack.
    pub group: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InstKind {
    /// Static stack slot; result is its address.
    Alloca {
        size: u32,
        align: u32,
    },
    Load {
        ty: Type,
        ptr: Operand,
        volatile: bool,
    },
    Store {
        ty: Type,
        val: Operand,
        ptr: Operand,
        volatile: bool,
    },
    MemCopy {
        dst: Operand,
        src: Operand,
        size: u64,
        align: u32,
    },
    MemSet {
        dst: Operand,
        byte: u8,
        size: u64,
        align: u32,
    },
    Bin {
        op: BinOp,
        ty: Type,
        lhs: Operand,
        rhs: Operand,
    },
    Un {
        op: UnOp,
        ty: Type,
        val: Operand,
    },
    /// Result is `i32` 0/1.
    ICmp {
        pred: IPred,
        ty: Type,
        lhs: Operand,
        rhs: Operand,
    },
    FCmp {
        pred: FPred,
        ty: Type,
        lhs: Operand,
        rhs: Operand,
    },
    Cast {
        op: CastOp,
        from: Type,
        to: Type,
        val: Operand,
    },
    /// `base + offset` (bytes); `offset` is an `i64`.
    PtrAdd {
        base: Operand,
        offset: Operand,
    },
    Select {
        ty: Type,
        cond: Operand,
        a: Operand,
        b: Operand,
    },
    Phi {
        ty: Type,
        incoming: Vec<(BlockId, Operand)>,
    },
    /// `dst` (and `dst2` for two-register returns) receive `rets` in order.
    Call {
        callee: Callee,
        args: Vec<CallArg>,
        rets: Vec<Type>,
        variadic: bool,
        tail: bool,
    },
    /// Address of the register save area of a variadic function.
    VaRegSave,
    /// Address of the first stack-passed argument of the current function.
    VaStackArgs,
    Trap,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Inst {
    pub kind: InstKind,
    pub dst: Option<ValueId>,
    pub dst2: Option<ValueId>,
    /// 1-based source line (0 = none).
    pub line: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Term {
    /// Under construction; rejected by the verifier.
    None,
    Br(BlockId),
    CondBr {
        cond: Operand,
        then_bb: BlockId,
        else_bb: BlockId,
    },
    Switch {
        ty: Type,
        val: Operand,
        cases: Vec<(i64, BlockId)>,
        default: BlockId,
    },
    Ret(Vec<Operand>),
    Unreachable,
}

impl Term {
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Term::Br(b) => vec![*b],
            Term::CondBr { then_bb, else_bb, .. } => vec![*then_bb, *else_bb],
            Term::Switch { cases, default, .. } => {
                let mut v: Vec<BlockId> = cases.iter().map(|c| c.1).collect();
                v.push(*default);
                v
            }
            Term::Ret(_) | Term::Unreachable | Term::None => Vec::new(),
        }
    }

    pub fn operands(&self) -> Vec<Operand> {
        match self {
            Term::CondBr { cond, .. } => vec![*cond],
            Term::Switch { val, .. } => vec![*val],
            Term::Ret(v) => v.clone(),
            _ => Vec::new(),
        }
    }

    pub fn is_terminated(&self) -> bool {
        !matches!(self, Term::None)
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    pub name: String,
    pub insts: Vec<InstId>,
    pub term: Term,
    pub term_line: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParamKind {
    Value(Type),
    /// An aggregate passed on the stack; the parameter value is a pointer to it.
    ByVal {
        size: u32,
        align: u32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub kind: ParamKind,
    pub group: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueDef {
    Inst(InstId),
    Param(u32),
}

#[derive(Clone, Debug)]
pub struct ValueData {
    pub ty: Type,
    pub name: Option<Symbol>,
    pub def: ValueDef,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Linkage {
    External,
    Internal,
}

#[derive(Clone, Debug)]
pub struct Func {
    pub name: Symbol,
    pub sym: SymId,
    pub linkage: Linkage,
    pub params: Vec<Param>,
    pub param_values: Vec<ValueId>,
    pub rets: Vec<Type>,
    pub variadic: bool,
    pub blocks: Vec<Block>,
    pub insts: Vec<Inst>,
    pub values: Vec<ValueData>,
    /// Instructions removed from blocks stay in the arena; this marks them.
    pub dead_insts: Vec<bool>,
    pub is_inline: bool,
    pub noreturn: bool,
    /// Line of the function definition.
    pub line: u32,
}

impl Func {
    pub fn new(name: Symbol, sym: SymId, linkage: Linkage) -> Func {
        Func {
            name,
            sym,
            linkage,
            params: Vec::new(),
            param_values: Vec::new(),
            rets: Vec::new(),
            variadic: false,
            blocks: Vec::new(),
            insts: Vec::new(),
            values: Vec::new(),
            dead_insts: Vec::new(),
            is_inline: false,
            noreturn: false,
            line: 0,
        }
    }

    pub fn entry(&self) -> BlockId {
        BlockId(0)
    }

    pub fn new_block(&mut self, name: &str) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(Block { name: name.to_string(), insts: Vec::new(), term: Term::None, term_line: 0 });
        id
    }

    pub fn new_value(&mut self, ty: Type, name: Option<Symbol>, def: ValueDef) -> ValueId {
        let id = ValueId(self.values.len() as u32);
        self.values.push(ValueData { ty, name, def });
        id
    }

    pub fn add_param(&mut self, kind: ParamKind, group: Option<u32>, name: Option<Symbol>) -> ValueId {
        let ty = match &kind {
            ParamKind::Value(t) => *t,
            ParamKind::ByVal { .. } => Type::Ptr,
        };
        let idx = self.params.len() as u32;
        let v = self.new_value(ty, name, ValueDef::Param(idx));
        self.params.push(Param { kind, group });
        self.param_values.push(v);
        v
    }

    fn new_inst(&mut self, kind: InstKind, line: u32) -> InstId {
        let id = InstId(self.insts.len() as u32);
        self.insts.push(Inst { kind, dst: None, dst2: None, line });
        self.dead_insts.push(false);
        id
    }

    /// Append an instruction producing a value of type `ty` (or none).
    pub fn push(
        &mut self,
        b: BlockId,
        kind: InstKind,
        ty: Option<Type>,
        name: Option<Symbol>,
        line: u32,
    ) -> Option<Operand> {
        let id = self.new_inst(kind, line);
        let dst = ty.map(|t| {
            let v = self.new_value(t, name, ValueDef::Inst(id));
            self.insts[id.idx()].dst = Some(v);
            v
        });
        self.blocks[b.idx()].insts.push(id);
        dst.map(Operand::Value)
    }

    /// Insert an instruction at position `pos` of block `b`.
    pub fn insert(
        &mut self,
        b: BlockId,
        pos: usize,
        kind: InstKind,
        ty: Option<Type>,
        name: Option<Symbol>,
        line: u32,
    ) -> Option<Operand> {
        let id = self.new_inst(kind, line);
        let dst = ty.map(|t| {
            let v = self.new_value(t, name, ValueDef::Inst(id));
            self.insts[id.idx()].dst = Some(v);
            v
        });
        self.blocks[b.idx()].insts.insert(pos, id);
        dst.map(Operand::Value)
    }

    pub fn operand_ty(&self, o: Operand) -> Type {
        match o {
            Operand::Value(v) => self.values[v.idx()].ty,
            Operand::Int(_, t) | Operand::Float(_, t) | Operand::Undef(t) => t,
            Operand::Global(_) => Type::Ptr,
        }
    }

    pub fn inst(&self, id: InstId) -> &Inst {
        &self.insts[id.idx()]
    }

    /// The instruction defining a value, if it is not a parameter.
    pub fn def_inst(&self, v: ValueId) -> Option<InstId> {
        match self.values[v.idx()].def {
            ValueDef::Inst(i) => Some(i),
            ValueDef::Param(_) => None,
        }
    }

    pub fn is_phi(&self, id: InstId) -> bool {
        matches!(self.insts[id.idx()].kind, InstKind::Phi { .. })
    }

    /// Remove an instruction from its block (it stays in the arena, marked dead).
    pub fn kill(&mut self, id: InstId) {
        self.dead_insts[id.idx()] = true;
    }

    /// Drop dead instructions from block lists.
    pub fn sweep(&mut self) {
        let dead = &self.dead_insts;
        for b in &mut self.blocks {
            b.insts.retain(|i| !dead[i.idx()]);
        }
    }

    pub fn num_insts(&self) -> usize {
        self.blocks.iter().map(|b| b.insts.len()).sum()
    }
}

// ───────────────────────────── instruction operand access ─────────────────────────────

impl InstKind {
    /// Visit every operand.
    pub fn for_each_operand(&self, mut f: impl FnMut(Operand)) {
        match self {
            InstKind::Alloca { .. } | InstKind::VaRegSave | InstKind::VaStackArgs | InstKind::Trap => {}
            InstKind::Load { ptr, .. } => f(*ptr),
            InstKind::Store { val, ptr, .. } => {
                f(*val);
                f(*ptr);
            }
            InstKind::MemCopy { dst, src, .. } => {
                f(*dst);
                f(*src);
            }
            InstKind::MemSet { dst, .. } => f(*dst),
            InstKind::Bin { lhs, rhs, .. } | InstKind::ICmp { lhs, rhs, .. } | InstKind::FCmp { lhs, rhs, .. } => {
                f(*lhs);
                f(*rhs);
            }
            InstKind::Un { val, .. } | InstKind::Cast { val, .. } => f(*val),
            InstKind::PtrAdd { base, offset } => {
                f(*base);
                f(*offset);
            }
            InstKind::Select { cond, a, b, .. } => {
                f(*cond);
                f(*a);
                f(*b);
            }
            InstKind::Phi { incoming, .. } => {
                for (_, o) in incoming {
                    f(*o);
                }
            }
            InstKind::Call { callee, args, .. } => {
                if let Callee::Indirect(o) = callee {
                    f(*o);
                }
                for a in args {
                    f(a.val);
                }
            }
        }
    }

    /// Mutably visit every operand (for rewriting uses).
    pub fn for_each_operand_mut(&mut self, mut f: impl FnMut(&mut Operand)) {
        match self {
            InstKind::Alloca { .. } | InstKind::VaRegSave | InstKind::VaStackArgs | InstKind::Trap => {}
            InstKind::Load { ptr, .. } => f(ptr),
            InstKind::Store { val, ptr, .. } => {
                f(val);
                f(ptr);
            }
            InstKind::MemCopy { dst, src, .. } => {
                f(dst);
                f(src);
            }
            InstKind::MemSet { dst, .. } => f(dst),
            InstKind::Bin { lhs, rhs, .. } | InstKind::ICmp { lhs, rhs, .. } | InstKind::FCmp { lhs, rhs, .. } => {
                f(lhs);
                f(rhs);
            }
            InstKind::Un { val, .. } | InstKind::Cast { val, .. } => f(val),
            InstKind::PtrAdd { base, offset } => {
                f(base);
                f(offset);
            }
            InstKind::Select { cond, a, b, .. } => {
                f(cond);
                f(a);
                f(b);
            }
            InstKind::Phi { incoming, .. } => {
                for (_, o) in incoming {
                    f(o);
                }
            }
            InstKind::Call { callee, args, .. } => {
                if let Callee::Indirect(o) = callee {
                    f(o);
                }
                for a in args {
                    f(&mut a.val);
                }
            }
        }
    }

    /// Does this instruction have effects beyond producing its value?
    pub fn has_side_effects(&self) -> bool {
        match self {
            InstKind::Store { .. }
            | InstKind::MemCopy { .. }
            | InstKind::MemSet { .. }
            | InstKind::Call { .. }
            | InstKind::Trap => true,
            InstKind::Load { volatile, .. } => *volatile,
            _ => false,
        }
    }

    pub fn is_pure(&self) -> bool {
        matches!(
            self,
            InstKind::Bin { .. }
                | InstKind::Un { .. }
                | InstKind::ICmp { .. }
                | InstKind::FCmp { .. }
                | InstKind::Cast { .. }
                | InstKind::PtrAdd { .. }
                | InstKind::Select { .. }
        )
    }
}

// ───────────────────────────── module ─────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum DataItem {
    Bytes(Vec<u8>),
    Zero(u64),
    /// 8-byte address of a symbol plus offset (needs a relocation).
    Addr {
        sym: SymId,
        offset: i64,
    },
}

#[derive(Clone, Debug)]
pub struct DataDef {
    pub size: u64,
    pub align: u64,
    pub items: Vec<DataItem>,
    pub readonly: bool,
    /// Entirely zero: goes in `.bss` / `.comm`.
    pub zero: bool,
}

#[derive(Clone, Debug)]
pub enum SymBody {
    /// Function with a body in `Module::funcs`.
    Func { defined: bool },
    /// `None` = declared but not defined here.
    Data(Option<DataDef>),
}

#[derive(Clone, Debug)]
pub struct Sym {
    pub name: Symbol,
    pub linkage: Linkage,
    pub body: SymBody,
}

#[derive(Default)]
pub struct Module {
    pub syms: Vec<Sym>,
    pub funcs: Vec<Func>,
    pub source_file: String,
}

impl Module {
    pub fn sym(&self, id: SymId) -> &Sym {
        &self.syms[id.idx()]
    }

    pub fn func_for(&self, id: SymId) -> Option<&Func> {
        self.funcs.iter().find(|f| f.sym == id)
    }
}
