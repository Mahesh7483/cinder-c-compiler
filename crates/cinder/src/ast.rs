//! Syntax tree produced by the parser.
//!
//! This is purely *syntactic*: declaration specifiers and declarators are kept
//! as written (typedef names are not resolved, no types are computed).
//! Semantic analysis turns this into the typed HIR.

use crate::intern::Symbol;
use crate::literal::StrKind;
use crate::source::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ident {
    pub name: Symbol,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quals {
    pub is_const: bool,
    pub is_volatile: bool,
    pub is_restrict: bool,
    pub is_atomic: bool,
}

impl Quals {
    pub fn is_empty(&self) -> bool {
        !(self.is_const || self.is_volatile || self.is_restrict || self.is_atomic)
    }

    pub fn union(self, o: Quals) -> Quals {
        Quals {
            is_const: self.is_const | o.is_const,
            is_volatile: self.is_volatile | o.is_volatile,
            is_restrict: self.is_restrict | o.is_restrict,
            is_atomic: self.is_atomic | o.is_atomic,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageClass {
    Typedef,
    Extern,
    Static,
    Auto,
    Register,
}

/// Basic arithmetic/void types after combining specifier keywords
/// (`unsigned long int` → `ULong`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseType {
    Void,
    Bool,
    Char,
    SChar,
    UChar,
    Short,
    UShort,
    Int,
    UInt,
    Long,
    ULong,
    LongLong,
    ULongLong,
    Float,
    Double,
    LongDouble,
    /// `__builtin_va_list`
    VaList,
}

#[derive(Clone, Debug)]
pub struct Attr {
    pub name: Ident,
    pub args: Vec<Expr>,
}

#[derive(Clone, Debug)]
pub struct TypeSpec {
    pub kind: TypeSpecKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TypeSpecKind {
    Base(BaseType),
    Record(RecordSpec),
    Enum(EnumSpec),
    Typedef(Ident),
    Atomic(Box<TypeName>),
}

#[derive(Clone, Debug)]
pub enum AlignSpec {
    Type(Box<TypeName>),
    Expr(Box<Expr>),
}

#[derive(Clone, Debug, Default)]
pub struct DeclSpecs {
    pub span: Span,
    pub storage: Option<(StorageClass, Span)>,
    pub thread_local: bool,
    pub quals: Quals,
    pub inline: bool,
    pub noreturn: bool,
    pub align: Option<(AlignSpec, Span)>,
    /// `None` when the declaration has no type specifier (implicit `int`).
    pub ty: Option<TypeSpec>,
    pub attrs: Vec<Attr>,
}

#[derive(Clone, Debug)]
pub struct RecordSpec {
    pub is_union: bool,
    pub tag: Option<Ident>,
    /// `None`: a reference (`struct S`); `Some`: a definition (`struct S { ... }`).
    pub members: Option<Vec<MemberDecl>>,
    /// `#pragma pack` value in effect where the struct was written.
    pub pack: Option<u32>,
    pub attrs: Vec<Attr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum MemberDecl {
    Field { specs: DeclSpecs, declarators: Vec<MemberDeclarator>, span: Span },
    StaticAssert(StaticAssert),
}

#[derive(Clone, Debug)]
pub struct MemberDeclarator {
    pub declarator: Option<Declarator>,
    pub bit_width: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct EnumSpec {
    pub tag: Option<Ident>,
    pub enumerators: Option<Vec<Enumerator>>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Enumerator {
    pub name: Ident,
    pub value: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct StaticAssert {
    pub cond: Expr,
    pub message: Option<String>,
    pub span: Span,
}

// ───────────────────────────── declarators ─────────────────────────────

/// A declarator in "inside-out" form: `pointers` apply to the base type
/// first, then `suffixes` right-to-left, then `inner` is applied to that
/// result. `int *(*fp)(int)` is `pointers=[*]`, `inner=Nested(*fp)`,
/// `suffixes=[(int)]`.
#[derive(Clone, Debug)]
pub struct Declarator {
    pub span: Span,
    pub pointers: Vec<PointerQuals>,
    pub inner: DeclaratorCore,
    pub suffixes: Vec<Suffix>,
    pub attrs: Vec<Attr>,
}

#[derive(Clone, Debug)]
pub struct PointerQuals {
    pub quals: Quals,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum DeclaratorCore {
    /// No name (abstract declarator).
    Abstract,
    Name(Ident),
    Nested(Box<Declarator>),
}

#[derive(Clone, Debug)]
pub enum Suffix {
    Array { size: ArraySize, quals: Quals, is_static: bool, span: Span },
    Function { params: Vec<ParamDecl>, variadic: bool, span: Span },
}

#[derive(Clone, Debug)]
pub enum ArraySize {
    /// `[]`
    Unspecified,
    /// `[*]` in a prototype
    Star,
    Expr(Box<Expr>),
}

#[derive(Clone, Debug)]
pub struct ParamDecl {
    pub specs: DeclSpecs,
    pub declarator: Declarator,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeName {
    pub specs: DeclSpecs,
    pub declarator: Declarator,
    pub span: Span,
}

impl Declarator {
    /// The identifier being declared, found by looking through nesting.
    pub fn name(&self) -> Option<Ident> {
        match &self.inner {
            DeclaratorCore::Name(i) => Some(*i),
            DeclaratorCore::Nested(d) => d.name(),
            DeclaratorCore::Abstract => None,
        }
    }

    /// Does the declared identifier have function type? `f(int)` and
    /// `(*pick(int))(int)` do; `(*fp)(int)` (a pointer to function) does not.
    pub fn is_function(&self) -> bool {
        self.outer_kind() == DerivedKind::Function
    }

    /// The outermost type constructor applied at the declared name, following
    /// the inside-out application order: pointers, then suffixes (the first
    /// suffix in source order is applied last), then the nested declarator.
    pub fn outer_kind(&self) -> DerivedKind {
        let own = match self.suffixes.first() {
            Some(Suffix::Function { .. }) => DerivedKind::Function,
            Some(Suffix::Array { .. }) => DerivedKind::Array,
            None if !self.pointers.is_empty() => DerivedKind::Pointer,
            None => DerivedKind::Base,
        };
        match &self.inner {
            DeclaratorCore::Nested(n) => match n.outer_kind() {
                DerivedKind::Base => own,
                k => k,
            },
            _ => own,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DerivedKind {
    /// Nothing derived: the declarator names a value of the base type.
    Base,
    Pointer,
    Array,
    Function,
}

// ───────────────────────────── expressions ─────────────────────────────

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Plus,
    Neg,
    BitNot,
    LogNot,
    Deref,
    AddrOf,
    PreInc,
    PreDec,
    PostInc,
    PostDec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,
    BitAnd,
    BitXor,
    BitOr,
    LogAnd,
    LogOr,
}

impl BinOp {
    pub fn spelling(self) -> &'static str {
        use BinOp::*;
        match self {
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Rem => "%",
            Shl => "<<",
            Shr => ">>",
            Lt => "<",
            Gt => ">",
            Le => "<=",
            Ge => ">=",
            Eq => "==",
            Ne => "!=",
            BitAnd => "&",
            BitXor => "^",
            BitOr => "|",
            LogAnd => "&&",
            LogOr => "||",
        }
    }
}

impl UnOp {
    pub fn spelling(self) -> &'static str {
        use UnOp::*;
        match self {
            Plus => "+",
            Neg => "-",
            BitNot => "~",
            LogNot => "!",
            Deref => "*",
            AddrOf => "&",
            PreInc | PostInc => "++",
            PreDec | PostDec => "--",
        }
    }
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    /// Spelling of an integer constant (validated by sema).
    IntLit(Symbol),
    FloatLit(Symbol),
    CharLit(Symbol),
    /// Adjacent string literals already concatenated and decoded.
    StrLit {
        kind: StrKind,
        units: Vec<u32>,
    },
    Ident(Symbol),
    Paren(Box<Expr>),
    Unary {
        op: UnOp,
        operand: Box<Expr>,
        op_span: Span,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        op_span: Span,
    },
    /// `lhs op= rhs`; `op` is `None` for plain `=`.
    Assign {
        op: Option<BinOp>,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        op_span: Span,
    },
    Cond {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Box<Expr>,
    },
    Comma(Box<Expr>, Box<Expr>),
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    Member {
        base: Box<Expr>,
        member: Ident,
        arrow: bool,
    },
    Cast {
        ty: Box<TypeName>,
        operand: Box<Expr>,
    },
    SizeofExpr(Box<Expr>),
    SizeofType(Box<TypeName>),
    AlignofType(Box<TypeName>),
    CompoundLiteral {
        ty: Box<TypeName>,
        init: InitList,
    },
    Generic {
        controlling: Box<Expr>,
        assocs: Vec<GenericAssoc>,
    },
    VaArg {
        ap: Box<Expr>,
        ty: Box<TypeName>,
    },
    Offsetof {
        ty: Box<TypeName>,
        path: Vec<OffsetofStep>,
    },
}

#[derive(Clone, Debug)]
pub struct GenericAssoc {
    /// `None` for `default:`.
    pub ty: Option<TypeName>,
    pub expr: Expr,
}

#[derive(Clone, Debug)]
pub enum OffsetofStep {
    Field(Ident),
    Index(Expr),
}

// ───────────────────────────── initializers ─────────────────────────────

#[derive(Clone, Debug)]
pub enum Initializer {
    Expr(Expr),
    List(InitList),
}

impl Initializer {
    pub fn span(&self) -> Span {
        match self {
            Initializer::Expr(e) => e.span,
            Initializer::List(l) => l.span,
        }
    }
}

#[derive(Clone, Debug)]
pub struct InitList {
    pub items: Vec<InitItem>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct InitItem {
    pub designators: Vec<Designator>,
    pub init: Initializer,
}

#[derive(Clone, Debug)]
pub enum Designator {
    Field(Ident),
    Index(Expr),
}

// ───────────────────────────── statements ─────────────────────────────

#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum BlockItem {
    Decl(Declaration),
    Stmt(Stmt),
}

#[derive(Clone, Debug)]
pub enum ForInit {
    None,
    Expr(Expr),
    Decl(Declaration),
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum StmtKind {
    Empty,
    Expr(Expr),
    Compound(Vec<BlockItem>),
    If { cond: Expr, then: Box<Stmt>, els: Option<Box<Stmt>> },
    While { cond: Expr, body: Box<Stmt> },
    DoWhile { body: Box<Stmt>, cond: Expr },
    For { init: ForInit, cond: Option<Expr>, step: Option<Expr>, body: Box<Stmt> },
    Switch { cond: Expr, body: Box<Stmt> },
    Case { value: Expr, body: Box<Stmt> },
    Default { body: Box<Stmt> },
    Break,
    Continue,
    Return(Option<Expr>),
    Goto(Ident),
    Label { name: Ident, body: Box<Stmt> },
    StaticAssert(StaticAssert),
}

// ───────────────────────────── declarations ─────────────────────────────

#[derive(Clone, Debug)]
pub struct Declaration {
    pub specs: DeclSpecs,
    pub declarators: Vec<InitDeclarator>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct InitDeclarator {
    pub declarator: Declarator,
    pub init: Option<Initializer>,
}

#[derive(Clone, Debug)]
pub struct FuncDef {
    pub specs: DeclSpecs,
    pub declarator: Declarator,
    pub body: Stmt,
    pub span: Span,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ExternalDecl {
    Decl(Declaration),
    Func(FuncDef),
    StaticAssert(StaticAssert),
}

#[derive(Clone, Debug, Default)]
pub struct TranslationUnit {
    pub decls: Vec<ExternalDecl>,
}
