//! Textual form of the IR (`--emit-ir`), in an LLVM-flavoured syntax.

use super::*;
use std::fmt::Write;

pub fn print_module(m: &Module) -> String {
    let mut s = String::new();
    // data first
    for sym in &m.syms {
        if let SymBody::Data(def) = &sym.body {
            let link = if sym.linkage == Linkage::Internal { "internal " } else { "" };
            match def {
                None => {
                    let _ = writeln!(s, "@{} = external global", sym.name);
                }
                Some(d) => {
                    let kind = if d.readonly {
                        "constant"
                    } else if d.zero {
                        "zeroinit"
                    } else {
                        "global"
                    };
                    let _ = write!(s, "@{} = {}{} size {}, align {}", sym.name, link, kind, d.size, d.align);
                    if !d.zero {
                        s.push_str(" { ");
                        let items: Vec<String> = d
                            .items
                            .iter()
                            .map(|it| match it {
                                DataItem::Bytes(b) => format!("bytes {}", bytes_text(b)),
                                DataItem::Zero(n) => format!("zero {}", n),
                                DataItem::Addr { sym, offset } => {
                                    if *offset == 0 {
                                        format!("addr @{}", m.syms[sym.idx()].name)
                                    } else {
                                        format!("addr @{}{:+}", m.syms[sym.idx()].name, offset)
                                    }
                                }
                            })
                            .collect();
                        s.push_str(&items.join(", "));
                        s.push_str(" }");
                    }
                    s.push('\n');
                }
            }
        }
    }
    for sym in &m.syms {
        if let SymBody::Func { defined: false } = sym.body {
            let _ = writeln!(s, "declare @{}", sym.name);
        }
    }
    if !s.is_empty() {
        s.push('\n');
    }
    for f in &m.funcs {
        s.push_str(&print_func(m, f));
        s.push('\n');
    }
    s
}

fn bytes_text(b: &[u8]) -> String {
    let mut o = String::from("\"");
    for &c in b {
        match c {
            b'"' => o.push_str("\\22"),
            b'\\' => o.push_str("\\5C"),
            32..=126 => o.push(c as char),
            _ => {
                let _ = write!(o, "\\{:02X}", c);
            }
        }
    }
    o.push('"');
    o
}

fn ty_name(t: Type) -> &'static str {
    match t {
        Type::I8 => "i8",
        Type::I16 => "i16",
        Type::I32 => "i32",
        Type::I64 => "i64",
        Type::Ptr => "ptr",
        Type::F32 => "f32",
        Type::F64 => "f64",
    }
}

struct P<'a> {
    m: &'a Module,
    f: &'a Func,
}

impl<'a> P<'a> {
    fn val(&self, v: ValueId) -> String {
        match self.f.values[v.idx()].name {
            Some(n) if !n.as_str().is_empty() => format!("%{}.{}", n, v.0),
            _ => format!("%{}", v.0),
        }
    }

    fn op(&self, o: Operand) -> String {
        match o {
            Operand::Value(v) => self.val(v),
            Operand::Int(0, Type::Ptr) => "null".to_string(),
            Operand::Int(v, _) => v.to_string(),
            Operand::Float(b, _) => {
                let f = f64::from_bits(b);
                if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
                    format!("{:.1}", f)
                } else {
                    format!("{:?}", f)
                }
            }
            Operand::Global(s) => format!("@{}", self.m.syms[s.idx()].name),
            Operand::Undef(_) => "undef".to_string(),
        }
    }

    fn blk(&self, b: BlockId) -> String {
        let blk = &self.f.blocks[b.idx()];
        if b.0 == 0 && blk.name == "entry" {
            "entry".to_string()
        } else {
            format!("{}.{}", blk.name, b.0)
        }
    }

    fn inst(&self, id: InstId) -> String {
        let i = &self.f.insts[id.idx()];
        let lhs = match (i.dst, i.dst2) {
            (Some(a), Some(b)) => format!("{}, {} = ", self.val(a), self.val(b)),
            (Some(a), None) => format!("{} = ", self.val(a)),
            _ => String::new(),
        };
        let body = match &i.kind {
            InstKind::Alloca { size, align } => format!("alloca {}, align {}", size, align),
            InstKind::Load { ty, ptr, volatile } => {
                format!("load{} {}, {}", if *volatile { " volatile" } else { "" }, ty_name(*ty), self.op(*ptr))
            }
            InstKind::Store { ty, val, ptr, volatile } => {
                format!(
                    "store{} {} {}, {}",
                    if *volatile { " volatile" } else { "" },
                    ty_name(*ty),
                    self.op(*val),
                    self.op(*ptr)
                )
            }
            InstKind::MemCopy { dst, src, size, align } => {
                format!("memcpy {}, {}, {}, align {}", self.op(*dst), self.op(*src), size, align)
            }
            InstKind::MemSet { dst, byte, size, align } => {
                format!("memset {}, {}, {}, align {}", self.op(*dst), byte, size, align)
            }
            InstKind::Bin { op, ty, lhs, rhs } => {
                format!("{} {} {}, {}", op.name(), ty_name(*ty), self.op(*lhs), self.op(*rhs))
            }
            InstKind::Un { op, ty, val } => {
                let n = match op {
                    UnOp::Neg => "neg",
                    UnOp::Not => "not",
                    UnOp::FNeg => "fneg",
                };
                format!("{} {} {}", n, ty_name(*ty), self.op(*val))
            }
            InstKind::ICmp { pred, ty, lhs, rhs } => {
                format!("icmp {} {} {}, {}", pred.name(), ty_name(*ty), self.op(*lhs), self.op(*rhs))
            }
            InstKind::FCmp { pred, ty, lhs, rhs } => {
                format!("fcmp {} {} {}, {}", pred.name(), ty_name(*ty), self.op(*lhs), self.op(*rhs))
            }
            InstKind::Cast { op, from, to, val } => {
                format!("{} {} {} to {}", op.name(), ty_name(*from), self.op(*val), ty_name(*to))
            }
            InstKind::PtrAdd { base, offset } => format!("ptradd {}, {}", self.op(*base), self.op(*offset)),
            InstKind::Select { ty, cond, a, b } => {
                format!("select {} {}, {}, {}", ty_name(*ty), self.op(*cond), self.op(*a), self.op(*b))
            }
            InstKind::Phi { ty, incoming } => {
                let items: Vec<String> =
                    incoming.iter().map(|(b, o)| format!("[ {}, {} ]", self.op(*o), self.blk(*b))).collect();
                format!("phi {} {}", ty_name(*ty), items.join(", "))
            }
            InstKind::Call { callee, args, rets, variadic, tail } => {
                let c = match callee {
                    Callee::Direct(s) => format!("@{}", self.m.syms[s.idx()].name),
                    Callee::Indirect(o) => self.op(*o),
                };
                let a: Vec<String> = args
                    .iter()
                    .map(|a| {
                        let t = self.f.operand_ty(a.val);
                        match &a.kind {
                            ArgKind::Value => format!("{} {}", ty_name(t), self.op(a.val)),
                            ArgKind::ByVal { size, align } => format!("byval({},{}) {}", size, align, self.op(a.val)),
                        }
                    })
                    .collect();
                let r: Vec<&str> = rets.iter().map(|t| ty_name(*t)).collect();
                let rs = match r.len() {
                    0 => "void".to_string(),
                    1 => r[0].to_string(),
                    _ => format!("{{{}}}", r.join(", ")),
                };
                format!(
                    "{}call{} {} {}({})",
                    if *tail { "tail " } else { "" },
                    if *variadic { " variadic" } else { "" },
                    rs,
                    c,
                    a.join(", ")
                )
            }
            InstKind::DynAlloca { size, align } => format!("dynalloca {}, align {}", self.op(*size), align),
            InstKind::StackSave => "stacksave".to_string(),
            InstKind::StackRestore { ptr } => format!("stackrestore {}", self.op(*ptr)),
            InstKind::VaRegSave => "va_reg_save_area".to_string(),
            InstKind::VaStackArgs => "va_stack_args".to_string(),
            InstKind::Trap => "trap".to_string(),
        };
        format!("  {}{}", lhs, body)
    }

    fn term(&self, t: &Term) -> String {
        match t {
            Term::None => "  <no terminator>".to_string(),
            Term::Br(b) => format!("  br {}", self.blk(*b)),
            Term::CondBr { cond, then_bb, else_bb } => {
                format!("  condbr {}, {}, {}", self.op(*cond), self.blk(*then_bb), self.blk(*else_bb))
            }
            Term::Switch { ty, val, cases, default } => {
                let cs: Vec<String> = cases.iter().map(|(v, b)| format!("{}: {}", v, self.blk(*b))).collect();
                format!(
                    "  switch {} {}, default {} [ {} ]",
                    ty_name(*ty),
                    self.op(*val),
                    self.blk(*default),
                    cs.join(", ")
                )
            }
            Term::Ret(vs) => {
                if vs.is_empty() {
                    "  ret void".to_string()
                } else {
                    let items: Vec<String> =
                        vs.iter().map(|v| format!("{} {}", ty_name(self.f.operand_ty(*v)), self.op(*v))).collect();
                    format!("  ret {}", items.join(", "))
                }
            }
            Term::Unreachable => "  unreachable".to_string(),
        }
    }
}

pub fn print_func(m: &Module, f: &Func) -> String {
    let p = P { m, f };
    let mut s = String::new();
    let rets: Vec<&str> = f.rets.iter().map(|t| ty_name(*t)).collect();
    let rs = match rets.len() {
        0 => "void".to_string(),
        1 => rets[0].to_string(),
        _ => format!("{{{}}}", rets.join(", ")),
    };
    let params: Vec<String> = f
        .params
        .iter()
        .zip(&f.param_values)
        .map(|(pa, v)| match &pa.kind {
            ParamKind::Value(t) => format!("{} {}", ty_name(*t), p.val(*v)),
            ParamKind::ByVal { size, align } => format!("byval({},{}) {}", size, align, p.val(*v)),
        })
        .chain(if f.variadic { Some("...".to_string()) } else { None })
        .collect();
    let link = if f.linkage == Linkage::Internal { "internal " } else { "" };
    let _ = writeln!(s, "define {}{} @{}({}) {{", link, rs, f.name, params.join(", "));
    for (bi, b) in f.blocks.iter().enumerate() {
        let _ = writeln!(s, "{}:", p.blk(BlockId(bi as u32)));
        for &id in &b.insts {
            let _ = writeln!(s, "{}", p.inst(id));
        }
        let _ = writeln!(s, "{}", p.term(&b.term));
    }
    s.push_str("}\n");
    s
}
