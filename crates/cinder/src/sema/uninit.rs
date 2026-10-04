//! Use of uninitialized variables: a flow-sensitive "definite assignment"
//! analysis over the typed HIR (in the style of Java's rules).
//!
//! For every scalar automatic variable declared without an initializer we
//! track two sets while walking the function in evaluation order:
//!
//! * `def`   — assigned on *every* path reaching this point;
//! * `maybe` — assigned on *some* path.
//!
//! Reading a variable that is not in `def` warns: `uninitialized` when it is
//! not even in `maybe` (no path assigns it), `maybe-uninitialized` otherwise.
//! Each variable is reported once.
//!
//! The analysis is deliberately conservative towards *not* warning:
//! taking a variable's address counts as initializing it (`scanf("%d", &x)`),
//! calls to `_Noreturn` functions end a path, conditions are analysed with
//! separate "when true" / "when false" states (so `if (a && (x = f())) use(x)`
//! is fine), and after a label (a `goto` can arrive from anywhere) everything
//! is assumed initialized. Aggregates and variables under `volatile` are not tracked.

use crate::diag::Warn;
use crate::hir::*;
use crate::source::Span;
use crate::types::TypeTable;

pub struct Finding {
    pub flag: Warn,
    pub span: Span,
    pub message: String,
    pub declared: Span,
    pub name: String,
}

#[derive(Clone)]
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize, full: bool) -> Bits {
        Bits(vec![if full { u64::MAX } else { 0 }; n.div_ceil(64).max(1)])
    }
    fn get(&self, i: usize) -> bool {
        self.0[i / 64] >> (i % 64) & 1 == 1
    }
    fn set(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }
    fn clear(&mut self, i: usize) {
        self.0[i / 64] &= !(1 << (i % 64));
    }
}

/// The state at a program point; `dead` marks code that cannot be reached
/// (after `return`, `break`, a `_Noreturn` call, ...), where everything is vacuously initialized.
#[derive(Clone)]
struct State {
    dead: bool,
    def: Bits,
    maybe: Bits,
}

impl State {
    fn dead(n: usize) -> State {
        State { dead: true, def: Bits::new(n, true), maybe: Bits::new(n, false) }
    }

    /// Merge of two control-flow paths.
    fn join(a: &State, b: &State) -> State {
        if a.dead {
            return b.clone();
        }
        if b.dead {
            return a.clone();
        }
        State {
            dead: false,
            def: Bits(a.def.0.iter().zip(&b.def.0).map(|(x, y)| x & y).collect()),
            maybe: Bits(a.maybe.0.iter().zip(&b.maybe.0).map(|(x, y)| x | y).collect()),
        }
    }
}

/// Variables assigned in `src` are possibly assigned in `dst` too (a loop
/// condition is evaluated again after the body, so the exit sees the body's assignments).
fn add_maybe(dst: &mut State, src: &State) {
    if dst.dead || src.dead {
        return;
    }
    for (d, s) in dst.maybe.0.iter_mut().zip(&src.maybe.0) {
        *d |= *s;
    }
}

struct Ctx {
    is_loop: bool,
    breaks: Vec<State>,
    continues: Vec<State>,
    /// Entry state of a `switch` (what every case label starts from).
    switch_entry: Option<State>,
}

struct Analyzer<'a> {
    locals: &'a [Local],
    syms: &'a [GlobalSym],
    n: usize,
    tracked: Vec<bool>,
    reported: Vec<bool>,
    ctxs: Vec<Ctx>,
    out: Vec<Finding>,
}

pub fn analyze(body: &HStmt, locals: &[Local], types: &TypeTable, syms: &[GlobalSym]) -> Vec<Finding> {
    let n = locals.len();
    let tracked: Vec<bool> = locals
        .iter()
        .map(|l| {
            !l.is_param
                && !l.name.as_str().is_empty()
                && !l.name.as_str().starts_with('<')
                && types.is_scalar(l.ty)
                && !types.quals(l.ty).is_volatile
        })
        .collect();
    let mut a = Analyzer { locals, syms, n, tracked, reported: vec![false; n], ctxs: Vec::new(), out: Vec::new() };
    let start = State { dead: false, def: Bits::new(n, false), maybe: Bits::new(n, false) };
    a.stmt(body, start);
    a.out
}

impl<'a> Analyzer<'a> {
    fn full(&self) -> State {
        State { dead: false, def: Bits::new(self.n, true), maybe: Bits::new(self.n, true) }
    }

    fn define(&self, l: LocalId, st: &mut State) {
        let i = l.0 as usize;
        st.def.set(i);
        st.maybe.set(i);
    }

    fn read(&mut self, l: LocalId, span: Span, st: &mut State) {
        let i = l.0 as usize;
        if st.dead || !self.tracked[i] || st.def.get(i) || self.reported[i] {
            return;
        }
        self.reported[i] = true;
        let name = self.locals[i].name.to_string();
        let (flag, message) = if st.maybe.get(i) {
            (Warn::MaybeUninitialized, format!("variable '{}' may be uninitialized when used here", name))
        } else {
            (Warn::Uninitialized, format!("variable '{}' is uninitialized when used here", name))
        };
        self.out.push(Finding { flag, span, message, declared: self.locals[i].span, name });
        // one report is enough
        self.define(l, st);
    }

    // ───────────────────────────── expressions ─────────────────────────────

    /// The sub-expressions evaluated by a place, without touching the place itself.
    fn place_children(&mut self, p: &HExpr, st: &mut State) {
        match &p.kind {
            HExprKind::Local(_) | HExprKind::Global(_) | HExprKind::Str(_) => {}
            HExprKind::Deref(x) => self.expr(x, st),
            HExprKind::Member(b, _) => self.place_children(b, st),
            _ => self.expr(p, st),
        }
    }

    fn write_place(&mut self, p: &HExpr, st: &mut State) {
        match &p.kind {
            HExprKind::Local(l) => self.define(*l, st),
            _ => self.place_children(p, st),
        }
    }

    fn read_place(&mut self, p: &HExpr, st: &mut State) {
        match &p.kind {
            HExprKind::Local(l) => self.read(*l, p.span, st),
            _ => self.place_children(p, st),
        }
    }

    fn is_noreturn_call(&self, callee: &HExpr) -> bool {
        let mut c = callee;
        while let HExprKind::Cast(_, inner) = &c.kind {
            c = inner;
        }
        matches!(&c.kind, HExprKind::Global(s) if self.syms[s.0 as usize].noreturn)
    }

    fn expr(&mut self, e: &HExpr, st: &mut State) {
        match &e.kind {
            HExprKind::Int(_)
            | HExprKind::Float(_)
            | HExprKind::Str(_)
            | HExprKind::Global(_)
            | HExprKind::Local(_)
            | HExprKind::VlaSizeof(_)
            | HExprKind::Error => {}
            HExprKind::Trap => st.dead = true,
            HExprKind::Cast(kind, x) => {
                if *kind == CastKind::LValueToRValue {
                    if let HExprKind::Local(l) = &x.kind {
                        self.read(*l, x.span, st);
                        return;
                    }
                }
                self.expr(x, st);
            }
            HExprKind::AddrOf(x) => match &x.kind {
                HExprKind::Local(l) => self.define(*l, st), // may be initialized through the pointer
                _ => self.place_children(x, st),
            },
            HExprKind::Deref(x) => self.expr(x, st),
            HExprKind::Member(b, _) => self.place_children(b, st),
            HExprKind::CompoundLit { init, .. } => {
                for en in &init.entries {
                    if let InitValue::Expr(v) = &en.value {
                        self.expr(v, st);
                    }
                }
            }
            HExprKind::Unary(_, x) => self.expr(x, st),
            HExprKind::Binary(_, a, b) => {
                self.expr(a, st);
                self.expr(b, st);
            }
            HExprKind::PtrAdd { ptr, idx, .. } => {
                self.expr(ptr, st);
                self.expr(idx, st);
            }
            HExprKind::PtrDiff { l, r, .. } => {
                self.expr(l, st);
                self.expr(r, st);
            }
            HExprKind::LogAnd(..) | HExprKind::LogOr(..) => {
                let (t, f) = self.cond(e, st.clone());
                *st = State::join(&t, &f);
            }
            HExprKind::Cond(c, a, b) => {
                let (ct, cf) = self.cond(c, st.clone());
                let (mut sa, mut sb) = (ct, cf);
                self.expr(a, &mut sa);
                self.expr(b, &mut sb);
                *st = State::join(&sa, &sb);
            }
            HExprKind::Comma(a, b) => {
                self.expr(a, st);
                self.expr(b, st);
            }
            HExprKind::Assign(p, v) => {
                self.expr(v, st);
                self.write_place(p, st);
            }
            HExprKind::CompoundAssign { place, value, .. } => {
                self.read_place(place, st);
                self.expr(value, st);
                self.write_place(place, st);
            }
            HExprKind::IncDec { place, .. } => {
                self.read_place(place, st);
                self.write_place(place, st);
            }
            HExprKind::Call { callee, args } => {
                self.expr(callee, st);
                for a in args {
                    self.expr(a, st);
                }
                if self.is_noreturn_call(callee) {
                    st.dead = true;
                }
            }
            HExprKind::VaStart(x) | HExprKind::VaEnd(x) | HExprKind::VaArg(x) => self.expr(x, st),
            HExprKind::VaCopy(a, b) => {
                self.expr(a, st);
                self.expr(b, st);
            }
        }
    }

    /// Analyse a controlling expression: the states when it is true and when it is false.
    fn cond(&mut self, e: &HExpr, st: State) -> (State, State) {
        match &e.kind {
            HExprKind::Int(v) => {
                let dead = State::dead(self.n);
                if *v != 0 {
                    (st, dead)
                } else {
                    (dead, st)
                }
            }
            HExprKind::LogAnd(a, b) => {
                let (at, af) = self.cond(a, st);
                let (bt, bf) = self.cond(b, at);
                (bt, State::join(&af, &bf))
            }
            HExprKind::LogOr(a, b) => {
                let (at, af) = self.cond(a, st);
                let (bt, bf) = self.cond(b, af);
                (State::join(&at, &bt), bf)
            }
            HExprKind::Unary(UnKind::LogNot, x) => {
                let (t, f) = self.cond(x, st);
                (f, t)
            }
            _ => {
                let mut s = st;
                self.expr(e, &mut s);
                (s.clone(), s)
            }
        }
    }

    // ───────────────────────────── statements ─────────────────────────────

    fn seq(&mut self, items: &[HStmt], mut st: State) -> State {
        for it in items {
            st = self.stmt(it, st);
        }
        st
    }

    fn jump_target(&mut self, want_loop: bool) -> Option<usize> {
        self.ctxs.iter().rposition(|c| c.is_loop || !want_loop)
    }

    fn stmt(&mut self, s: &HStmt, mut st: State) -> State {
        match &s.kind {
            HStmtKind::Empty | HStmtKind::VlaDecl { .. } => st,
            HStmtKind::Expr(e) => {
                self.expr(e, &mut st);
                st
            }
            HStmtKind::Decl { local, init } => {
                let i = local.0 as usize;
                // a fresh variable (the declaration may run again in a loop)
                st.def.clear(i);
                st.maybe.clear(i);
                if let Some(plan) = init {
                    for en in &plan.entries {
                        if let InitValue::Expr(v) = &en.value {
                            self.expr(v, &mut st);
                        }
                    }
                    self.define(*local, &mut st);
                }
                st
            }
            HStmtKind::Block(items) | HStmtKind::VlaScope(items) => self.seq(items, st),
            HStmtKind::If(c, t, e) => {
                let (ct, cf) = self.cond(c, st);
                let s1 = self.stmt(t, ct);
                let s2 = match e {
                    Some(e) => self.stmt(e, cf),
                    None => cf,
                };
                State::join(&s1, &s2)
            }
            HStmtKind::While(c, body) => {
                let (ct, cf) = self.cond(c, st);
                self.ctxs.push(Ctx { is_loop: true, breaks: Vec::new(), continues: Vec::new(), switch_entry: None });
                let end = self.stmt(body, ct);
                let ctx = self.ctxs.pop().unwrap();
                let mut out = ctx.breaks.iter().fold(cf, |acc, b| State::join(&acc, b));
                add_maybe(&mut out, &end);
                for c in &ctx.continues {
                    add_maybe(&mut out, c);
                }
                out
            }
            HStmtKind::DoWhile(body, c) => {
                self.ctxs.push(Ctx { is_loop: true, breaks: Vec::new(), continues: Vec::new(), switch_entry: None });
                let end = self.stmt(body, st);
                let ctx = self.ctxs.pop().unwrap();
                let at_cond = ctx.continues.iter().fold(end, |acc, b| State::join(&acc, b));
                let (_, cf) = self.cond(c, at_cond);
                ctx.breaks.iter().fold(cf, |acc, b| State::join(&acc, b))
            }
            HStmtKind::For { init, cond, step, body } => {
                let st = self.seq(init, st);
                let (ct, cf) = match cond {
                    Some(c) => self.cond(c, st),
                    None => (st, State::dead(self.n)),
                };
                self.ctxs.push(Ctx { is_loop: true, breaks: Vec::new(), continues: Vec::new(), switch_entry: None });
                let end = self.stmt(body, ct);
                let ctx = self.ctxs.pop().unwrap();
                let mut at_step = ctx.continues.iter().fold(end, |acc, b| State::join(&acc, b));
                if let Some(step) = step {
                    self.expr(step, &mut at_step);
                }
                let mut out = ctx.breaks.iter().fold(cf, |acc, b| State::join(&acc, b));
                add_maybe(&mut out, &at_step);
                out
            }
            HStmtKind::Switch { cond, body, default, .. } => {
                self.expr(cond, &mut st);
                let entry = st.clone();
                self.ctxs.push(Ctx {
                    is_loop: false,
                    breaks: Vec::new(),
                    continues: Vec::new(),
                    switch_entry: Some(entry.clone()),
                });
                let end = self.stmt(body, State::dead(self.n));
                let ctx = self.ctxs.pop().unwrap();
                let mut out = ctx.breaks.iter().fold(end, |acc, b| State::join(&acc, b));
                if default.is_none() {
                    out = State::join(&out, &entry);
                }
                out
            }
            HStmtKind::CaseLabel(_) => {
                // reachable from the switch itself or by falling through from the previous case
                let entry = self.ctxs.iter().rev().find_map(|c| c.switch_entry.clone());
                match entry {
                    Some(e) => State::join(&st, &e),
                    None => st,
                }
            }
            HStmtKind::Break => {
                if let Some(i) = self.jump_target(false) {
                    self.ctxs[i].breaks.push(st);
                }
                State::dead(self.n)
            }
            HStmtKind::Continue => {
                if let Some(i) = self.jump_target(true) {
                    self.ctxs[i].continues.push(st);
                }
                State::dead(self.n)
            }
            HStmtKind::Return(v) => {
                if let Some(e) = v {
                    self.expr(e, &mut st);
                }
                State::dead(self.n)
            }
            HStmtKind::Goto(_) => State::dead(self.n),
            // a goto can arrive here from anywhere: assume everything is initialized
            HStmtKind::Label(_) => self.full(),
        }
    }
}
