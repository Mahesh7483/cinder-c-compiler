//! The C type system: interned types, record (struct/union) layout following
//! the System V x86-64 ABI, compatibility, conversions and type printing.

use crate::intern::Symbol;
use crate::source::Span;
use std::collections::HashMap;

/// Handle to an interned type. Equal handles mean identical types
/// (including qualifiers).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Ty(u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct TyQuals {
    pub is_const: bool,
    pub is_volatile: bool,
    pub is_restrict: bool,
}

impl TyQuals {
    pub const NONE: TyQuals = TyQuals { is_const: false, is_volatile: false, is_restrict: false };

    pub fn is_empty(self) -> bool {
        !(self.is_const || self.is_volatile || self.is_restrict)
    }

    pub fn union(self, o: TyQuals) -> TyQuals {
        TyQuals {
            is_const: self.is_const | o.is_const,
            is_volatile: self.is_volatile | o.is_volatile,
            is_restrict: self.is_restrict | o.is_restrict,
        }
    }

    /// Does `self` contain every qualifier of `o`?
    pub fn contains(self, o: TyQuals) -> bool {
        (self.is_const || !o.is_const) && (self.is_volatile || !o.is_volatile) && (self.is_restrict || !o.is_restrict)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RecordId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EnumId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ArrayLen {
    Known(u64),
    /// `int a[]`
    Incomplete,
    /// Variable length array; the size is a run-time value.
    Vla,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FuncSig {
    pub ret: Ty,
    pub params: Vec<Ty>,
    pub variadic: bool,
    /// Declared with an empty parameter list `f()`: parameters unspecified.
    pub unspecified: bool,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TyKind {
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
    Ptr(Ty),
    Array(Ty, ArrayLen),
    Func(FuncSig),
    Record(RecordId),
    Enum(EnumId),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct TypeData {
    kind: TyKind,
    quals: TyQuals,
}

/// Where a bit-field lives, relative to its record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitInfo {
    /// Absolute bit offset from the start of the record.
    pub bit_offset: u64,
    pub width: u32,
}

#[derive(Clone, Debug)]
pub struct Field {
    /// `None` for anonymous struct/union members and unnamed bit-fields.
    pub name: Option<Symbol>,
    pub ty: Ty,
    /// Byte offset (for bit-fields: byte containing the first bit).
    pub offset: u64,
    pub bit: Option<BitInfo>,
    /// An anonymous struct/union member whose fields are accessible directly.
    pub anonymous: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct RecordData {
    pub tag: Option<Symbol>,
    pub is_union: bool,
    pub fields: Vec<Field>,
    pub complete: bool,
    pub size: u64,
    pub align: u64,
    pub has_flexible_array: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnumData {
    pub tag: Option<Symbol>,
    pub underlying: Ty,
    pub complete: bool,
}

/// Pre-interned primitive types.
#[derive(Clone, Copy, Debug)]
pub struct Prims {
    pub void: Ty,
    pub bool_: Ty,
    pub char_: Ty,
    pub schar: Ty,
    pub uchar: Ty,
    pub short: Ty,
    pub ushort: Ty,
    pub int: Ty,
    pub uint: Ty,
    pub long: Ty,
    pub ulong: Ty,
    pub llong: Ty,
    pub ullong: Ty,
    pub float: Ty,
    pub double: Ty,
    /// `__builtin_va_list` (`struct __va_list_tag[1]`)
    pub va_list: Ty,
    pub va_list_tag: RecordId,
}

/// One member as seen by layout (before offsets are known).
pub struct MemberInput {
    pub name: Option<Symbol>,
    pub ty: Ty,
    pub bit_width: Option<u32>,
    /// `_Alignas` / `aligned` attribute on the member.
    pub align: Option<u64>,
    pub anonymous: bool,
    pub span: Span,
}

pub struct TypeTable {
    data: Vec<TypeData>,
    map: HashMap<TypeData, Ty>,
    pub records: Vec<RecordData>,
    pub enums: Vec<EnumData>,
    pub p: Prims,
}

impl Default for TypeTable {
    fn default() -> TypeTable {
        TypeTable::new()
    }
}

impl TypeTable {
    pub fn new() -> TypeTable {
        let dummy = Ty(0);
        let mut t = TypeTable {
            data: Vec::new(),
            map: HashMap::new(),
            records: Vec::new(),
            enums: Vec::new(),
            p: Prims {
                void: dummy,
                bool_: dummy,
                char_: dummy,
                schar: dummy,
                uchar: dummy,
                short: dummy,
                ushort: dummy,
                int: dummy,
                uint: dummy,
                long: dummy,
                ulong: dummy,
                llong: dummy,
                ullong: dummy,
                float: dummy,
                double: dummy,
                va_list: dummy,
                va_list_tag: RecordId(0),
            },
        };
        let mk = |t: &mut TypeTable, k: TyKind| t.intern(k, TyQuals::NONE);
        t.p.void = mk(&mut t, TyKind::Void);
        t.p.bool_ = mk(&mut t, TyKind::Bool);
        t.p.char_ = mk(&mut t, TyKind::Char);
        t.p.schar = mk(&mut t, TyKind::SChar);
        t.p.uchar = mk(&mut t, TyKind::UChar);
        t.p.short = mk(&mut t, TyKind::Short);
        t.p.ushort = mk(&mut t, TyKind::UShort);
        t.p.int = mk(&mut t, TyKind::Int);
        t.p.uint = mk(&mut t, TyKind::UInt);
        t.p.long = mk(&mut t, TyKind::Long);
        t.p.ulong = mk(&mut t, TyKind::ULong);
        t.p.llong = mk(&mut t, TyKind::LongLong);
        t.p.ullong = mk(&mut t, TyKind::ULongLong);
        t.p.float = mk(&mut t, TyKind::Float);
        t.p.double = mk(&mut t, TyKind::Double);

        // struct __va_list_tag { unsigned gp_offset, fp_offset; void *overflow_arg_area, *reg_save_area; }
        let vp = t.ptr(t.p.void);
        let rid = t.new_record(Some(Symbol::new("__va_list_tag")), false, Span::DUMMY);
        let member = |name: &str, ty: Ty| MemberInput {
            name: Some(Symbol::new(name)),
            ty,
            bit_width: None,
            align: None,
            anonymous: false,
            span: Span::DUMMY,
        };
        let uint = t.p.uint;
        t.layout_record(
            rid,
            vec![
                member("gp_offset", uint),
                member("fp_offset", uint),
                member("overflow_arg_area", vp),
                member("reg_save_area", vp),
            ],
            None,
        );
        let rec_ty = t.record_type(rid);
        t.p.va_list_tag = rid;
        t.p.va_list = t.array(rec_ty, ArrayLen::Known(1));
        t
    }

    fn intern(&mut self, kind: TyKind, quals: TyQuals) -> Ty {
        let d = TypeData { kind, quals };
        if let Some(&t) = self.map.get(&d) {
            return t;
        }
        let id = Ty(self.data.len() as u32);
        self.data.push(d.clone());
        self.map.insert(d, id);
        id
    }

    // ───────────────────────────── constructors ─────────────────────────────

    pub fn ptr(&mut self, pointee: Ty) -> Ty {
        self.intern(TyKind::Ptr(pointee), TyQuals::NONE)
    }

    pub fn array(&mut self, elem: Ty, len: ArrayLen) -> Ty {
        self.intern(TyKind::Array(elem, len), TyQuals::NONE)
    }

    pub fn func(&mut self, sig: FuncSig) -> Ty {
        self.intern(TyKind::Func(sig), TyQuals::NONE)
    }

    pub fn record_type(&mut self, id: RecordId) -> Ty {
        self.intern(TyKind::Record(id), TyQuals::NONE)
    }

    pub fn enum_type(&mut self, id: EnumId) -> Ty {
        self.intern(TyKind::Enum(id), TyQuals::NONE)
    }

    pub fn new_record(&mut self, tag: Option<Symbol>, is_union: bool, span: Span) -> RecordId {
        let id = RecordId(self.records.len() as u32);
        self.records.push(RecordData {
            tag,
            is_union,
            fields: Vec::new(),
            complete: false,
            size: 0,
            align: 1,
            has_flexible_array: false,
            span,
        });
        id
    }

    pub fn new_enum(&mut self, tag: Option<Symbol>) -> EnumId {
        let id = EnumId(self.enums.len() as u32);
        let int = self.p.int;
        self.enums.push(EnumData { tag, underlying: int, complete: false });
        id
    }

    // ───────────────────────────── accessors ─────────────────────────────

    pub fn kind(&self, t: Ty) -> &TyKind {
        &self.data[t.0 as usize].kind
    }

    pub fn quals(&self, t: Ty) -> TyQuals {
        self.data[t.0 as usize].quals
    }

    pub fn unqual(&mut self, t: Ty) -> Ty {
        if self.quals(t).is_empty() {
            return t;
        }
        let k = self.kind(t).clone();
        self.intern(k, TyQuals::NONE)
    }

    /// Add qualifiers. Qualifiers on an array type apply to its element type.
    pub fn qualified(&mut self, t: Ty, q: TyQuals) -> Ty {
        if q.is_empty() {
            return t;
        }
        if let TyKind::Array(elem, len) = self.kind(t).clone() {
            let e = self.qualified(elem, q);
            return self.array(e, len);
        }
        let k = self.kind(t).clone();
        let nq = self.quals(t).union(q);
        self.intern(k, nq)
    }

    pub fn record(&self, id: RecordId) -> &RecordData {
        &self.records[id.0 as usize]
    }

    pub fn record_mut(&mut self, id: RecordId) -> &mut RecordData {
        &mut self.records[id.0 as usize]
    }

    pub fn pointee(&self, t: Ty) -> Option<Ty> {
        match self.kind(t) {
            TyKind::Ptr(p) => Some(*p),
            _ => None,
        }
    }

    pub fn array_elem(&self, t: Ty) -> Option<Ty> {
        match self.kind(t) {
            TyKind::Array(e, _) => Some(*e),
            _ => None,
        }
    }

    pub fn func_sig(&self, t: Ty) -> Option<&FuncSig> {
        match self.kind(t) {
            TyKind::Func(s) => Some(s),
            _ => None,
        }
    }

    pub fn record_id(&self, t: Ty) -> Option<RecordId> {
        match self.kind(t) {
            TyKind::Record(r) => Some(*r),
            _ => None,
        }
    }

    // ───────────────────────────── classification ─────────────────────────────

    /// Integer type underlying `t`, resolving enums.
    pub fn int_repr(&self, t: Ty) -> Ty {
        match self.kind(t) {
            TyKind::Enum(e) => self.enums[e.0 as usize].underlying,
            _ => t,
        }
    }

    pub fn is_void(&self, t: Ty) -> bool {
        matches!(self.kind(t), TyKind::Void)
    }

    pub fn is_bool(&self, t: Ty) -> bool {
        matches!(self.kind(t), TyKind::Bool)
    }

    pub fn is_integer(&self, t: Ty) -> bool {
        matches!(
            self.kind(t),
            TyKind::Bool
                | TyKind::Char
                | TyKind::SChar
                | TyKind::UChar
                | TyKind::Short
                | TyKind::UShort
                | TyKind::Int
                | TyKind::UInt
                | TyKind::Long
                | TyKind::ULong
                | TyKind::LongLong
                | TyKind::ULongLong
                | TyKind::Enum(_)
        )
    }

    pub fn is_floating(&self, t: Ty) -> bool {
        matches!(self.kind(t), TyKind::Float | TyKind::Double)
    }

    pub fn is_arithmetic(&self, t: Ty) -> bool {
        self.is_integer(t) || self.is_floating(t)
    }

    pub fn is_pointer(&self, t: Ty) -> bool {
        matches!(self.kind(t), TyKind::Ptr(_))
    }

    pub fn is_scalar(&self, t: Ty) -> bool {
        self.is_arithmetic(t) || self.is_pointer(t)
    }

    pub fn is_array(&self, t: Ty) -> bool {
        matches!(self.kind(t), TyKind::Array(..))
    }

    pub fn is_function(&self, t: Ty) -> bool {
        matches!(self.kind(t), TyKind::Func(_))
    }

    pub fn is_record(&self, t: Ty) -> bool {
        matches!(self.kind(t), TyKind::Record(_))
    }

    /// struct, union or array
    pub fn is_aggregate(&self, t: Ty) -> bool {
        self.is_record(t) || self.is_array(t)
    }

    pub fn is_signed(&self, t: Ty) -> bool {
        matches!(
            self.kind(self.int_repr(t)),
            TyKind::Char
                | TyKind::SChar
                | TyKind::Short
                | TyKind::Int
                | TyKind::Long
                | TyKind::LongLong
                | TyKind::Float
                | TyKind::Double
        )
    }

    pub fn is_unsigned_int(&self, t: Ty) -> bool {
        self.is_integer(t) && !self.is_signed(t)
    }

    pub fn is_complete(&self, t: Ty) -> bool {
        match self.kind(t) {
            TyKind::Void => false,
            TyKind::Array(e, len) => *len != ArrayLen::Incomplete && self.is_complete(*e),
            TyKind::Record(r) => self.records[r.0 as usize].complete,
            TyKind::Enum(e) => self.enums[e.0 as usize].complete,
            TyKind::Func(_) => false,
            _ => true,
        }
    }

    pub fn is_vla(&self, t: Ty) -> bool {
        match self.kind(t) {
            TyKind::Array(e, len) => *len == ArrayLen::Vla || self.is_vla(*e),
            _ => false,
        }
    }

    // ───────────────────────────── size / alignment ─────────────────────────────

    /// Size in bytes, or `None` if the type has no size (incomplete, function, void, VLA).
    pub fn size_of(&self, t: Ty) -> Option<u64> {
        match self.kind(t) {
            TyKind::Void | TyKind::Func(_) => None,
            TyKind::Bool | TyKind::Char | TyKind::SChar | TyKind::UChar => Some(1),
            TyKind::Short | TyKind::UShort => Some(2),
            TyKind::Int | TyKind::UInt | TyKind::Float => Some(4),
            TyKind::Long | TyKind::ULong | TyKind::LongLong | TyKind::ULongLong | TyKind::Double | TyKind::Ptr(_) => {
                Some(8)
            }
            TyKind::Enum(e) => {
                let d = &self.enums[e.0 as usize];
                if d.complete {
                    self.size_of(d.underlying)
                } else {
                    None
                }
            }
            TyKind::Array(e, ArrayLen::Known(n)) => {
                let es = self.size_of(*e)?;
                es.checked_mul(*n)
            }
            TyKind::Array(_, _) => None,
            TyKind::Record(r) => {
                let d = &self.records[r.0 as usize];
                if d.complete {
                    Some(d.size)
                } else {
                    None
                }
            }
        }
    }

    pub fn align_of(&self, t: Ty) -> u64 {
        match self.kind(t) {
            TyKind::Array(e, _) => self.align_of(*e),
            TyKind::Record(r) => self.records[r.0 as usize].align.max(1),
            TyKind::Enum(e) => self.align_of(self.enums[e.0 as usize].underlying),
            TyKind::Void | TyKind::Func(_) => 1,
            _ => self.size_of(t).unwrap_or(1),
        }
    }

    /// Value range of an integer type (as i128 so `unsigned long` fits).
    pub fn int_range(&self, t: Ty) -> (i128, i128) {
        let r = self.int_repr(t);
        if self.is_bool(r) {
            return (0, 1);
        }
        let bits = self.size_of(r).unwrap_or(4) * 8;
        if self.is_signed(r) {
            (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
        } else {
            (0, (1i128 << bits) - 1)
        }
    }

    // ───────────────────────────── conversions ─────────────────────────────

    /// Array-to-pointer and function-to-pointer decay (C11 6.3.2.1p3-4).
    pub fn decay(&mut self, t: Ty) -> Ty {
        match self.kind(t).clone() {
            TyKind::Array(e, _) => self.ptr(e),
            TyKind::Func(_) => self.ptr(t),
            _ => t,
        }
    }

    /// Integer promotions (C11 6.3.1.1p2). Non-integers pass through.
    pub fn promote(&self, t: Ty) -> Ty {
        if !self.is_integer(t) {
            return t;
        }
        let r = self.int_repr(t);
        match self.kind(r) {
            TyKind::Bool | TyKind::Char | TyKind::SChar | TyKind::UChar | TyKind::Short | TyKind::UShort => self.p.int,
            _ => r,
        }
    }

    fn rank(&self, t: Ty) -> u8 {
        match self.kind(self.int_repr(t)) {
            TyKind::Bool => 0,
            TyKind::Char | TyKind::SChar | TyKind::UChar => 1,
            TyKind::Short | TyKind::UShort => 2,
            TyKind::Int | TyKind::UInt => 3,
            TyKind::Long | TyKind::ULong => 4,
            TyKind::LongLong | TyKind::ULongLong => 5,
            _ => 0,
        }
    }

    fn to_unsigned(&self, t: Ty) -> Ty {
        match self.kind(self.int_repr(t)) {
            TyKind::Int => self.p.uint,
            TyKind::Long => self.p.ulong,
            TyKind::LongLong => self.p.ullong,
            _ => self.int_repr(t),
        }
    }

    /// The usual arithmetic conversions (C11 6.3.1.8) for two arithmetic types.
    pub fn usual_arith(&self, a: Ty, b: Ty) -> Ty {
        if self.kind(a) == &TyKind::Double || self.kind(b) == &TyKind::Double {
            return self.p.double;
        }
        if self.kind(a) == &TyKind::Float || self.kind(b) == &TyKind::Float {
            return self.p.float;
        }
        let a = self.promote(a);
        let b = self.promote(b);
        if a == b {
            return a;
        }
        let (sa, sb) = (self.is_signed(a), self.is_signed(b));
        let (ra, rb) = (self.rank(a), self.rank(b));
        if sa == sb {
            return if ra >= rb { a } else { b };
        }
        let (u, s) = if sa { (b, a) } else { (a, b) };
        let (ru, rs) = (self.rank(u), self.rank(s));
        if ru >= rs {
            return u;
        }
        if self.size_of(s) > self.size_of(u) {
            return s;
        }
        self.to_unsigned(s)
    }

    // ───────────────────────────── compatibility ─────────────────────────────

    /// C11 6.2.7 type compatibility, including qualifiers.
    pub fn compatible(&self, a: Ty, b: Ty) -> bool {
        if a == b {
            return true;
        }
        if self.quals(a) != self.quals(b) {
            return false;
        }
        self.compatible_unqual(a, b)
    }

    /// Compatibility ignoring the top-level qualifiers.
    pub fn compatible_unqual(&self, a: Ty, b: Ty) -> bool {
        let (ka, kb) = (self.kind(a), self.kind(b));
        match (ka, kb) {
            (TyKind::Ptr(x), TyKind::Ptr(y)) => self.compatible(*x, *y),
            (TyKind::Array(x, lx), TyKind::Array(y, ly)) => {
                if !self.compatible(*x, *y) {
                    return false;
                }
                match (lx, ly) {
                    (ArrayLen::Known(p), ArrayLen::Known(q)) => p == q,
                    _ => true,
                }
            }
            (TyKind::Func(fa), TyKind::Func(fb)) => {
                if !self.compatible(fa.ret, fb.ret) {
                    return false;
                }
                if fa.unspecified || fb.unspecified {
                    return !(fa.variadic || fb.variadic);
                }
                fa.variadic == fb.variadic
                    && fa.params.len() == fb.params.len()
                    && fa.params.iter().zip(&fb.params).all(|(x, y)| self.compatible_unqual(*x, *y))
            }
            (TyKind::Record(x), TyKind::Record(y)) => x == y,
            (TyKind::Enum(x), TyKind::Enum(y)) => x == y,
            (TyKind::Enum(e), _) if self.is_integer(b) => self.int_repr_eq(self.enums[e.0 as usize].underlying, b),
            (_, TyKind::Enum(e)) if self.is_integer(a) => self.int_repr_eq(self.enums[e.0 as usize].underlying, a),
            _ => ka == kb,
        }
    }

    fn int_repr_eq(&self, a: Ty, b: Ty) -> bool {
        self.kind(self.int_repr(a)) == self.kind(self.int_repr(b))
    }

    /// The composite of two compatible types (e.g. `int[]` + `int[3]` = `int[3]`).
    pub fn composite(&mut self, a: Ty, b: Ty) -> Ty {
        if a == b {
            return a;
        }
        let q = self.quals(a);
        let (ka, kb) = (self.kind(a).clone(), self.kind(b).clone());
        let r = match (ka, kb) {
            (TyKind::Array(x, lx), TyKind::Array(y, ly)) => {
                let e = self.composite(x, y);
                let len = match (lx, ly) {
                    (ArrayLen::Known(n), _) | (_, ArrayLen::Known(n)) => ArrayLen::Known(n),
                    (ArrayLen::Vla, _) | (_, ArrayLen::Vla) => ArrayLen::Vla,
                    _ => ArrayLen::Incomplete,
                };
                self.array(e, len)
            }
            (TyKind::Ptr(x), TyKind::Ptr(y)) => {
                let p = self.composite(x, y);
                self.ptr(p)
            }
            (TyKind::Func(fa), TyKind::Func(fb)) => {
                let ret = self.composite(fa.ret, fb.ret);
                let (params, unspecified) = if fa.unspecified {
                    (fb.params.clone(), fb.unspecified)
                } else if fb.unspecified {
                    (fa.params.clone(), fa.unspecified)
                } else {
                    let ps = fa.params.iter().zip(&fb.params).map(|(x, y)| self.composite(*x, *y)).collect();
                    (ps, false)
                };
                self.func(FuncSig { ret, params, variadic: fa.variadic && fb.variadic, unspecified })
            }
            (TyKind::Enum(_), _) => self.unqual(b),
            _ => self.unqual(a),
        };
        self.qualified(r, q)
    }

    // ───────────────────────────── printing ─────────────────────────────

    /// Clang-style spelling: `int`, `const char *`, `int (*)(int, char)`, `struct S`.
    pub fn show(&self, t: Ty) -> String {
        self.declarator_string(t, String::new())
    }

    fn quals_prefix(q: TyQuals) -> String {
        let mut s = String::new();
        if q.is_const {
            s.push_str("const ");
        }
        if q.is_volatile {
            s.push_str("volatile ");
        }
        if q.is_restrict {
            s.push_str("restrict ");
        }
        s
    }

    fn base_name(&self, t: Ty) -> String {
        let q = Self::quals_prefix(self.quals(t));
        let n = match self.kind(t) {
            TyKind::Void => "void".to_string(),
            TyKind::Bool => "_Bool".to_string(),
            TyKind::Char => "char".to_string(),
            TyKind::SChar => "signed char".to_string(),
            TyKind::UChar => "unsigned char".to_string(),
            TyKind::Short => "short".to_string(),
            TyKind::UShort => "unsigned short".to_string(),
            TyKind::Int => "int".to_string(),
            TyKind::UInt => "unsigned int".to_string(),
            TyKind::Long => "long".to_string(),
            TyKind::ULong => "unsigned long".to_string(),
            TyKind::LongLong => "long long".to_string(),
            TyKind::ULongLong => "unsigned long long".to_string(),
            TyKind::Float => "float".to_string(),
            TyKind::Double => "double".to_string(),
            TyKind::Record(r) => {
                let d = &self.records[r.0 as usize];
                let kw = if d.is_union { "union" } else { "struct" };
                match d.tag {
                    Some(t) => format!("{} {}", kw, t),
                    None => format!("{} (anonymous)", kw),
                }
            }
            TyKind::Enum(e) => match self.enums[e.0 as usize].tag {
                Some(t) => format!("enum {}", t),
                None => "enum (anonymous)".to_string(),
            },
            TyKind::Ptr(_) | TyKind::Array(..) | TyKind::Func(_) => unreachable!(),
        };
        format!("{}{}", q, n)
    }

    fn declarator_string(&self, t: Ty, inner: String) -> String {
        match self.kind(t) {
            TyKind::Ptr(p) => {
                let q = self.quals(t);
                let mut s = format!("*{}", inner);
                if !q.is_empty() {
                    let qs = Self::quals_prefix(q);
                    s = format!(
                        "*{}{}",
                        qs.trim_end(),
                        if inner.is_empty() { String::new() } else { format!(" {}", inner) }
                    );
                }
                if matches!(self.kind(*p), TyKind::Array(..) | TyKind::Func(_)) {
                    s = format!("({})", s);
                }
                self.declarator_string(*p, s)
            }
            TyKind::Array(e, len) => {
                let l = match len {
                    ArrayLen::Known(n) => n.to_string(),
                    ArrayLen::Incomplete => String::new(),
                    ArrayLen::Vla => "*".to_string(),
                };
                self.declarator_string(*e, format!("{}[{}]", inner, l))
            }
            TyKind::Func(sig) => {
                let mut ps: Vec<String> = sig.params.iter().map(|&p| self.show(p)).collect();
                if sig.variadic {
                    ps.push("...".to_string());
                } else if sig.params.is_empty() && !sig.unspecified {
                    ps.push("void".to_string());
                }
                self.declarator_string(sig.ret, format!("{}({})", inner, ps.join(", ")))
            }
            _ => {
                let base = self.base_name(t);
                if inner.is_empty() {
                    base
                } else {
                    format!("{} {}", base, inner)
                }
            }
        }
    }

    // ───────────────────────────── layout ─────────────────────────────

    /// Compute offsets, size and alignment of a struct/union (SysV x86-64),
    /// including bit-fields, `#pragma pack` / `packed` and member alignment
    /// overrides. Returns diagnostics-worthy problems as messages with spans.
    pub fn layout_record(&mut self, id: RecordId, members: Vec<MemberInput>, pack: Option<u32>) -> Vec<(Span, String)> {
        let mut problems: Vec<(Span, String)> = Vec::new();
        let is_union = self.records[id.0 as usize].is_union;
        let pack = pack.map(|p| p as u64);
        let mut fields: Vec<Field> = Vec::new();
        let mut bit_pos: u64 = 0; // running position in bits (structs)
        let mut max_size_bits: u64 = 0; // unions: largest member
        let mut align: u64 = 1;
        let mut has_flex = false;
        let n = members.len();

        for (i, m) in members.into_iter().enumerate() {
            let last = i + 1 == n;
            let mut mty = m.ty;
            let mut flex = false;
            if let TyKind::Array(_, ArrayLen::Incomplete) = self.kind(mty) {
                if last && !is_union {
                    flex = true;
                    has_flex = true;
                } else {
                    problems.push((m.span, "flexible array member must be the last member of a struct".to_string()));
                    mty = self.p.int;
                }
            } else if !self.is_complete(mty) && !self.is_vla(mty) {
                let what = self.show(mty);
                problems.push((m.span, format!("field has incomplete type '{}'", what)));
                mty = self.p.int;
            }
            let size = if flex { 0 } else { self.size_of(mty).unwrap_or(4) };
            let mut falign = self.align_of(mty);
            if let Some(p) = pack {
                falign = falign.min(p);
            }
            if let Some(a) = m.align {
                falign = falign.max(a);
            }

            if let Some(width) = m.bit_width {
                // ── bit-field ──
                let tsize = self.size_of(mty).unwrap_or(4);
                let tbits = tsize * 8;
                let width = width as u64;
                if width > tbits {
                    problems
                        .push((m.span, format!("width of bit-field exceeds the width of its type ({} bits)", tbits)));
                }
                let width = width.min(tbits);
                let unit_align_bits = falign.max(1) * 8;
                if is_union {
                    fields.push(Field {
                        name: m.name,
                        ty: mty,
                        offset: 0,
                        bit: Some(BitInfo { bit_offset: 0, width: width as u32 }),
                        anonymous: m.anonymous,
                        span: m.span,
                    });
                    max_size_bits = max_size_bits.max(width);
                    if m.name.is_some() {
                        align = align.max(falign);
                    }
                    continue;
                }
                if width == 0 {
                    // zero-width: pad to the next boundary of the declared type
                    bit_pos = bit_pos.div_ceil(tbits.max(8)) * tbits.max(8);
                    continue;
                }
                if pack.is_none() {
                    // A bit-field may not straddle the storage unit of its declared type.
                    let unit = tbits;
                    if bit_pos % unit + width > unit {
                        bit_pos = bit_pos.div_ceil(unit) * unit;
                    }
                }
                let off = bit_pos;
                fields.push(Field {
                    name: m.name,
                    ty: mty,
                    offset: off / 8,
                    bit: Some(BitInfo { bit_offset: off, width: width as u32 }),
                    anonymous: m.anonymous,
                    span: m.span,
                });
                bit_pos += width;
                if m.name.is_some() {
                    align = align.max(if pack.is_some() { falign } else { unit_align_bits / 8 });
                }
                continue;
            }

            // ── ordinary member ──
            if is_union {
                fields.push(Field {
                    name: m.name,
                    ty: mty,
                    offset: 0,
                    bit: None,
                    anonymous: m.anonymous,
                    span: m.span,
                });
                max_size_bits = max_size_bits.max(size * 8);
            } else {
                let aligned = bit_pos.div_ceil(falign * 8) * falign * 8;
                let off = aligned / 8;
                fields.push(Field {
                    name: m.name,
                    ty: mty,
                    offset: off,
                    bit: None,
                    anonymous: m.anonymous,
                    span: m.span,
                });
                bit_pos = aligned + size * 8;
            }
            align = align.max(falign);
        }

        let raw_bits = if is_union { max_size_bits } else { bit_pos };
        let raw = raw_bits.div_ceil(8);
        let size = raw.div_ceil(align) * align;
        let d = &mut self.records[id.0 as usize];
        d.fields = fields;
        d.size = size;
        d.align = align;
        d.complete = true;
        d.has_flexible_array = has_flex;
        problems
    }

    /// Find a field by name, searching anonymous struct/union members.
    /// Returns the field and the total byte offset from the start of `id`.
    pub fn find_field(&self, id: RecordId, name: Symbol) -> Option<(Field, u64)> {
        for f in &self.records[id.0 as usize].fields {
            if f.name == Some(name) {
                return Some((f.clone(), f.offset));
            }
            if f.anonymous {
                if let TyKind::Record(inner) = self.kind(f.ty) {
                    if let Some((found, off)) = self.find_field(*inner, name) {
                        return Some((found, off + f.offset));
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, ty: Ty) -> MemberInput {
        MemberInput {
            name: Some(Symbol::new(name)),
            ty,
            bit_width: None,
            align: None,
            anonymous: false,
            span: Span::DUMMY,
        }
    }

    fn bits(name: &str, ty: Ty, w: u32) -> MemberInput {
        MemberInput {
            name: Some(Symbol::new(name)),
            ty,
            bit_width: Some(w),
            align: None,
            anonymous: false,
            span: Span::DUMMY,
        }
    }

    fn record(t: &mut TypeTable, is_union: bool, ms: Vec<MemberInput>, pack: Option<u32>) -> Ty {
        let id = t.new_record(None, is_union, Span::DUMMY);
        let p = t.layout_record(id, ms, pack);
        assert!(p.is_empty(), "{:?}", p);
        t.record_type(id)
    }

    #[test]
    fn primitive_sizes() {
        let t = TypeTable::new();
        let p = t.p;
        let sz = |ty| t.size_of(ty).unwrap();
        assert_eq!(
            [sz(p.bool_), sz(p.char_), sz(p.short), sz(p.int), sz(p.long), sz(p.llong), sz(p.float), sz(p.double)],
            [1, 1, 2, 4, 8, 8, 4, 8]
        );
        assert_eq!(t.size_of(p.void), None);
    }

    #[test]
    fn interning_and_qualifiers() {
        let mut t = TypeTable::new();
        let i = t.p.int;
        let p1 = t.ptr(i);
        let p2 = t.ptr(i);
        assert_eq!(p1, p2);
        let ci = t.qualified(i, TyQuals { is_const: true, ..Default::default() });
        assert_ne!(ci, i);
        assert_eq!(t.unqual(ci), i);
        assert!(t.compatible_unqual(ci, i));
        assert!(!t.compatible(ci, i));
        // qualifiers on arrays go to the element
        let arr = t.array(i, ArrayLen::Known(3));
        let carr = t.qualified(arr, TyQuals { is_const: true, ..Default::default() });
        assert_eq!(t.array_elem(carr), Some(ci));
    }

    #[test]
    fn struct_layout_matches_sysv() {
        let mut t = TypeTable::new();
        let p = t.p;
        // struct { char c; int i; char d; }  -> 12 bytes, align 4
        let s = record(&mut t, false, vec![member("c", p.char_), member("i", p.int), member("d", p.char_)], None);
        assert_eq!((t.size_of(s), t.align_of(s)), (Some(12), 4));
        let r = t.record(t.record_id(s).unwrap());
        assert_eq!(r.fields.iter().map(|f| f.offset).collect::<Vec<_>>(), [0, 4, 8]);
        // struct { char c; double d; } -> 16, align 8
        let s = record(&mut t, false, vec![member("c", p.char_), member("d", p.double)], None);
        assert_eq!((t.size_of(s), t.align_of(s)), (Some(16), 8));
        // struct { double d; char c; } -> 16 (tail padding)
        let s = record(&mut t, false, vec![member("d", p.double), member("c", p.char_)], None);
        assert_eq!(t.size_of(s), Some(16));
        // empty struct (GNU) is 0 bytes
        let s = record(&mut t, false, vec![], None);
        assert_eq!(t.size_of(s), Some(0));
    }

    #[test]
    fn union_and_nested_layout() {
        let mut t = TypeTable::new();
        let p = t.p;
        let arr = t.array(p.char_, ArrayLen::Known(5));
        let u = record(&mut t, true, vec![member("i", p.int), member("a", arr), member("d", p.double)], None);
        assert_eq!((t.size_of(u), t.align_of(u)), (Some(8), 8));
        let inner = record(&mut t, false, vec![member("x", p.short), member("y", p.char_)], None);
        assert_eq!((t.size_of(inner), t.align_of(inner)), (Some(4), 2));
        let outer = record(&mut t, false, vec![member("c", p.char_), member("s", inner), member("l", p.long)], None);
        let r = t.record(t.record_id(outer).unwrap());
        assert_eq!(r.fields.iter().map(|f| f.offset).collect::<Vec<_>>(), [0, 2, 8]);
        assert_eq!(r.size, 16);
    }

    #[test]
    fn packed_structs() {
        let mut t = TypeTable::new();
        let p = t.p;
        let s = record(&mut t, false, vec![member("c", p.char_), member("i", p.int), member("l", p.long)], Some(1));
        assert_eq!((t.size_of(s), t.align_of(s)), (Some(13), 1));
        let s = record(&mut t, false, vec![member("c", p.char_), member("i", p.int), member("l", p.long)], Some(2));
        assert_eq!((t.size_of(s), t.align_of(s)), (Some(14), 2));
    }

    #[test]
    fn member_alignment_override() {
        let mut t = TypeTable::new();
        let p = t.p;
        let mut m = member("c", p.char_);
        m.align = Some(16);
        let s = record(&mut t, false, vec![m, member("x", p.char_)], None);
        assert_eq!((t.size_of(s), t.align_of(s)), (Some(16), 16));
    }

    #[test]
    fn bitfield_layout() {
        let mut t = TypeTable::new();
        let p = t.p;
        // struct { unsigned a:3, b:5; unsigned c:30; }
        // a: bits 0-2, b: bits 3-7, c wouldn't fit in the remaining 24 bits of the first unit -> bit 32
        let s = record(&mut t, false, vec![bits("a", p.uint, 3), bits("b", p.uint, 5), bits("c", p.uint, 30)], None);
        let r = t.record(t.record_id(s).unwrap());
        let off: Vec<u64> = r.fields.iter().map(|f| f.bit.unwrap().bit_offset).collect();
        assert_eq!(off, [0, 3, 32]);
        assert_eq!(r.size, 8);
        // mixed: char c; int x:4; int y:4;  -> c at 0, x at bit 8, y at bit 12, size 4
        let s = record(&mut t, false, vec![member("c", p.char_), bits("x", p.int, 4), bits("y", p.int, 4)], None);
        let r = t.record(t.record_id(s).unwrap());
        assert_eq!(r.fields[1].bit.unwrap().bit_offset, 8);
        assert_eq!(r.fields[2].bit.unwrap().bit_offset, 12);
        assert_eq!(r.size, 4);
        // zero-width pads to the next unit
        let s = record(&mut t, false, vec![bits("a", p.int, 3), bits("z", p.int, 0), bits("b", p.int, 3)], None);
        let r = t.record(t.record_id(s).unwrap());
        assert_eq!(r.fields[1].bit.unwrap().bit_offset, 32);
        assert_eq!(r.size, 8);
        // exactly full unit
        let s = record(&mut t, false, vec![bits("a", p.uchar, 8), bits("b", p.uchar, 8)], None);
        assert_eq!(t.size_of(s), Some(2));
    }

    #[test]
    fn flexible_array_member() {
        let mut t = TypeTable::new();
        let p = t.p;
        let flex = t.array(p.int, ArrayLen::Incomplete);
        let s = record(&mut t, false, vec![member("n", p.int), member("data", flex)], None);
        assert_eq!(t.size_of(s), Some(4));
        assert!(t.record(t.record_id(s).unwrap()).has_flexible_array);
        // not last: an error
        let id = t.new_record(None, false, Span::DUMMY);
        let probs = t.layout_record(id, vec![member("data", flex), member("n", p.int)], None);
        assert_eq!(probs.len(), 1);
    }

    #[test]
    fn incomplete_field_is_reported() {
        let mut t = TypeTable::new();
        let inc = t.new_record(None, false, Span::DUMMY);
        let inc_ty = t.record_type(inc);
        let id = t.new_record(None, false, Span::DUMMY);
        let m = member("x", inc_ty);
        let probs = t.layout_record(id, vec![m], None);
        assert!(probs[0].1.contains("incomplete type"));
    }

    #[test]
    fn va_list_is_24_bytes() {
        let t = TypeTable::new();
        let va = t.p.va_list;
        assert_eq!(t.size_of(va), Some(24));
        assert_eq!(t.align_of(va), 8);
        assert_eq!(t.show(va), "struct __va_list_tag [1]");
    }

    #[test]
    fn integer_promotions() {
        let t = TypeTable::new();
        let p = t.p;
        for small in [p.bool_, p.char_, p.schar, p.uchar, p.short, p.ushort] {
            assert_eq!(t.promote(small), p.int);
        }
        assert_eq!(t.promote(p.uint), p.uint);
        assert_eq!(t.promote(p.long), p.long);
        assert_eq!(t.promote(p.double), p.double);
    }

    #[test]
    fn usual_arithmetic_conversions() {
        let t = TypeTable::new();
        let p = t.p;
        assert_eq!(t.usual_arith(p.int, p.int), p.int);
        assert_eq!(t.usual_arith(p.char_, p.short), p.int);
        assert_eq!(t.usual_arith(p.int, p.uint), p.uint);
        assert_eq!(t.usual_arith(p.int, p.long), p.long);
        assert_eq!(t.usual_arith(p.uint, p.long), p.long); // long can represent all of uint
        assert_eq!(t.usual_arith(p.ulong, p.long), p.ulong);
        assert_eq!(t.usual_arith(p.long, p.llong), p.llong);
        assert_eq!(t.usual_arith(p.int, p.float), p.float);
        assert_eq!(t.usual_arith(p.float, p.double), p.double);
        assert_eq!(t.usual_arith(p.long, p.double), p.double);
        assert_eq!(t.usual_arith(p.uint, p.uchar), p.uint);
        assert_eq!(t.usual_arith(p.ulong, p.int), p.ulong);
        // same size, different signedness, and the signed type has the higher
        // rank: the unsigned version of the signed type
        assert_eq!(t.usual_arith(p.llong, p.ulong), p.ullong);
        assert_eq!(t.usual_arith(p.long, p.ullong), p.ullong);
    }

    #[test]
    fn compatibility() {
        let mut t = TypeTable::new();
        let p = t.p;
        let pi = t.ptr(p.int);
        let pi2 = t.ptr(p.int);
        let pc = t.ptr(p.char_);
        assert!(t.compatible(pi, pi2));
        assert!(!t.compatible(pi, pc));
        assert!(!t.compatible(p.long, p.llong));
        assert!(!t.compatible(p.char_, p.schar));
        let a3 = t.array(p.int, ArrayLen::Known(3));
        let a4 = t.array(p.int, ArrayLen::Known(4));
        let a_ = t.array(p.int, ArrayLen::Incomplete);
        assert!(t.compatible(a3, a_));
        assert!(!t.compatible(a3, a4));
        let comp = t.composite(a_, a3);
        assert_eq!(comp, a3);
        let f1 = t.func(FuncSig { ret: p.int, params: vec![p.int], variadic: false, unspecified: false });
        let f2 = t.func(FuncSig { ret: p.int, params: vec![p.int], variadic: false, unspecified: false });
        let f3 = t.func(FuncSig { ret: p.int, params: vec![p.long], variadic: false, unspecified: false });
        let fu = t.func(FuncSig { ret: p.int, params: vec![], variadic: false, unspecified: true });
        assert!(t.compatible(f1, f2));
        assert!(!t.compatible(f1, f3));
        assert!(t.compatible(f1, fu));
    }

    #[test]
    fn enum_compatibility_and_size() {
        let mut t = TypeTable::new();
        let id = t.new_enum(Some(Symbol::new("E")));
        t.enums[id.0 as usize].complete = true;
        let e = t.enum_type(id);
        assert_eq!(t.size_of(e), Some(4));
        assert!(t.is_integer(e));
        assert!(t.compatible(e, t.p.int));
        assert!(!t.compatible(e, t.p.long));
        assert_eq!(t.promote(e), t.p.int);
    }

    #[test]
    fn decay() {
        let mut t = TypeTable::new();
        let p = t.p;
        let arr = t.array(p.int, ArrayLen::Known(3));
        let pi = t.ptr(p.int);
        assert_eq!(t.decay(arr), pi);
        let f = t.func(FuncSig { ret: p.int, params: vec![], variadic: false, unspecified: false });
        let pf = t.ptr(f);
        assert_eq!(t.decay(f), pf);
        assert_eq!(t.decay(p.int), p.int);
    }

    #[test]
    fn type_printing() {
        let mut t = TypeTable::new();
        let p = t.p;
        let cc = t.qualified(p.char_, TyQuals { is_const: true, ..Default::default() });
        let pcc = t.ptr(cc);
        assert_eq!(t.show(pcc), "const char *");
        let pc = t.ptr(p.char_);
        let cpc = t.qualified(pc, TyQuals { is_const: true, ..Default::default() });
        assert_eq!(t.show(cpc), "char *const");
        let a3 = t.array(p.int, ArrayLen::Known(3));
        assert_eq!(t.show(a3), "int [3]");
        let pa = t.ptr(a3);
        assert_eq!(t.show(pa), "int (*)[3]");
        let pi = t.ptr(p.int);
        let ap = t.array(pi, ArrayLen::Known(3));
        assert_eq!(t.show(ap), "int *[3]");
        let f = t.func(FuncSig { ret: p.int, params: vec![p.int, pc], variadic: false, unspecified: false });
        assert_eq!(t.show(f), "int (int, char *)");
        let pf = t.ptr(f);
        assert_eq!(t.show(pf), "int (*)(int, char *)");
        let fv = t.func(FuncSig { ret: p.void, params: vec![], variadic: false, unspecified: false });
        assert_eq!(t.show(fv), "void (void)");
        let fe = t.func(FuncSig { ret: p.void, params: vec![], variadic: false, unspecified: true });
        assert_eq!(t.show(fe), "void ()");
        let fvar = t.func(FuncSig { ret: p.int, params: vec![pcc], variadic: true, unspecified: false });
        assert_eq!(t.show(fvar), "int (const char *, ...)");
        let pp = t.ptr(pf);
        assert_eq!(t.show(pp), "int (**)(int, char *)");
        assert_eq!(t.show(p.ulong), "unsigned long");
        let id = t.new_record(Some(Symbol::new("S")), false, Span::DUMMY);
        let s = t.record_type(id);
        assert_eq!(t.show(s), "struct S");
        let ps = t.ptr(s);
        assert_eq!(t.show(ps), "struct S *");
    }

    #[test]
    fn int_ranges() {
        let t = TypeTable::new();
        let p = t.p;
        assert_eq!(t.int_range(p.char_), (-128, 127));
        assert_eq!(t.int_range(p.uchar), (0, 255));
        assert_eq!(t.int_range(p.int), (i32::MIN as i128, i32::MAX as i128));
        assert_eq!(t.int_range(p.ulong), (0, u64::MAX as i128));
        assert_eq!(t.int_range(p.bool_), (0, 1));
    }

    #[test]
    fn anonymous_member_lookup() {
        let mut t = TypeTable::new();
        let p = t.p;
        let inner = record(&mut t, false, vec![member("x", p.int), member("y", p.int)], None);
        let mut anon = member("", inner);
        anon.name = None;
        anon.anonymous = true;
        let outer = record(&mut t, false, vec![member("a", p.int), anon], None);
        let rid = t.record_id(outer).unwrap();
        let (f, off) = t.find_field(rid, Symbol::new("y")).unwrap();
        assert_eq!(off, 8);
        assert_eq!(f.ty, p.int);
        assert!(t.find_field(rid, Symbol::new("nope")).is_none());
    }
}
