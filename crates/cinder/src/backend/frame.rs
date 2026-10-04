//! Stack frame layout.
//!
//! ```text
//!   high addresses
//!     incoming stack arguments      rbp + 16 ...
//!     return address                rbp + 8
//!     saved rbp                     rbp
//!     callee-saved registers        rbp - 8 ... rbp - 8k
//!     locals and spill slots        below that, each at its alignment
//!     outgoing argument area        at rsp (bottom of the frame)
//!   low addresses
//! ```
//!
//! The total below `rbp` is a multiple of 16 so `rsp` is 16-byte aligned at
//! every call.

use super::mir::*;

fn align_up(v: u32, a: u32) -> u32 {
    v.div_ceil(a) * a
}

pub fn layout(mf: &mut MFunc) {
    let k = mf.used_callee_saved.len() as u32;
    let mut off = 8 * k;
    for s in &mut mf.slots {
        off = align_up(off + s.size, s.align);
        s.offset = off as i32;
    }
    // With run-time stack allocation the area between the lowest local and `%rsp`
    // must hold the whole rounded outgoing area, so dynamic memory can start right above it.
    let total =
        if mf.dyn_alloca { align_up(off, 16) + align_up(mf.outgoing, 16) } else { align_up(off + mf.outgoing, 16) };
    mf.frame_size = total - 8 * k;
}
