//! Shared state threaded through every compiler stage.

use crate::diag::DiagCtx;
use crate::source::SourceMap;

#[derive(Default)]
pub struct Session {
    pub sources: SourceMap,
    pub diags: DiagCtx,
}

impl Session {
    pub fn new() -> Session {
        Session::default()
    }
}
