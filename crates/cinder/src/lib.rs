//! Cinder: a from-scratch C11 compiler targeting x86-64 Linux (System V ABI).
//!
//! The pipeline is documented stage by stage under `docs/`; the module layout
//! mirrors it.

pub mod diag;
pub mod intern;
pub mod session;
pub mod source;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
