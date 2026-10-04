//! Initializers: flatten `{ ... }` lists (with brace elision and designators,
//! C11 6.7.9) into byte-offset entries, size incomplete arrays, and for
//! static storage require every element to be a compile-time constant.

use super::expr::ConvCtx;
use super::*;
use crate::literal::StrKind;

struct Cursor<'b> {
    items: &'b [InitItem],
    pos: usize,
    /// An item whose expression was already type-checked (to decide on brace elision).
    checked: Option<(usize, HExpr)>,
}

impl<'b> Cursor<'b> {
    fn new(items: &'b [InitItem]) -> Cursor<'b> {
        Cursor { items, pos: 0, checked: None }
    }

    fn done(&self) -> bool {
        self.pos >= self.items.len()
    }

    fn peek(&self) -> &'b InitItem {
        &self.items[self.pos]
    }

    fn advance(&mut self) {
        self.pos += 1;
    }
}

/// Strip parentheses.
fn unparen(e: &Expr) -> &Expr {
    match &e.kind {
        ExprKind::Paren(i) => unparen(i),
        _ => e,
    }
}

impl<'a> Sema<'a> {
    /// Check an initializer for an object of type `ty`. Returns the (possibly
    /// completed) type and the flattened plan.
    pub(crate) fn initialize(
        &mut self,
        ty: Ty,
        init: &Initializer,
        is_static: bool,
        name_span: Span,
    ) -> (Ty, Option<InitPlan>) {
        let _ = name_span;
        let mut entries: Vec<InitEntry> = Vec::new();
        let final_ty = match init {
            Initializer::List(l) => {
                if !self.types.is_aggregate(ty) && !self.types.is_scalar(ty) {
                    let t = self.show(ty);
                    self.error(
                        l.span,
                        format!("cannot initialize a variable of type '{}' with an initializer list", t),
                    );
                    return (ty, None);
                }
                self.init_braced(ty, 0, &l.items, &mut entries)
            }
            Initializer::Expr(e) => {
                let items = [InitItem { designators: Vec::new(), init: Initializer::Expr(e.clone()) }];
                let mut cur = Cursor::new(&items);
                if self.types.is_array(ty) && !is_string_literal(e) {
                    self.error(e.span, "array initializer must be an initializer list or string literal");
                    return (ty, None);
                }
                if self.types.is_record(ty) {
                    // Without braces, a struct/union is initialised from an expression of its own type.
                    let h = self.checked_expr(&mut cur, e);
                    if self.types.record_id(h.ty) != self.types.record_id(ty) {
                        let (a, b) = (self.show(ty), self.show(h.ty));
                        if !matches!(h.kind, HExprKind::Error) {
                            self.error(
                                e.span,
                                format!("initializing '{}' with an expression of incompatible type '{}'", a, b),
                            );
                        }
                        return (ty, None);
                    }
                    let unq = self.types.unqual(ty);
                    self.push_entry(
                        &mut entries,
                        InitEntry { offset: 0, ty: unq, bit: None, value: InitValue::Expr(h) },
                    );
                    ty
                } else {
                    self.init_object(ty, 0, &mut cur, false, &mut entries)
                }
            }
        };
        if !self.types.is_complete(final_ty) && !self.types.is_vla(final_ty) {
            let t = self.show(final_ty);
            self.error(init.span(), format!("variable has incomplete type '{}'", t));
            return (final_ty, None);
        }
        let size = self.types.size_of(final_ty).unwrap_or(0);
        let mut plan = InitPlan { entries, needs_zero: false, size };
        self.finalize_plan(&mut plan);
        if is_static {
            self.make_constant(&mut plan);
        }
        (final_ty, Some(plan))
    }

    fn finalize_plan(&mut self, plan: &mut InitPlan) {
        plan.entries.sort_by_key(|e| e.offset);
        let mut expected = 0u64;
        let mut needs_zero = false;
        for e in &plan.entries {
            if e.bit.is_some() {
                needs_zero = true;
                continue;
            }
            if e.offset != expected {
                needs_zero = true;
            }
            let len = match &e.value {
                InitValue::Bytes(b) => b.len() as u64,
                _ => self.types.size_of(e.ty).unwrap_or(0),
            };
            expected = e.offset + len;
        }
        if expected != plan.size {
            needs_zero = true;
        }
        plan.needs_zero = needs_zero;
    }

    /// Static storage: every element must be a constant expression.
    fn make_constant(&mut self, plan: &mut InitPlan) {
        for e in &mut plan.entries {
            if let InitValue::Expr(h) = &e.value {
                if matches!(h.kind, HExprKind::Error) {
                    continue;
                }
                match consteval::eval(&self.types, h) {
                    Some(c) => e.value = InitValue::Const(c),
                    None => {
                        let span = h.span;
                        self.sess
                            .diags
                            .emit(Diagnostic::error(span, "initializer element is not a compile-time constant"));
                    }
                }
            }
        }
    }

    // ───────────────────────────── core recursion ─────────────────────────────

    /// Initialise the object `ty` at `off` from a complete brace pair's items.
    fn init_braced(&mut self, ty: Ty, off: u64, items: &[InitItem], out: &mut Vec<InitEntry>) -> Ty {
        let mut cur = Cursor::new(items);
        let t = self.init_object(ty, off, &mut cur, true, out);
        if !cur.done() {
            let it = cur.peek();
            let sp = it.init.span();
            let what = match self.types.kind(ty) {
                TyKind::Array(..) => "array",
                TyKind::Record(r) if self.types.record(*r).is_union => "union",
                TyKind::Record(_) => "struct",
                _ => "scalar",
            };
            self.warn(Warn::ExcessInitializers, sp, format!("excess elements in {} initializer", what));
        }
        t
    }

    fn init_object(&mut self, ty: Ty, off: u64, cur: &mut Cursor, braced: bool, out: &mut Vec<InitEntry>) -> Ty {
        match self.types.kind(ty).clone() {
            TyKind::Array(elem, len) => self.init_array(ty, elem, len, off, cur, braced, out),
            TyKind::Record(rid) => {
                self.init_record(ty, rid, off, cur, braced, out);
                ty
            }
            _ => {
                self.init_scalar(ty, off, None, cur, out);
                ty
            }
        }
    }

    /// Check (once) and return the expression of the current item.
    fn checked_expr(&mut self, cur: &mut Cursor, e: &Expr) -> HExpr {
        if let Some((i, h)) = &cur.checked {
            if *i == cur.pos {
                return h.clone();
            }
        }
        let h = self.expr(e);
        self.rvalue(h)
    }

    fn init_scalar(&mut self, ty: Ty, off: u64, bit: Option<BitInfo>, cur: &mut Cursor, out: &mut Vec<InitEntry>) {
        if cur.done() {
            return;
        }
        let item = cur.peek();
        match &item.init {
            Initializer::List(l) => {
                cur.advance();
                // `{ expr }` around a scalar
                let mut inner = Vec::new();
                self.init_braced(ty, off, &l.items, &mut inner);
                for mut e in inner {
                    if bit.is_some() {
                        e.bit = bit;
                    }
                    self.push_entry(out, e);
                }
            }
            Initializer::Expr(e) => {
                let h = self.checked_expr(cur, e);
                cur.advance();
                cur.checked = None;
                let v = self.convert(h, ty, ConvCtx::Init, e.span);
                self.push_entry(out, InitEntry { offset: off, ty, bit, value: InitValue::Expr(v) });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn init_array(
        &mut self,
        ty: Ty,
        elem: Ty,
        len: ArrayLen,
        off: u64,
        cur: &mut Cursor,
        braced: bool,
        out: &mut Vec<InitEntry>,
    ) -> Ty {
        let esize = self.types.size_of(elem).unwrap_or(0);
        // A string literal initialises a character array.
        if !cur.done() && cur.peek().designators.is_empty() {
            if let Initializer::Expr(e) = &cur.peek().init {
                if is_string_literal(e) && self.is_char_like(elem) {
                    cur.advance();
                    return self.init_string(ty, elem, len, off, e, out);
                }
            }
        }
        let limit = match len {
            ArrayLen::Known(n) => Some(n),
            _ => None,
        };
        let mut idx: u64 = 0;
        let mut max_idx: u64 = 0;
        while !cur.done() {
            let item = cur.peek();
            if !item.designators.is_empty() {
                if !braced {
                    break;
                }
                match &item.designators[0] {
                    Designator::Index(ie) => {
                        let h = self.expr(ie);
                        let h = self.rvalue(h);
                        match consteval::eval_int(&self.types, &h) {
                            Some(v) if (v as i64) >= 0 && limit.is_none_or(|n| v < n) => idx = v,
                            Some(_) => {
                                self.error(ie.span, "array designator value is out of bounds");
                                cur.advance();
                                continue;
                            }
                            None => {
                                self.error(ie.span, "designator is not an integer constant expression");
                                cur.advance();
                                continue;
                            }
                        }
                    }
                    Designator::Field(f) => {
                        self.error(f.span, "field designator cannot initialize an array");
                        cur.advance();
                        continue;
                    }
                }
                cur.advance();
                self.init_designated(elem, off + idx * esize, &item.designators[1..], &item.init, out);
                idx += 1;
                max_idx = max_idx.max(idx);
                continue;
            }
            if limit.is_some_and(|n| idx >= n) {
                break;
            }
            self.init_element(elem, off + idx * esize, cur, out);
            idx += 1;
            max_idx = max_idx.max(idx);
        }
        match len {
            ArrayLen::Incomplete => self.types.array(elem, ArrayLen::Known(max_idx)),
            _ => ty,
        }
    }

    fn is_char_like(&self, t: Ty) -> bool {
        let r = self.types.int_repr(t);
        self.types.is_integer(r) && !self.types.is_bool(r) && matches!(self.types.size_of(r), Some(1 | 2 | 4))
    }

    fn init_string(&mut self, ty: Ty, elem: Ty, len: ArrayLen, off: u64, e: &Expr, out: &mut Vec<InitEntry>) -> Ty {
        let ExprKind::StrLit { kind, units } = &unparen(e).kind else { unreachable!() };
        let esize = self.types.size_of(elem).unwrap_or(1);
        let compatible = match kind {
            StrKind::Plain | StrKind::Utf8 => esize == 1,
            StrKind::Wide | StrKind::Utf32 => esize == 4,
            StrKind::Utf16 => esize == 2,
        };
        if !compatible {
            let t = self.show(ty);
            self.error(
                e.span,
                format!("initializing wide char array with non-wide string literal or vice versa ('{}')", t),
            );
            return ty;
        }
        let n_units = units.len() as u64;
        let total = match len {
            ArrayLen::Known(n) => n,
            _ => n_units + 1,
        };
        if n_units > total {
            self.warn(Warn::ExcessInitializers, e.span, "initializer-string for char array is too long");
        }
        let take = n_units.min(total) as usize;
        if esize == 1 {
            let mut bytes: Vec<u8> = units[..take].iter().map(|&u| u as u8).collect();
            if (take as u64) < total {
                bytes.push(0); // terminating NUL fits
            }
            let arr_ty = self.types.array(elem, ArrayLen::Known(bytes.len() as u64));
            self.push_entry(out, InitEntry { offset: off, ty: arr_ty, bit: None, value: InitValue::Bytes(bytes) });
        } else {
            let mut vals: Vec<u32> = units[..take].to_vec();
            if (take as u64) < total {
                vals.push(0);
            }
            let ety = self.types.unqual(elem);
            for (i, u) in vals.iter().enumerate() {
                let h = HExpr::new(HExprKind::Int(*u as u64), ety, e.span);
                self.push_entry(
                    out,
                    InitEntry { offset: off + i as u64 * esize, ty: ety, bit: None, value: InitValue::Expr(h) },
                );
            }
        }
        match len {
            ArrayLen::Incomplete => self.types.array(elem, ArrayLen::Known(total)),
            _ => ty,
        }
    }

    fn init_record(
        &mut self,
        ty: Ty,
        rid: RecordId,
        off: u64,
        cur: &mut Cursor,
        braced: bool,
        out: &mut Vec<InitEntry>,
    ) {
        let _ = ty;
        if !self.types.record(rid).complete {
            let t = self.show(ty);
            let sp = if cur.done() { Span::DUMMY } else { cur.peek().init.span() };
            self.error(sp, format!("variable has incomplete type '{}'", t));
            cur.pos = cur.items.len();
            return;
        }
        let is_union = self.types.record(rid).is_union;
        let fields = self.types.record(rid).fields.clone();
        let mut fi = 0usize;
        let mut first_done = false;
        while !cur.done() {
            let item = cur.peek();
            if !item.designators.is_empty() {
                if !braced {
                    break;
                }
                match &item.designators[0] {
                    Designator::Field(f) => match self.types.find_field(rid, f.name) {
                        Some((field, total_off)) => {
                            cur.advance();
                            // Position the positional cursor after this field (top-level fields only).
                            if let Some(pos) = fields.iter().position(|x| {
                                x.name == Some(f.name) || (x.anonymous && self.contains_field(x.ty, f.name))
                            }) {
                                fi = pos + 1;
                            }
                            first_done = true;
                            self.init_designated_field(
                                &field,
                                off + total_off,
                                &item.designators[1..],
                                &item.init,
                                out,
                            );
                            continue;
                        }
                        None => {
                            let t = self.show(ty);
                            self.error(
                                f.span,
                                format!("field designator '{}' does not refer to any field in type '{}'", f.name, t),
                            );
                            cur.advance();
                            continue;
                        }
                    },
                    Designator::Index(ie) => {
                        self.error(ie.span, "array designator cannot initialize a non-array type");
                        cur.advance();
                        continue;
                    }
                }
            }
            // Skip unnamed bit-fields (they cannot be initialised).
            while fi < fields.len() && fields[fi].name.is_none() && fields[fi].bit.is_some() {
                fi += 1;
            }
            if fi >= fields.len() || (is_union && first_done) {
                break;
            }
            let f = fields[fi].clone();
            if matches!(self.types.kind(f.ty), TyKind::Array(_, ArrayLen::Incomplete)) {
                let sp = item.init.span();
                self.error(sp, "initialization of flexible array member is not allowed");
                cur.advance();
                fi += 1;
                continue;
            }
            if f.bit.is_some() {
                self.init_scalar(f.ty, off + f.offset, f.bit, cur, out);
            } else {
                self.init_element(f.ty, off + f.offset, cur, out);
            }
            first_done = true;
            fi += 1;
        }
    }

    fn contains_field(&self, ty: Ty, name: Symbol) -> bool {
        match self.types.record_id(ty) {
            Some(r) => self.types.find_field(r, name).is_some(),
            None => false,
        }
    }

    fn init_designated_field(
        &mut self,
        field: &Field,
        off: u64,
        rest: &[Designator],
        init: &Initializer,
        out: &mut Vec<InitEntry>,
    ) {
        if field.bit.is_some() {
            let items = [InitItem { designators: Vec::new(), init: init.clone() }];
            let mut cur = Cursor::new(&items);
            self.init_scalar(field.ty, off, field.bit, &mut cur, out);
            return;
        }
        self.init_designated(field.ty, off, rest, init, out);
    }

    /// Apply the remaining designators of `.a.b[2]` style chains to `ty` at `off`.
    fn init_designated(&mut self, ty: Ty, off: u64, rest: &[Designator], init: &Initializer, out: &mut Vec<InitEntry>) {
        let Some(first) = rest.first() else {
            match init {
                Initializer::List(l) => {
                    self.init_braced(ty, off, &l.items, out);
                }
                Initializer::Expr(_) => {
                    let items = [InitItem { designators: Vec::new(), init: init.clone() }];
                    let mut cur = Cursor::new(&items);
                    self.init_object(ty, off, &mut cur, false, out);
                }
            }
            return;
        };
        match (first, self.types.kind(ty).clone()) {
            (Designator::Field(f), TyKind::Record(rid)) => match self.types.find_field(rid, f.name) {
                Some((field, total_off)) => self.init_designated_field(&field, off + total_off, &rest[1..], init, out),
                None => {
                    let t = self.show(ty);
                    self.error(
                        f.span,
                        format!("field designator '{}' does not refer to any field in type '{}'", f.name, t),
                    );
                }
            },
            (Designator::Index(ie), TyKind::Array(elem, len)) => {
                let h = self.expr(ie);
                let h = self.rvalue(h);
                match consteval::eval_int(&self.types, &h) {
                    Some(v)
                        if (v as i64) >= 0 && matches!(len, ArrayLen::Known(n) if v < n)
                            || matches!(len, ArrayLen::Incomplete) =>
                    {
                        let es = self.types.size_of(elem).unwrap_or(0);
                        self.init_designated(elem, off + v * es, &rest[1..], init, out);
                    }
                    _ => self.error(ie.span, "array designator value is out of bounds"),
                }
            }
            (Designator::Field(f), _) => {
                self.error(f.span, "field designator cannot initialize a non-struct, non-union type")
            }
            (Designator::Index(ie), _) => self.error(ie.span, "array designator cannot initialize a non-array type"),
        }
    }

    /// One positional element: a braced sub-object, a whole-object expression,
    /// or (brace elision) the start of a sub-aggregate's flat initializer run.
    fn init_element(&mut self, ty: Ty, off: u64, cur: &mut Cursor, out: &mut Vec<InitEntry>) {
        let item = cur.peek();
        match &item.init {
            Initializer::List(l) => {
                cur.advance();
                self.init_braced(ty, off, &l.items, out);
            }
            Initializer::Expr(e) => {
                if self.types.is_record(ty) {
                    // `struct S s = other;` or `{ other }` initialises the whole sub-object.
                    let h = self.checked_expr(cur, e);
                    if let TyKind::Record(r2) = self.types.kind(h.ty) {
                        if Some(*r2) == self.types.record_id(ty) {
                            cur.advance();
                            cur.checked = None;
                            let unq = self.types.unqual(ty);
                            self.push_entry(
                                out,
                                InitEntry { offset: off, ty: unq, bit: None, value: InitValue::Expr(h) },
                            );
                            return;
                        }
                    }
                    cur.checked = Some((cur.pos, h));
                }
                self.init_object(ty, off, cur, false, out);
            }
        }
    }
}

fn is_string_literal(e: &Expr) -> bool {
    matches!(unparen(e).kind, ExprKind::StrLit { .. })
}

impl<'a> Sema<'a> {
    /// Byte range covered by an entry.
    fn entry_range(&self, e: &InitEntry) -> (u64, u64) {
        let len = match &e.value {
            InitValue::Bytes(b) => b.len() as u64,
            _ => self.types.size_of(e.ty).unwrap_or(1),
        };
        (e.offset, e.offset + len.max(1))
    }

    /// Add an entry, replacing any earlier entries it overlaps (designated
    /// initializers: the last one wins). Bit-fields that merely share a
    /// storage unit are kept.
    fn push_entry(&self, out: &mut Vec<InitEntry>, e: InitEntry) {
        let (s, t) = self.entry_range(&e);
        // Fast path: entries arrive in increasing offset order.
        if out.last().is_none_or(|l| self.entry_range(l).1 <= s) {
            out.push(e);
            return;
        }
        out.retain(|x| {
            let (xs, xe) = self.entry_range(x);
            if xe <= s || xs >= t {
                return true;
            }
            match (x.bit, e.bit) {
                (Some(a), Some(b)) => a.bit_offset != b.bit_offset,
                _ => false,
            }
        });
        out.push(e);
    }
}
