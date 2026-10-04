//! SysV classification of aggregate (struct/union) parameters and results.

use crate::ir::Type;
use crate::types::{ArrayLen, Ty, TyKind, TypeTable};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Integer,
    Sse,
}

/// One eightbyte of an aggregate passed or returned in a register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub ty: Type,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AggAbi {
    /// Zero-sized: nothing is passed.
    Empty,
    /// Passed in registers as these pieces (one or two eightbytes).
    Regs(Vec<Piece>),
    /// Larger than 16 bytes, or not register-classifiable: passed in memory.
    Memory,
}

/// Visit the scalar leaves of `ty`, calling `f(offset, size, is_float)`.
/// Returns false if the layout forces MEMORY class (unaligned fields).
fn leaves(t: &TypeTable, ty: Ty, base: u64, f: &mut dyn FnMut(u64, u64, bool) -> bool) -> bool {
    match t.kind(ty) {
        TyKind::Array(elem, ArrayLen::Known(n)) => {
            let es = t.size_of(*elem).unwrap_or(0);
            for i in 0..*n {
                if !leaves(t, *elem, base + i * es, f) {
                    return false;
                }
            }
            true
        }
        TyKind::Record(rid) => {
            let fields = t.record(*rid).fields.clone();
            for fld in &fields {
                if let Some(b) = fld.bit {
                    // Bit-fields are integer class; they occupy the bytes they touch.
                    let first = b.bit_offset / 8;
                    let last = (b.bit_offset + b.width as u64 - 1) / 8;
                    for byte in first..=last {
                        if !f(base + byte, 1, false) {
                            return false;
                        }
                    }
                } else if !leaves(t, fld.ty, base + fld.offset, f) {
                    return false;
                }
            }
            true
        }
        _ => {
            let size = t.size_of(ty).unwrap_or(8);
            let is_float = t.is_floating(ty);
            f(base, size, is_float)
        }
    }
}

pub fn classify(t: &TypeTable, ty: Ty) -> AggAbi {
    let size = t.size_of(ty).unwrap_or(0);
    if size == 0 {
        return AggAbi::Empty;
    }
    if size > 16 {
        return AggAbi::Memory;
    }
    let neb = size.div_ceil(8) as usize;
    let mut classes: [Option<Class>; 2] = [None, None];
    let mut ok = true;
    let mut visit = |off: u64, sz: u64, is_float: bool| -> bool {
        if sz == 0 {
            return true;
        }
        // misaligned or straddling a boundary => memory class
        if off % sz.min(8) != 0 || off / 8 != (off + sz - 1) / 8 {
            ok = false;
            return false;
        }
        let idx = (off / 8) as usize;
        let c = if is_float { Class::Sse } else { Class::Integer };
        classes[idx] = Some(match (classes[idx], c) {
            (Some(Class::Integer), _) | (_, Class::Integer) => Class::Integer,
            _ => Class::Sse,
        });
        true
    };
    if !leaves(t, ty, 0, &mut visit) || !ok {
        return AggAbi::Memory;
    }
    let mut pieces = Vec::new();
    for (i, cls) in classes.iter().enumerate().take(neb) {
        let bytes = if i + 1 == neb { size - 8 * i as u64 } else { 8 };
        let c = cls.unwrap_or(Class::Integer);
        let ty = match (c, bytes) {
            (Class::Sse, 1..=4) => Type::F32,
            (Class::Sse, _) => Type::F64,
            (Class::Integer, 1) => Type::I8,
            (Class::Integer, 2) => Type::I16,
            (Class::Integer, 3..=4) => Type::I32,
            (Class::Integer, _) => Type::I64,
        };
        pieces.push(Piece { ty, offset: (8 * i) as u32 });
    }
    AggAbi::Regs(pieces)
}

/// Do loading/storing these pieces touch exactly `size` bytes (no overrun)?
pub fn pieces_exact(pieces: &[Piece], size: u64) -> bool {
    match pieces.last() {
        Some(p) => p.offset as u64 + p.ty.size() as u64 == size,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intern::Symbol;
    use crate::source::Span;
    use crate::types::MemberInput;

    fn rec(t: &mut TypeTable, members: &[(&str, Ty)], is_union: bool) -> Ty {
        let id = t.new_record(None, is_union, Span::DUMMY);
        let ms = members
            .iter()
            .map(|(n, ty)| MemberInput {
                name: Some(Symbol::new(n)),
                ty: *ty,
                bit_width: None,
                align: None,
                anonymous: false,
                span: Span::DUMMY,
            })
            .collect();
        t.layout_record(id, ms, None);
        t.record_type(id)
    }

    #[test]
    fn small_integer_structs() {
        let mut t = TypeTable::new();
        let p = t.p;
        let s = rec(&mut t, &[("a", p.int), ("b", p.int)], false);
        assert_eq!(classify(&t, s), AggAbi::Regs(vec![Piece { ty: Type::I64, offset: 0 }]));
        let s = rec(&mut t, &[("a", p.long), ("b", p.long)], false);
        assert_eq!(
            classify(&t, s),
            AggAbi::Regs(vec![Piece { ty: Type::I64, offset: 0 }, Piece { ty: Type::I64, offset: 8 }])
        );
        let s = rec(&mut t, &[("a", p.char_)], false);
        assert_eq!(classify(&t, s), AggAbi::Regs(vec![Piece { ty: Type::I8, offset: 0 }]));
        let s = rec(&mut t, &[("a", p.int), ("c", p.char_)], false); // size 8
        assert_eq!(classify(&t, s), AggAbi::Regs(vec![Piece { ty: Type::I64, offset: 0 }]));
        let s = rec(&mut t, &[("a", p.long), ("c", p.char_)], false); // size 16
        assert_eq!(
            classify(&t, s),
            AggAbi::Regs(vec![Piece { ty: Type::I64, offset: 0 }, Piece { ty: Type::I64, offset: 8 }])
        );
    }

    #[test]
    fn float_and_mixed_structs() {
        let mut t = TypeTable::new();
        let p = t.p;
        let s = rec(&mut t, &[("x", p.double), ("y", p.double)], false);
        assert_eq!(
            classify(&t, s),
            AggAbi::Regs(vec![Piece { ty: Type::F64, offset: 0 }, Piece { ty: Type::F64, offset: 8 }])
        );
        let s = rec(&mut t, &[("x", p.float), ("y", p.float)], false);
        assert_eq!(classify(&t, s), AggAbi::Regs(vec![Piece { ty: Type::F64, offset: 0 }]));
        let s = rec(&mut t, &[("x", p.float)], false);
        assert_eq!(classify(&t, s), AggAbi::Regs(vec![Piece { ty: Type::F32, offset: 0 }]));
        let s = rec(&mut t, &[("d", p.double), ("l", p.long)], false);
        assert_eq!(
            classify(&t, s),
            AggAbi::Regs(vec![Piece { ty: Type::F64, offset: 0 }, Piece { ty: Type::I64, offset: 8 }])
        );
        // float + int in one eightbyte merge to INTEGER
        let s = rec(&mut t, &[("f", p.float), ("i", p.int)], false);
        assert_eq!(classify(&t, s), AggAbi::Regs(vec![Piece { ty: Type::I64, offset: 0 }]));
    }

    #[test]
    fn memory_class_and_empty() {
        let mut t = TypeTable::new();
        let p = t.p;
        let s = rec(&mut t, &[("a", p.long), ("b", p.long), ("c", p.long)], false);
        assert_eq!(classify(&t, s), AggAbi::Memory);
        let s = rec(&mut t, &[], false);
        assert_eq!(classify(&t, s), AggAbi::Empty);
        let arr = t.array(p.int, ArrayLen::Known(5));
        let s = rec(&mut t, &[("a", arr)], false);
        assert_eq!(classify(&t, s), AggAbi::Memory);
        let arr = t.array(p.int, ArrayLen::Known(3));
        let s = rec(&mut t, &[("a", arr)], false); // 12 bytes of ints
        assert_eq!(
            classify(&t, s),
            AggAbi::Regs(vec![Piece { ty: Type::I64, offset: 0 }, Piece { ty: Type::I32, offset: 8 }])
        );
    }

    #[test]
    fn nested_and_union() {
        let mut t = TypeTable::new();
        let p = t.p;
        let inner = rec(&mut t, &[("x", p.double)], false);
        let s = rec(&mut t, &[("in", inner), ("n", p.long)], false);
        assert_eq!(
            classify(&t, s),
            AggAbi::Regs(vec![Piece { ty: Type::F64, offset: 0 }, Piece { ty: Type::I64, offset: 8 }])
        );
        let u = rec(&mut t, &[("d", p.double), ("l", p.long)], true);
        assert_eq!(classify(&t, u), AggAbi::Regs(vec![Piece { ty: Type::I64, offset: 0 }]));
    }

    #[test]
    fn odd_sizes_need_padding() {
        let mut t = TypeTable::new();
        let p = t.p;
        let arr = t.array(p.char_, ArrayLen::Known(3));
        let s = rec(&mut t, &[("a", arr)], false);
        let AggAbi::Regs(pieces) = classify(&t, s) else { panic!() };
        assert!(!pieces_exact(&pieces, 3));
        let s = rec(&mut t, &[("a", p.int), ("b", p.int)], false);
        let AggAbi::Regs(pieces) = classify(&t, s) else { panic!() };
        assert!(pieces_exact(&pieces, 8));
    }
}
