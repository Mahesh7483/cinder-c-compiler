//! System V x86-64 calling-convention argument assignment, shared by IR
//! lowering (to compute `va_start` offsets) and the backend (to place
//! arguments), so both always agree.
//!
//! Integer-class values take the next of `rdi, rsi, rdx, rcx, r8, r9`;
//! SSE-class values take the next of `xmm0..xmm7`; everything else goes on
//! the stack in 8-byte slots. The pieces of one aggregate (a "group") are
//! assigned atomically: if they do not all fit in the remaining registers,
//! the whole aggregate goes to the stack.

use crate::ir::Type;

pub const MAX_GPR_ARGS: u32 = 6;
pub const MAX_XMM_ARGS: u32 = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Loc {
    Gpr(u8),
    Xmm(u8),
    /// Byte offset from the first stack-passed argument.
    Stack(u32),
}

#[derive(Clone, Debug)]
pub struct ArgDesc {
    /// Scalar type, or `None` for a by-value aggregate copied to the stack.
    pub ty: Option<Type>,
    pub byval: Option<(u32, u32)>,
    pub group: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Assignment {
    pub locs: Vec<Loc>,
    /// Bytes of stack argument space used.
    pub stack_size: u32,
    pub gp_used: u32,
    pub fp_used: u32,
}

fn round_up(v: u32, a: u32) -> u32 {
    v.div_ceil(a) * a
}

pub fn assign(args: &[ArgDesc]) -> Assignment {
    let mut locs: Vec<Loc> = Vec::with_capacity(args.len());
    let (mut gp, mut fp, mut stack) = (0u32, 0u32, 0u32);
    let mut i = 0;
    while i < args.len() {
        // Gather the group starting here (a single argument is its own group).
        let mut j = i + 1;
        if let Some(g) = args[i].group {
            while j < args.len() && args[j].group == Some(g) {
                j += 1;
            }
        }
        let group = &args[i..j];
        let need_gp = group.iter().filter(|a| a.ty.is_some_and(|t| t.is_gpr())).count() as u32;
        let need_fp = group.iter().filter(|a| a.ty.is_some_and(|t| t.is_float())).count() as u32;
        let fits = gp + need_gp <= MAX_GPR_ARGS && fp + need_fp <= MAX_XMM_ARGS;
        for a in group {
            match (a.ty, a.byval) {
                (Some(t), _) if fits && t.is_gpr() => {
                    locs.push(Loc::Gpr(gp as u8));
                    gp += 1;
                }
                (Some(_), _) if fits => {
                    locs.push(Loc::Xmm(fp as u8));
                    fp += 1;
                }
                (Some(_), _) => {
                    locs.push(Loc::Stack(stack));
                    stack += 8;
                }
                (None, Some((size, align))) => {
                    stack = round_up(stack, align.max(8));
                    locs.push(Loc::Stack(stack));
                    stack += round_up(size, 8);
                }
                (None, None) => unreachable!("argument without type or byval"),
            }
        }
        i = j;
    }
    Assignment { locs, stack_size: round_up(stack, 8), gp_used: gp, fp_used: fp }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(t: Type) -> ArgDesc {
        ArgDesc { ty: Some(t), byval: None, group: None }
    }

    #[test]
    fn integers_then_floats_then_stack() {
        let mut args: Vec<ArgDesc> = (0..8).map(|_| v(Type::I64)).collect();
        args.extend((0..9).map(|_| v(Type::F64)));
        let a = assign(&args);
        assert_eq!(a.locs[0], Loc::Gpr(0));
        assert_eq!(a.locs[5], Loc::Gpr(5));
        assert_eq!(a.locs[6], Loc::Stack(0));
        assert_eq!(a.locs[7], Loc::Stack(8));
        assert_eq!(a.locs[8], Loc::Xmm(0));
        assert_eq!(a.locs[15], Loc::Xmm(7));
        assert_eq!(a.locs[16], Loc::Stack(16));
        assert_eq!(a.stack_size, 24);
        assert_eq!((a.gp_used, a.fp_used), (6, 8));
    }

    #[test]
    fn mixed_int_float_use_independent_counters() {
        let a = assign(&[v(Type::I32), v(Type::F64), v(Type::Ptr), v(Type::F32)]);
        assert_eq!(a.locs, [Loc::Gpr(0), Loc::Xmm(0), Loc::Gpr(1), Loc::Xmm(1)]);
    }

    #[test]
    fn aggregate_pieces_are_atomic() {
        // five integer args leave one GPR: a two-piece struct must go entirely to the stack
        let mut args: Vec<ArgDesc> = (0..5).map(|_| v(Type::I64)).collect();
        args.push(ArgDesc { ty: Some(Type::I64), byval: None, group: Some(0) });
        args.push(ArgDesc { ty: Some(Type::I64), byval: None, group: Some(0) });
        args.push(v(Type::I64)); // a later scalar can still use the last GPR
        let a = assign(&args);
        assert_eq!(a.locs[5], Loc::Stack(0));
        assert_eq!(a.locs[6], Loc::Stack(8));
        assert_eq!(a.locs[7], Loc::Gpr(5));
    }

    #[test]
    fn byval_goes_to_the_stack_with_alignment() {
        let a = assign(&[
            v(Type::I64),
            ArgDesc { ty: None, byval: Some((20, 8)), group: None },
            v(Type::I64),
            ArgDesc { ty: None, byval: Some((16, 16)), group: None },
        ]);
        assert_eq!(a.locs[0], Loc::Gpr(0));
        assert_eq!(a.locs[1], Loc::Stack(0));
        assert_eq!(a.locs[2], Loc::Gpr(1));
        assert_eq!(a.locs[3], Loc::Stack(32)); // 24 rounded up to 16-byte alignment
        assert_eq!(a.stack_size, 48);
    }
}
