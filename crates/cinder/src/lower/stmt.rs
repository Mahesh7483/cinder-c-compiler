//! Statement lowering.

use super::*;
use crate::hir::{HExpr, HStmt, HStmtKind};

impl<'a> FnLower<'a> {
    pub fn stmt(&mut self, s: &'a HStmt) {
        let l = self.line_of(s.span);
        if l != 0 {
            self.line = l;
        }
        match &s.kind {
            HStmtKind::Empty => {}
            HStmtKind::Expr(e) => {
                self.rv(e);
            }
            HStmtKind::Decl { local, init } => {
                if let Some(plan) = init {
                    let slot = self.slots[local.0 as usize];
                    let ty = self.hf.locals[local.0 as usize].ty;
                    self.emit_init(slot, plan, ty);
                }
            }
            HStmtKind::Block(items) => {
                for it in items {
                    self.stmt(it);
                }
            }
            HStmtKind::If(c, t, e) => {
                let then_bb = self.new_block("if.then");
                let end = self.new_block("if.end");
                let else_bb = if e.is_some() { self.new_block("if.else") } else { end };
                self.cond_branch(c, then_bb, else_bb);
                self.set_cur(then_bb);
                self.stmt(t);
                self.br(end);
                if let Some(e) = e {
                    self.set_cur(else_bb);
                    self.stmt(e);
                    self.br(end);
                }
                self.set_cur(end);
            }
            HStmtKind::While(c, body) => {
                let cond_bb = self.new_block("while.cond");
                let body_bb = self.new_block("while.body");
                let end = self.new_block("while.end");
                self.br(cond_bb);
                self.set_cur(cond_bb);
                self.cond_branch(c, body_bb, end);
                self.set_cur(body_bb);
                self.in_loop(end, cond_bb, |s| s.stmt(body));
                self.br(cond_bb);
                self.set_cur(end);
            }
            HStmtKind::DoWhile(body, c) => {
                let body_bb = self.new_block("do.body");
                let cond_bb = self.new_block("do.cond");
                let end = self.new_block("do.end");
                self.br(body_bb);
                self.set_cur(body_bb);
                self.in_loop(end, cond_bb, |s| s.stmt(body));
                self.br(cond_bb);
                self.set_cur(cond_bb);
                self.cond_branch(c, body_bb, end);
                self.set_cur(end);
            }
            HStmtKind::For { init, cond, step, body } => {
                for i in init {
                    self.stmt(i);
                }
                let cond_bb = self.new_block("for.cond");
                let body_bb = self.new_block("for.body");
                let step_bb = self.new_block("for.inc");
                let end = self.new_block("for.end");
                self.br(cond_bb);
                self.set_cur(cond_bb);
                match cond {
                    Some(c) => self.cond_branch(c, body_bb, end),
                    None => self.br(body_bb),
                }
                self.set_cur(body_bb);
                self.in_loop(end, step_bb, |s| s.stmt(body));
                self.br(step_bb);
                self.set_cur(step_bb);
                if let Some(st) = step {
                    self.rv(st);
                }
                self.br(cond_bb);
                self.set_cur(end);
            }
            HStmtKind::Switch { cond, body, cases, default } => {
                let v = self.rv(cond);
                let ty = self.f.operand_ty(v);
                let end = self.new_block("sw.epilog");
                let mut targets: Vec<(i64, BlockId)> = Vec::new();
                for (val, id) in cases {
                    let b = self.new_block("sw.bb");
                    self.case_blocks.insert(id.0, b);
                    targets.push((*val, b));
                }
                let default_bb = match default {
                    Some(id) => {
                        let b = self.new_block("sw.default");
                        self.case_blocks.insert(id.0, b);
                        b
                    }
                    None => end,
                };
                self.terminate(Term::Switch { ty, val: v, cases: targets, default: default_bb });
                self.start_dead_block();
                self.break_targets.push(end);
                self.stmt(body);
                self.break_targets.pop();
                self.br(end);
                self.set_cur(end);
            }
            HStmtKind::CaseLabel(id) => {
                let b = self.case_blocks[&id.0];
                self.br(b);
                self.set_cur(b);
            }
            HStmtKind::Break => {
                let t = *self.break_targets.last().expect("break outside loop/switch");
                self.br(t);
                self.start_dead_block();
            }
            HStmtKind::Continue => {
                let t = *self.continue_targets.last().expect("continue outside loop");
                self.br(t);
                self.start_dead_block();
            }
            HStmtKind::Return(v) => {
                self.lower_return(v.as_ref());
                self.start_dead_block();
            }
            HStmtKind::Goto(l) => {
                let b = self.label_block(l.0);
                self.br(b);
                self.start_dead_block();
            }
            HStmtKind::Label(l) => {
                let b = self.label_block(l.0);
                self.br(b);
                self.set_cur(b);
            }
        }
    }

    fn label_block(&mut self, id: u32) -> BlockId {
        if let Some(&b) = self.labels.get(&id) {
            return b;
        }
        let name =
            self.hf.labels.get(id as usize).map(|s| format!("label.{}", s)).unwrap_or_else(|| "label".to_string());
        let b = self.new_block(&name);
        self.labels.insert(id, b);
        b
    }

    fn in_loop(&mut self, brk: BlockId, cont: BlockId, f: impl FnOnce(&mut Self)) {
        self.break_targets.push(brk);
        self.continue_targets.push(cont);
        f(self);
        self.break_targets.pop();
        self.continue_targets.pop();
    }

    fn lower_return(&mut self, v: Option<&'a HExpr>) {
        let Some(e) = v else {
            self.terminate(Term::Ret(Vec::new()));
            return;
        };
        let ret_ty = self.hf.ret;
        if self.hir.types.is_void(ret_ty) {
            self.rv(e);
            self.terminate(Term::Ret(Vec::new()));
            return;
        }
        if self.is_agg(ret_ty) {
            let addr = self.rv(e);
            match self.passing(ret_ty) {
                Passing::ByVal => {
                    let sret = self.sret.expect("sret parameter");
                    self.copy_aggregate(sret, addr, ret_ty);
                    self.terminate(Term::Ret(vec![sret]));
                }
                Passing::Pieces(pieces) => {
                    let size = self.size_of(ret_ty);
                    let src = if super::abi::pieces_exact(&pieces, size) {
                        addr
                    } else {
                        let tmp = self.temp_for(ret_ty);
                        self.emit(InstKind::MemSet { dst: tmp, byte: 0, size: round_up(size, 8), align: 8 }, None);
                        self.emit(InstKind::MemCopy { dst: tmp, src: addr, size, align: self.align_of(ret_ty) }, None);
                        tmp
                    };
                    let mut vals = Vec::new();
                    for p in pieces {
                        let ptr = self.ptr_add(src, p.offset as u64);
                        vals.push(self.load(p.ty, ptr, false));
                    }
                    self.terminate(Term::Ret(vals));
                }
                _ => self.terminate(Term::Ret(Vec::new())),
            }
            return;
        }
        let val = self.rv(e);
        let natural = self.ity(ret_ty);
        let abi_t = self.f.rets.first().copied().unwrap_or(natural);
        let val = if abi_t != natural && natural.is_int() {
            let signed = self.is_signed(ret_ty);
            self.cast(if signed { CastOp::SExt } else { CastOp::ZExt }, natural, abi_t, val)
        } else {
            val
        };
        self.terminate(Term::Ret(vec![val]));
    }
}
