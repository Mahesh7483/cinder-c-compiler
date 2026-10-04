//! Textual rendering of the syntax tree: an indented tree for `--emit-ast`,
//! plus compact C-like text for declarators/types and S-expressions for
//! expressions (used by tests and as node labels).

use crate::ast::*;
use crate::literal::StrKind;
use crate::source::{SourceMap, Span};

// ───────────────────────────── text helpers ─────────────────────────────

pub fn quals_text(q: &Quals) -> String {
    let mut v = Vec::new();
    if q.is_const {
        v.push("const");
    }
    if q.is_volatile {
        v.push("volatile");
    }
    if q.is_restrict {
        v.push("restrict");
    }
    if q.is_atomic {
        v.push("_Atomic");
    }
    v.join(" ")
}

pub fn base_name(b: BaseType) -> &'static str {
    use BaseType::*;
    match b {
        Void => "void",
        Bool => "_Bool",
        Char => "char",
        SChar => "signed char",
        UChar => "unsigned char",
        Short => "short",
        UShort => "unsigned short",
        Int => "int",
        UInt => "unsigned int",
        Long => "long",
        ULong => "unsigned long",
        LongLong => "long long",
        ULongLong => "unsigned long long",
        Float => "float",
        Double => "double",
        LongDouble => "long double",
        VaList => "__builtin_va_list",
    }
}

pub fn specs_text(s: &DeclSpecs) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some((sc, _)) = s.storage {
        parts.push(
            match sc {
                StorageClass::Typedef => "typedef",
                StorageClass::Extern => "extern",
                StorageClass::Static => "static",
                StorageClass::Auto => "auto",
                StorageClass::Register => "register",
            }
            .to_string(),
        );
    }
    if s.thread_local {
        parts.push("_Thread_local".into());
    }
    if s.inline {
        parts.push("inline".into());
    }
    if s.noreturn {
        parts.push("_Noreturn".into());
    }
    let q = quals_text(&s.quals);
    if !q.is_empty() {
        parts.push(q);
    }
    match &s.ty {
        Some(t) => parts.push(typespec_text(&t.kind)),
        None => parts.push("<implicit int>".into()),
    }
    parts.join(" ")
}

fn typespec_text(k: &TypeSpecKind) -> String {
    match k {
        TypeSpecKind::Base(b) => base_name(*b).to_string(),
        TypeSpecKind::Record(r) => {
            let kw = if r.is_union { "union" } else { "struct" };
            match &r.tag {
                Some(t) => format!("{} {}", kw, t.name),
                None => format!("{} <anonymous>", kw),
            }
        }
        TypeSpecKind::Enum(e) => match &e.tag {
            Some(t) => format!("enum {}", t.name),
            None => "enum <anonymous>".into(),
        },
        TypeSpecKind::Typedef(i) => i.name.to_string(),
        TypeSpecKind::Atomic(t) => format!("_Atomic({})", type_name_text(t)),
    }
}

/// C-like spelling of a declarator: `*(*fp)(int, char)`, `a[3][4]`.
pub fn declarator_text(d: &Declarator) -> String {
    let mut s = String::new();
    for p in &d.pointers {
        s.push('*');
        let q = quals_text(&p.quals);
        if !q.is_empty() {
            s.push_str(&q);
            s.push(' ');
        }
    }
    match &d.inner {
        DeclaratorCore::Abstract => {}
        DeclaratorCore::Name(i) => s.push_str(i.name.as_str()),
        DeclaratorCore::Nested(n) => {
            s.push('(');
            s.push_str(&declarator_text(n));
            s.push(')');
        }
    }
    for suf in &d.suffixes {
        match suf {
            Suffix::Array { size, is_static, quals, .. } => {
                s.push('[');
                if *is_static {
                    s.push_str("static ");
                }
                let q = quals_text(quals);
                if !q.is_empty() {
                    s.push_str(&q);
                    s.push(' ');
                }
                match size {
                    ArraySize::Unspecified => {}
                    ArraySize::Star => s.push('*'),
                    ArraySize::Expr(e) => s.push_str(&sexp(e)),
                }
                s.push(']');
            }
            Suffix::Function { params, variadic, .. } => {
                s.push('(');
                let mut ps: Vec<String> = params
                    .iter()
                    .map(|p| {
                        let d = declarator_text(&p.declarator);
                        if d.is_empty() {
                            specs_text(&p.specs)
                        } else {
                            format!("{} {}", specs_text(&p.specs), d)
                        }
                    })
                    .collect();
                if *variadic {
                    ps.push("...".into());
                }
                s.push_str(&ps.join(", "));
                s.push(')');
            }
        }
    }
    s
}

pub fn type_name_text(t: &TypeName) -> String {
    let d = declarator_text(&t.declarator);
    if d.is_empty() {
        specs_text(&t.specs)
    } else {
        format!("{} {}", specs_text(&t.specs), d)
    }
}

pub fn str_lit_text(kind: StrKind, units: &[u32]) -> String {
    let prefix = match kind {
        StrKind::Plain => "",
        StrKind::Utf8 => "u8",
        StrKind::Utf16 => "u",
        StrKind::Utf32 => "U",
        StrKind::Wide => "L",
    };
    let mut s = String::from(prefix);
    s.push('"');
    for &u in units {
        match u {
            10 => s.push_str("\\n"),
            9 => s.push_str("\\t"),
            0 => s.push_str("\\0"),
            34 => s.push_str("\\\""),
            92 => s.push_str("\\\\"),
            32..=126 => s.push(u as u8 as char),
            _ => s.push_str(&format!("\\x{:x}", u)),
        }
    }
    s.push('"');
    s
}

/// S-expression rendering of an expression (parentheses are transparent).
pub fn sexp(e: &Expr) -> String {
    match &e.kind {
        ExprKind::IntLit(s) | ExprKind::FloatLit(s) | ExprKind::CharLit(s) => s.as_str().to_string(),
        ExprKind::StrLit { kind, units } => str_lit_text(*kind, units),
        ExprKind::Ident(n) => n.as_str().to_string(),
        ExprKind::Paren(inner) => sexp(inner),
        ExprKind::Unary { op, operand, .. } => {
            let name = match op {
                UnOp::PreInc => "pre++",
                UnOp::PreDec => "pre--",
                UnOp::PostInc => "post++",
                UnOp::PostDec => "post--",
                UnOp::Deref => "deref",
                UnOp::AddrOf => "addr",
                UnOp::Neg => "neg",
                UnOp::Plus => "pos",
                UnOp::BitNot => "~",
                UnOp::LogNot => "!",
            };
            format!("({} {})", name, sexp(operand))
        }
        ExprKind::Binary { op, lhs, rhs, .. } => format!("({} {} {})", op.spelling(), sexp(lhs), sexp(rhs)),
        ExprKind::Assign { op, lhs, rhs, .. } => {
            let s = match op {
                None => "=".to_string(),
                Some(o) => format!("{}=", o.spelling()),
            };
            format!("({} {} {})", s, sexp(lhs), sexp(rhs))
        }
        ExprKind::Cond { cond, then, els } => format!("(?: {} {} {})", sexp(cond), sexp(then), sexp(els)),
        ExprKind::Comma(a, b) => format!("(, {} {})", sexp(a), sexp(b)),
        ExprKind::Call { callee, args } => {
            let mut s = format!("(call {}", sexp(callee));
            for a in args {
                s.push(' ');
                s.push_str(&sexp(a));
            }
            s.push(')');
            s
        }
        ExprKind::Index { base, index } => format!("([] {} {})", sexp(base), sexp(index)),
        ExprKind::Member { base, member, arrow } => {
            format!("({} {} {})", if *arrow { "->" } else { "." }, sexp(base), member.name)
        }
        ExprKind::Cast { ty, operand } => format!("(cast <{}> {})", type_name_text(ty), sexp(operand)),
        ExprKind::SizeofExpr(x) => format!("(sizeof {})", sexp(x)),
        ExprKind::SizeofType(t) => format!("(sizeof <{}>)", type_name_text(t)),
        ExprKind::AlignofType(t) => format!("(alignof <{}>)", type_name_text(t)),
        ExprKind::CompoundLiteral { ty, init } => format!("(complit <{}> {})", type_name_text(ty), init_text(init)),
        ExprKind::Generic { controlling, assocs } => {
            let mut s = format!("(_Generic {}", sexp(controlling));
            for a in assocs {
                match &a.ty {
                    Some(t) => s.push_str(&format!(" <{}>:{}", type_name_text(t), sexp(&a.expr))),
                    None => s.push_str(&format!(" default:{}", sexp(&a.expr))),
                }
            }
            s.push(')');
            s
        }
        ExprKind::VaArg { ap, ty } => format!("(va_arg {} <{}>)", sexp(ap), type_name_text(ty)),
        ExprKind::Offsetof { ty, path } => {
            let mut s = format!("(offsetof <{}>", type_name_text(ty));
            for p in path {
                match p {
                    OffsetofStep::Field(i) => s.push_str(&format!(" .{}", i.name)),
                    OffsetofStep::Index(e) => s.push_str(&format!(" [{}]", sexp(e))),
                }
            }
            s.push(')');
            s
        }
    }
}

pub fn init_text(l: &InitList) -> String {
    let items: Vec<String> = l
        .items
        .iter()
        .map(|it| {
            let mut s = String::new();
            for d in &it.designators {
                match d {
                    Designator::Field(i) => s.push_str(&format!(".{}", i.name)),
                    Designator::Index(e) => s.push_str(&format!("[{}]", sexp(e))),
                }
            }
            if !it.designators.is_empty() {
                s.push('=');
            }
            match &it.init {
                Initializer::Expr(e) => s.push_str(&sexp(e)),
                Initializer::List(l) => s.push_str(&init_text(l)),
            }
            s
        })
        .collect();
    format!("{{{}}}", items.join(", "))
}

// ───────────────────────────── tree dump ─────────────────────────────

struct Node {
    label: String,
    children: Vec<Node>,
}

impl Node {
    fn new(label: impl Into<String>) -> Node {
        Node { label: label.into(), children: Vec::new() }
    }

    fn with(mut self, c: Node) -> Node {
        self.children.push(c);
        self
    }
}

struct Dumper<'a> {
    sm: &'a SourceMap,
}

impl<'a> Dumper<'a> {
    fn loc(&self, sp: Span) -> String {
        if sp.is_dummy() {
            return String::new();
        }
        let f = self.sm.file(sp.file);
        let (a, b) = f.orig_range(sp.lo, sp.hi);
        let (l0, c0) = f.line_col_orig(a);
        let (l1, c1) = f.line_col_orig(b);
        if l0 == l1 {
            format!(" <{}:{}-{}>", l0, c0, c1)
        } else {
            format!(" <{}:{}-{}:{}>", l0, c0, l1, c1)
        }
    }

    fn tu(&self, tu: &TranslationUnit) -> Node {
        let mut n = Node::new("TranslationUnit");
        for d in &tu.decls {
            n.children.push(match d {
                ExternalDecl::Decl(d) => self.declaration(d),
                ExternalDecl::Func(f) => self.func(f),
                ExternalDecl::StaticAssert(s) => self.static_assert(s),
            });
        }
        n
    }

    fn static_assert(&self, s: &StaticAssert) -> Node {
        Node::new(format!("StaticAssert{}", self.loc(s.span))).with(self.expr(&s.cond))
    }

    fn specs_node(&self, s: &DeclSpecs) -> Node {
        let mut n = Node::new(format!("Specs '{}'", specs_text(s)));
        if let Some(t) = &s.ty {
            match &t.kind {
                TypeSpecKind::Record(r) => n.children.push(self.record(r)),
                TypeSpecKind::Enum(e) => {
                    if let Some(list) = &e.enumerators {
                        let mut en =
                            Node::new(format!("EnumDef {}", e.tag.map(|t| t.name.to_string()).unwrap_or_default()));
                        for x in list {
                            let mut c = Node::new(format!("Enumerator {}", x.name.name));
                            if let Some(v) = &x.value {
                                c.children.push(self.expr(v));
                            }
                            en.children.push(c);
                        }
                        n.children.push(en);
                    }
                }
                _ => {}
            }
        }
        n
    }

    fn record(&self, r: &RecordSpec) -> Node {
        let kw = if r.is_union { "UnionDef" } else { "StructDef" };
        let mut n =
            Node::new(format!("{} {}{}", kw, r.tag.map(|t| t.name.to_string()).unwrap_or_default(), self.loc(r.span)));
        if let Some(p) = r.pack {
            n.label.push_str(&format!(" pack({})", p));
        }
        if let Some(members) = &r.members {
            for m in members {
                match m {
                    MemberDecl::Field { specs, declarators, .. } => {
                        let mut f = Node::new(format!("Field '{}'", specs_text(specs)));
                        f.children.push(self.specs_node(specs));
                        for d in declarators {
                            let mut dn = Node::new(match &d.declarator {
                                Some(x) => format!("Declarator '{}'", declarator_text(x)),
                                None => "Declarator <unnamed>".to_string(),
                            });
                            if let Some(w) = &d.bit_width {
                                dn.children.push(Node::new("BitWidth").with(self.expr(w)));
                            }
                            f.children.push(dn);
                        }
                        n.children.push(f);
                    }
                    MemberDecl::StaticAssert(s) => n.children.push(self.static_assert(s)),
                }
            }
        }
        n
    }

    fn declaration(&self, d: &Declaration) -> Node {
        let mut n = Node::new(format!("Declaration{}", self.loc(d.span)));
        n.children.push(self.specs_node(&d.specs));
        for id in &d.declarators {
            let mut dn = Node::new(format!("InitDeclarator '{}'", declarator_text(&id.declarator)));
            if let Some(init) = &id.init {
                dn.children.push(self.initializer(init));
            }
            n.children.push(dn);
        }
        n
    }

    fn initializer(&self, i: &Initializer) -> Node {
        match i {
            Initializer::Expr(e) => self.expr(e),
            Initializer::List(l) => {
                let mut n = Node::new(format!("InitList{}", self.loc(l.span)));
                for it in &l.items {
                    let mut label = String::from("Item");
                    for d in &it.designators {
                        match d {
                            Designator::Field(f) => label.push_str(&format!(" .{}", f.name)),
                            Designator::Index(e) => label.push_str(&format!(" [{}]", sexp(e))),
                        }
                    }
                    n.children.push(Node::new(label).with(self.initializer(&it.init)));
                }
                n
            }
        }
    }

    fn func(&self, f: &FuncDef) -> Node {
        let name = f.declarator.name().map(|i| i.name.to_string()).unwrap_or_default();
        Node::new(format!("FunctionDef {}{}", name, self.loc(f.span)))
            .with(self.specs_node(&f.specs))
            .with(Node::new(format!("Declarator '{}'", declarator_text(&f.declarator))))
            .with(self.stmt(&f.body))
    }

    fn expr(&self, e: &Expr) -> Node {
        let loc = self.loc(e.span);
        match &e.kind {
            ExprKind::IntLit(s) => Node::new(format!("IntLiteral {}{}", s, loc)),
            ExprKind::FloatLit(s) => Node::new(format!("FloatLiteral {}{}", s, loc)),
            ExprKind::CharLit(s) => Node::new(format!("CharLiteral {}{}", s, loc)),
            ExprKind::StrLit { kind, units } => {
                Node::new(format!("StringLiteral {}{}", str_lit_text(*kind, units), loc))
            }
            ExprKind::Ident(n) => Node::new(format!("Ident {}{}", n, loc)),
            ExprKind::Paren(x) => Node::new(format!("Paren{}", loc)).with(self.expr(x)),
            ExprKind::Unary { op, operand, .. } => {
                let kind = match op {
                    UnOp::PostInc | UnOp::PostDec => "PostfixOp",
                    _ => "UnaryOp",
                };
                Node::new(format!("{} '{}'{}", kind, op.spelling(), loc)).with(self.expr(operand))
            }
            ExprKind::Binary { op, lhs, rhs, .. } => {
                Node::new(format!("BinaryOp '{}'{}", op.spelling(), loc)).with(self.expr(lhs)).with(self.expr(rhs))
            }
            ExprKind::Assign { op, lhs, rhs, .. } => {
                let s = match op {
                    None => "=".to_string(),
                    Some(o) => format!("{}=", o.spelling()),
                };
                Node::new(format!("AssignOp '{}'{}", s, loc)).with(self.expr(lhs)).with(self.expr(rhs))
            }
            ExprKind::Cond { cond, then, els } => Node::new(format!("Conditional{}", loc))
                .with(self.expr(cond))
                .with(self.expr(then))
                .with(self.expr(els)),
            ExprKind::Comma(a, b) => Node::new(format!("Comma{}", loc)).with(self.expr(a)).with(self.expr(b)),
            ExprKind::Call { callee, args } => {
                let mut n = Node::new(format!("Call{}", loc)).with(self.expr(callee));
                for a in args {
                    n.children.push(self.expr(a));
                }
                n
            }
            ExprKind::Index { base, index } => {
                Node::new(format!("Index{}", loc)).with(self.expr(base)).with(self.expr(index))
            }
            ExprKind::Member { base, member, arrow } => {
                Node::new(format!("Member '{}{}'{}", if *arrow { "->" } else { "." }, member.name, loc))
                    .with(self.expr(base))
            }
            ExprKind::Cast { ty, operand } => {
                Node::new(format!("Cast '{}'{}", type_name_text(ty), loc)).with(self.expr(operand))
            }
            ExprKind::SizeofExpr(x) => Node::new(format!("SizeofExpr{}", loc)).with(self.expr(x)),
            ExprKind::SizeofType(t) => Node::new(format!("SizeofType '{}'{}", type_name_text(t), loc)),
            ExprKind::AlignofType(t) => Node::new(format!("AlignofType '{}'{}", type_name_text(t), loc)),
            ExprKind::CompoundLiteral { ty, init } => {
                Node::new(format!("CompoundLiteral '{}'{}", type_name_text(ty), loc))
                    .with(self.initializer(&Initializer::List(init.clone())))
            }
            ExprKind::Generic { controlling, assocs } => {
                let mut n = Node::new(format!("Generic{}", loc)).with(self.expr(controlling));
                for a in assocs {
                    let label = match &a.ty {
                        Some(t) => format!("Association '{}'", type_name_text(t)),
                        None => "Association default".to_string(),
                    };
                    n.children.push(Node::new(label).with(self.expr(&a.expr)));
                }
                n
            }
            ExprKind::VaArg { ap, ty } => {
                Node::new(format!("VaArg '{}'{}", type_name_text(ty), loc)).with(self.expr(ap))
            }
            ExprKind::Offsetof { .. } => Node::new(format!("Offsetof {}{}", sexp(e), loc)),
        }
    }

    fn stmt(&self, s: &Stmt) -> Node {
        let loc = self.loc(s.span);
        match &s.kind {
            StmtKind::Empty => Node::new(format!("NullStmt{}", loc)),
            StmtKind::Expr(e) => Node::new(format!("ExprStmt{}", loc)).with(self.expr(e)),
            StmtKind::Compound(items) => {
                let mut n = Node::new(format!("CompoundStmt{}", loc));
                for it in items {
                    n.children.push(match it {
                        BlockItem::Decl(d) => self.declaration(d),
                        BlockItem::Stmt(s) => self.stmt(s),
                    });
                }
                n
            }
            StmtKind::If { cond, then, els } => {
                let mut n = Node::new(format!("IfStmt{}", loc)).with(self.expr(cond)).with(self.stmt(then));
                if let Some(e) = els {
                    n.children.push(self.stmt(e));
                }
                n
            }
            StmtKind::While { cond, body } => {
                Node::new(format!("WhileStmt{}", loc)).with(self.expr(cond)).with(self.stmt(body))
            }
            StmtKind::DoWhile { body, cond } => {
                Node::new(format!("DoStmt{}", loc)).with(self.stmt(body)).with(self.expr(cond))
            }
            StmtKind::For { init, cond, step, body } => {
                let mut n = Node::new(format!("ForStmt{}", loc));
                n.children.push(match init {
                    ForInit::None => Node::new("Init <none>"),
                    ForInit::Expr(e) => Node::new("Init").with(self.expr(e)),
                    ForInit::Decl(d) => Node::new("Init").with(self.declaration(d)),
                });
                n.children.push(match cond {
                    Some(c) => Node::new("Cond").with(self.expr(c)),
                    None => Node::new("Cond <none>"),
                });
                n.children.push(match step {
                    Some(c) => Node::new("Step").with(self.expr(c)),
                    None => Node::new("Step <none>"),
                });
                n.children.push(self.stmt(body));
                n
            }
            StmtKind::Switch { cond, body } => {
                Node::new(format!("SwitchStmt{}", loc)).with(self.expr(cond)).with(self.stmt(body))
            }
            StmtKind::Case { value, body } => {
                Node::new(format!("CaseStmt{}", loc)).with(self.expr(value)).with(self.stmt(body))
            }
            StmtKind::Default { body } => Node::new(format!("DefaultStmt{}", loc)).with(self.stmt(body)),
            StmtKind::Break => Node::new(format!("BreakStmt{}", loc)),
            StmtKind::Continue => Node::new(format!("ContinueStmt{}", loc)),
            StmtKind::Return(v) => {
                let mut n = Node::new(format!("ReturnStmt{}", loc));
                if let Some(e) = v {
                    n.children.push(self.expr(e));
                }
                n
            }
            StmtKind::Goto(l) => Node::new(format!("GotoStmt {}{}", l.name, loc)),
            StmtKind::Label { name, body } => {
                Node::new(format!("LabelStmt {}{}", name.name, loc)).with(self.stmt(body))
            }
            StmtKind::StaticAssert(sa) => self.static_assert(sa),
        }
    }
}

fn render(n: &Node, prefix: &str, last: bool, root: bool, out: &mut String) {
    if root {
        out.push_str(&n.label);
        out.push('\n');
    } else {
        out.push_str(prefix);
        out.push_str(if last { "`-" } else { "|-" });
        out.push_str(&n.label);
        out.push('\n');
    }
    let child_prefix = if root { String::new() } else { format!("{}{}", prefix, if last { "  " } else { "| " }) };
    for (i, c) in n.children.iter().enumerate() {
        render(c, &child_prefix, i + 1 == n.children.len(), false, out);
    }
}

/// `--emit-ast` text for a whole translation unit.
pub fn dump(sm: &SourceMap, tu: &TranslationUnit) -> String {
    let d = Dumper { sm };
    let root = d.tu(tu);
    let mut out = String::new();
    render(&root, "", true, true, &mut out);
    out
}
