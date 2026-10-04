//! Semantic analysis: name resolution, type checking and conversion
//! insertion, constant evaluation, struct layout and initializer
//! flattening. Produces the typed [`HirModule`].
//!
//! Errors never abort the pass: a failed expression becomes
//! [`HExprKind::Error`] with type `int`, so one mistake yields one
//! diagnostic rather than a cascade, and unrelated errors elsewhere are
//! still reported.

pub mod consteval;
mod expr;
mod init;
mod stmt;
#[cfg(test)]
mod tests;

use crate::ast::*;
use crate::diag::{Diagnostic, Warn};
use crate::hir::*;
use crate::intern::Symbol;
use crate::literal::StrKind;
use crate::session::Session;
use crate::source::Span;
use crate::types::*;
use std::collections::HashMap;

#[derive(Clone, Debug)]
enum EntKind {
    Global(SymId),
    Local(LocalId),
    Typedef(Ty),
    EnumConst(i64, Ty),
}

#[derive(Clone, Debug)]
struct Ent {
    kind: EntKind,
    span: Span,
}

#[derive(Default)]
struct Scope {
    ordinary: HashMap<Symbol, Ent>,
    tags: HashMap<Symbol, Ty>,
    /// Locals declared here, checked for "unused variable" at scope exit.
    locals: Vec<LocalId>,
}

struct LabelInfo {
    id: LabelId,
    name: Symbol,
    defined: Option<Span>,
    used: bool,
    first_use: Span,
}

struct SwitchCtx {
    /// Type of the controlling expression after integer promotion.
    ty: Ty,
    cases: Vec<(i64, CaseId, Span)>,
    default: Option<(CaseId, Span)>,
}

struct FnCtx {
    name: Symbol,
    ret: Ty,
    locals: Vec<Local>,
    labels: Vec<LabelInfo>,
    label_map: HashMap<Symbol, usize>,
    switches: Vec<SwitchCtx>,
    loops: u32,
    breakables: u32,
    next_case: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeclCtx {
    File,
    Block,
    Param,
    Member,
    TypeName,
}

#[derive(Clone, Debug)]
pub(crate) struct ParamInfo {
    name: Option<Ident>,
    ty: Ty,
    span: Span,
}

pub struct Sema<'a> {
    pub(crate) sess: &'a mut Session,
    pub(crate) types: TypeTable,
    pub(crate) syms: Vec<GlobalSym>,
    funcs: Vec<HirFunc>,
    pub(crate) strings: Vec<StrData>,
    string_map: HashMap<(StrKind, Vec<u32>), StrId>,
    scopes: Vec<Scope>,
    /// File-scope entities with linkage, by assembler name.
    global_names: HashMap<Symbol, SymId>,
    f: Option<FnCtx>,
    anon_counter: u32,
}

/// Analyze a parsed translation unit. Returns `None` if any error was reported.
pub fn analyze(sess: &mut Session, tu: &TranslationUnit) -> Option<HirModule> {
    let errors_before = sess.diags.error_count();
    let mut s = Sema {
        sess,
        types: TypeTable::new(),
        syms: Vec::new(),
        funcs: Vec::new(),
        strings: Vec::new(),
        string_map: HashMap::new(),
        scopes: vec![Scope::default()],
        global_names: HashMap::new(),
        f: None,
        anon_counter: 0,
    };
    for d in &tu.decls {
        match d {
            ExternalDecl::Decl(d) => s.file_declaration(d),
            ExternalDecl::Func(f) => s.function_def(f),
            ExternalDecl::StaticAssert(sa) => s.static_assert(sa),
        }
    }
    s.finish();
    if s.sess.diags.error_count() > errors_before || s.sess.diags.has_errors() {
        return None;
    }
    Some(HirModule { types: s.types, syms: s.syms, funcs: s.funcs, strings: s.strings })
}

// ───────────────────────────── helpers ─────────────────────────────

fn attr_name(a: &Attr) -> &'static str {
    a.name.name.as_str()
}

const KNOWN_ATTRS: &[&str] = &[
    "noreturn",
    "packed",
    "aligned",
    "unused",
    "used",
    "always_inline",
    "noinline",
    "pure",
    "const",
    "cold",
    "hot",
    "malloc",
    "deprecated",
    "format",
    "nonnull",
    "warn_unused_result",
    "fallthrough",
    "returns_nonnull",
    "visibility",
    "leaf",
    "nothrow",
    "artificial",
    "gnu_inline",
    "optimize",
    "alloc_size",
    "format_arg",
    "sentinel",
    "noclone",
    "flatten",
];

impl<'a> Sema<'a> {
    pub(crate) fn error(&mut self, span: Span, msg: impl Into<String>) {
        self.sess.diags.emit(Diagnostic::error(span, msg));
    }

    pub(crate) fn warn(&mut self, flag: Warn, span: Span, msg: impl Into<String>) {
        self.sess.diags.warn(flag, span, msg);
    }

    pub(crate) fn emit(&mut self, d: Diagnostic) {
        self.sess.diags.emit(d);
    }

    pub(crate) fn show(&self, t: Ty) -> String {
        self.types.show(t)
    }

    fn check_attrs(&mut self, attrs: &[Attr]) {
        for a in attrs {
            if !KNOWN_ATTRS.contains(&attr_name(a)) {
                self.warn(
                    Warn::UnknownAttributes,
                    a.name.span,
                    format!("unknown attribute '{}' ignored", attr_name(a)),
                );
            }
        }
    }

    fn has_attr(attrs: &[Attr], name: &str) -> bool {
        attrs.iter().any(|a| attr_name(a) == name)
    }

    /// `aligned(n)` (or bare `aligned` = 16).
    fn attr_align(&mut self, attrs: &[Attr]) -> Option<u64> {
        for a in attrs {
            if attr_name(a) == "aligned" {
                return match a.args.first() {
                    None => Some(16),
                    Some(e) => {
                        let h = self.expr(e);
                        match consteval::eval_int(&self.types, &h) {
                            Some(n) if n.is_power_of_two() => Some(n),
                            _ => {
                                self.error(e.span, "requested alignment must be a power of 2");
                                None
                            }
                        }
                    }
                };
            }
        }
        None
    }

    /// `_Alignas(n | type)` on a declaration.
    fn spec_align(&mut self, specs: &DeclSpecs) -> Option<u64> {
        let (spec, span) = specs.align.as_ref()?;
        match spec {
            AlignSpec::Type(t) => {
                let ty = self.type_name(t);
                Some(self.types.align_of(ty))
            }
            AlignSpec::Expr(e) => {
                let h = self.expr(e);
                match consteval::eval_int(&self.types, &h) {
                    Some(0) => None,
                    Some(n) if n.is_power_of_two() => Some(n),
                    _ => {
                        self.error(*span, "requested alignment is not a power of 2");
                        None
                    }
                }
            }
        }
    }

    // ───────────────────────────── scopes ─────────────────────────────

    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    pub(crate) fn pop_scope(&mut self) {
        let sc = self.scopes.pop().expect("scope");
        let Some(f) = &self.f else { return };
        let mut unused: Vec<(Span, Symbol)> = Vec::new();
        for id in &sc.locals {
            let l = &f.locals[id.0 as usize];
            if !l.used && !l.is_param {
                unused.push((l.span, l.name));
            }
        }
        for (span, name) in unused {
            self.warn(Warn::UnusedVariable, span, format!("unused variable '{}'", name));
        }
    }

    fn lookup(&self, name: Symbol) -> Option<&Ent> {
        self.scopes.iter().rev().find_map(|s| s.ordinary.get(&name))
    }

    fn lookup_tag(&self, name: Symbol) -> Option<Ty> {
        self.scopes.iter().rev().find_map(|s| s.tags.get(&name).copied())
    }

    fn lookup_tag_current(&self, name: Symbol) -> Option<Ty> {
        self.scopes.last().and_then(|s| s.tags.get(&name).copied())
    }

    fn in_function(&self) -> bool {
        self.f.is_some()
    }

    fn declare_ordinary(&mut self, name: Symbol, kind: EntKind, span: Span) {
        self.scopes.last_mut().unwrap().ordinary.insert(name, Ent { kind, span });
    }

    // ───────────────────────────── symbols ─────────────────────────────

    fn new_sym(&mut self, sym: GlobalSym) -> SymId {
        let id = SymId(self.syms.len() as u32);
        self.syms.push(sym);
        id
    }

    pub(crate) fn mark_used(&mut self, id: SymId) {
        self.syms[id.0 as usize].used = true;
    }

    fn sym_span_note(&self, id: SymId) -> Span {
        self.syms[id.0 as usize].span
    }

    /// Declare (or re-declare) a function at file scope; returns its symbol.
    fn declare_global_func(
        &mut self,
        name: Ident,
        ty: Ty,
        storage: Option<StorageClass>,
        inline: bool,
        noreturn: bool,
    ) -> SymId {
        let is_static = storage == Some(StorageClass::Static);
        if let Some(&id) = self.global_names.get(&name.name) {
            let existing = self.syms[id.0 as usize].clone();
            if existing.kind != SymKind::Func {
                self.emit(
                    Diagnostic::error(
                        name.span,
                        format!("redefinition of '{}' as different kind of symbol", name.name),
                    )
                    .with_note(existing.span, "previous definition is here"),
                );
                return id;
            }
            if !self.types.compatible_unqual(existing.ty, ty) {
                let (a, b) = (self.show(existing.ty), self.show(ty));
                self.emit(
                    Diagnostic::error(name.span, format!("conflicting types for '{}': '{}' vs '{}'", name.name, b, a))
                        .with_note(existing.span, "previous declaration is here"),
                );
                return id;
            }
            let comp = self.types.composite(existing.ty, ty);
            let sym = &mut self.syms[id.0 as usize];
            sym.ty = comp;
            if is_static && sym.linkage == Linkage::External {
                let prev = sym.span;
                self.emit(
                    Diagnostic::error(
                        name.span,
                        format!("static declaration of '{}' follows non-static declaration", name.name),
                    )
                    .with_note(prev, "previous declaration is here"),
                );
            }
            let sym = &mut self.syms[id.0 as usize];
            sym.is_inline |= inline;
            sym.noreturn |= noreturn;
            return id;
        }
        let id = self.new_sym(GlobalSym {
            name: name.name,
            kind: SymKind::Func,
            ty,
            linkage: if is_static { Linkage::Internal } else { Linkage::External },
            defined: false,
            tentative: false,
            init: None,
            is_const: false,
            align: 1,
            span: name.span,
            used: false,
            is_inline: inline,
            noreturn,
        });
        self.global_names.insert(name.name, id);
        id
    }

    /// Declare a file-scope variable (or `extern` block-scope variable).
    fn declare_global_var(
        &mut self,
        name: Ident,
        ty: Ty,
        storage: Option<StorageClass>,
        has_init: bool,
        align: u64,
    ) -> SymId {
        let is_static = storage == Some(StorageClass::Static);
        let is_extern = storage == Some(StorageClass::Extern);
        if let Some(&id) = self.global_names.get(&name.name) {
            let existing = self.syms[id.0 as usize].clone();
            if existing.kind != SymKind::Var {
                self.emit(
                    Diagnostic::error(
                        name.span,
                        format!("redefinition of '{}' as different kind of symbol", name.name),
                    )
                    .with_note(existing.span, "previous definition is here"),
                );
                return id;
            }
            if !self.types.compatible_unqual(existing.ty, ty) {
                let (a, b) = (self.show(existing.ty), self.show(ty));
                self.emit(
                    Diagnostic::error(
                        name.span,
                        format!("redefinition of '{}' with a different type: '{}' vs '{}'", name.name, b, a),
                    )
                    .with_note(existing.span, "previous declaration is here"),
                );
                return id;
            }
            let comp = self.types.composite(existing.ty, ty);
            if existing.linkage == Linkage::Internal && !is_static && !is_extern {
                self.emit(
                    Diagnostic::error(
                        name.span,
                        format!("non-static declaration of '{}' follows static declaration", name.name),
                    )
                    .with_note(existing.span, "previous declaration is here"),
                );
            } else if existing.linkage == Linkage::External && is_static {
                self.emit(
                    Diagnostic::error(
                        name.span,
                        format!("static declaration of '{}' follows non-static declaration", name.name),
                    )
                    .with_note(existing.span, "previous declaration is here"),
                );
            }
            if has_init && existing.defined && !existing.tentative {
                self.emit(
                    Diagnostic::error(name.span, format!("redefinition of '{}'", name.name))
                        .with_note(existing.span, "previous definition is here"),
                );
            }
            let sym = &mut self.syms[id.0 as usize];
            sym.ty = comp;
            sym.align = sym.align.max(align);
            if has_init {
                sym.defined = true;
                sym.tentative = false;
                sym.span = name.span;
            } else if !is_extern && !sym.defined {
                sym.defined = true;
                sym.tentative = true;
                sym.span = name.span;
            }
            return id;
        }
        let id = self.new_sym(GlobalSym {
            name: name.name,
            kind: SymKind::Var,
            ty,
            linkage: if is_static { Linkage::Internal } else { Linkage::External },
            defined: has_init || !is_extern,
            tentative: !has_init && !is_extern,
            init: None,
            is_const: self.types.quals(ty).is_const,
            align: align.max(1),
            span: name.span,
            used: false,
            is_inline: false,
            noreturn: false,
        });
        self.global_names.insert(name.name, id);
        id
    }

    /// A file-scope-lifetime variable that is not visible by name (static
    /// locals, compound literals at file scope).
    fn new_anon_global(&mut self, name: String, ty: Ty, span: Span, align: u64) -> SymId {
        self.anon_counter += 1;
        let sym = Symbol::new(&format!("{}.{}", name, self.anon_counter));
        let is_const = self.types.quals(ty).is_const;
        self.new_sym(GlobalSym {
            name: sym,
            kind: SymKind::Var,
            ty,
            linkage: Linkage::Internal,
            defined: true,
            tentative: false,
            init: None,
            is_const,
            align: align.max(1),
            span,
            used: true,
            is_inline: false,
            noreturn: false,
        })
    }

    pub(crate) fn intern_string(&mut self, kind: StrKind, units: Vec<u32>) -> StrId {
        let key = (kind, units.clone());
        if let Some(&id) = self.string_map.get(&key) {
            return id;
        }
        let id = StrId(self.strings.len() as u32);
        self.strings.push(StrData { kind, units });
        self.string_map.insert(key, id);
        id
    }

    pub(crate) fn new_local(&mut self, name: Symbol, ty: Ty, span: Span, is_param: bool, align: u64) -> LocalId {
        let f = self.f.as_mut().expect("local outside function");
        let id = LocalId(f.locals.len() as u32);
        f.locals.push(Local { name, ty, span, is_param, used: false, align });
        self.scopes.last_mut().unwrap().locals.push(id);
        id
    }

    // ───────────────────────────── types from the AST ─────────────────────────────

    pub(crate) fn type_name(&mut self, t: &TypeName) -> Ty {
        let base = self.type_from_specs(&t.specs);
        let (ty, _, _) = self.apply_declarator(base, &t.declarator, DeclCtx::TypeName);
        ty
    }

    fn to_ty_quals(q: Quals) -> TyQuals {
        TyQuals { is_const: q.is_const, is_volatile: q.is_volatile, is_restrict: q.is_restrict }
    }

    pub(crate) fn type_from_specs(&mut self, specs: &DeclSpecs) -> Ty {
        self.check_attrs(&specs.attrs);
        let mut ty = match &specs.ty {
            None => {
                self.warn(Warn::ImplicitInt, specs.span, "type specifier missing, defaults to 'int'");
                self.types.p.int
            }
            Some(t) => match &t.kind {
                TypeSpecKind::Base(b) => self.base_type(*b, t.span),
                TypeSpecKind::Typedef(id) => match self.lookup(id.name).map(|e| e.kind.clone()) {
                    Some(EntKind::Typedef(ty)) => ty,
                    _ => {
                        self.error(id.span, format!("unknown type name '{}'", id.name));
                        self.types.p.int
                    }
                },
                TypeSpecKind::Record(r) => self.record_spec(r, specs),
                TypeSpecKind::Enum(e) => self.enum_spec(e),
                TypeSpecKind::Atomic(_) => {
                    self.error(t.span, "not yet supported: _Atomic types");
                    self.types.p.int
                }
            },
        };
        if specs.quals.is_atomic {
            self.error(specs.span, "not yet supported: _Atomic qualifier");
        }
        ty = self.types.qualified(ty, Self::to_ty_quals(specs.quals));
        ty
    }

    fn base_type(&mut self, b: BaseType, span: Span) -> Ty {
        let p = self.types.p;
        match b {
            BaseType::Void => p.void,
            BaseType::Bool => p.bool_,
            BaseType::Char => p.char_,
            BaseType::SChar => p.schar,
            BaseType::UChar => p.uchar,
            BaseType::Short => p.short,
            BaseType::UShort => p.ushort,
            BaseType::Int => p.int,
            BaseType::UInt => p.uint,
            BaseType::Long => p.long,
            BaseType::ULong => p.ulong,
            BaseType::LongLong => p.llong,
            BaseType::ULongLong => p.ullong,
            BaseType::Float => p.float,
            BaseType::Double => p.double,
            BaseType::LongDouble => {
                self.error(span, "not yet supported: long double");
                p.double
            }
            BaseType::VaList => p.va_list,
        }
    }

    // ── struct / union ──

    fn record_spec(&mut self, r: &RecordSpec, outer_specs: &DeclSpecs) -> Ty {
        let _ = outer_specs;
        self.check_attrs(&r.attrs);
        // Find or create the record this specifier names.
        let existing =
            r.tag.and_then(
                |t| if r.members.is_some() { self.lookup_tag_current(t.name) } else { self.lookup_tag(t.name) },
            );
        let rid = match existing.and_then(|t| self.types.record_id(t)) {
            Some(id) => {
                if self.types.record(id).is_union != r.is_union {
                    let t = r.tag.unwrap();
                    let kw = if self.types.record(id).is_union { "union" } else { "struct" };
                    self.emit(
                        Diagnostic::error(
                            t.span,
                            format!("use of '{}' with tag type that does not match previous declaration", t.name),
                        )
                        .with_note(self.types.record(id).span, format!("previous use is '{}'", kw)),
                    );
                }
                id
            }
            None => {
                if existing.is_some() {
                    // tag already names an enum in this scope
                    let t = r.tag.unwrap();
                    self.error(t.span, format!("'{}' defined as wrong kind of tag", t.name));
                }
                let id = self.types.new_record(r.tag.map(|t| t.name), r.is_union, r.span);
                let ty = self.types.record_type(id);
                if let Some(t) = r.tag {
                    self.scopes.last_mut().unwrap().tags.insert(t.name, ty);
                }
                id
            }
        };
        let ty = self.types.record_type(rid);
        let Some(members) = &r.members else { return ty };

        if self.types.record(rid).complete {
            let t = r.tag.map(|t| t.name);
            let kw = if r.is_union { "union" } else { "struct" };
            let msg = match t {
                Some(t) => format!("redefinition of '{} {}'", kw, t),
                None => format!("redefinition of '{}'", kw),
            };
            let prev = self.types.record(rid).span;
            self.emit(Diagnostic::error(r.span, msg).with_note(prev, "previous definition is here"));
            return ty;
        }
        self.types.record_mut(rid).span = r.span;

        // Resolve members.
        let mut inputs: Vec<MemberInput> = Vec::new();
        let mut seen: HashMap<Symbol, Span> = HashMap::new();
        for m in members {
            match m {
                MemberDecl::StaticAssert(sa) => self.static_assert(sa),
                MemberDecl::Field { specs, declarators, span } => {
                    let base = self.type_from_specs(specs);
                    let spec_align = self.spec_align(specs);
                    if declarators.is_empty() {
                        // anonymous struct/union member, or a stray declaration
                        let is_anon_record = matches!(&specs.ty, Some(TypeSpec { kind: TypeSpecKind::Record(rs), .. }) if rs.tag.is_none());
                        if is_anon_record && self.types.is_record(base) {
                            inputs.push(MemberInput {
                                name: None,
                                ty: base,
                                bit_width: None,
                                align: spec_align,
                                anonymous: true,
                                span: *span,
                            });
                        } else {
                            self.warn(Warn::UnusedValue, *span, "declaration does not declare anything");
                        }
                        continue;
                    }
                    for md in declarators {
                        let (ty, name) = match &md.declarator {
                            Some(d) => {
                                let (t, n, _) = self.apply_declarator(base, d, DeclCtx::Member);
                                self.check_attrs(&d.attrs);
                                (t, n)
                            }
                            None => (base, None),
                        };
                        let mut align = spec_align;
                        if let Some(d) = &md.declarator {
                            let a = self.attr_align(&d.attrs);
                            align = align.max(a);
                        }
                        let mut bit_width = None;
                        if let Some(w) = &md.bit_width {
                            let h = self.expr(w);
                            match consteval::eval_int(&self.types, &h) {
                                Some(v) if (v as i64) >= 0 => {
                                    if !self.types.is_integer(ty) {
                                        let t = self.show(ty);
                                        self.error(w.span, format!("bit-field has non-integral type '{}'", t));
                                    }
                                    bit_width = Some(v as u32);
                                }
                                Some(_) => self.error(w.span, "bit-field has negative width"),
                                None => self.error(w.span, "expression is not an integer constant expression"),
                            }
                            if bit_width == Some(0) && name.is_some() {
                                self.error(w.span, "named bit-field cannot have zero width");
                            }
                        }
                        if let Some(n) = name {
                            if let Some(prev) = seen.insert(n.name, n.span) {
                                self.emit(
                                    Diagnostic::error(n.span, format!("duplicate member '{}'", n.name))
                                        .with_note(prev, "previous declaration is here"),
                                );
                                continue;
                            }
                        }
                        if self.types.is_function(ty) {
                            self.error(name.map(|n| n.span).unwrap_or(*span), "field declared as a function");
                            continue;
                        }
                        inputs.push(MemberInput {
                            name: name.map(|n| n.name),
                            ty,
                            bit_width,
                            align,
                            anonymous: false,
                            span: name.map(|n| n.span).unwrap_or(*span),
                        });
                    }
                }
            }
        }
        let mut pack = r.pack;
        if Self::has_attr(&r.attrs, "packed") {
            pack = Some(1);
        }
        let problems = self.types.layout_record(rid, inputs, pack);
        for (sp, msg) in problems {
            self.error(sp, msg);
        }
        if let Some(a) = self.attr_align(&r.attrs) {
            let d = self.types.record_mut(rid);
            d.align = d.align.max(a);
            d.size = d.size.div_ceil(d.align) * d.align;
        }
        ty
    }

    // ── enum ──

    fn enum_spec(&mut self, e: &EnumSpec) -> Ty {
        let existing = e.tag.and_then(|t| {
            if e.enumerators.is_some() {
                self.lookup_tag_current(t.name)
            } else {
                self.lookup_tag(t.name)
            }
        });
        let eid = match existing.and_then(|t| match self.types.kind(t) {
            TyKind::Enum(id) => Some(*id),
            _ => None,
        }) {
            Some(id) => id,
            None => {
                if existing.is_some() {
                    let t = e.tag.unwrap();
                    self.error(t.span, format!("'{}' defined as wrong kind of tag", t.name));
                }
                let id = self.types.new_enum(e.tag.map(|t| t.name));
                let ty = self.types.enum_type(id);
                if let Some(t) = e.tag {
                    self.scopes.last_mut().unwrap().tags.insert(t.name, ty);
                }
                id
            }
        };
        let ty = self.types.enum_type(eid);
        let Some(list) = &e.enumerators else {
            if !self.types.enums[eid.0 as usize].complete {
                // `enum E;` forward reference is a GNU extension; allow, treat as int-sized
            }
            return ty;
        };
        if self.types.enums[eid.0 as usize].complete {
            let t = e.tag.map(|t| t.name.to_string()).unwrap_or_default();
            self.error(e.span, format!("redefinition of 'enum {}'", t));
            return ty;
        }
        let mut next: i128 = 0;
        let mut values: Vec<(Ident, i64)> = Vec::new();
        let (mut min, mut max) = (0i128, 0i128);
        for en in list {
            if let Some(v) = &en.value {
                let h = self.expr(v);
                match consteval::eval_int(&self.types, &h) {
                    Some(bits) => {
                        let ty_h = h.ty;
                        next = if self.types.is_signed(ty_h) { bits as i64 as i128 } else { bits as i128 };
                    }
                    None => {
                        self.error(v.span, "expression is not an integer constant expression");
                    }
                }
            }
            if let Some(prev) = self.scopes.last().unwrap().ordinary.get(&en.name.name) {
                let prev_span = prev.span;
                self.emit(
                    Diagnostic::error(en.name.span, format!("redefinition of '{}'", en.name.name))
                        .with_note(prev_span, "previous definition is here"),
                );
            }
            min = min.min(next);
            max = max.max(next);
            values.push((en.name, next as i64));
            // Each enumerator is in scope for the ones that follow it.
            let cty =
                if next >= i32::MIN as i128 && next <= i32::MAX as i128 { self.types.p.int } else { self.types.p.long };
            self.declare_ordinary(en.name.name, EntKind::EnumConst(next as i64, cty), en.name.span);
            next += 1;
        }
        // Choose the underlying type like GCC: int, else unsigned int, else long/unsigned long.
        let p = self.types.p;
        let underlying = if min >= i32::MIN as i128 && max <= i32::MAX as i128 {
            p.int
        } else if min >= 0 && max <= u32::MAX as i128 {
            p.uint
        } else if min >= i64::MIN as i128 && max <= i64::MAX as i128 {
            p.long
        } else {
            p.ulong
        };
        {
            let d = &mut self.types.enums[eid.0 as usize];
            d.underlying = underlying;
            d.complete = true;
        }
        let _ = values;
        ty
    }

    // ───────────────────────────── declarators ─────────────────────────────

    /// Apply a declarator to a base type, inside-out. Returns the type, the
    /// declared name, and (when the resulting type is a function type) the
    /// parameters of that function.
    pub(crate) fn apply_declarator(
        &mut self,
        base: Ty,
        d: &Declarator,
        ctx: DeclCtx,
    ) -> (Ty, Option<Ident>, Option<Vec<ParamInfo>>) {
        let mut t = base;
        for p in &d.pointers {
            t = self.types.ptr(t);
            t = self.types.qualified(t, Self::to_ty_quals(p.quals));
        }
        let mut own_params: Option<Vec<ParamInfo>> = None;
        for (i, suf) in d.suffixes.iter().enumerate().rev() {
            match suf {
                Suffix::Array { size, span, .. } => t = self.array_suffix(t, size, *span, ctx),
                Suffix::Function { params, variadic, span } => {
                    let (ft, infos) = self.function_suffix(t, params, *variadic, *span);
                    t = ft;
                    if i == 0 {
                        own_params = Some(infos);
                    }
                }
            }
        }
        match &d.inner {
            DeclaratorCore::Abstract => (t, None, own_params.filter(|_| d.outer_kind() == DerivedKind::Function)),
            DeclaratorCore::Name(n) => (t, Some(*n), own_params.filter(|_| d.outer_kind() == DerivedKind::Function)),
            DeclaratorCore::Nested(inner) => {
                let (ty, name, nested_params) = self.apply_declarator(t, inner, ctx);
                let params = if inner.outer_kind() == DerivedKind::Function {
                    nested_params
                } else if inner.outer_kind() == DerivedKind::Base && d.outer_kind() == DerivedKind::Function {
                    own_params
                } else {
                    None
                };
                (ty, name, params)
            }
        }
    }

    fn array_suffix(&mut self, elem: Ty, size: &ArraySize, span: Span, ctx: DeclCtx) -> Ty {
        if self.types.is_function(elem) {
            self.error(span, "array of functions is invalid");
            return self.types.p.int;
        }
        if !self.types.is_complete(elem) && !self.types.is_vla(elem) {
            let what = self.show(elem);
            let code = "array has incomplete element type";
            self.error(span, format!("{} '{}'", code, what));
            return self.types.p.int;
        }
        let len = match size {
            ArraySize::Unspecified | ArraySize::Star => ArrayLen::Incomplete,
            ArraySize::Expr(e) => {
                let h = self.expr(e);
                let h = self.rvalue(h);
                if !self.types.is_integer(h.ty) {
                    let t = self.show(h.ty);
                    self.error(e.span, format!("size of array has non-integer type '{}'", t));
                    return self.types.p.int;
                }
                match consteval::eval_int(&self.types, &h) {
                    Some(v) => {
                        let sv = if self.types.is_signed(h.ty) { v as i64 as i128 } else { v as i128 };
                        if sv < 0 {
                            self.error(e.span, format!("array size is negative ({})", sv));
                            return self.types.p.int;
                        }
                        ArrayLen::Known(sv as u64)
                    }
                    None => {
                        if ctx == DeclCtx::File || ctx == DeclCtx::Member {
                            self.error(e.span, "variable length array declaration is not allowed here");
                        } else {
                            self.error(e.span, "not yet supported: variable length arrays");
                        }
                        return self.types.p.int;
                    }
                }
            }
        };
        self.types.array(elem, len)
    }

    fn function_suffix(&mut self, ret: Ty, params: &[ParamDecl], variadic: bool, span: Span) -> (Ty, Vec<ParamInfo>) {
        if self.types.is_array(ret) {
            self.error(span, "function cannot return array type");
        } else if self.types.is_function(ret) {
            self.error(span, "function cannot return function type");
        }
        let unspecified = params.is_empty() && !variadic;
        let mut infos: Vec<ParamInfo> = Vec::new();
        // `(void)` means no parameters.
        let void_only = params.len() == 1
            && matches!(&params[0].specs.ty, Some(TypeSpec { kind: TypeSpecKind::Base(BaseType::Void), .. }))
            && params[0].declarator.pointers.is_empty()
            && params[0].declarator.suffixes.is_empty()
            && matches!(params[0].declarator.inner, DeclaratorCore::Abstract);
        if !void_only {
            for p in params {
                let base = self.type_from_specs(&p.specs);
                if let Some((sc, sp)) = p.specs.storage {
                    if sc != StorageClass::Register {
                        self.error(sp, "invalid storage class specifier in function declarator");
                    }
                }
                let (mut ty, name, _) = self.apply_declarator(base, &p.declarator, DeclCtx::Param);
                self.check_attrs(&p.declarator.attrs);
                // Parameter adjustment (C11 6.7.6.3p7-8).
                if let TyKind::Array(elem, _) = self.types.kind(ty).clone() {
                    let mut ptr = self.types.ptr(elem);
                    if let Some(Suffix::Array { quals, .. }) = name_level(&p.declarator).suffixes.first() {
                        ptr = self.types.qualified(ptr, Self::to_ty_quals(*quals));
                    }
                    ty = ptr;
                } else if self.types.is_function(ty) {
                    ty = self.types.ptr(ty);
                }
                if self.types.is_void(ty) {
                    self.error(p.span, "parameter may not have void type");
                    ty = self.types.p.int;
                }
                infos.push(ParamInfo { name, ty, span: p.span });
            }
        }
        let sig = FuncSig { ret, params: infos.iter().map(|i| i.ty).collect(), variadic, unspecified };
        (self.types.func(sig), infos)
    }

    // ───────────────────────────── declarations ─────────────────────────────

    fn static_assert(&mut self, sa: &StaticAssert) {
        let h = self.expr(&sa.cond);
        let h = self.rvalue(h);
        match consteval::eval_int(&self.types, &h) {
            Some(0) => {
                let msg = sa.message.clone().unwrap_or_default();
                self.error(sa.cond.span, format!("static assertion failed: {}", msg));
            }
            Some(_) => {}
            None => {
                if !matches!(h.kind, HExprKind::Error) {
                    self.error(sa.cond.span, "static assertion expression is not an integral constant expression");
                }
            }
        }
    }

    fn require_complete(&mut self, ty: Ty, span: Span, what: &str) -> bool {
        if self.types.is_complete(ty) || self.types.is_vla(ty) {
            return true;
        }
        let t = self.show(ty);
        self.error(span, format!("{} has incomplete type '{}'", what, t));
        false
    }

    fn file_declaration(&mut self, d: &Declaration) {
        let base = self.type_from_specs(&d.specs);
        let storage = d.specs.storage.map(|(s, _)| s);
        let spec_align = self.spec_align(&d.specs);
        for id in &d.declarators {
            let decl = &id.declarator;
            let (ty, name, _) = self.apply_declarator(base, decl, DeclCtx::File);
            self.check_attrs(&decl.attrs);
            let Some(name) = name else { continue };
            if storage == Some(StorageClass::Typedef) {
                self.declare_typedef(name, ty);
                continue;
            }
            if self.types.is_function(ty) {
                if id.init.is_some() {
                    self.error(name.span, "illegal initializer (only variables can be initialized)");
                }
                if matches!(storage, Some(StorageClass::Auto | StorageClass::Register)) {
                    self.error(name.span, "illegal storage class on file-scoped function");
                }
                let noreturn = d.specs.noreturn
                    || Self::has_attr(&d.specs.attrs, "noreturn")
                    || Self::has_attr(&decl.attrs, "noreturn");
                let sid = self.declare_global_func(name, ty, storage, d.specs.inline, noreturn);
                self.declare_ordinary(name.name, EntKind::Global(sid), name.span);
                continue;
            }
            if matches!(storage, Some(StorageClass::Auto | StorageClass::Register)) {
                self.error(name.span, "illegal storage class on file-scoped variable");
            }
            if d.specs.thread_local {
                self.error(name.span, "not yet supported: thread-local storage");
            }
            let mut align = spec_align.unwrap_or(0);
            if let Some(a) = self.attr_align(&decl.attrs).max(self.attr_align(&d.specs.attrs)) {
                align = align.max(a);
            }
            let mut ty = ty;
            let has_init = id.init.is_some();
            // Initializer (may complete an array type).
            let mut plan = None;
            if let Some(init) = &id.init {
                if storage == Some(StorageClass::Extern) {
                    self.warn(
                        Warn::ExternInitializer,
                        name.span,
                        format!("'extern' variable '{}' has an initializer", name.name),
                    );
                }
                let (t2, p) = self.initialize(ty, init, true, name.span);
                ty = t2;
                plan = p;
            } else if !self.types.is_complete(ty) && storage != Some(StorageClass::Extern) {
                // Tentative definition of an incomplete array is completed to one element (C11 6.9.2p5).
                if let TyKind::Array(e, ArrayLen::Incomplete) = self.types.kind(ty).clone() {
                    self.warn(
                        Warn::TentativeDefinition,
                        name.span,
                        "tentative array definition assumed to have one element",
                    );
                    ty = self.types.array(e, ArrayLen::Known(1));
                } else {
                    let t = self.show(ty);
                    self.error(name.span, format!("variable has incomplete type '{}'", t));
                }
            }
            let natural = self.types.align_of(ty);
            let sid = self.declare_global_var(name, ty, storage, has_init, align.max(natural));
            if let Some(p) = plan {
                self.syms[sid.0 as usize].init = Some(p);
            }
            self.declare_ordinary(name.name, EntKind::Global(sid), name.span);
        }
    }

    fn declare_typedef(&mut self, name: Ident, ty: Ty) {
        if let Some(prev) = self.scopes.last().unwrap().ordinary.get(&name.name).cloned() {
            match prev.kind {
                EntKind::Typedef(pt) if self.types.compatible(pt, ty) => return,
                EntKind::Typedef(pt) => {
                    let (a, b) = (self.show(pt), self.show(ty));
                    self.emit(
                        Diagnostic::error(
                            name.span,
                            format!("typedef redefinition with different types ('{}' vs '{}')", b, a),
                        )
                        .with_note(prev.span, "previous definition is here"),
                    );
                    return;
                }
                _ => {
                    self.emit(
                        Diagnostic::error(
                            name.span,
                            format!("redefinition of '{}' as different kind of symbol", name.name),
                        )
                        .with_note(prev.span, "previous definition is here"),
                    );
                    return;
                }
            }
        }
        self.declare_ordinary(name.name, EntKind::Typedef(ty), name.span);
    }

    /// A declaration inside a function body. Returns the statements to run.
    pub(crate) fn block_declaration(&mut self, d: &Declaration) -> Vec<HStmt> {
        let base = self.type_from_specs(&d.specs);
        let storage = d.specs.storage.map(|(s, _)| s);
        let spec_align = self.spec_align(&d.specs);
        let mut out: Vec<HStmt> = Vec::new();
        for id in &d.declarators {
            let decl = &id.declarator;
            let (ty, name, _) = self.apply_declarator(base, decl, DeclCtx::Block);
            self.check_attrs(&decl.attrs);
            let Some(name) = name else { continue };
            if storage == Some(StorageClass::Typedef) {
                self.declare_typedef(name, ty);
                continue;
            }
            if self.types.is_function(ty) {
                if id.init.is_some() {
                    self.error(name.span, "illegal initializer (only variables can be initialized)");
                }
                if storage == Some(StorageClass::Static) {
                    self.error(name.span, "function declared in block scope cannot have 'static' storage class");
                }
                let noreturn = d.specs.noreturn || Self::has_attr(&d.specs.attrs, "noreturn");
                let sid = self.declare_global_func(name, ty, None, d.specs.inline, noreturn);
                self.declare_ordinary(name.name, EntKind::Global(sid), name.span);
                continue;
            }
            if d.specs.thread_local {
                self.error(name.span, "not yet supported: thread-local storage");
            }
            if let Some(prev) = self.scopes.last().unwrap().ordinary.get(&name.name).cloned() {
                self.emit(
                    Diagnostic::error(name.span, format!("redefinition of '{}'", name.name))
                        .with_note(prev.span, "previous definition is here"),
                );
                continue;
            }
            let mut align = spec_align.unwrap_or(0);
            if let Some(a) = self.attr_align(&decl.attrs).max(self.attr_align(&d.specs.attrs)) {
                align = align.max(a);
            }
            let mut ty = ty;
            match storage {
                Some(StorageClass::Extern) => {
                    if id.init.is_some() {
                        self.error(name.span, "'extern' variable cannot have an initializer");
                    }
                    let sid = self.declare_global_var(name, ty, storage, false, align);
                    self.declare_ordinary(name.name, EntKind::Global(sid), name.span);
                }
                Some(StorageClass::Static) => {
                    // A static local lives in static storage under a mangled, file-private name.
                    let fname = self.f.as_ref().map(|f| f.name.to_string()).unwrap_or_default();
                    let mut plan = None;
                    if let Some(init) = &id.init {
                        let (t2, p) = self.initialize(ty, init, true, name.span);
                        ty = t2;
                        plan = p;
                    } else if !self.require_complete(ty, name.span, "variable") {
                        continue;
                    }
                    let natural = self.types.align_of(ty);
                    let sid =
                        self.new_anon_global(format!("{}.{}", fname, name.name), ty, name.span, align.max(natural));
                    let sym = &mut self.syms[sid.0 as usize];
                    sym.init = plan;
                    sym.tentative = sym.init.is_none();
                    self.declare_ordinary(name.name, EntKind::Global(sid), name.span);
                }
                _ => {
                    let mut plan = None;
                    if let Some(init) = &id.init {
                        let (t2, p) = self.initialize(ty, init, false, name.span);
                        ty = t2;
                        plan = p;
                    } else if !self.require_complete(ty, name.span, "variable") {
                        continue;
                    }
                    if self.types.is_vla(ty) {
                        continue;
                    }
                    let natural = self.types.align_of(ty);
                    let lid = self.new_local(name.name, ty, name.span, false, align.max(natural));
                    if Self::has_attr(&d.specs.attrs, "unused") || Self::has_attr(&decl.attrs, "unused") {
                        self.f.as_mut().unwrap().locals[lid.0 as usize].used = true;
                    }
                    self.declare_ordinary(name.name, EntKind::Local(lid), name.span);
                    out.push(HStmt { kind: HStmtKind::Decl { local: lid, init: plan }, span: name.span });
                }
            }
        }
        out
    }

    // ───────────────────────────── functions ─────────────────────────────

    fn function_def(&mut self, f: &FuncDef) {
        let base = self.type_from_specs(&f.specs);
        let storage = f.specs.storage.map(|(s, _)| s);
        let (fty, name, params) = self.apply_declarator(base, &f.declarator, DeclCtx::File);
        self.check_attrs(&f.declarator.attrs);
        let Some(name) = name else { return };
        let Some(sig) = self.types.func_sig(fty).cloned() else {
            self.error(name.span, "function definition does not have function type");
            return;
        };
        if matches!(storage, Some(StorageClass::Auto | StorageClass::Register | StorageClass::Typedef)) {
            self.error(name.span, "illegal storage class on function");
        }
        let noreturn = f.specs.noreturn
            || Self::has_attr(&f.specs.attrs, "noreturn")
            || Self::has_attr(&f.declarator.attrs, "noreturn");
        let sid = self.declare_global_func(name, fty, storage, f.specs.inline, noreturn);
        self.declare_ordinary(name.name, EntKind::Global(sid), name.span);
        if self.syms[sid.0 as usize].defined {
            let prev = self.syms[sid.0 as usize].span;
            self.emit(
                Diagnostic::error(name.span, format!("redefinition of '{}'", name.name))
                    .with_note(prev, "previous definition is here"),
            );
            return;
        }
        {
            let s = &mut self.syms[sid.0 as usize];
            s.defined = true;
            s.span = name.span;
        }
        if !self.types.is_void(sig.ret) && !self.types.is_complete(sig.ret) {
            let t = self.show(sig.ret);
            self.error(name.span, format!("incomplete result type '{}' in function definition", t));
        }

        self.f = Some(FnCtx {
            name: name.name,
            ret: sig.ret,
            locals: Vec::new(),
            labels: Vec::new(),
            label_map: HashMap::new(),
            switches: Vec::new(),
            loops: 0,
            breakables: 0,
            next_case: 0,
        });
        self.push_scope();
        let mut param_ids = Vec::new();
        for p in params.unwrap_or_default() {
            if !self.types.is_complete(p.ty) {
                let t = self.show(p.ty);
                self.error(p.span, format!("variable has incomplete type '{}'", t));
            }
            let (pname, span) = match p.name {
                Some(n) => (n.name, n.span),
                None => (Symbol::new(""), p.span),
            };
            let align = self.types.align_of(p.ty);
            let id = self.new_local(pname, p.ty, span, true, align);
            if let Some(n) = p.name {
                if self.scopes.last().unwrap().ordinary.contains_key(&n.name) {
                    self.error(n.span, format!("redefinition of parameter '{}'", n.name));
                }
                self.declare_ordinary(n.name, EntKind::Local(id), n.span);
            }
            param_ids.push(id);
        }
        // The outermost block of the body shares the parameter scope.
        let body = match &f.body.kind {
            StmtKind::Compound(items) => {
                let stmts = self.block_items(items);
                HStmt { kind: HStmtKind::Block(stmts), span: f.body.span }
            }
            _ => unreachable!("function body is a compound statement"),
        };
        self.pop_scope();
        let ctx = self.f.take().unwrap();
        // Labels: every goto target must be defined; unused labels warn.
        let mut label_names = Vec::new();
        for l in &ctx.labels {
            label_names.push(l.name);
            match l.defined {
                None => self.error(l.first_use, format!("use of undeclared label '{}'", l.name)),
                Some(sp) if !l.used => self.warn(Warn::UnusedLabel, sp, format!("unused label '{}'", l.name)),
                _ => {}
            }
        }
        self.funcs.push(HirFunc {
            sym: sid,
            name: name.name,
            params: param_ids,
            locals: ctx.locals,
            body,
            ret: sig.ret,
            variadic: sig.variadic,
            labels: label_names,
            span: f.span,
            is_static: storage == Some(StorageClass::Static),
            is_inline: f.specs.inline,
            noreturn,
        });
    }

    fn finish(&mut self) {
        // Unused file-local functions and variables.
        let mut unused: Vec<(Span, String, bool)> = Vec::new();
        for s in &self.syms {
            if s.linkage == Linkage::Internal && !s.used && s.defined && !s.is_inline {
                let is_func = s.kind == SymKind::Func;
                if !is_func && s.is_const {
                    continue;
                }
                if !s.name.as_str().contains('.') {
                    unused.push((s.span, s.name.to_string(), is_func));
                }
            }
        }
        for (span, name, is_func) in unused {
            if is_func {
                self.warn(Warn::UnusedFunction, span, format!("unused function '{}'", name));
            } else {
                self.warn(Warn::UnusedVariable, span, format!("unused variable '{}'", name));
            }
        }
    }
}

/// The declarator level that directly contains the declared name.
fn name_level(d: &Declarator) -> &Declarator {
    match &d.inner {
        DeclaratorCore::Nested(n) => name_level(n),
        _ => d,
    }
}
