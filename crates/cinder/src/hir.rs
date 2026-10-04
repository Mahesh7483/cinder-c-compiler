//! Typed high-level IR produced by semantic analysis.
//!
//! Compared to the syntax tree, the HIR is fully resolved and explicit:
//! every identifier refers to a local or global entity, every expression has
//! a type, and *all* implicit conversions are spelled out as [`HExprKind::Cast`]
//! nodes (lvalue-to-rvalue, array/function decay, usual arithmetic
//! conversions, assignment conversions). Operands of binary operators already
//! have identical types, pointer arithmetic is a separate node with the
//! element size attached, and initializers are flattened into byte-offset
//! entries. Lowering to SSA IR is therefore mostly mechanical.

use crate::intern::Symbol;
use crate::literal::StrKind;
use crate::source::Span;
use crate::types::{BitInfo, Ty, TypeTable};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct LocalId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct SymId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LabelId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CaseId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StrId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Linkage {
    External,
    Internal,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymKind {
    Var,
    Func,
}

#[derive(Clone, Debug)]
pub struct GlobalSym {
    /// Assembler symbol name.
    pub name: Symbol,
    pub kind: SymKind,
    pub ty: Ty,
    pub linkage: Linkage,
    /// A definition exists in this translation unit (function body, initialized
    /// variable, or tentative definition).
    pub defined: bool,
    /// `int x;` at file scope with no initializer anywhere.
    pub tentative: bool,
    pub init: Option<InitPlan>,
    pub is_const: bool,
    pub align: u64,
    pub span: Span,
    pub used: bool,
    pub is_inline: bool,
    pub noreturn: bool,
}

#[derive(Clone, Debug)]
pub struct StrData {
    pub kind: StrKind,
    /// Code units without the terminating NUL.
    pub units: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct Local {
    pub name: Symbol,
    pub ty: Ty,
    pub span: Span,
    pub is_param: bool,
    pub used: bool,
    pub align: u64,
}

pub struct HirModule {
    pub types: TypeTable,
    pub syms: Vec<GlobalSym>,
    pub funcs: Vec<HirFunc>,
    pub strings: Vec<StrData>,
}

pub struct HirFunc {
    pub sym: SymId,
    pub name: Symbol,
    pub params: Vec<LocalId>,
    pub locals: Vec<Local>,
    pub body: HStmt,
    pub ret: Ty,
    pub variadic: bool,
    /// Number of named labels (for diagnostics/dumps).
    pub labels: Vec<Symbol>,
    pub span: Span,
    pub is_static: bool,
    pub is_inline: bool,
    pub noreturn: bool,
}

// ───────────────────────────── initializers ─────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum AddrBase {
    Global(SymId),
    Str(StrId),
}

/// A value known at compile time.
#[derive(Clone, Debug, PartialEq)]
pub enum ConstVal {
    /// Integer/pointer bit pattern. Signed values are sign-extended to 64 bits.
    Int(u64),
    Float(f64),
    /// Address of a global/string plus a byte offset.
    Addr {
        base: AddrBase,
        offset: i64,
    },
}

#[derive(Clone, Debug)]
pub enum InitValue {
    Expr(HExpr),
    Const(ConstVal),
    /// Raw bytes (a string literal initializing a char array, already sized).
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct InitEntry {
    pub offset: u64,
    /// Type of the stored scalar (or the whole array for `Bytes`).
    pub ty: Ty,
    pub bit: Option<BitInfo>,
    pub value: InitValue,
}

#[derive(Clone, Debug, Default)]
pub struct InitPlan {
    pub entries: Vec<InitEntry>,
    /// True when entries do not cover the whole object, so it must be zeroed first.
    pub needs_zero: bool,
    pub size: u64,
}

// ───────────────────────────── expressions ─────────────────────────────

#[derive(Clone, Debug)]
pub struct HExpr {
    pub kind: HExprKind,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CastKind {
    /// Read the value from a place. For aggregates the "value" is the place's address.
    LValueToRValue,
    ArrayToPointer,
    FunctionToPointer,
    /// Integer width/sign conversion (also bool/enum/char sources).
    IntToInt,
    /// Any scalar `!= 0` into `_Bool`.
    ToBool,
    IntToFloat,
    FloatToInt,
    FloatToFloat,
    PtrToInt,
    IntToPtr,
    /// Pointer to pointer, or a qualification/identity change: no code.
    NoOp,
    ToVoid,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnKind {
    Neg,
    BitNot,
    /// `!x`: yields `int` 0/1.
    LogNot,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinKind {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl BinKind {
    pub fn is_comparison(self) -> bool {
        matches!(self, BinKind::Eq | BinKind::Ne | BinKind::Lt | BinKind::Gt | BinKind::Le | BinKind::Ge)
    }

    pub fn spelling(self) -> &'static str {
        match self {
            BinKind::Add => "+",
            BinKind::Sub => "-",
            BinKind::Mul => "*",
            BinKind::Div => "/",
            BinKind::Rem => "%",
            BinKind::And => "&",
            BinKind::Or => "|",
            BinKind::Xor => "^",
            BinKind::Shl => "<<",
            BinKind::Shr => ">>",
            BinKind::Eq => "==",
            BinKind::Ne => "!=",
            BinKind::Lt => "<",
            BinKind::Gt => ">",
            BinKind::Le => "<=",
            BinKind::Ge => ">=",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberRef {
    /// Byte offset of the field (of the containing byte for bit-fields).
    pub offset: u64,
    pub bit: Option<BitInfo>,
}

#[derive(Clone, Debug)]
pub enum HExprKind {
    /// Integer (or null-pointer) constant; bits are truncated to `ty`'s width.
    Int(u64),
    Float(f64),
    /// String literal: an lvalue of array type.
    Str(StrId),
    Local(LocalId),
    Global(SymId),
    /// `*p`: a place of the pointee type.
    Deref(Box<HExpr>),
    /// `base.field`; a place if `base` is.
    Member(Box<HExpr>, MemberRef),
    /// `(T){ ... }` with automatic storage: initialises `local`, then is a place.
    CompoundLit {
        local: LocalId,
        init: InitPlan,
    },
    Cast(CastKind, Box<HExpr>),
    Unary(UnKind, Box<HExpr>),
    /// Operands have identical (arithmetic or pointer) types; comparisons yield `int`.
    /// For shifts the right operand keeps its own promoted type.
    Binary(BinKind, Box<HExpr>, Box<HExpr>),
    /// `ptr + idx * scale` (or `-` when `negate`); `idx` is `long`.
    PtrAdd {
        ptr: Box<HExpr>,
        idx: Box<HExpr>,
        scale: u64,
        negate: bool,
    },
    /// `(l - r) / elem_size`, a `long`.
    PtrDiff {
        l: Box<HExpr>,
        r: Box<HExpr>,
        elem_size: u64,
    },
    LogAnd(Box<HExpr>, Box<HExpr>),
    LogOr(Box<HExpr>, Box<HExpr>),
    Cond(Box<HExpr>, Box<HExpr>, Box<HExpr>),
    Comma(Box<HExpr>, Box<HExpr>),
    /// `place = value`; yields the stored value. `value` already has the place's type.
    Assign(Box<HExpr>, Box<HExpr>),
    /// `place op= value`. The operation is performed in `calc` (arithmetic) or
    /// as pointer arithmetic (when `place` is a pointer): the stored result is
    /// converted back to the place's type.
    CompoundAssign {
        op: BinKind,
        place: Box<HExpr>,
        value: Box<HExpr>,
        calc: Ty,
    },
    IncDec {
        place: Box<HExpr>,
        is_inc: bool,
        is_prefix: bool,
    },
    Call {
        callee: Box<HExpr>,
        args: Vec<HExpr>,
    },
    AddrOf(Box<HExpr>),
    VaStart(Box<HExpr>),
    VaEnd(Box<HExpr>),
    VaCopy(Box<HExpr>, Box<HExpr>),
    VaArg(Box<HExpr>),
    /// `__builtin_trap()` / `__builtin_unreachable()`
    Trap,
    /// An expression that failed to type-check (already diagnosed).
    Error,
}

impl HExpr {
    pub fn new(kind: HExprKind, ty: Ty, span: Span) -> HExpr {
        HExpr { kind, ty, span }
    }

    /// Is this expression a place (lvalue) that can be loaded from / stored to?
    pub fn is_place(&self) -> bool {
        match &self.kind {
            HExprKind::Local(_)
            | HExprKind::Global(_)
            | HExprKind::Deref(_)
            | HExprKind::Str(_)
            | HExprKind::CompoundLit { .. } => true,
            HExprKind::Member(b, _) => b.is_place(),
            _ => false,
        }
    }
}

// ───────────────────────────── statements ─────────────────────────────

#[derive(Clone, Debug)]
pub struct HStmt {
    pub kind: HStmtKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum HStmtKind {
    Empty,
    Expr(HExpr),
    /// Definition of an automatic variable, with an optional initializer.
    Decl {
        local: LocalId,
        init: Option<InitPlan>,
    },
    Block(Vec<HStmt>),
    If(HExpr, Box<HStmt>, Option<Box<HStmt>>),
    While(HExpr, Box<HStmt>),
    DoWhile(Box<HStmt>, HExpr),
    For {
        init: Vec<HStmt>,
        cond: Option<HExpr>,
        step: Option<HExpr>,
        body: Box<HStmt>,
    },
    Switch {
        cond: HExpr,
        body: Box<HStmt>,
        cases: Vec<(i64, CaseId)>,
        default: Option<CaseId>,
    },
    /// Position of a `case`/`default` label inside a switch body.
    CaseLabel(CaseId),
    Break,
    Continue,
    Return(Option<HExpr>),
    Goto(LabelId),
    Label(LabelId),
}
