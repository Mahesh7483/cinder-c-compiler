//! Cinder: a from-scratch C11 compiler targeting x86-64 Linux (System V ABI).
//!
//! The pipeline is documented stage by stage under `docs/`; the module layout
//! mirrors it.

pub mod abi;
pub mod ast;
pub mod ast_dump;
pub mod diag;
pub mod driver;
pub mod headers;
pub mod hir;
pub mod hir_dump;
pub mod intern;
pub mod ir;
pub mod lex;
pub mod literal;
pub mod lower;
pub mod options;
pub mod parse;
pub mod pp;
pub mod sema;
pub mod session;
pub mod source;
pub mod types;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
