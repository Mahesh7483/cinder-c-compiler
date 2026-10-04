//! Optimizer unit tests: lower a small C function, run selected passes with the
//! verifier checking the IR after every one, and inspect the result.

use super::*;
use crate::ir::cfg;
use crate::ir::print::{print_func, print_module};
use crate::ir::verify::verify_module;
use crate::lower::lower_module;
use crate::parse;
use crate::pp::{self, PpOptions};
use crate::sema;
use crate::session::Session;

fn lower(src: &str) -> Module {
    let mut sess = Session::new();
    sess.diags.config.enable_all();
    let id = sess.sources.add_file("t.c", None, src.to_string());
    let toks = pp::preprocess(&mut sess, id, &PpOptions::default());
    let tu = parse::parse(&mut sess, toks);
    let hir = sema::analyze(&mut sess, &tu);
    assert!(
        !sess.diags.has_errors(),
        "front-end errors for {src:?}: {:?}",
        sess.diags.diagnostics().iter().map(|d| d.message.clone()).collect::<Vec<_>>()
    );
    let m = lower_module(&mut sess, &hir.unwrap(), "t.c");
    verify_module(&m).unwrap_or_else(|e| panic!("lowering produced invalid IR: {}", e.join("\n")));
    m
}

/// Run exactly the named passes (on top of -O0) with verification after each.
fn run(src: &str, passes: &[&str]) -> Module {
    let mut m = lower(src);
    let flags: Vec<(String, bool)> = passes.iter().map(|p| (p.to_string(), true)).collect();
    let cfg = OptConfig::new(0, &flags).unwrap().with_verify(true);
    optimize(&mut m, &cfg);
    if let Err(e) = verify_module(&m) {
        panic!("invalid IR after {passes:?}: {}\n{}", e.join("\n"), print_module(&m));
    }
    m
}

/// Full pipeline at an -O level.
fn at(src: &str, level: u8) -> Module {
    let mut m = lower(src);
    let cfg = OptConfig::new(level, &[]).unwrap().with_verify(true);
    optimize(&mut m, &cfg);
    if let Err(e) = verify_module(&m) {
        panic!("invalid IR at -O{level}: {}\n{}", e.join("\n"), print_module(&m));
    }
    m
}

fn func<'a>(m: &'a Module, name: &str) -> &'a Func {
    m.funcs.iter().find(|f| f.name.as_str() == name).unwrap_or_else(|| panic!("no function {name}"))
}

fn text(m: &Module, name: &str) -> String {
    print_func(m, func(m, name))
}

fn count(t: &str, needle: &str) -> usize {
    t.matches(needle).count()
}

/// Is any instruction accepted by `pred` inside a loop of `f`?
fn in_loop(f: &Func, pred: impl Fn(&InstKind) -> bool) -> bool {
    let g = cfg::build(f);
    let dom = cfg::dominators(f, &g);
    for l in cfg::find_loops(f, &g, &dom) {
        for b in &l.blocks {
            if f.blocks[b.idx()].insts.iter().any(|&i| pred(&f.insts[i.idx()].kind)) {
                return true;
            }
        }
    }
    false
}

fn has_op(f: &Func, pred: impl Fn(&InstKind) -> bool) -> bool {
    f.blocks.iter().flat_map(|b| &b.insts).any(|&i| pred(&f.insts[i.idx()].kind))
}

// ───────────────────────────── configuration ─────────────────────────────

#[test]
fn levels_enable_the_documented_passes() {
    let c0 = OptConfig::new(0, &[]).unwrap();
    assert!(!c0.is_enabled("mem2reg") && !c0.is_enabled("inline"));
    assert!(c0.is_enabled("peephole"));
    let c1 = OptConfig::new(1, &[]).unwrap();
    for p in ["mem2reg", "sccp", "strength", "copyprop", "cse", "dce", "simplifycfg"] {
        assert!(c1.is_enabled(p), "{p} at -O1");
    }
    assert!(!c1.is_enabled("licm") && !c1.is_enabled("inline") && !c1.is_enabled("tailcall"));
    let c2 = OptConfig::new(2, &[]).unwrap();
    for p in ["licm", "inline", "tailcall"] {
        assert!(c2.is_enabled(p), "{p} at -O2");
    }
}

#[test]
fn pass_flags_override_levels_in_order() {
    let flags = vec![("inline".to_string(), false), ("mem2reg".to_string(), false), ("mem2reg".to_string(), true)];
    let c = OptConfig::new(2, &flags).unwrap();
    assert!(!c.is_enabled("inline"));
    assert!(c.is_enabled("mem2reg"));
    // unknown flags are ignored
    assert!(OptConfig::new(1, &[("whatever".to_string(), true)]).is_ok());
}

#[test]
fn nothing_runs_at_o0() {
    let m = at("int f(int a) { int b = a + 1; return b; }", 0);
    assert!(text(&m, "f").contains("alloca"));
}

#[test]
fn disabling_mem2reg_keeps_the_slots() {
    let mut m = lower("int f(int a) { int b = a + 1; return b; }");
    // (cse + dce would otherwise forward and delete the slots on their own)
    let off: Vec<(String, bool)> = ["mem2reg", "cse", "dce"].iter().map(|p| (p.to_string(), false)).collect();
    let cfg = OptConfig::new(2, &off).unwrap().with_verify(true);
    optimize(&mut m, &cfg);
    assert!(text(&m, "f").contains("alloca"));
}

// ───────────────────────────── folding helpers ─────────────────────────────

#[test]
fn integer_folding_edge_cases() {
    assert_eq!(fold_int_bin(BinOp::Add, Type::I32, i32::MAX as i64, 1), Some(i32::MIN as i64));
    assert_eq!(fold_int_bin(BinOp::Mul, Type::I8, 100, 3), Some(44));
    assert_eq!(fold_int_bin(BinOp::SDiv, Type::I32, 7, 0), None);
    assert_eq!(fold_int_bin(BinOp::SDiv, Type::I32, i32::MIN as i64, -1), None);
    assert_eq!(fold_int_bin(BinOp::SRem, Type::I32, i32::MIN as i64, -1), None);
    assert_eq!(fold_int_bin(BinOp::SDiv, Type::I32, -7, 2), Some(-3));
    assert_eq!(fold_int_bin(BinOp::SRem, Type::I32, -7, 2), Some(-1));
    assert_eq!(fold_int_bin(BinOp::UDiv, Type::I32, -1, 2), Some(0x7fff_ffff));
    assert_eq!(fold_int_bin(BinOp::URem, Type::I8, -1, 10), Some(5));
    assert_eq!(fold_int_bin(BinOp::Shl, Type::I32, 1, 31), Some(i32::MIN as i64));
    assert_eq!(fold_int_bin(BinOp::Shl, Type::I32, 1, 32), None);
    assert_eq!(fold_int_bin(BinOp::LShr, Type::I32, -1, 28), Some(15));
    assert_eq!(fold_int_bin(BinOp::AShr, Type::I32, -16, 2), Some(-4));
    assert_eq!(fold_int_bin(BinOp::Xor, Type::I64, -1, 0xff), Some(!0xffi64));
}

#[test]
fn comparison_folding_respects_signedness_and_width() {
    assert!(fold_icmp(IPred::Slt, Type::I32, -1, 0));
    assert!(!fold_icmp(IPred::Ult, Type::I32, -1, 0));
    assert!(fold_icmp(IPred::Ugt, Type::I8, -1, 1));
    assert!(fold_icmp(IPred::Sge, Type::I64, 5, 5));
}

// ───────────────────────────── mem2reg ─────────────────────────────

#[test]
fn mem2reg_promotes_scalars() {
    let m = run("int f(int a) { int b = a + 1; return b; }", &["mem2reg"]);
    let t = text(&m, "f");
    assert!(!t.contains("alloca") && !t.contains("load") && !t.contains("store"), "{t}");
    assert!(t.contains("add i32"), "{t}");
}

#[test]
fn mem2reg_keeps_address_taken_slots() {
    let m = run("int g(int *p); int f(void) { int x = 1; g(&x); return x; }", &["mem2reg"]);
    let t = text(&m, "f");
    assert!(t.contains("alloca") && t.contains("load i32"), "{t}");
}

#[test]
fn mem2reg_places_phis_at_joins() {
    let m = run("int f(int c) { int x; if (c) x = 1; else x = 2; return x; }", &["mem2reg"]);
    let t = text(&m, "f");
    assert!(t.contains("phi i32") && !t.contains("alloca"), "{t}");
}

#[test]
fn mem2reg_builds_loop_phis() {
    let m = run("int f(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }", &["mem2reg"]);
    let t = text(&m, "f");
    assert!(count(&t, "phi i32") >= 2, "{t}");
    assert!(!t.contains("alloca"), "{t}");
}

#[test]
fn mem2reg_leaves_arrays_and_structs_alone() {
    let m = run("int f(void) { int a[4]; a[0] = 1; a[1] = 2; return a[0] + a[1]; }", &["mem2reg"]);
    assert!(text(&m, "f").contains("alloca"));
    let m =
        run("struct S { int a, b; }; int f(void) { struct S s; s.a = 1; s.b = 2; return s.a + s.b; }", &["mem2reg"]);
    assert!(text(&m, "f").contains("alloca"));
}

#[test]
fn mem2reg_reads_of_uninitialized_slots_are_undef() {
    let m = run("int f(void) { int x; return x; }", &["mem2reg"]);
    let t = text(&m, "f");
    assert!(t.contains("ret i32 undef"), "{t}");
}

#[test]
fn mem2reg_does_not_promote_volatile() {
    let m = run("int f(void) { volatile int x = 3; return x; }", &["mem2reg"]);
    assert!(text(&m, "f").contains("alloca"));
}

// ───────────────────────────── sccp ─────────────────────────────

#[test]
fn sccp_folds_arithmetic_chains() {
    let m = run("int f(void) { int a = 2; int b = a * 3 + 4; return b; }", &["mem2reg", "sccp"]);
    assert!(text(&m, "f").contains("ret i32 10"), "{}", text(&m, "f"));
}

#[test]
fn sccp_prunes_constant_branches() {
    let m = run("int f(int x) { int c = 1; if (c) return 5; return x; }", &["mem2reg", "sccp"]);
    let t = text(&m, "f");
    assert!(!t.contains("condbr"), "{t}");
    assert!(t.contains("ret i32 5"), "{t}");
}

#[test]
fn sccp_does_not_fold_division_by_zero() {
    let m = run("int f(void) { int z = 0; return 1 / z; }", &["mem2reg", "sccp"]);
    assert!(text(&m, "f").contains("sdiv"), "{}", text(&m, "f"));
}

#[test]
fn sccp_merges_equal_phi_inputs() {
    let m = run("int f(int c) { int x; if (c) x = 7; else x = 7; return x; }", &["mem2reg", "sccp"]);
    assert!(text(&m, "f").contains("ret i32 7"), "{}", text(&m, "f"));
}

#[test]
fn sccp_propagates_through_loops() {
    let m = run(
        "int f(int n) { int k = 3; int s = 0; for (int i = 0; i < n; i++) s += k; return s; }",
        &["mem2reg", "sccp"],
    );
    let t = text(&m, "f");
    assert!(t.contains(", 3"), "constant not propagated into the loop: {t}");
}

#[test]
fn sccp_ignores_dead_loop_back_edges() {
    // `x` stays 1 because the branch that changes it is never taken
    let m = run(
        "int f(int n) { int x = 1; for (int i = 0; i < n; i++) { if (x != 1) x = i; } return x; }",
        &["mem2reg", "sccp"],
    );
    assert!(text(&m, "f").contains("ret i32 1"), "{}", text(&m, "f"));
}

#[test]
fn sccp_folds_floating_point() {
    let m = run("double f(void) { double a = 1.5; return a * 2.0; }", &["mem2reg", "sccp"]);
    assert!(text(&m, "f").contains("ret f64 3.0"), "{}", text(&m, "f"));
}

#[test]
fn sccp_wraps_like_the_hardware() {
    let m =
        run("int f(void) { int a = 2147483647; unsigned b = (unsigned)a + 1u; return (int)b; }", &["mem2reg", "sccp"]);
    assert!(text(&m, "f").contains("ret i32 -2147483648"), "{}", text(&m, "f"));
}

#[test]
fn sccp_int_to_float_rounds_once() {
    // 2^53 + 1 is not representable as a double; the nearest-even result is 2^53
    let m = run("double f(void) { long long x = 9007199254740993LL; return (double)x; }", &["mem2reg", "sccp"]);
    assert!(text(&m, "f").contains("ret f64 9007199254740992.0"), "{}", text(&m, "f"));
}

#[test]
fn sccp_leaves_out_of_range_float_to_int_conversions() {
    let m = run("int f(void) { double d = 1e30; return (int)d; }", &["mem2reg", "sccp"]);
    assert!(text(&m, "f").contains("fptosi"), "{}", text(&m, "f"));
}

// ───────────────────────────── strength ─────────────────────────────

#[test]
fn strength_multiplication_by_power_of_two() {
    let m = run("int f(int a) { return a * 8; }", &["mem2reg", "strength"]);
    let t = text(&m, "f");
    assert!(t.contains("shl i32") && !t.contains("mul i32"), "{t}");
}

#[test]
fn strength_unsigned_division_and_remainder() {
    let m = run("unsigned f(unsigned a) { return a / 16 + a % 16; }", &["mem2reg", "strength"]);
    let t = text(&m, "f");
    assert!(t.contains("lshr") && t.contains("and i32"), "{t}");
    assert!(!t.contains("udiv") && !t.contains("urem"), "{t}");
}

#[test]
fn strength_signed_division_rounds_toward_zero() {
    let m = run("int f(int a) { return a / 4; }", &["mem2reg", "strength"]);
    let t = text(&m, "f");
    assert!(!t.contains("sdiv") && t.contains("ashr"), "{t}");
    let m = run("int f(int a) { return a % 8; }", &["mem2reg", "strength"]);
    assert!(!text(&m, "f").contains("srem"), "{}", text(&m, "f"));
}

#[test]
fn strength_algebraic_identities() {
    let m = run("int f(int a) { return ((a + 0) * 1 - 0) ^ 0; }", &["mem2reg", "strength"]);
    assert!(text(&m, "f").contains("ret i32 %a"), "{}", text(&m, "f"));
    let m = run("int f(int a) { return a - a; }", &["mem2reg", "strength"]);
    assert!(text(&m, "f").contains("ret i32 0"), "{}", text(&m, "f"));
    let m = run("int f(int a) { return a & 0; }", &["mem2reg", "strength"]);
    assert!(text(&m, "f").contains("ret i32 0"), "{}", text(&m, "f"));
}

#[test]
fn strength_canonicalizes_subtraction_of_constants() {
    let m = run("int f(int a) { return a - 5; }", &["mem2reg", "strength"]);
    assert!(text(&m, "f").contains("add i32 %a.0, -5") || text(&m, "f").contains(", -5"), "{}", text(&m, "f"));
}

#[test]
fn strength_reassociates_constant_additions() {
    let m = run("int f(int a) { return (a + 3) + 4; }", &["mem2reg", "strength", "dce"]);
    let t = text(&m, "f");
    assert_eq!(count(&t, "add i32"), 1, "{t}");
    assert!(t.contains(", 7"), "{t}");
}

#[test]
fn strength_simplifies_comparison_of_comparison() {
    let m = run("int f(int a, int b) { if (a < b) return 1; return 0; }", &["mem2reg", "strength", "dce"]);
    let t = text(&m, "f");
    assert!(!t.contains("icmp ne"), "{t}");
}

#[test]
fn strength_cast_chains() {
    let m = run("int f(int a) { return (int)(long long)a; }", &["mem2reg", "strength", "dce"]);
    let t = text(&m, "f");
    assert!(!t.contains("sext") && !t.contains("trunc"), "{t}");
}

#[test]
fn strength_unsigned_compare_with_zero() {
    let m = run("int f(unsigned a) { return a >= 0u; }", &["mem2reg", "strength"]);
    assert!(text(&m, "f").contains("ret i32 1"), "{}", text(&m, "f"));
}

// ───────────────────────────── cse ─────────────────────────────

#[test]
fn cse_merges_identical_expressions() {
    let m = run("int f(int a, int b) { return (a * b) + (a * b); }", &["mem2reg", "cse"]);
    assert_eq!(count(&text(&m, "f"), "mul i32"), 1, "{}", text(&m, "f"));
}

#[test]
fn cse_treats_commutative_operands_alike() {
    let m = run("int f(int a, int b) { return (a + b) * (b + a); }", &["mem2reg", "cse"]);
    assert_eq!(count(&text(&m, "f"), "add i32"), 1, "{}", text(&m, "f"));
}

#[test]
fn cse_works_across_dominated_blocks() {
    let m =
        run("int f(int a, int b, int c) { int x = a * b; if (c) return a * b + x; return x; }", &["mem2reg", "cse"]);
    assert_eq!(count(&text(&m, "f"), "mul i32"), 1, "{}", text(&m, "f"));
}

#[test]
fn cse_does_not_merge_across_sibling_branches() {
    let m = run("int f(int a, int b, int c) { if (c) return a * b; return a * b + 1; }", &["mem2reg", "cse"]);
    assert_eq!(count(&text(&m, "f"), "mul i32"), 2, "{}", text(&m, "f"));
}

#[test]
fn cse_forwards_loads() {
    let m = run("int f(int *p) { int a = *p; int b = *p; return a + b; }", &["mem2reg", "cse"]);
    assert_eq!(count(&text(&m, "f"), "load i32"), 1, "{}", text(&m, "f"));
}

#[test]
fn cse_forwards_stores_to_loads() {
    let m = run("int f(int *p) { *p = 5; return *p; }", &["mem2reg", "cse"]);
    let t = text(&m, "f");
    assert!(!t.contains("load"), "{t}");
}

#[test]
fn cse_reloads_after_a_possibly_aliasing_store() {
    let m = run("int f(int *p, int *q) { int a = *p; *q = 5; int b = *p; return a + b; }", &["mem2reg", "cse"]);
    assert_eq!(count(&text(&m, "f"), "load i32"), 2, "{}", text(&m, "f"));
}

#[test]
fn cse_keeps_loads_across_stores_to_distinct_objects() {
    let m =
        run("int a[4], b[4]; int f(void) { int x = a[1]; b[2] = 9; int y = a[1]; return x + y; }", &["mem2reg", "cse"]);
    assert_eq!(count(&text(&m, "f"), "load i32"), 1, "{}", text(&m, "f"));
}

#[test]
fn cse_reloads_after_calls() {
    let m = run("void g(void); int f(int *p) { int a = *p; g(); int b = *p; return a + b; }", &["mem2reg", "cse"]);
    assert_eq!(count(&text(&m, "f"), "load i32"), 2, "{}", text(&m, "f"));
}

#[test]
fn cse_keeps_private_slots_across_calls() {
    let m = run("void g(void); int f(void) { int a[2]; a[0] = 4; g(); return a[0]; }", &["mem2reg", "cse"]);
    assert!(!text(&m, "f").contains("load"), "{}", text(&m, "f"));
}

#[test]
fn cse_never_merges_volatile_loads() {
    let m = run("int f(volatile int *p) { int a = *p; int b = *p; return a + b; }", &["mem2reg", "cse"]);
    assert_eq!(count(&text(&m, "f"), "load volatile"), 2, "{}", text(&m, "f"));
}

// ───────────────────────────── dce ─────────────────────────────

#[test]
fn dce_removes_unused_computation() {
    let m = run("int f(int a) { int unused = a * 7 + 3; return a; }", &["mem2reg", "dce"]);
    let t = text(&m, "f");
    assert!(!t.contains("mul") && !t.contains("add"), "{t}");
}

#[test]
fn dce_removes_write_only_locals() {
    let m = run("int f(int a) { int buf[4]; buf[0] = a; buf[1] = 2; return a; }", &["mem2reg", "dce"]);
    let t = text(&m, "f");
    assert!(!t.contains("alloca") && !t.contains("store"), "{t}");
}

#[test]
fn dce_keeps_calls_and_volatile_accesses() {
    let m = run("int g(int); int f(int a) { g(a); return 0; }", &["mem2reg", "dce"]);
    assert!(text(&m, "f").contains("call"), "{}", text(&m, "f"));
    let m = run("int f(volatile int *p) { *p; return 0; }", &["mem2reg", "dce"]);
    assert!(text(&m, "f").contains("load volatile"), "{}", text(&m, "f"));
}

#[test]
fn dce_removes_dead_phi_cycles() {
    let m = run("int f(int n) { int dead = 0; for (int i = 0; i < n; i++) dead += i; return n; }", &["mem2reg", "dce"]);
    let t = text(&m, "f");
    // only the loop counter's phi survives
    assert_eq!(count(&t, "phi i32"), 1, "{t}");
}

#[test]
fn dce_drops_overwritten_stores() {
    let m = run("void f(int *p) { *p = 1; *p = 2; }", &["mem2reg", "dce"]);
    assert_eq!(count(&text(&m, "f"), "store"), 1, "{}", text(&m, "f"));
}

#[test]
fn dce_keeps_stores_that_are_read_in_between() {
    let m = run("int f(int *p) { *p = 1; int a = *p; *p = 2; return a; }", &["mem2reg", "dce"]);
    assert_eq!(count(&text(&m, "f"), "store"), 2, "{}", text(&m, "f"));
}

// ───────────────────────────── copyprop / simplifycfg ─────────────────────────────

#[test]
fn copyprop_removes_trivial_phis() {
    let m = run("int f(int a, int c) { int x = a; if (c) x = a; return x; }", &["mem2reg", "copyprop"]);
    assert!(!text(&m, "f").contains("phi"), "{}", text(&m, "f"));
}

#[test]
fn simplifycfg_merges_straight_line_blocks() {
    let m = run("int f(int a) { { { a = a + 1; } } return a; }", &["mem2reg", "simplifycfg"]);
    assert_eq!(func(&m, "f").blocks.len(), 1, "{}", text(&m, "f"));
}

#[test]
fn simplifycfg_folds_constant_conditions() {
    let m = run("int f(void) { if (0) return 1; return 2; }", &["simplifycfg"]);
    let t = text(&m, "f");
    assert!(!t.contains("condbr") && !t.contains("ret i32 1"), "{t}");
}

#[test]
fn simplifycfg_threads_empty_blocks() {
    let m = run("int f(int a, int b) { if (a) { if (b) { } } return a; }", &["mem2reg", "simplifycfg"]);
    // no block consists of just a jump
    let f = func(&m, "f");
    for b in f.blocks.iter().skip(1) {
        assert!(!(b.insts.is_empty() && matches!(b.term, Term::Br(_))), "{}", text(&m, "f"));
    }
}

#[test]
fn simplifycfg_keeps_the_entry_first() {
    let m = run(
        "int f(int n) { int s = 0; int i = 0; while (i < n) { if (i % 3 == 0) s += i; else s -= 1; i++; } return s; }",
        &["mem2reg", "simplifycfg"],
    );
    let f = func(&m, "f");
    assert_eq!(f.blocks[0].name, "entry");
}

#[test]
fn simplifycfg_threads_small_return_blocks() {
    let m = run("int f(int c, int a, int b) { return c ? a : b; }", &["mem2reg", "simplifycfg"]);
    let t = text(&m, "f");
    assert!(!t.contains("phi"), "{t}");
    assert_eq!(count(&t, "ret i32"), 2, "{t}");
}

// ───────────────────────────── licm ─────────────────────────────

#[test]
fn licm_hoists_invariant_arithmetic() {
    let m = run(
        "int f(int a, int b, int n) { int s = 0; for (int i = 0; i < n; i++) s += a * b; return s; }",
        &["mem2reg", "licm"],
    );
    let f = func(&m, "f");
    assert!(has_op(f, |k| matches!(k, InstKind::Bin { op: BinOp::Mul, .. })), "{}", text(&m, "f"));
    assert!(!in_loop(f, |k| matches!(k, InstKind::Bin { op: BinOp::Mul, .. })), "{}", text(&m, "f"));
}

#[test]
fn licm_keeps_variant_computation_in_the_loop() {
    let m = run(
        "int f(int a, int n) { int s = 0; for (int i = 0; i < n; i++) s += a * i; return s; }",
        &["mem2reg", "licm"],
    );
    assert!(in_loop(func(&m, "f"), |k| matches!(k, InstKind::Bin { op: BinOp::Mul, .. })));
}

#[test]
fn licm_does_not_speculate_divisions() {
    let m = run(
        "int f(int a, int b, int n) { int s = 0; for (int i = 0; i < n; i++) if (b) s += a / b; return s; }",
        &["mem2reg", "licm"],
    );
    assert!(in_loop(func(&m, "f"), |k| matches!(k, InstKind::Bin { op: BinOp::SDiv, .. })), "{}", text(&m, "f"));
}

#[test]
fn licm_hoists_divisions_by_safe_constants() {
    let m = run(
        "int f(int a, int n) { int s = 0; for (int i = 0; i < n; i++) s += a / 3; return s; }",
        &["mem2reg", "licm"],
    );
    assert!(!in_loop(func(&m, "f"), |k| matches!(k, InstKind::Bin { op: BinOp::SDiv, .. })), "{}", text(&m, "f"));
}

#[test]
fn licm_hoists_global_loads_when_nothing_writes_them() {
    let m =
        run("int k; int f(int n) { int s = 0; for (int i = 0; i < n; i++) s += k; return s; }", &["mem2reg", "licm"]);
    assert!(!in_loop(func(&m, "f"), |k| matches!(k, InstKind::Load { .. })), "{}", text(&m, "f"));
}

#[test]
fn licm_keeps_loads_that_the_loop_overwrites() {
    let m = run(
        "int k; int f(int n) { int s = 0; for (int i = 0; i < n; i++) { s += k; k = i; } return s; }",
        &["mem2reg", "licm"],
    );
    assert!(in_loop(func(&m, "f"), |k| matches!(k, InstKind::Load { .. })), "{}", text(&m, "f"));
}

#[test]
fn licm_keeps_loads_when_the_loop_calls_out() {
    let m = run(
        "int k; void g(void); int f(int n) { int s = 0; for (int i = 0; i < n; i++) { s += k; g(); } return s; }",
        &["mem2reg", "licm"],
    );
    assert!(in_loop(func(&m, "f"), |k| matches!(k, InstKind::Load { .. })), "{}", text(&m, "f"));
}

#[test]
fn licm_does_not_speculate_loads_through_pointers() {
    // *p might fault when n == 0, so it must stay behind the loop test
    let m = run(
        "int f(int *p, int n) { int s = 0; for (int i = 0; i < n; i++) if (i > 5) s += *p; return s; }",
        &["mem2reg", "licm"],
    );
    assert!(in_loop(func(&m, "f"), |k| matches!(k, InstKind::Load { .. })), "{}", text(&m, "f"));
}

#[test]
fn licm_handles_nested_loops() {
    let m = run(
        "int f(int a, int b, int n) { int s = 0; for (int i = 0; i < n; i++) for (int j = 0; j < n; j++) s += a * b; return s; }",
        &["mem2reg", "licm"],
    );
    assert!(!in_loop(func(&m, "f"), |k| matches!(k, InstKind::Bin { op: BinOp::Mul, .. })), "{}", text(&m, "f"));
}

// ───────────────────────────── inline ─────────────────────────────

#[test]
fn inline_replaces_small_calls_and_drops_the_static_callee() {
    let m = run("static int sq(int x) { return x * x; } int f(int a) { return sq(a) + 1; }", &["mem2reg", "inline"]);
    assert!(!text(&m, "f").contains("call"), "{}", text(&m, "f"));
    assert!(m.funcs.iter().all(|f| f.name.as_str() != "sq"), "static callee should be removed");
}

#[test]
fn inline_keeps_external_callees() {
    let m = run("int sq(int x) { return x * x; } int f(int a) { return sq(a) + 1; }", &["mem2reg", "inline"]);
    assert!(!text(&m, "f").contains("call"));
    assert!(m.funcs.iter().any(|f| f.name.as_str() == "sq"));
}

#[test]
fn inline_skips_recursion() {
    let m = run(
        "int fact(int n) { return n <= 1 ? 1 : n * fact(n - 1); } int f(int a) { return fact(a); }",
        &["mem2reg", "inline"],
    );
    assert!(text(&m, "f").contains("call"), "{}", text(&m, "f"));
}

#[test]
fn inline_skips_variadic_callees() {
    let m = run(
        "#include <stdarg.h>\nint sum(int n, ...) { va_list ap; va_start(ap, n); int s = 0; while (n--) s += va_arg(ap, int); va_end(ap); return s; }\nint f(void) { return sum(2, 3, 4); }",
        &["mem2reg", "inline"],
    );
    assert!(text(&m, "f").contains("call"), "{}", text(&m, "f"));
}

#[test]
fn inline_merges_multiple_returns_with_a_phi() {
    let m = run(
        "static int pick(int c, int a, int b) { if (c) return a; return b; } int f(int c) { return pick(c, 10, 20); }",
        &["mem2reg", "inline"],
    );
    let t = text(&m, "f");
    assert!(!t.contains("call"), "{t}");
    assert!(t.contains("phi i32"), "{t}");
}

#[test]
fn inline_moves_callee_slots_into_the_entry_block() {
    let m = run(
        "static int sum3(int a, int b, int c) { int t[3]; t[0] = a; t[1] = b; t[2] = c; return t[0] + t[1] + t[2]; } int f(int n) { int s = 0; for (int i = 0; i < n; i++) s += sum3(i, 1, 2); return s; }",
        &["mem2reg", "inline"],
    );
    let f = func(&m, "f");
    // verifier already insists allocas live in the entry block; make sure they were copied
    assert!(f.blocks[0].insts.iter().any(|&i| matches!(f.insts[i.idx()].kind, InstKind::Alloca { .. })));
    assert!(!text(&m, "f").contains("call"));
}

#[test]
fn inline_copies_byvalue_aggregates() {
    let m = run(
        "struct S { int a[8]; }; static int first(struct S s) { s.a[0] += 1; return s.a[0]; } int f(struct S *p) { int r = first(*p); return r + p->a[0]; }",
        &["mem2reg", "inline"],
    );
    let t = text(&m, "f");
    assert!(!t.contains("call"), "{t}");
    assert!(t.contains("memcpy"), "{t}");
}

#[test]
fn inline_handles_noreturn_style_callees() {
    // the callee never returns; the continuation becomes dead
    let m = run(
        "void abort(void); static void die(void) { abort(); for (;;) {} } int f(int c) { if (c) die(); return 1; }",
        &["mem2reg", "inline"],
    );
    assert!(!text(&m, "f").contains("@die"), "{}", text(&m, "f"));
}

// ───────────────────────────── tailcall ─────────────────────────────

#[test]
fn tailcall_turns_self_recursion_into_a_loop() {
    let m = run(
        "int sum(int n, int acc) { if (n == 0) return acc; return sum(n - 1, acc + n); }",
        &["mem2reg", "tailcall"],
    );
    let f = func(&m, "sum");
    assert!(!has_op(f, |k| matches!(k, InstKind::Call { .. })), "{}", text(&m, "sum"));
    assert!(has_op(f, |k| matches!(k, InstKind::Phi { .. })), "{}", text(&m, "sum"));
}

#[test]
fn tailcall_ignores_non_tail_recursion() {
    let m = run("int fib(int n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }", &["mem2reg", "tailcall"]);
    assert!(has_op(func(&m, "fib"), |k| matches!(k, InstKind::Call { .. })));
}

#[test]
fn tailcall_marks_sibling_calls() {
    let m = run("int g(int); int f(int x) { return g(x + 1); }", &["mem2reg", "tailcall"]);
    assert!(has_op(func(&m, "f"), |k| matches!(k, InstKind::Call { tail: true, .. })), "{}", text(&m, "f"));
}

#[test]
fn tailcall_requires_matching_results() {
    // the result is used after the call, so it is not a tail call
    let m = run("int g(int); int f(int x) { return g(x) + 1; }", &["mem2reg", "tailcall"]);
    assert!(!has_op(func(&m, "f"), |k| matches!(k, InstKind::Call { tail: true, .. })));
}

#[test]
fn tailcall_not_with_stack_arguments() {
    let m = run(
        "int g(int, int, int, int, int, int, int, int); int f(int x) { return g(x, 1, 2, 3, 4, 5, 6, 7); }",
        &["mem2reg", "tailcall"],
    );
    assert!(!has_op(func(&m, "f"), |k| matches!(k, InstKind::Call { tail: true, .. })));
}

#[test]
fn tailcall_not_when_a_local_address_escapes() {
    let m = run("int g(int *); int f(int x) { int y = x; return g(&y); }", &["mem2reg", "tailcall"]);
    assert!(!has_op(func(&m, "f"), |k| matches!(k, InstKind::Call { tail: true, .. })));
}

#[test]
fn tailcall_not_for_variadic_callees() {
    let m = run("int printf(const char *, ...); int f(int x) { return printf(\"%d\", x); }", &["mem2reg", "tailcall"]);
    assert!(!has_op(func(&m, "f"), |k| matches!(k, InstKind::Call { tail: true, .. })));
}

#[test]
fn tailcall_through_a_conditional_expression() {
    let m = run("int g(int); int f(int c, int x) { return c ? 0 : g(x); }", &["mem2reg", "simplifycfg", "tailcall"]);
    assert!(has_op(func(&m, "f"), |k| matches!(k, InstKind::Call { tail: true, .. })), "{}", text(&m, "f"));
}

#[test]
fn tailcall_self_recursion_through_a_conditional_expression() {
    let m = run(
        "int sum(int n, int acc) { return n == 0 ? acc : sum(n - 1, acc + n); }",
        &["mem2reg", "simplifycfg", "tailcall"],
    );
    assert!(!has_op(func(&m, "sum"), |k| matches!(k, InstKind::Call { .. })), "{}", text(&m, "sum"));
}

#[test]
fn tailcall_void_functions() {
    let m = run("void g(int); void f(int x) { g(x); }", &["mem2reg", "tailcall"]);
    assert!(has_op(func(&m, "f"), |k| matches!(k, InstKind::Call { tail: true, .. })), "{}", text(&m, "f"));
}

// ───────────────────────────── whole pipeline ─────────────────────────────

const KERNEL: &str = r#"
static int gcd(int a, int b) { while (b) { int t = a % b; a = b; b = t; } return a; }
static int tri(int n, int acc) { if (n == 0) return acc; return tri(n - 1, acc + n); }
int table[16];
int mix(int n) {
    int s = 0;
    for (int i = 0; i < n; i++) {
        table[i & 15] += i * 3;
        s += gcd(i + 1, 12) + tri(i & 7, 0) + table[(i + 1) & 15] / 4 + (i % 5) * 8;
    }
    return s;
}
"#;

#[test]
fn full_pipeline_output_verifies_at_every_level() {
    for level in 0..=2 {
        let m = at(KERNEL, level);
        assert!(m.funcs.iter().any(|f| f.name.as_str() == "mix"));
    }
}

#[test]
fn o2_removes_slots_and_calls_in_the_kernel() {
    let m = at(KERNEL, 2);
    let t = text(&m, "mix");
    assert!(!t.contains("alloca"), "{t}");
    assert!(!t.contains("@gcd") && !t.contains("@tri"), "{t}");
    assert!(m.funcs.iter().all(|f| f.name.as_str() != "gcd" && f.name.as_str() != "tri"));
}

#[test]
fn optimizing_twice_changes_nothing_further() {
    let mut m = lower(KERNEL);
    let cfg = OptConfig::new(2, &[]).unwrap().with_verify(true);
    optimize(&mut m, &cfg);
    let once = print_module(&m);
    optimize(&mut m, &cfg);
    let twice = print_module(&m);
    assert_eq!(once, twice);
}

// ───────────────────────────── variable length arrays ─────────────────────────────

#[test]
fn inline_skips_callees_that_allocate_stack_dynamically() {
    let m = run(
        "static int sum(int n) { int a[n]; for (int i = 0; i < n; i++) a[i] = i; return a[n - 1]; } int f(int k) { return sum(k) + 1; }",
        &["mem2reg", "inline"],
    );
    assert!(text(&m, "f").contains("call"), "{}", text(&m, "f"));
}

#[test]
fn tailcall_is_not_formed_in_functions_with_vlas() {
    let m = run("int g(int); int f(int n) { int a[n]; a[0] = n; return g(a[0]); }", &["mem2reg", "tailcall"]);
    assert!(!has_op(func(&m, "f"), |k| matches!(k, InstKind::Call { tail: true, .. })), "{}", text(&m, "f"));
}

#[test]
fn dynamic_allocation_survives_every_pass() {
    let m = at("int f(int n) { int a[n]; a[0] = 1; return n; }", 2);
    // the allocation has an observable effect on the stack pointer, so it is not dead code
    assert!(has_op(func(&m, "f"), |k| matches!(k, InstKind::DynAlloca { .. })), "{}", text(&m, "f"));
}
