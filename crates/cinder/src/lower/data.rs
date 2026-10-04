//! Static data: global variables, string literals, constant initializers.

use super::*;
use crate::hir::{AddrBase, ConstVal, GlobalSym, InitPlan, InitValue};
use crate::literal::StrKind;
use crate::types::TyKind;

fn put_le(buf: &mut [u8], off: usize, size: usize, v: u64) {
    for i in 0..size.min(8) {
        if off + i < buf.len() {
            buf[off + i] = (v >> (8 * i)) as u8;
        }
    }
}

/// Write `width` bits of `v` at absolute bit offset `bit_off` (little-endian bit order).
fn put_bits(buf: &mut [u8], bit_off: u64, width: u32, v: u64) {
    for i in 0..width as u64 {
        let pos = bit_off + i;
        let (byte, bit) = ((pos / 8) as usize, (pos % 8) as u8);
        if byte >= buf.len() {
            break;
        }
        if (v >> i) & 1 == 1 {
            buf[byte] |= 1 << bit;
        } else {
            buf[byte] &= !(1 << bit);
        }
    }
}

impl ModBuilder {
    pub(super) fn build_var_data(&mut self, hir: &HirModule, hs: &GlobalSym) -> DataDef {
        let size = hir.types.size_of(hs.ty).unwrap_or(0).max(1);
        let align = hs.align.max(1);
        match &hs.init {
            None => DataDef { size, align, items: Vec::new(), readonly: false, zero: true },
            Some(plan) => {
                let readonly = hs.is_const;
                self.plan_to_data(hir, plan, size, align, readonly)
            }
        }
    }

    /// Lay a constant initializer plan out as bytes plus relocations.
    pub(super) fn plan_to_data(
        &mut self,
        hir: &HirModule,
        plan: &InitPlan,
        size: u64,
        align: u64,
        readonly: bool,
    ) -> DataDef {
        let mut buf = vec![0u8; size as usize];
        let mut relocs: Vec<(usize, SymId, i64)> = Vec::new();
        for e in &plan.entries {
            let off = e.offset as usize;
            match &e.value {
                InitValue::Const(ConstVal::Int(v)) => {
                    if let Some(b) = e.bit {
                        put_bits(&mut buf, b.bit_offset + (e.offset - b.bit_offset / 8) * 8, b.width, *v);
                    } else {
                        let sz = hir.types.size_of(e.ty).unwrap_or(8) as usize;
                        put_le(&mut buf, off, sz, *v);
                    }
                }
                InitValue::Const(ConstVal::Float(f)) => {
                    if matches!(hir.types.kind(e.ty), TyKind::Float) {
                        put_le(&mut buf, off, 4, (*f as f32).to_bits() as u64);
                    } else {
                        put_le(&mut buf, off, 8, f.to_bits());
                    }
                }
                InitValue::Const(ConstVal::Addr { base, offset }) => {
                    let sym = match base {
                        AddrBase::Global(s) => self.sym(hir, *s),
                        AddrBase::Str(s) => self.str_sym(hir, *s),
                    };
                    relocs.push((off, sym, *offset));
                }
                InitValue::Bytes(b) => {
                    let end = (off + b.len()).min(buf.len());
                    buf[off..end].copy_from_slice(&b[..end - off]);
                }
                InitValue::Expr(_) => unreachable!("static initializers are constant-folded by sema"),
            }
        }
        relocs.sort_by_key(|r| r.0);
        let mut items: Vec<DataItem> = Vec::new();
        let mut pos = 0usize;
        for (off, sym, addend) in &relocs {
            push_bytes(&mut items, &buf[pos..*off]);
            items.push(DataItem::Addr { sym: *sym, offset: *addend });
            pos = off + 8;
        }
        push_bytes(&mut items, &buf[pos.min(buf.len())..]);
        let zero = relocs.is_empty() && buf.iter().all(|&b| b == 0);
        if zero {
            items.clear();
        }
        DataDef { size, align, items, readonly: readonly && !zero, zero }
    }

    /// The data symbol of a string literal (created once).
    pub fn str_sym(&mut self, hir: &HirModule, id: StrId) -> SymId {
        if let Some(&s) = self.str_syms.get(&id) {
            return s;
        }
        let d = &hir.strings[id.0 as usize];
        let elem = match d.kind {
            StrKind::Plain | StrKind::Utf8 => 1usize,
            StrKind::Utf16 => 2,
            StrKind::Wide | StrKind::Utf32 => 4,
        };
        let mut bytes = Vec::with_capacity((d.units.len() + 1) * elem);
        for &u in d.units.iter().chain(std::iter::once(&0u32)) {
            for i in 0..elem {
                bytes.push((u >> (8 * i)) as u8);
            }
        }
        let def = DataDef {
            size: bytes.len() as u64,
            align: elem as u64,
            items: vec![DataItem::Bytes(bytes)],
            readonly: true,
            zero: false,
        };
        let s = self.add_anon_data(".L.str", def);
        self.str_syms.insert(id, s);
        s
    }

    /// An anonymous read-only blob (used for large constant local initializers).
    pub fn const_blob(&mut self, bytes: Vec<u8>, align: u64) -> SymId {
        let def = DataDef {
            size: bytes.len() as u64,
            align,
            items: vec![DataItem::Bytes(bytes)],
            readonly: true,
            zero: false,
        };
        self.add_anon_data(".L.init", def)
    }
}

/// Append raw bytes, turning long zero runs into `Zero` items.
fn push_bytes(items: &mut Vec<DataItem>, bytes: &[u8]) {
    const MIN_ZERO_RUN: usize = 16;
    let mut i = 0;
    let mut start = 0;
    while i < bytes.len() {
        if bytes[i] == 0 {
            let mut j = i;
            while j < bytes.len() && bytes[j] == 0 {
                j += 1;
            }
            if j - i >= MIN_ZERO_RUN {
                if start < i {
                    items.push(DataItem::Bytes(bytes[start..i].to_vec()));
                }
                items.push(DataItem::Zero((j - i) as u64));
                start = j;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    if start < bytes.len() {
        items.push(DataItem::Bytes(bytes[start..].to_vec()));
    }
}
