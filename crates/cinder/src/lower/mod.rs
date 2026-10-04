//! Lowering from the typed HIR to the SSA IR.
//!
//! Locals become `alloca` slots accessed with `load`/`store` (like Clang's
//! `-O0`); the `mem2reg` pass later promotes them to SSA values. Aggregates
//! are always handled by address. Calls, parameters and returns follow the
//! System V ABI: structs of up to 16 bytes travel in registers (split into
//! eightbyte pieces), larger ones in memory (`byval` arguments, hidden
//! `sret` result pointer).

pub mod abi;
mod data;
mod expr;
mod stmt;
#[cfg(test)]
mod tests;

use crate::diag::Warn;
use crate::hir::{self, HirFunc, HirModule, LocalId, StrId};
use crate::intern::Symbol;
use crate::ir::*;
use crate::lower::abi::{classify, AggAbi, Piece};
use crate::session::Session;
use crate::source::Span;
use crate::types::{ArrayLen, Ty, TyKind};
use std::collections::HashMap;

/// Stack frames are 16-byte aligned; slots needing more are realigned in IR.
pub const MAX_FRAME_ALIGN: u32 = 16;

pub struct ModBuilder {
    pub m: Module,
    sym_map: Vec<Option<SymId>>,
    str_syms: HashMap<StrId, SymId>,
    anon: u32,
}

/// Lower a whole translation unit.
pub fn lower_module(sess: &mut Session, hir: &HirModule, source_file: &str) -> Module {
    let mut mb = ModBuilder {
        m: Module { syms: Vec::new(), funcs: Vec::new(), source_file: source_file.to_string() },
        sym_map: vec![None; hir.syms.len()],
        str_syms: HashMap::new(),
        anon: 0,
    };
    // Defined symbols first so output order is deterministic.
    for (i, s) in hir.syms.iter().enumerate() {
        if s.defined {
            mb.sym(hir, hir::SymId(i as u32));
        }
    }
    for hf in &hir.funcs {
        let f = FnLower::lower(sess, hir, &mut mb, hf);
        mb.m.funcs.push(f);
    }
    mb.m
}

impl ModBuilder {
    /// The IR symbol for a HIR symbol, created on first use.
    pub fn sym(&mut self, hir: &HirModule, id: hir::SymId) -> SymId {
        if let Some(s) = self.sym_map[id.0 as usize] {
            return s;
        }
        let hs = &hir.syms[id.0 as usize];
        let linkage = match hs.linkage {
            hir::Linkage::External => Linkage::External,
            hir::Linkage::Internal => Linkage::Internal,
        };
        let sid = SymId(self.m.syms.len() as u32);
        // Reserve the slot first: data initializers may refer back to this symbol.
        self.m.syms.push(Sym { name: hs.name, linkage, body: SymBody::Data(None) });
        self.sym_map[id.0 as usize] = Some(sid);
        let body = match hs.kind {
            hir::SymKind::Func => SymBody::Func { defined: hs.defined },
            hir::SymKind::Var => {
                if hs.defined {
                    SymBody::Data(Some(self.build_var_data(hir, hs)))
                } else {
                    SymBody::Data(None)
                }
            }
        };
        self.m.syms[sid.idx()].body = body;
        sid
    }

    fn fresh_name(&mut self, prefix: &str) -> Symbol {
        self.anon += 1;
        Symbol::new(&format!("{}.{}", prefix, self.anon))
    }

    /// Add an anonymous read-only/internal data symbol.
    pub fn add_anon_data(&mut self, prefix: &str, def: DataDef) -> SymId {
        let name = self.fresh_name(prefix);
        let id = SymId(self.m.syms.len() as u32);
        self.m.syms.push(Sym { name, linkage: Linkage::Internal, body: SymBody::Data(Some(def)) });
        id
    }
}

fn round_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// Sign-extend the low bits of `v` to the width of `ty` (the canonical form
/// of integer constants in the IR).
pub fn canon(v: u64, ty: Type) -> i64 {
    match ty {
        Type::I8 => v as i8 as i64,
        Type::I16 => v as i16 as i64,
        Type::I32 => v as i32 as i64,
        _ => v as i64,
    }
}

pub(crate) struct FnLower<'a> {
    pub hir: &'a HirModule,
    pub hf: &'a HirFunc,
    pub mb: &'a mut ModBuilder,
    pub sess: &'a mut Session,
    pub f: Func,
    pub cur: BlockId,
    pub slots: Vec<Operand>,
    pub labels: HashMap<u32, BlockId>,
    pub case_blocks: HashMap<u32, BlockId>,
    pub break_targets: Vec<BlockId>,
    pub continue_targets: Vec<BlockId>,
    /// Saved stack pointers of the enclosing blocks that declare VLAs, outermost first.
    pub vla_scopes: Vec<Operand>,
    /// `vla_scopes.len()` when each enclosing loop/switch was entered (what `break`/`continue` unwind to).
    pub break_vla: Vec<usize>,
    pub continue_vla: Vec<usize>,
    pub sret: Option<Operand>,
    pub line: u32,
    pub next_group: u32,
}

/// How a parameter or result of a given C type is passed.
pub(crate) enum Passing {
    /// One scalar, widened to at least `i32` for integers.
    Scalar(Type),
    Pieces(Vec<Piece>),
    ByVal,
    Nothing,
}

impl<'a> FnLower<'a> {
    pub fn lower(sess: &'a mut Session, hir: &'a HirModule, mb: &'a mut ModBuilder, hf: &'a HirFunc) -> Func {
        let sym = hir::SymId(hf.sym.0);
        let isym = mb.sym(hir, sym);
        let hs = &hir.syms[sym.0 as usize];
        let linkage = match hs.linkage {
            hir::Linkage::External => Linkage::External,
            hir::Linkage::Internal => Linkage::Internal,
        };
        let mut f = Func::new(hf.name, isym, linkage);
        f.is_inline = hf.is_inline;
        f.noreturn = hf.noreturn;
        f.variadic = hf.variadic;
        let entry = f.new_block("entry");
        let mut fl = FnLower {
            hir,
            hf,
            mb,
            sess,
            f,
            cur: entry,
            slots: Vec::new(),
            labels: HashMap::new(),
            case_blocks: HashMap::new(),
            break_targets: Vec::new(),
            continue_targets: Vec::new(),
            vla_scopes: Vec::new(),
            break_vla: Vec::new(),
            continue_vla: Vec::new(),
            sret: None,
            line: 0,
            next_group: 0,
        };
        fl.f.line = fl.line_of(hf.span);
        fl.line = fl.f.line;
        fl.setup();
        fl.lower_body();
        fl.finish()
    }

    // ───────────────────────────── builder helpers ─────────────────────────────

    pub fn line_of(&self, span: Span) -> u32 {
        self.sess.sources.line_of(span)
    }

    pub fn emit(&mut self, kind: InstKind, ty: Option<Type>) -> Option<Operand> {
        let line = self.line;
        self.f.push(self.cur, kind, ty, None, line)
    }

    pub fn emit_named(&mut self, kind: InstKind, ty: Type, name: Symbol) -> Operand {
        let line = self.line;
        self.f.push(self.cur, kind, Some(ty), Some(name), line).unwrap()
    }

    pub fn val(&mut self, kind: InstKind, ty: Type) -> Operand {
        self.emit(kind, Some(ty)).expect("value-producing instruction")
    }

    pub fn new_block(&mut self, name: &str) -> BlockId {
        self.f.new_block(name)
    }

    pub fn set_cur(&mut self, b: BlockId) {
        self.cur = b;
    }

    pub fn is_terminated(&self) -> bool {
        self.f.blocks[self.cur.idx()].term.is_terminated()
    }

    pub fn terminate(&mut self, t: Term) {
        if !self.is_terminated() {
            let line = self.line;
            let b = &mut self.f.blocks[self.cur.idx()];
            b.term = t;
            b.term_line = line;
        }
    }

    /// Branch to `to` (if the current block is still open).
    pub fn br(&mut self, to: BlockId) {
        self.terminate(Term::Br(to));
    }

    /// After a terminator: continue emitting into a fresh block that has no
    /// predecessors (dead code), removed later by unreachable-block cleanup.
    pub fn start_dead_block(&mut self) {
        let b = self.new_block("dead");
        self.cur = b;
    }

    pub fn ptr_add(&mut self, base: Operand, off: u64) -> Operand {
        if off == 0 {
            return base;
        }
        self.val(InstKind::PtrAdd { base, offset: Operand::Int(off as i64, Type::I64) }, Type::Ptr)
    }

    // ───────────────────────────── type helpers ─────────────────────────────

    pub fn is_agg(&self, ty: Ty) -> bool {
        self.hir.types.is_record(ty) || self.hir.types.is_array(ty)
    }

    /// IR type of a scalar C type (exact width, for memory access).
    pub fn ity(&self, ty: Ty) -> Type {
        let t = &self.hir.types;
        let r = t.int_repr(ty);
        match t.kind(r) {
            TyKind::Bool | TyKind::Char | TyKind::SChar | TyKind::UChar => Type::I8,
            TyKind::Short | TyKind::UShort => Type::I16,
            TyKind::Int | TyKind::UInt => Type::I32,
            TyKind::Long | TyKind::ULong | TyKind::LongLong | TyKind::ULongLong => Type::I64,
            TyKind::Float => Type::F32,
            TyKind::Double => Type::F64,
            TyKind::Ptr(_) | TyKind::Array(..) | TyKind::Func(_) => Type::Ptr,
            TyKind::Void | TyKind::Record(_) | TyKind::Enum(_) => Type::I32,
        }
    }

    /// Round the address of a stack slot (allocated with `align - 1` spare bytes) up to `align`.
    pub fn realign(&mut self, raw: Operand, align: u32) -> Operand {
        let i = self.cast(CastOp::PtrToInt, Type::Ptr, Type::I64, raw);
        let i = self.bin(BinOp::Add, Type::I64, i, Operand::Int(align as i64 - 1, Type::I64));
        let i = self.bin(BinOp::And, Type::I64, i, Operand::Int(-(align as i64), Type::I64));
        self.cast(CastOp::IntToPtr, Type::I64, Type::Ptr, i)
    }

    /// The size in bytes of `ty`, as a run-time value (variable length arrays).
    pub fn runtime_size(&mut self, ty: Ty) -> Operand {
        if !self.hir.types.is_vla(ty) {
            return Operand::Int(self.size_of(ty) as i64, Type::I64);
        }
        match self.hir.types.kind(ty).clone() {
            TyKind::Array(elem, len) => {
                let n = match len {
                    ArrayLen::Vla(id) => self.vla_len(id),
                    ArrayLen::Known(n) => Operand::Int(n as i64, Type::I64),
                    ArrayLen::Incomplete => Operand::Int(0, Type::I64),
                };
                let es = self.runtime_size(elem);
                self.bin(BinOp::Mul, Type::I64, n, es)
            }
            _ => Operand::Int(self.size_of(ty) as i64, Type::I64),
        }
    }

    /// The run-time length held in the hidden local of VLA declarator `id`.
    fn vla_len(&mut self, id: u32) -> Operand {
        let Some(&(_, lid)) = self.hf.vla_lens.iter().find(|(i, _)| *i == id) else {
            return Operand::Int(0, Type::I64);
        };
        let slot = self.slots[lid.0 as usize];
        self.load(Type::I64, slot, false)
    }

    /// Size of the pointee of `ptr_ty` (which is a variable length array type) at run time.
    pub fn dyn_pointee_size(&mut self, ptr_ty: Ty) -> Operand {
        match self.hir.types.pointee(ptr_ty) {
            Some(p) => self.runtime_size(p),
            None => Operand::Int(1, Type::I64),
        }
    }

    pub fn is_vla_pointee(&self, ptr_ty: Ty) -> bool {
        self.hir.types.pointee(ptr_ty).is_some_and(|p| self.hir.types.is_vla(p))
    }

    pub fn size_of(&self, ty: Ty) -> u64 {
        self.hir.types.size_of(ty).unwrap_or(0)
    }

    pub fn align_of(&self, ty: Ty) -> u32 {
        self.hir.types.align_of(ty).max(1) as u32
    }

    pub fn is_signed(&self, ty: Ty) -> bool {
        self.hir.types.is_signed(ty) && !self.hir.types.is_pointer(ty)
    }

    fn passing(&self, ty: Ty) -> Passing {
        if self.hir.types.is_record(ty) {
            return match classify(&self.hir.types, ty) {
                AggAbi::Empty => Passing::Nothing,
                AggAbi::Regs(p) => Passing::Pieces(p),
                AggAbi::Memory => Passing::ByVal,
            };
        }
        let t = self.ity(ty);
        Passing::Scalar(if t.is_int() && t.size() < 4 { Type::I32 } else { t })
    }

    // ───────────────────────────── function setup ─────────────────────────────

    fn setup(&mut self) {
        let hf = self.hf;
        let types = &self.hir.types;
        // Result passing.
        let ret_ty = hf.ret;
        if !types.is_void(ret_ty) {
            match self.passing(ret_ty) {
                Passing::Scalar(t) => self.f.rets = vec![t],
                Passing::Pieces(p) => self.f.rets = p.iter().map(|x| x.ty).collect(),
                Passing::ByVal => {
                    let v = self.f.add_param(ParamKind::Value(Type::Ptr), None, Some(Symbol::new("sret")));
                    self.sret = Some(Operand::Value(v));
                    self.f.rets = vec![Type::Ptr];
                }
                Passing::Nothing => {}
            }
        }
        // Parameters.
        struct Incoming {
            local: LocalId,
            vals: Vec<(Operand, Piece)>,
            byval: Option<Operand>,
            scalar: Option<Operand>,
        }
        let mut incoming: Vec<Incoming> = Vec::new();
        for &lid in &hf.params {
            let l = &hf.locals[lid.0 as usize];
            let name = if l.name.as_str().is_empty() { None } else { Some(l.name) };
            match self.passing(l.ty) {
                Passing::Scalar(t) => {
                    let v = self.f.add_param(ParamKind::Value(t), None, name);
                    incoming.push(Incoming { local: lid, vals: vec![], byval: None, scalar: Some(Operand::Value(v)) });
                }
                Passing::Pieces(pieces) => {
                    let g = self.next_group;
                    self.next_group += 1;
                    let mut vals = Vec::new();
                    for p in pieces {
                        let v = self.f.add_param(ParamKind::Value(p.ty), Some(g), name);
                        vals.push((Operand::Value(v), p));
                    }
                    incoming.push(Incoming { local: lid, vals, byval: None, scalar: None });
                }
                Passing::ByVal => {
                    let size = self.size_of(l.ty) as u32;
                    let align = self.align_of(l.ty);
                    let v = self.f.add_param(ParamKind::ByVal { size, align }, None, name);
                    incoming.push(Incoming { local: lid, vals: vec![], byval: Some(Operand::Value(v)), scalar: None });
                }
                Passing::Nothing => incoming.push(Incoming { local: lid, vals: vec![], byval: None, scalar: None }),
            }
        }
        // Allocas for every local (static slots in the entry block). Frames are only
        // 16-byte aligned, so a slot that needs more is over-allocated and its address
        // rounded up in IR (see `realign`).
        let mut over_aligned: Vec<(usize, u32)> = Vec::new();
        for (i, l) in hf.locals.iter().enumerate() {
            if self.hir.types.is_vla(l.ty) {
                // allocated on the stack where the declaration is reached (`HStmtKind::VlaDecl`)
                self.slots.push(Operand::Undef(Type::Ptr));
                continue;
            }
            let size = round_up(self.size_of(l.ty).max(1), 8);
            let size = if self.is_agg(l.ty) { size } else { self.size_of(l.ty).max(1) } as u32;
            let align = (l.align as u32).max(self.align_of(l.ty)).max(1);
            let kind = if align > MAX_FRAME_ALIGN {
                over_aligned.push((i, align));
                InstKind::Alloca { size: size + align - 1, align: MAX_FRAME_ALIGN }
            } else {
                InstKind::Alloca { size, align }
            };
            let slot = self.emit_named(kind, Type::Ptr, l.name);
            self.slots.push(slot);
        }
        for (i, align) in over_aligned {
            self.slots[i] = self.realign(self.slots[i], align);
        }
        // By-value aggregate parameters live in the caller-provided stack copy.
        for inc in &incoming {
            if let Some(p) = inc.byval {
                self.slots[inc.local.0 as usize] = p;
            }
        }
        // Spill incoming values into the parameters' slots.
        for inc in incoming {
            let l = &hf.locals[inc.local.0 as usize];
            let slot = self.slots[inc.local.0 as usize];
            if let Some(v) = inc.scalar {
                let natural = self.ity(l.ty);
                let v = if self.f.operand_ty(v) != natural && natural.is_int() {
                    self.val(InstKind::Cast { op: CastOp::Trunc, from: Type::I32, to: natural, val: v }, natural)
                } else {
                    v
                };
                self.emit(InstKind::Store { ty: natural, val: v, ptr: slot, volatile: false }, None);
            }
            for (v, piece) in inc.vals {
                let ptr = self.ptr_add(slot, piece.offset as u64);
                self.emit(InstKind::Store { ty: piece.ty, val: v, ptr, volatile: false }, None);
            }
        }
    }

    fn lower_body(&mut self) {
        let body = &self.hf.body;
        self.stmt(body);
    }

    fn finish(mut self) -> Func {
        // Falling off the end of the function.
        if !self.is_terminated() {
            let name = self.hf.name;
            let is_main = name.as_str() == "main";
            let rets = self.f.rets.clone();
            let ret_is_void = self.hir.types.is_void(self.hf.ret);
            if !ret_is_void && !is_main {
                // Warn only if the end of the function is actually reachable.
                let cfg = cfg::build(&self.f);
                let reach = cfg::reachable(&self.f, &cfg);
                if reach[self.cur.idx()] {
                    let span = self.hf.body.span;
                    let close = Span::new(span.file, span.hi.saturating_sub(1), span.hi);
                    self.sess.diags.warn(
                        Warn::ReturnType,
                        close,
                        "non-void function does not return a value in all control paths",
                    );
                }
            }
            let vals: Vec<Operand> = if let Some(s) = self.sret {
                vec![s]
            } else {
                rets.iter()
                    .map(|t| match t {
                        Type::F32 | Type::F64 => Operand::float(0.0, *t),
                        _ => Operand::Int(0, *t),
                    })
                    .collect()
            };
            self.terminate(Term::Ret(vals));
        }
        // Dead blocks (code after return/break/goto) are never wanted.
        for b in &mut self.f.blocks {
            if !b.term.is_terminated() {
                b.term = Term::Unreachable;
            }
        }
        cfg::remove_unreachable(&mut self.f);
        self.f
    }
}
