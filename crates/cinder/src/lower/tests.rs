use super::*;
use crate::diag::Level;
use crate::ir::print::print_module;
use crate::ir::verify::verify_module;
use crate::parse;
use crate::pp::{self, PpOptions};
use crate::sema;

fn build(src: &str) -> (Module, Vec<String>) {
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
    let hir = hir.unwrap();
    let m = lower_module(&mut sess, &hir, "t.c");
    let warnings: Vec<String> =
        sess.diags.diagnostics().iter().filter(|d| d.level == Level::Warning).map(|d| d.message.clone()).collect();
    (m, warnings)
}

/// Lower, verify, and return the IR text.
fn ir(src: &str) -> String {
    let (m, _) = build(src);
    if let Err(e) = verify_module(&m) {
        panic!("verifier failed for {src:?}:\n{}\n{}", e.join("\n"), print_module(&m));
    }
    print_module(&m)
}

fn func<'a>(m: &'a Module, name: &str) -> &'a Func {
    m.funcs.iter().find(|f| f.name.as_str() == name).unwrap_or_else(|| panic!("no function {name}"))
}

// ───────────────────────────── basics ─────────────────────────────

#[test]
fn minimal_function() {
    let t = ir("int main(void) { return 0; }");
    assert!(t.contains("define i32 @main()"), "{t}");
    assert!(t.contains("ret i32 0"), "{t}");
}

#[test]
fn locals_are_allocas() {
    let t = ir("int f(int a) { int b = a + 1; return b * 2; }");
    assert!(t.contains("alloca 4, align 4"), "{t}");
    assert!(t.contains("store i32"), "{t}");
    assert!(t.contains("load i32"), "{t}");
    assert!(t.contains("add i32"), "{t}");
    assert!(t.contains("mul i32"), "{t}");
}

#[test]
fn arithmetic_signedness() {
    let t = ir("int f(int a, int b, unsigned c, unsigned d) { return a / b + a % b + (int)(c / d) + (int)(c % d) + (a >> 1) + (int)(c >> 1); }");
    for op in ["sdiv", "srem", "udiv", "urem", "ashr", "lshr"] {
        assert!(t.contains(op), "missing {op}: {t}");
    }
}

#[test]
fn floating_point_ops_and_conversions() {
    let t = ir("double f(double a, float b, int i) { return a * b + i - a / 2.0; }");
    for op in ["fmul", "fadd", "fsub", "fdiv", "fpext", "sitofp"] {
        assert!(t.contains(op), "missing {op}: {t}");
    }
    let t = ir("int g(double d) { return (int)d; } unsigned h(double d) { return (unsigned)d; } float k(double d) { return (float)d; }");
    assert!(t.contains("fptosi"), "{t}");
    assert!(t.contains("fptoui"), "{t}");
    assert!(t.contains("fptrunc"), "{t}");
}

#[test]
fn integer_conversions() {
    let t = ir("long f(int a) { return a; } unsigned long g(unsigned a) { return a; } char h(int a) { return (char)a; } long k(unsigned char c) { return c; }");
    assert!(t.contains("sext i32"), "{t}");
    assert!(t.contains("zext i32"), "{t}");
    assert!(t.contains("trunc i32"), "{t}");
    assert!(t.contains("zext i8"), "{t}");
}

#[test]
fn comparisons() {
    let t = ir("int f(int a, int b, unsigned c, unsigned d, double x, double y, int *p, int *q) { return (a < b) + (c < d) + (x < y) + (x == y) + (x != y) + (p == q) + (p < q); }");
    for p in ["icmp slt", "icmp ult", "fcmp olt", "fcmp oeq", "fcmp une", "icmp eq ptr", "icmp ult ptr"] {
        assert!(t.contains(p), "missing {p}: {t}");
    }
}

#[test]
fn bool_conversion() {
    let t = ir("_Bool f(int x) { return x; } _Bool g(double d) { return d; } _Bool h(int *p) { return p; }");
    assert!(t.contains("icmp ne i32"), "{t}");
    assert!(t.contains("fcmp une f64"), "{t}");
    assert!(t.contains("icmp ne ptr"), "{t}");
    assert!(t.contains("trunc i32"), "{t}");
}

#[test]
fn pointer_arithmetic_is_scaled() {
    let t = ir("int f(int *p, long i) { return *(p + i); } long g(int *p, int *q) { return p - q; }");
    assert!(t.contains("mul i64"), "{t}");
    assert!(t.contains("ptradd"), "{t}");
    assert!(t.contains("sdiv i64"), "{t}");
    // constant index folds into the offset
    let t = ir("int f(int *p) { return p[3]; }");
    assert!(t.contains("ptradd %"), "{t}");
    assert!(t.contains(", 12"), "{t}");
}

#[test]
fn struct_member_offsets() {
    let t = ir("struct S { char c; int i; double d; }; double f(struct S *s) { return s->d; } int g(struct S *s) { return s->i; }");
    assert!(t.contains(", 8"), "{t}");
    assert!(t.contains(", 4"), "{t}");
}

// ───────────────────────────── control flow ─────────────────────────────

#[test]
fn if_else_blocks() {
    let t = ir("int f(int x) { if (x > 0) return 1; else return -1; }");
    assert!(t.contains("if.then"), "{t}");
    assert!(t.contains("if.else"), "{t}");
    assert!(t.contains("condbr"), "{t}");
}

#[test]
fn loops() {
    let t = ir("int f(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; while (n--) s++; do s--; while (s > 100); return s; }");
    for b in ["for.cond", "for.body", "for.inc", "for.end", "while.cond", "while.body", "do.body", "do.cond"] {
        assert!(t.contains(b), "missing {b}: {t}");
    }
}

#[test]
fn infinite_loop_has_no_exit_edge() {
    let (m, w) = build("int f(void) { while (1) { } }");
    assert!(w.is_empty(), "no missing-return warning for an infinite loop: {w:?}");
    verify_module(&m).unwrap();
    let t = print_module(&m);
    assert!(!t.contains("while.end"), "{t}");
}

#[test]
fn short_circuit_evaluation() {
    let t = ir("int g(void); int f(int a) { return a && g(); }");
    assert!(t.contains("phi i32"), "{t}");
    let t = ir("int g(void); int f(int a) { if (a || g()) return 1; return 0; }");
    assert!(t.contains("lor.rhs"), "{t}");
    assert!(!t.contains("phi"), "branch context needs no phi: {t}");
}

#[test]
fn conditional_expression_uses_phi() {
    let t = ir("int f(int c, int a, int b) { return c ? a : b; }");
    assert!(t.contains("cond.true") && t.contains("cond.false") && t.contains("phi i32"), "{t}");
}

#[test]
fn switch_statement() {
    let t = ir("int f(int x) { switch (x) { case 1: return 10; case 2: case 3: return 20; default: return 0; } }");
    assert!(t.contains("switch i32"), "{t}");
    assert!(t.contains("1: sw.bb"), "{t}");
    assert!(t.contains("default sw.default"), "{t}");
}

#[test]
fn switch_fallthrough_and_break() {
    ir("int f(int x) { int r = 0; switch (x) { case 1: r += 1; case 2: r += 2; break; case 3: r = 9; } return r; }");
}

#[test]
fn goto_and_labels() {
    let t = ir("int f(int n) { int i = 0; top: if (i >= n) goto done; i++; goto top; done: return i; }");
    assert!(t.contains("label.top"), "{t}");
    assert!(t.contains("label.done"), "{t}");
}

#[test]
fn break_continue_targets() {
    ir("int f(int n) { int s = 0; for (int i = 0; i < n; i++) { if (i == 3) continue; if (i == 7) break; s += i; } return s; }");
    ir("int f(int n) { int s = 0; while (n) { switch (n) { case 1: break; default: continue; } s++; n--; } return s; }");
}

#[test]
fn dead_code_after_return_is_dropped() {
    let t = ir("int f(void) { return 1; int x = 5; x++; return x; }");
    assert_eq!(t.matches("ret ").count(), 1, "{t}");
}

// ───────────────────────────── warnings from lowering ─────────────────────────────

#[test]
fn missing_return_warning() {
    let (_, w) = build("int f(int x) { if (x) return 1; }");
    assert!(w.iter().any(|m| m.contains("does not return a value in all control paths")), "{w:?}");
    let (_, w) = build("int f(int x) { if (x) return 1; else return 2; }");
    assert!(w.is_empty(), "{w:?}");
    let (_, w) = build("void f(void) { }");
    assert!(w.is_empty(), "{w:?}");
    // main may fall off the end (C99: implicit return 0)
    let (m, w) = build("int main(void) { }");
    assert!(w.is_empty(), "{w:?}");
    assert!(print_module(&m).contains("ret i32 0"));
    // a switch whose cases all return still falls off if no default
    let (_, w) = build("int f(int x) { switch (x) { case 1: return 1; case 2: return 2; } }");
    assert!(!w.is_empty());
    let (_, w) = build("int f(int x) { switch (x) { case 1: return 1; default: return 2; } }");
    assert!(w.is_empty(), "{w:?}");
}

// ───────────────────────────── calls & ABI ─────────────────────────────

#[test]
fn direct_and_indirect_calls() {
    let t = ir("int g(int); int f(int (*fp)(int)) { return g(1) + fp(2); }");
    assert!(t.contains("call i32 @g(i32 1)"), "{t}");
    assert!(t.contains("call i32 %"), "{t}");
}

#[test]
fn variadic_calls_are_flagged() {
    let t = ir("int printf(const char *, ...); void f(void) { printf(\"%d %f\\n\", 1, 2.5); }");
    assert!(t.contains("call variadic i32 @printf("), "{t}");
    assert!(t.contains("@.L.str.1"), "{t}");
}

#[test]
fn small_int_args_are_widened() {
    let t = ir("void g(char, short, _Bool); void f(char c, short s) { g(c, s, 1); }");
    assert!(t.contains("sext i8"), "{t}");
    assert!(t.contains("sext i16"), "{t}");
    assert!(t.contains("call void @g(i32 %"), "{t}");
    // a constant is widened at compile time
    assert!(t.contains(", i32 1)"), "{t}");
    // callee side: narrow parameters arrive as i32 and are truncated into their slots
    let t = ir("char h(char c) { return c; }");
    assert!(t.contains("define i32 @h(i32 "), "{t}");
    assert!(t.contains("trunc i32"), "{t}");
}

#[test]
fn small_struct_params_are_passed_in_pieces() {
    let (m, _) = build("struct P { int x, y; }; struct Q { long a, b; }; struct F { double d; long l; }; int f(struct P p, struct Q q, struct F f) { return p.x + (int)q.b + (int)f.l; }");
    verify_module(&m).unwrap();
    let f = func(&m, "f");
    let tys: Vec<Type> = f
        .params
        .iter()
        .map(|p| match &p.kind {
            ParamKind::Value(t) => *t,
            _ => panic!("unexpected byval"),
        })
        .collect();
    // P -> one i64; Q -> two i64; F -> f64 + i64
    assert_eq!(tys, [Type::I64, Type::I64, Type::I64, Type::F64, Type::I64]);
    // pieces of one aggregate share a group
    assert_eq!(f.params[1].group, f.params[2].group);
    assert_ne!(f.params[0].group, f.params[1].group);
}

#[test]
fn large_struct_params_are_byval() {
    let (m, _) = build("struct Big { long a, b, c; }; long f(struct Big b) { return b.c; }");
    let f = func(&m, "f");
    assert_eq!(f.params[0].kind, ParamKind::ByVal { size: 24, align: 8 });
    verify_module(&m).unwrap();
}

#[test]
fn struct_returns() {
    let (m, _) = build("struct P { int x, y; }; struct P mk(int a) { struct P p = {a, a + 1}; return p; }");
    assert_eq!(func(&m, "mk").rets, [Type::I64]);
    let (m, _) = build("struct D { double a, b; }; struct D mk(void) { struct D d = {1.0, 2.0}; return d; }");
    assert_eq!(func(&m, "mk").rets, [Type::F64, Type::F64]);
    let (m, _) = build("struct Big { long a, b, c; }; struct Big mk(void) { struct Big b = {1, 2, 3}; return b; }");
    let f = func(&m, "mk");
    assert_eq!(f.rets, [Type::Ptr]);
    assert!(matches!(f.params[0].kind, ParamKind::Value(Type::Ptr)), "hidden sret parameter");
    verify_module(&m).unwrap();
}

#[test]
fn calling_functions_that_return_structs() {
    ir("struct P { int x, y; }; struct P mk(int); int f(void) { struct P p = mk(3); return p.x + mk(4).y; }");
    ir("struct Big { long a, b, c; }; struct Big mk(void); long f(void) { struct Big b = mk(); return b.c + mk().a; }");
    ir("struct D { double a, b; }; struct D mk(void); double f(void) { struct D d = mk(); return d.a + d.b; }");
}

#[test]
fn passing_structs_to_calls() {
    ir("struct P { int x, y; }; int g(struct P); int f(void) { struct P p = {1, 2}; return g(p); }");
    ir("struct Big { long a, b, c; }; int g(struct Big, int); int f(struct Big *b) { return g(*b, 1); }");
    ir("struct T { char c[3]; }; int g(struct T); int f(struct T t) { return g(t); }");
}

#[test]
fn struct_assignment_uses_memcpy() {
    let t = ir("struct S { int a; double b; char c[20]; }; void f(struct S *d, struct S *s) { *d = *s; }");
    assert!(t.contains("memcpy"), "{t}");
}

#[test]
fn varargs_function_definition() {
    let t = ir("#include <stdarg.h>\nint sum(int n, ...) { va_list ap; va_start(ap, n); int t = 0; while (n--) t += va_arg(ap, int); va_end(ap); return t; }");
    assert!(t.contains("va_reg_save_area"), "{t}");
    assert!(t.contains("va_stack_args"), "{t}");
    assert!(t.contains("va.reg") && t.contains("va.stack"), "{t}");
    let (m, _) = build("#include <stdarg.h>\ndouble f(int n, ...) { va_list ap; va_start(ap, n); double d = va_arg(ap, double); va_end(ap); return d; }");
    assert!(func(&m, "f").variadic);
}

#[test]
fn va_start_records_named_arguments() {
    // one named GPR argument: gp_offset = 8; no named SSE: fp_offset = 48
    let t = ir("#include <stdarg.h>\nvoid f(int n, ...) { va_list ap; va_start(ap, n); va_end(ap); }");
    assert!(t.contains("store i32 8,"), "{t}");
    assert!(t.contains("store i32 48,"), "{t}");
    let t =
        ir("#include <stdarg.h>\nvoid f(int a, double b, long c, ...) { va_list ap; va_start(ap, c); va_end(ap); }");
    assert!(t.contains("store i32 16,"), "{t}");
    assert!(t.contains("store i32 64,"), "{t}");
}

// ───────────────────────────── bit-fields ─────────────────────────────

#[test]
fn bitfield_access() {
    let t = ir("struct B { unsigned a : 3; int b : 5; unsigned c : 10; }; unsigned f(struct B *p) { p->b = -3; return p->a + p->c; }");
    assert!(t.contains("shl i32"), "{t}");
    assert!(t.contains("lshr i32"), "{t}");
    assert!(t.contains("and i32"), "{t}");
    assert!(t.contains("or i32"), "{t}");
    let t = ir("struct B { int b : 5; }; int g(struct B *p) { return p->b; }");
    assert!(t.contains("ashr i32"), "signed bit-field sign-extends: {t}");
}

#[test]
fn bitfield_compound_assign_and_incdec() {
    ir("struct B { unsigned a : 3; unsigned b : 4; }; void f(struct B *p) { p->a += 2; p->b++; --p->b; p->a |= 1; }");
}

// ───────────────────────────── initializers ─────────────────────────────

#[test]
fn local_initializers() {
    let t = ir("int f(int x) { int a[4] = {x, x + 1}; return a[1]; }");
    assert!(t.contains("memset"), "partial initializer zero-fills: {t}");
    let t = ir("int f(void) { int a[3] = {1, 2, 3}; return a[2]; }");
    assert!(!t.contains("memset"), "full initializer needs no zero-fill: {t}");
    let t = ir("int f(void) { char s[] = \"hello\"; return s[1]; }");
    assert!(t.contains("store i32") || t.contains("store i64"), "{t}");
    ir("struct P {int x, y;}; int f(int a) { struct P p = {.y = a}; return p.y; }");
}

#[test]
fn large_string_initializer_copies_from_rodata() {
    let t = ir("int f(void) { char s[] = \"a fairly long string that exceeds thirty-two bytes\"; return s[0]; }");
    assert!(t.contains("memcpy"), "{t}");
    assert!(t.contains("@.L.init"), "{t}");
}

#[test]
fn compound_literals() {
    ir("struct P {int x, y;}; int f(int a) { struct P *p = &(struct P){a, 2}; return p->x; }");
    ir("int f(int a) { int *p = (int[]){a, 2, 3}; return p[2]; }");
}

// ───────────────────────────── static data ─────────────────────────────

fn data<'a>(m: &'a Module, name: &str) -> &'a DataDef {
    let s = m.syms.iter().find(|s| s.name.as_str() == name).unwrap_or_else(|| panic!("no symbol {name}"));
    match &s.body {
        SymBody::Data(Some(d)) => d,
        other => panic!("{name} is not defined data: {other:?}"),
    }
}

#[test]
fn global_scalars_and_arrays() {
    let (m, _) = build("int x = 5; long y = -2; char c = 'A'; double d = 1.5; int a[3] = {1, 2, 3}; int z;");
    assert_eq!(data(&m, "x").items, [DataItem::Bytes(vec![5, 0, 0, 0])]);
    assert_eq!(data(&m, "y").items, [DataItem::Bytes(vec![0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])]);
    assert_eq!(data(&m, "c").items, [DataItem::Bytes(vec![65])]);
    assert_eq!(data(&m, "d").items, [DataItem::Bytes(1.5f64.to_le_bytes().to_vec())]);
    assert_eq!(data(&m, "a").items, [DataItem::Bytes(vec![1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0])]);
    let z = data(&m, "z");
    assert!(z.zero && z.items.is_empty() && z.size == 4);
}

#[test]
fn global_structs_with_padding_and_bitfields() {
    let (m, _) =
        build("struct S { char c; int i; } s = {1, 2}; struct B { unsigned a : 3, b : 5; int c; } b = {5, 17, 9};");
    assert_eq!(data(&m, "s").items, [DataItem::Bytes(vec![1, 0, 0, 0, 2, 0, 0, 0])]);
    // a = 5 (bits 0-2), b = 17 (bits 3-7): byte 0 = 5 | 17 << 3 = 141
    assert_eq!(data(&m, "b").items, [DataItem::Bytes(vec![141, 0, 0, 0, 9, 0, 0, 0])]);
}

#[test]
fn global_pointers_use_relocations() {
    let (m, _) = build("int x; int arr[4]; int *p = &x; int *q = &arr[2]; char *s = \"hi\"; int (*fp)(void); int f(void) { return 0; } int (*g)(void) = f;");
    let items = &data(&m, "q").items;
    assert!(matches!(items.as_slice(), [DataItem::Addr { offset: 8, .. }]), "{items:?}");
    let items = &data(&m, "p").items;
    assert!(matches!(items.as_slice(), [DataItem::Addr { offset: 0, .. }]));
    assert!(matches!(data(&m, "g").items.as_slice(), [DataItem::Addr { .. }]));
    assert!(data(&m, "fp").zero);
    let items = &data(&m, "s").items;
    let DataItem::Addr { sym, .. } = &items[0] else { panic!() };
    let target = &m.syms[sym.idx()];
    assert!(target.name.as_str().starts_with(".L.str"));
}

#[test]
fn string_literals_are_deduplicated_read_only_data() {
    let (m, _) = build("const char *a(void) { return \"same\"; } const char *b(void) { return \"same\"; } const char *c(void) { return \"other\"; }");
    let strs: Vec<&Sym> = m.syms.iter().filter(|s| s.name.as_str().starts_with(".L.str")).collect();
    assert_eq!(strs.len(), 2);
    for s in strs {
        let SymBody::Data(Some(d)) = &s.body else { panic!() };
        assert!(d.readonly);
        assert_eq!(s.linkage, Linkage::Internal);
    }
}

#[test]
fn wide_strings_use_four_byte_units() {
    let (m, _) = build("const int *w(void) { return L\"ab\"; }");
    let s = m.syms.iter().find(|s| s.name.as_str().starts_with(".L.str")).unwrap();
    let SymBody::Data(Some(d)) = &s.body else { panic!() };
    assert_eq!(d.size, 12);
    assert_eq!(d.align, 4);
}

#[test]
fn const_and_static_data_properties() {
    let (m, _) = build("const int k = 7; static int s = 3; int g = 1; int use(void) { return s; }");
    assert!(data(&m, "k").readonly);
    assert!(!data(&m, "g").readonly);
    let s = m.syms.iter().find(|s| s.name.as_str() == "s").unwrap();
    assert_eq!(s.linkage, Linkage::Internal);
}

#[test]
fn static_locals() {
    let (m, _) = build("int next(void) { static int n = 10; return n++; }");
    let s = m.syms.iter().find(|s| s.name.as_str().starts_with("next.n.")).expect("mangled static local");
    assert_eq!(s.linkage, Linkage::Internal);
    verify_module(&m).unwrap();
}

#[test]
fn long_zero_runs_become_zero_items() {
    let (m, _) = build("int big[100] = {1}; ");
    let d = data(&m, "big");
    assert_eq!(d.size, 400);
    assert!(d.items.iter().any(|i| matches!(i, DataItem::Zero(n) if *n >= 300)), "{:?}", d.items);
}

// ───────────────────────────── whole programs ─────────────────────────────

#[test]
fn every_bundled_header_lowers() {
    for name in crate::headers::BUNDLED_NAMES {
        ir(&format!("#include <{}>\n", name));
    }
}

#[test]
fn realistic_programs_verify() {
    ir(r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct node { int value; struct node *next; } node_t;

static node_t *push(node_t *head, int v) {
    node_t *n = malloc(sizeof *n);
    if (!n) { perror("malloc"); exit(1); }
    n->value = v;
    n->next = head;
    return n;
}

static int cmp(const void *a, const void *b) { return *(const int *)a - *(const int *)b; }

int main(int argc, char **argv) {
    node_t *list = NULL;
    for (int i = 0; i < 10; i++) list = push(list, i * i);
    int sum = 0, n = 0;
    int vals[10];
    for (node_t *p = list; p; p = p->next) { sum += p->value; vals[n++] = p->value; }
    qsort(vals, 10, sizeof vals[0], cmp);
    switch (sum % 3) {
    case 0: puts("zero"); break;
    case 1: puts("one"); break;
    default: puts("two");
    }
    char buf[64];
    snprintf(buf, sizeof buf, "%d %d %s", sum, vals[0], argc > 1 ? argv[1] : "none");
    printf("%s %zu\n", buf, strlen(buf));
    return sum > 100 ? 0 : 1;
}
"#);
}

#[test]
fn numeric_programs_verify() {
    ir(r#"
#include <math.h>
double dot(const double *a, const double *b, int n) { double s = 0; for (int i = 0; i < n; i++) s += a[i] * b[i]; return s; }
float lerp(float a, float b, float t) { return a + (b - a) * t; }
long fib(int n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
unsigned long long mix(unsigned long long x) { x ^= x >> 33; x *= 0xff51afd7ed558ccdULL; x ^= x >> 33; return x; }
int clampi(int x, int lo, int hi) { return x < lo ? lo : x > hi ? hi : x; }
double norm(double x, double y) { return sqrt(x * x + y * y); }
int signum(double d) { return (d > 0) - (d < 0); }
"#);
}

#[test]
fn unions_enums_and_function_pointers_verify() {
    ir(r#"
union U { int i; float f; unsigned char b[4]; };
enum Op { ADD, SUB, MUL };
typedef int (*binop)(int, int);
static int add(int a, int b) { return a + b; }
static int sub(int a, int b) { return a - b; }
static int mul(int a, int b) { return a * b; }
static binop ops[] = { add, sub, mul };
int apply(enum Op op, int a, int b) { return ops[op](a, b); }
float as_float(int i) { union U u; u.i = i; return u.f; }
int byte0(float f) { union U u = { .f = f }; return u.b[0]; }
"#);
}

#[test]
fn compound_assignment_and_incdec_verify() {
    ir(r#"
void f(int *p, char *c, double *d, unsigned short *s, long long *l) {
    *p += 5; *p -= 2; *p *= 3; *p /= 2; *p %= 7; *p <<= 1; *p >>= 1; *p &= 0xff; *p |= 1; *p ^= 2;
    (*c)++; --*c; *c += 100; *d += 1.5; (*d)--; (*s)++; *s <<= 3; *l += *p;
    p++; --p; p += 3; p -= 1; c++;
}
"#);
}

#[test]
fn many_arguments_verify() {
    ir("long f(long a, long b, long c, long d, long e, long g, long h, long i, double x, double y, double z) { return a + b + c + d + e + g + h + i + (long)(x + y + z); } long g(void) { return f(1,2,3,4,5,6,7,8,1.0,2.0,3.0); }");
}

#[test]
fn recursive_struct_and_arrays_of_structs_verify() {
    ir(r#"
struct Tree { struct Tree *l, *r; int v; };
struct Tree pool[16];
int depth(const struct Tree *t) { if (!t) return 0; int a = depth(t->l), b = depth(t->r); return 1 + (a > b ? a : b); }
void init(void) { for (int i = 0; i < 16; i++) { pool[i].v = i; pool[i].l = i * 2 + 1 < 16 ? &pool[i * 2 + 1] : 0; pool[i].r = i * 2 + 2 < 16 ? &pool[i * 2 + 2] : 0; } }
"#);
}

#[test]
fn emit_ir_text_has_expected_shape() {
    let t = ir("int add(int a, int b) { return a + b; }");
    let expected_lines =
        ["define i32 @add(i32 %a.0, i32 %b.1) {", "entry:", "  %a.2 = alloca 4, align 4", "  ret i32 %"];
    for l in expected_lines {
        assert!(t.contains(l), "missing {l:?} in:\n{t}");
    }
}

// ───────────────────────────── variable length arrays ─────────────────────────────

#[test]
fn vla_uses_dynamic_stack_and_restores_it_at_block_exit() {
    let t = ir("int f(int n) { int s = 0; { int a[n]; a[0] = 1; s = a[0]; } return s; }");
    assert!(t.contains("dynalloca"), "{t}");
    assert!(t.contains("stacksave") && t.contains("stackrestore"), "{t}");
}

#[test]
fn vla_strides_are_computed_at_run_time() {
    let t = ir("int f(int r, int c) { int m[r][c]; return m[1][2]; }");
    // the row stride is c * 4, a multiplication on loaded values, not a constant
    assert!(t.contains("mul i64"), "{t}");
    assert!(t.matches("dynalloca").count() == 1, "{t}");
}

#[test]
fn break_out_of_a_vla_block_releases_the_stack() {
    let t = ir("int f(int n) { for (;;) { int a[n]; a[0] = n; if (a[0]) break; } return 0; }");
    // one restore at the end of the block, one before the jump out of it
    assert!(t.matches("stackrestore").count() >= 2, "{t}");
}

#[test]
fn blocks_without_vlas_do_not_touch_the_stack_pointer() {
    let t = ir("int f(int n) { int a[8]; a[0] = n; return a[0]; }");
    assert!(!t.contains("stacksave") && !t.contains("dynalloca"), "{t}");
}

#[test]
fn annotated_ir_carries_source_lines() {
    let (m, _) = build("int f(int a) {\n  int b = a + 1;\n  return b * 2;\n}\n");
    let t = crate::ir::print::print_module_annotated(&m);
    assert!(t.contains("; L2"), "{t}");
    assert!(t.contains("; L3"), "{t}");
    // the plain printer is unchanged
    assert!(!crate::ir::print::print_module(&m).contains("; L"));
}
