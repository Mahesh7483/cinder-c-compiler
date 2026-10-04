//! Text rendering of the typed HIR for `--emit-hir`: every expression shows
//! its type and every implicit conversion is visible as a `Cast` node.

use crate::ast_dump::{render, str_lit_text, Node};
use crate::hir::*;
use crate::source::{SourceMap, Span};
use crate::types::Ty;

struct D<'a> {
    m: &'a HirModule,
    sm: &'a SourceMap,
    locals: &'a [Local],
}

impl<'a> D<'a> {
    fn ty(&self, t: Ty) -> String {
        self.m.types.show(t)
    }

    fn loc(&self, sp: Span) -> String {
        match self.sm.loc(sp) {
            Some(l) => format!(" <{}:{}>", l.line, l.col),
            None => String::new(),
        }
    }

    fn sym_name(&self, id: SymId) -> String {
        self.m.syms[id.0 as usize].name.to_string()
    }

    fn local_name(&self, id: LocalId) -> String {
        match self.locals.get(id.0 as usize) {
            Some(l) if !l.name.as_str().is_empty() => format!("{}#{}", l.name, id.0),
            _ => format!("#{}", id.0),
        }
    }

    fn expr(&self, e: &HExpr) -> Node {
        let t = self.ty(e.ty);
        let n = |label: String| Node::new(format!("{} : '{}'{}", label, t, self.loc(e.span)));
        match &e.kind {
            HExprKind::Int(v) => {
                let signed = self.m.types.is_signed(e.ty) && !self.m.types.is_pointer(e.ty);
                if signed {
                    n(format!("IntLiteral {}", *v as i64))
                } else {
                    n(format!("IntLiteral {}", v))
                }
            }
            HExprKind::Float(f) => n(format!("FloatLiteral {}", f)),
            HExprKind::Str(id) => {
                let s = &self.m.strings[id.0 as usize];
                n(format!("StringLiteral {}", str_lit_text(s.kind, &s.units)))
            }
            HExprKind::Local(id) => n(format!("Local {}", self.local_name(*id))),
            HExprKind::Global(id) => n(format!("Global {}", self.sym_name(*id))),
            HExprKind::Deref(x) => n("Deref".into()).with(self.expr(x)),
            HExprKind::Member(b, m) => {
                let bit = m.bit.map(|b| format!(" bit[{}:{}]", b.bit_offset, b.width)).unwrap_or_default();
                n(format!("Member +{}{}", m.offset, bit)).with(self.expr(b))
            }
            HExprKind::CompoundLit { local, init } => {
                let mut node = n(format!("CompoundLiteral {}", self.local_name(*local)));
                node.children.push(self.plan(init));
                node
            }
            HExprKind::Cast(k, x) => n(format!("Cast {:?}", k)).with(self.expr(x)),
            HExprKind::Unary(k, x) => n(format!("Unary {:?}", k)).with(self.expr(x)),
            HExprKind::Binary(k, a, b) => n(format!("Binary '{}'", k.spelling())).with(self.expr(a)).with(self.expr(b)),
            HExprKind::PtrAdd { ptr, idx, scale, negate } => {
                n(format!("PtrAdd {}*{}", if *negate { "-" } else { "+" }, scale))
                    .with(self.expr(ptr))
                    .with(self.expr(idx))
            }
            HExprKind::PtrDiff { l, r, elem_size } => {
                n(format!("PtrDiff /{}", elem_size)).with(self.expr(l)).with(self.expr(r))
            }
            HExprKind::LogAnd(a, b) => n("LogAnd".into()).with(self.expr(a)).with(self.expr(b)),
            HExprKind::LogOr(a, b) => n("LogOr".into()).with(self.expr(a)).with(self.expr(b)),
            HExprKind::Cond(c, a, b) => {
                n("Conditional".into()).with(self.expr(c)).with(self.expr(a)).with(self.expr(b))
            }
            HExprKind::Comma(a, b) => n("Comma".into()).with(self.expr(a)).with(self.expr(b)),
            HExprKind::Assign(p, v) => n("Assign".into()).with(self.expr(p)).with(self.expr(v)),
            HExprKind::CompoundAssign { op, place, value, calc } => {
                n(format!("CompoundAssign '{}=' in '{}'", op.spelling(), self.ty(*calc)))
                    .with(self.expr(place))
                    .with(self.expr(value))
            }
            HExprKind::IncDec { place, is_inc, is_prefix } => {
                let op = if *is_inc { "++" } else { "--" };
                n(format!("{}{}", if *is_prefix { "Pre" } else { "Post" }, op)).with(self.expr(place))
            }
            HExprKind::Call { callee, args } => {
                let mut node = n("Call".into()).with(self.expr(callee));
                for a in args {
                    node.children.push(self.expr(a));
                }
                node
            }
            HExprKind::AddrOf(x) => n("AddrOf".into()).with(self.expr(x)),
            HExprKind::VaStart(x) => n("VaStart".into()).with(self.expr(x)),
            HExprKind::VaEnd(x) => n("VaEnd".into()).with(self.expr(x)),
            HExprKind::VaCopy(a, b) => n("VaCopy".into()).with(self.expr(a)).with(self.expr(b)),
            HExprKind::VaArg(x) => n("VaArg".into()).with(self.expr(x)),
            HExprKind::Trap => n("Trap".into()),
            HExprKind::Error => n("<error>".into()),
        }
    }

    fn plan(&self, p: &InitPlan) -> Node {
        let mut node = Node::new(format!("Init size={}{}", p.size, if p.needs_zero { " zero-fill" } else { "" }));
        for e in &p.entries {
            let bit = e.bit.map(|b| format!(" bit[{}:{}]", b.bit_offset, b.width)).unwrap_or_default();
            let label = format!("@{} '{}'{}", e.offset, self.ty(e.ty), bit);
            let child = match &e.value {
                InitValue::Expr(x) => Node::new(label).with(self.expr(x)),
                InitValue::Const(c) => Node::new(format!("{} = {}", label, self.const_text(c))),
                InitValue::Bytes(b) => Node::new(format!("{} = bytes {:?}", label, String::from_utf8_lossy(b))),
            };
            node.children.push(child);
        }
        node
    }

    fn const_text(&self, c: &ConstVal) -> String {
        match c {
            ConstVal::Int(v) => format!("{}", *v as i64),
            ConstVal::Float(f) => format!("{}", f),
            ConstVal::Addr { base, offset } => {
                let b = match base {
                    AddrBase::Global(s) => format!("&{}", self.sym_name(*s)),
                    AddrBase::Str(s) => {
                        let d = &self.m.strings[s.0 as usize];
                        format!("&{}", str_lit_text(d.kind, &d.units))
                    }
                };
                if *offset == 0 {
                    b
                } else {
                    format!("{}{:+}", b, offset)
                }
            }
        }
    }

    fn stmt(&self, s: &HStmt) -> Node {
        let loc = self.loc(s.span);
        match &s.kind {
            HStmtKind::Empty => Node::new(format!("Empty{}", loc)),
            HStmtKind::Expr(e) => Node::new(format!("ExprStmt{}", loc)).with(self.expr(e)),
            HStmtKind::Decl { local, init } => {
                let l = &self.locals[local.0 as usize];
                let mut n = Node::new(format!("Decl {} : '{}'{}", self.local_name(*local), self.ty(l.ty), loc));
                if let Some(p) = init {
                    n.children.push(self.plan(p));
                }
                n
            }
            HStmtKind::Block(items) => {
                let mut n = Node::new(format!("Block{}", loc));
                for i in items {
                    n.children.push(self.stmt(i));
                }
                n
            }
            HStmtKind::If(c, t, e) => {
                let mut n = Node::new(format!("If{}", loc)).with(self.expr(c)).with(self.stmt(t));
                if let Some(e) = e {
                    n.children.push(self.stmt(e));
                }
                n
            }
            HStmtKind::While(c, b) => Node::new(format!("While{}", loc)).with(self.expr(c)).with(self.stmt(b)),
            HStmtKind::DoWhile(b, c) => Node::new(format!("DoWhile{}", loc)).with(self.stmt(b)).with(self.expr(c)),
            HStmtKind::For { init, cond, step, body } => {
                let mut n = Node::new(format!("For{}", loc));
                let mut i = Node::new("Init");
                for s in init {
                    i.children.push(self.stmt(s));
                }
                n.children.push(i);
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
            HStmtKind::Switch { cond, body, cases, default } => {
                let mut label = format!("Switch{} cases=[", loc);
                for (i, (v, id)) in cases.iter().enumerate() {
                    if i > 0 {
                        label.push_str(", ");
                    }
                    label.push_str(&format!("{}->case{}", v, id.0));
                }
                label.push(']');
                if let Some(d) = default {
                    label.push_str(&format!(" default->case{}", d.0));
                }
                Node::new(label).with(self.expr(cond)).with(self.stmt(body))
            }
            HStmtKind::CaseLabel(id) => Node::new(format!("CaseLabel case{}", id.0)),
            HStmtKind::Break => Node::new("Break"),
            HStmtKind::Continue => Node::new("Continue"),
            HStmtKind::Return(v) => {
                let mut n = Node::new(format!("Return{}", loc));
                if let Some(e) = v {
                    n.children.push(self.expr(e));
                }
                n
            }
            HStmtKind::Goto(l) => Node::new(format!("Goto label{}", l.0)),
            HStmtKind::Label(l) => Node::new(format!("Label label{}", l.0)),
        }
    }
}

pub fn dump(sm: &SourceMap, m: &HirModule) -> String {
    let mut root = Node::new("Module");
    let no_locals: Vec<Local> = Vec::new();
    let d0 = D { m, sm, locals: &no_locals };
    for (i, s) in m.syms.iter().enumerate() {
        if s.kind != SymKind::Var {
            continue;
        }
        let mut flags = vec![match s.linkage {
            Linkage::External => "external",
            Linkage::Internal => "internal",
        }];
        if s.tentative {
            flags.push("tentative");
        } else if s.defined {
            flags.push("defined");
        }
        if s.is_const {
            flags.push("const");
        }
        let mut n =
            Node::new(format!("Global {} : '{}' [{}] align={}", s.name, d0.ty(s.ty), flags.join(", "), s.align));
        let _ = i;
        if let Some(p) = &s.init {
            n.children.push(d0.plan(p));
        }
        root.children.push(n);
    }
    for f in &m.funcs {
        let d = D { m, sm, locals: &f.locals };
        let sym = &m.syms[f.sym.0 as usize];
        let mut fl = vec![match sym.linkage {
            Linkage::External => "external",
            Linkage::Internal => "internal",
        }];
        if f.is_inline {
            fl.push("inline");
        }
        if f.noreturn {
            fl.push("noreturn");
        }
        let mut n = Node::new(format!("Function {} : '{}' [{}]", f.name, d.ty(sym.ty), fl.join(", ")));
        for p in &f.params {
            let l = &f.locals[p.0 as usize];
            n.children.push(Node::new(format!("Param {} : '{}'", d.local_name(*p), d.ty(l.ty))));
        }
        n.children.push(d.stmt(&f.body));
        root.children.push(n);
    }
    let mut out = String::new();
    render(&root, "", true, true, &mut out);
    out
}
