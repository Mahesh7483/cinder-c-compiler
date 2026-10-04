//! x86-64 backend: instruction selection, register allocation, frame
//! layout, peephole clean-up and assembly emission (AT&T syntax).

pub mod emit;
pub mod frame;
pub mod isel;
mod isel_inst;
pub mod mir;
pub mod peephole;
pub mod regalloc;

use crate::ir::Module;

#[derive(Clone, Debug)]
pub struct BackendOptions {
    /// Emit `.loc` directives mapping assembly to source lines.
    pub emit_loc: bool,
    pub peephole: bool,
}

impl Default for BackendOptions {
    fn default() -> BackendOptions {
        BackendOptions { emit_loc: true, peephole: true }
    }
}

/// Generate x86-64 assembly for a whole module.
pub fn compile_module(m: &Module, opts: &BackendOptions) -> String {
    let mut e = emit::Emitter::new(m, opts.emit_loc);
    e.header(&m.source_file);
    for f in &m.funcs {
        let mut mf = isel::select(m, f);
        regalloc::allocate(&mut mf);
        if opts.peephole {
            peephole::run(&mut mf);
        }
        frame::layout(&mut mf);
        e.function(&mf);
    }
    e.data();
    e.footer();
    e.out
}
