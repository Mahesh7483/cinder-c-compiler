use super::*;
use crate::diag::Level;
use crate::hir_dump;
use crate::parse;
use crate::pp::{self, PpOptions};

struct R {
    hir: Option<HirModule>,
    errors: Vec<String>,
    warnings: Vec<String>,
    sess: Session,
}

fn run(src: &str) -> R {
    run_with(src, |c| c.enable_all())
}

fn run_with(src: &str, configure: impl FnOnce(&mut crate::diag::WarnConfig)) -> R {
    let mut sess = Session::new();
    configure(&mut sess.diags.config);
    let id = sess.sources.add_file("t.c", None, src.to_string());
    let toks = pp::preprocess(&mut sess, id, &PpOptions::default());
    let tu = parse::parse(&mut sess, toks);
    assert!(
        !sess.diags.has_errors(),
        "parse errors for {src:?}: {:?}",
        sess.diags.diagnostics().iter().map(|d| d.message.clone()).collect::<Vec<_>>()
    );
    let hir = analyze(&mut sess, &tu);
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for d in sess.diags.diagnostics() {
        match d.level {
            Level::Error | Level::Fatal => errors.push(d.message.clone()),
            Level::Warning => warnings.push(d.message.clone()),
            Level::Note => {}
        }
    }
    R { hir, errors, warnings, sess }
}

/// Analyze, expecting no errors and no warnings.
fn clean(src: &str) -> HirModule {
    let r = run(src);
    assert!(r.errors.is_empty(), "errors for {src:?}: {:?}", r.errors);
    assert!(r.warnings.is_empty(), "warnings for {src:?}: {:?}", r.warnings);
    r.hir.unwrap()
}

fn errors(src: &str) -> Vec<String> {
    let r = run(src);
    assert!(r.hir.is_none(), "expected failure for {src:?}");
    r.errors
}

fn warnings(src: &str) -> Vec<String> {
    let r = run(src);
    assert!(r.errors.is_empty(), "errors for {src:?}: {:?}", r.errors);
    r.warnings
}

fn dump(src: &str) -> String {
    let r = run(src);
    assert!(r.errors.is_empty(), "errors for {src:?}: {:?}", r.errors);
    hir_dump::dump(&r.sess.sources, r.hir.as_ref().unwrap())
}

/// Type of the expression statement `expr;` evaluated with `decls` in scope.
fn type_of(decls: &str, expr: &str) -> String {
    let src = format!("{} void f_(void) {{ {}; }}", decls, expr);
    let r = run(&src);
    assert!(r.errors.is_empty(), "errors for {src:?}: {:?}", r.errors);
    let m = r.hir.unwrap();
    let f = m.funcs.last().unwrap();
    let HStmtKind::Block(items) = &f.body.kind else { panic!() };
    let HStmtKind::Expr(e) = &items.last().unwrap().kind else {
        panic!("not an expression statement: {:?}", items.last())
    };
    m.types.show(e.ty)
}

fn sym<'a>(m: &'a HirModule, name: &str) -> &'a GlobalSym {
    m.syms.iter().find(|s| s.name.as_str() == name).unwrap_or_else(|| panic!("no symbol {name}"))
}

fn ty_of(m: &HirModule, name: &str) -> String {
    m.types.show(sym(m, name).ty)
}

// ───────────────────────────── globals & types ─────────────────────────────

#[test]
fn global_variables() {
    let m = clean("int x; int y = 3; static int z = 4; extern int e; const int c = 5; int getz(void) { return z; }");
    assert_eq!(ty_of(&m, "x"), "int");
    assert!(sym(&m, "x").tentative && sym(&m, "x").defined);
    assert!(!sym(&m, "y").tentative && sym(&m, "y").init.is_some());
    assert_eq!(sym(&m, "z").linkage, Linkage::Internal);
    assert!(!sym(&m, "e").defined);
    assert!(sym(&m, "c").is_const);
}

#[test]
fn sizeof_and_array_sizes() {
    let m = clean("struct S { char c; int i; }; int a[sizeof(struct S) * 2]; enum { N = 3 }; int b[N + 1]; char c[sizeof(int[5])];");
    assert_eq!(ty_of(&m, "a"), "int [16]");
    assert_eq!(ty_of(&m, "b"), "int [4]");
    assert_eq!(ty_of(&m, "c"), "char [20]");
}

#[test]
fn incomplete_arrays_completed_by_initializer() {
    let m = clean("int a[] = {1, 2, 3}; char s[] = \"abc\"; int b[][2] = {{1,2},{3,4},{5,6}}; int c[5] = {1}; char t[10] = \"hi\";");
    assert_eq!(ty_of(&m, "a"), "int [3]");
    assert_eq!(ty_of(&m, "s"), "char [4]");
    assert_eq!(ty_of(&m, "b"), "int [3][2]");
    assert_eq!(ty_of(&m, "c"), "int [5]");
    assert_eq!(ty_of(&m, "t"), "char [10]");
}

#[test]
fn designated_array_initializer_sizes_the_array() {
    let m = clean("int a[] = {[4] = 1, 2}; ");
    assert_eq!(ty_of(&m, "a"), "int [6]");
}

#[test]
fn enums() {
    let m = clean("enum Color { RED, GREEN = 5, BLUE }; enum Color c = BLUE; int x = BLUE; int y = GREEN * 2;");
    let ExprOf(e) = init_const(&m, "x");
    assert_eq!(e, 6);
    let ExprOf(e) = init_const(&m, "y");
    assert_eq!(e, 10);
    assert_eq!(ty_of(&m, "c"), "enum Color");
}

struct ExprOf(i64);

/// The constant integer initializer of a scalar global.
fn init_const(m: &HirModule, name: &str) -> ExprOf {
    let s = sym(m, name);
    let p = s.init.as_ref().unwrap();
    match &p.entries[0].value {
        InitValue::Const(ConstVal::Int(v)) => ExprOf(*v as i64),
        other => panic!("not an int constant: {:?}", other),
    }
}

#[test]
fn enum_values_may_use_earlier_ones_and_constant_expressions() {
    let m = clean("enum { A = 1 << 3, B = A | 2, C = B + 1, D = sizeof(int) }; int x = C; int y = D;");
    assert_eq!(init_const(&m, "x").0, 11);
    assert_eq!(init_const(&m, "y").0, 4);
}

#[test]
fn typedefs_and_struct_layout() {
    let m = clean("typedef struct { char c; double d; } P; int x = sizeof(P); int y = __builtin_offsetof(P, d); int z = _Alignof(P);");
    assert_eq!(init_const(&m, "x").0, 16);
    assert_eq!(init_const(&m, "y").0, 8);
    assert_eq!(init_const(&m, "z").0, 8);
}

#[test]
fn bitfield_struct_sizes() {
    let m = clean("struct B { unsigned a : 3; unsigned b : 5; unsigned c : 30; }; int x = sizeof(struct B);");
    assert_eq!(init_const(&m, "x").0, 8);
}

#[test]
fn anonymous_members_are_accessible() {
    clean("struct S { int a; union { int b; float c; }; }; int f(struct S *s) { return s->b + s->a; }");
}

#[test]
fn function_declarations_and_redeclarations() {
    let m = clean("int f(int); int f(int x) { return x; } int g(); int g(void); static int h(void); static int h(void) { return 1; } int use(void) { return h() + f(2) + g(); }");
    assert_eq!(ty_of(&m, "f"), "int (int)");
    assert!(sym(&m, "f").defined);
    assert!(!sym(&m, "g").defined);
}

#[test]
fn conflicting_declarations() {
    assert!(errors("int f(int); long f(int);")[0].contains("conflicting types for 'f'"));
    assert!(errors("int x; char x;")[0].contains("redefinition of 'x' with a different type"));
    assert!(errors("int f(void) { return 0; } int f(void) { return 1; }")[0].contains("redefinition of 'f'"));
    assert!(errors("int x = 1; int x = 2;")[0].contains("redefinition of 'x'"));
    // a later declaration without `static` inherits internal linkage (C11 6.2.2p4); the reverse is an error
    clean("static int f(void); int f(void); static int f(void) { return 0; } int g(void) { return f(); }");
    assert!(errors("int f(void); static int f(void) { return 0; }")[0]
        .contains("static declaration of 'f' follows non-static declaration"));
    assert!(errors("void f(void); int f;")[0].contains("redefinition of 'f' as different kind of symbol"));
}

#[test]
fn tentative_definitions_merge() {
    let m = clean("int x; int x; int x = 5; int y; extern int y;");
    assert!(!sym(&m, "x").tentative);
    assert!(sym(&m, "y").tentative);
}

#[test]
fn redefinition_in_block_scope() {
    assert!(errors("void f(void) { int a; int a; }")[0].contains("redefinition of 'a'"));
    // shadowing in an inner scope is fine
    let r = warnings("void f(void) { int a = 1; { int a = 2; (void)a; } (void)a; }");
    assert!(r.is_empty());
}

#[test]
fn struct_errors() {
    assert!(errors("struct S { int a; int a; };")[0].contains("duplicate member 'a'"));
    assert!(errors("struct S { int a; }; struct S { int b; };")[0].contains("redefinition of 'struct S'"));
    assert!(errors("struct S; struct S s;")[0].contains("variable has incomplete type 'struct S'"));
    assert!(errors("struct S { struct S inner; };")[0].contains("incomplete type"));
    assert!(errors("struct S { int a[]; int b; };")[0].contains("flexible array member"));
    assert!(errors("union U { int a; }; struct U u;")[0].contains("does not match previous declaration"));
}

#[test]
fn array_errors() {
    assert!(errors("int a[-1];")[0].contains("array size is negative"));
    assert!(errors("int n = 3; int a[n];")[0].contains("variable length array"));
    // variable length arrays are supported in blocks, with restrictions
    clean("void f(int n) { int a[n]; a[0] = 1; }");
    assert!(errors("void f(int n) { static int a[n]; }")[0].contains("static storage duration"));
    assert!(errors("void f(int n) { int a[n] = {1}; }")[0].contains("may not be initialized"));
    assert!(errors("struct S { int a[sizeof(int)]; int b; }; int n; struct T { int a[n]; };")[0]
        .contains("not allowed here"));
    assert!(errors("int f(void)[3];")[0].contains("function cannot return array type"));
    assert!(errors("int a[2.5];")[0].contains("non-integer"));
}

#[test]
fn vla_sizeof_is_a_run_time_value_and_fixed_arrays_stay_constant() {
    let d = dump("unsigned long f(int n) { int a[n]; int b[4]; return sizeof(a) + sizeof(b); }");
    assert!(d.contains("VlaSizeof"), "{d}");
    assert!(d.contains("VlaDecl"), "{d}");
    // the fixed-size array is still folded to a literal
    assert!(!d.contains("VlaDecl b"), "{d}");
}

#[test]
fn vla_parameters_may_name_earlier_parameters() {
    clean("int sum(int r, int c, int m[r][c]) { return m[r - 1][c - 1]; }");
    clean("void fill(int n, int a[n]);");
    clean("void g(int n, int (*p)[n]) { p[1][2] = 3; }");
}

#[test]
fn unsupported_types_are_diagnosed() {
    assert!(errors("long double x;")[0].contains("not yet supported: long double"));
    assert!(errors("_Atomic int x;")[0].contains("not yet supported: _Atomic"));
    assert!(errors("_Thread_local int x;")[0].contains("not yet supported: thread-local"));
    assert!(errors("float f = 1.0L;")[0].contains("not yet supported: long double"));
}

// ───────────────────────────── expression typing ─────────────────────────────

#[test]
fn integer_promotion_and_arithmetic_types() {
    assert_eq!(type_of("char a, b;", "a + b"), "int");
    assert_eq!(type_of("unsigned char a;", "-a"), "int");
    assert_eq!(type_of("short a;", "~a"), "int");
    assert_eq!(type_of("int a; long b;", "a + b"), "long");
    assert_eq!(type_of("int a; unsigned b;", "a * b"), "unsigned int");
    assert_eq!(type_of("long a; unsigned b;", "a - b"), "long");
    assert_eq!(type_of("unsigned long a; long b;", "a / b"), "unsigned long");
    assert_eq!(type_of("int a; double b;", "a + b"), "double");
    assert_eq!(type_of("float a; int b;", "a * b"), "float");
    assert_eq!(type_of("float a, b;", "a + b"), "float");
    assert_eq!(type_of("char a;", "a << 2"), "int");
    assert_eq!(type_of("long a; int b;", "b << a"), "int");
    assert_eq!(type_of("long a; int b;", "a << b"), "long");
    assert_eq!(type_of("", "1 + 1L"), "long");
    assert_eq!(type_of("", "1u + 1"), "unsigned int");
    assert_eq!(type_of("", "'a'"), "int");
    assert_eq!(type_of("", "1.5f"), "float");
    assert_eq!(type_of("", "1.5"), "double");
}

#[test]
fn comparison_and_logical_results_are_int() {
    assert_eq!(type_of("long a, b;", "a < b"), "int");
    assert_eq!(type_of("double a, b;", "a == b"), "int");
    assert_eq!(type_of("int *p;", "p && !p"), "int");
    assert_eq!(type_of("int *p, *q;", "p != q"), "int");
    assert_eq!(type_of("", "!5"), "int");
}

#[test]
fn integer_literal_types() {
    assert_eq!(type_of("", "2147483647"), "int");
    assert_eq!(type_of("", "2147483648"), "long");
    assert_eq!(type_of("", "0x7fffffff"), "int");
    assert_eq!(type_of("", "0xffffffff"), "unsigned int");
    assert_eq!(type_of("", "4294967296"), "long");
    assert_eq!(type_of("", "10u"), "unsigned int");
    assert_eq!(type_of("", "10l"), "long");
    assert_eq!(type_of("", "10ul"), "unsigned long");
    assert_eq!(type_of("", "10ll"), "long long");
    assert_eq!(type_of("", "0xffffffffffffffff"), "unsigned long");
}

#[test]
fn huge_decimal_literal_is_unsigned_with_warning() {
    let w = warnings("unsigned long long x = 18446744073709551615;");
    assert!(w[0].contains("too large to be represented in a signed integer type"));
}

#[test]
fn pointer_arithmetic() {
    assert_eq!(type_of("int *p;", "p + 1"), "int *");
    assert_eq!(type_of("int *p;", "1 + p"), "int *");
    assert_eq!(type_of("int *p;", "p - 1"), "int *");
    assert_eq!(type_of("int *p, *q;", "p - q"), "long");
    assert_eq!(type_of("int a[3];", "a + 1"), "int *");
    assert_eq!(type_of("int a[3];", "*a"), "int");
    assert_eq!(type_of("int a[3];", "a[1]"), "int");
    assert_eq!(type_of("int a[3];", "1[a]"), "int");
    // an array-typed lvalue statement keeps its array type (it only decays in value contexts)
    assert_eq!(type_of("int a[2][3];", "a[1]"), "int [3]");
    assert_eq!(type_of("int a[2][3];", "a[1] + 1"), "int *");
    assert_eq!(type_of("int a[2][3];", "a[1][2]"), "int");
    assert_eq!(type_of("int a[3];", "&a"), "int (*)[3]");
    assert_eq!(type_of("int a[3];", "&a[1]"), "int *");
    assert_eq!(type_of("struct S {int x;} *p;", "p->x"), "int");
    assert_eq!(type_of("void *p;", "p + 1"), "void *");
}

#[test]
fn pointer_arith_errors() {
    assert!(errors("void f(int *p, int *q) { p + q; }")[0]
        .contains("invalid operands to binary expression ('int *' and 'int *')"));
    assert!(errors("void f(int *p, char *q) { p - q; }")[0].contains("not pointers to compatible types"));
    assert!(
        errors("struct S; void f(struct S *p) { p + 1; }")[0].contains("arithmetic on a pointer to an incomplete type")
    );
    assert!(errors("void f(double d, int *p) { p + d; }")[0].contains("invalid operands"));
}

#[test]
fn hir_shows_scaled_pointer_arithmetic_and_conversions() {
    let d = dump("int f(int *p, long i) { return p[i]; }");
    assert!(d.contains("PtrAdd +*4"), "{d}");
    assert!(d.contains("Cast LValueToRValue"), "{d}");
    let d = dump("long f(int a) { return a; }");
    assert!(d.contains("Cast IntToInt : 'long'"), "{d}");
    let d = dump("int a[3]; int *f(void) { return a; }");
    assert!(d.contains("Cast ArrayToPointer"), "{d}");
    let d = dump("int g(void); int (*f(void))(void) { return g; }");
    assert!(d.contains("Cast FunctionToPointer"), "{d}");
}

#[test]
fn constants_are_folded() {
    let d = dump("int f(void) { return 2 + 3 * 4; }");
    assert!(d.contains("IntLiteral 14"), "{d}");
    let d = dump("double f(void) { return 1 / 2.0; }");
    assert!(d.contains("FloatLiteral 0.5"), "{d}");
    let d = dump("int f(void) { return (char)300; }");
    assert!(d.contains("IntLiteral 44"), "{d}");
}

#[test]
fn division_by_zero_warns_and_is_not_folded() {
    let w = warnings("int f(void) { return 5 / 0; }");
    assert_eq!(w, ["division by zero is undefined"]);
    let w = warnings("int f(int x) { return x % 0; }");
    assert_eq!(w, ["remainder by zero is undefined"]);
}

#[test]
fn shift_count_warnings() {
    assert!(warnings("int f(int x) { return x << 32; }")[0].contains("shift count >= width of type"));
    assert!(warnings("int f(int x) { return x >> -1; }")[0].contains("shift count is negative"));
    assert!(warnings("long f(long x) { return x << 63; }").is_empty());
}

#[test]
fn conditional_expression_types() {
    assert_eq!(type_of("int c; int a, b;", "c ? a : b"), "int");
    assert_eq!(type_of("int c; int a; long b;", "c ? a : b"), "long");
    assert_eq!(type_of("int c; int a; double b;", "c ? a : b"), "double");
    assert_eq!(type_of("int c; int *p;", "c ? p : 0"), "int *");
    assert_eq!(type_of("int c; int *p; void *v;", "c ? p : v"), "void *");
    assert_eq!(type_of("int c; const int *p; int *q;", "c ? p : q"), "const int *");
    assert_eq!(type_of("int c; struct S {int a;} a, b;", "c ? a : b"), "struct S");
    assert!(errors("struct S {int a;} s; void f(int c) { c ? s : 1; }")[0].contains("incompatible operand types"));
}

#[test]
fn comma_and_sizeof_expressions() {
    assert_eq!(type_of("int a; char b;", "(a, b)"), "char");
    assert_eq!(type_of("int a[10];", "sizeof a"), "unsigned long");
    assert_eq!(type_of("", "sizeof(int)"), "unsigned long");
    assert_eq!(type_of("int a[10];", "sizeof a / sizeof a[0]"), "unsigned long");
    assert!(errors("struct S; unsigned long n = sizeof(struct S);")[0]
        .contains("invalid application of 'sizeof' to an incomplete type 'struct S'"));
    assert!(errors("struct B { int a : 3; } b; unsigned long n = sizeof(b.a);")[0].contains("bit-field"));
}

#[test]
fn casts() {
    assert_eq!(type_of("double d;", "(int)d"), "int");
    assert_eq!(type_of("int i;", "(char)i"), "char");
    assert_eq!(type_of("int *p;", "(long)p"), "long");
    assert_eq!(type_of("long l;", "(int *)l"), "int *");
    assert_eq!(type_of("int i;", "(void)i"), "void");
    assert_eq!(type_of("char c;", "(_Bool)c"), "_Bool");
    assert_eq!(type_of("int *p;", "(char *)p"), "char *");
    assert!(errors("struct S {int a;} s; void f(void) { (int)s; }")[0]
        .contains("used type 'struct S' where arithmetic or pointer type is required"));
    assert!(errors("void f(double d) { (int *)d; }")[0].contains("cannot cast"));
    assert!(warnings("void *f(int i) { return (void *)i; }")[0]
        .contains("cast to 'void *' from smaller integer type 'int'"));
    // no warning for a constant or pointer-sized integer
    assert!(warnings("void *f(long i) { return (void *)i; }").is_empty());
    assert!(warnings("void *f(void) { return (void *)0; }").is_empty());
}

#[test]
fn unary_operators() {
    assert_eq!(type_of("int *p;", "*p"), "int");
    assert_eq!(type_of("int i;", "&i"), "int *");
    assert_eq!(type_of("int i;", "-i"), "int");
    assert_eq!(type_of("int i;", "i++"), "int");
    assert_eq!(type_of("int *p;", "++p"), "int *");
    assert_eq!(type_of("int f(void);", "&f"), "int (*)(void)");
    assert!(errors("void f(int i) { *i; }")[0].contains("indirection requires pointer operand ('int' invalid)"));
    assert!(errors("void f(void) { &5; }")[0].contains("cannot take the address of an rvalue"));
    assert!(errors("void f(int *p) { *(void *)p; }")[0].contains("cannot dereference a pointer to 'void'"));
    assert!(errors("struct S {int a;} s; void f(void) { -s; }")[0]
        .contains("invalid argument type 'struct S' to unary expression"));
    assert!(errors("void f(double d) { ~d; }")[0].contains("invalid argument type 'double'"));
    assert!(errors("struct B { int a : 3; } b; void f(void) { &b.a; }")[0].contains("address of bit-field requested"));
    assert!(errors("void f(void) { 5++; }")[0].contains("expression is not assignable"));
}

#[test]
fn member_access() {
    assert_eq!(type_of("struct S {int a; char b;} s;", "s.b"), "char");
    assert_eq!(type_of("struct S {int a; char b;} *p;", "p->a"), "int");
    assert_eq!(type_of("const struct S {int a;} s;", "s.a"), "int");
    assert!(errors("struct S {int a;} s; void f(void) { s.z; }")[0].contains("no member named 'z' in 'struct S'"));
    assert!(errors("struct S {int a;} *p; void f(void) { p.a; }")[0].contains("did you mean to use '->'?"));
    assert!(errors("struct S {int a;} s; void f(void) { s->a; }")[0].contains("did you mean to use '.'?"));
    assert!(errors("int i; void f(void) { i.x; }")[0].contains("is not a structure or union"));
    assert!(errors("struct S; void f(struct S *p) { p->x; }")[0].contains("incomplete definition of type 'struct S'"));
}

#[test]
fn const_and_lvalue_errors() {
    assert!(errors("const int x = 1; void f(void) { x = 2; }")[0]
        .contains("cannot assign to variable 'x' with const-qualified type 'const int'"));
    assert!(errors("void f(const int *p) { *p = 1; }")[0]
        .contains("cannot assign to this expression with const-qualified type 'const int'"));
    assert!(errors("const int x = 1; void f(void) { x++; }")[0].contains("cannot increment variable 'x'"));
    assert!(errors("const int x = 1; void f(void) { --x; }")[0].contains("cannot decrement variable 'x'"));
    assert!(errors("void f(void) { 1 = 2; }")[0].contains("expression is not assignable"));
    assert!(errors("int a[3], b[3]; void f(void) { a = b; }")[0].contains("array type 'int [3]' is not assignable"));
    assert!(errors("int g(void); void f(void) { g() = 1; }")[0].contains("expression is not assignable"));
    // struct with const members through a const struct
    assert!(errors("const struct S {int a;} s; void f(void) { s.a = 1; }")[0].contains("const-qualified"));
}

#[test]
fn assignment_types() {
    assert_eq!(type_of("int a; long b;", "a = b"), "int");
    assert_eq!(type_of("int a;", "a += 2"), "int");
    assert_eq!(type_of("int *p;", "p += 1"), "int *");
    assert_eq!(type_of("char c;", "c *= 2"), "char");
    assert_eq!(type_of("int a;", "a <<= 1"), "int");
    assert!(errors("void f(double d) { d %= 2; }")[0]
        .contains("invalid operands to binary expression ('double' and 'int')"));
    assert!(errors("void f(int *p, int *q) { p += q; }")[0].contains("invalid operands"));
    assert!(errors("void f(float x) { x <<= 1; }")[0].contains("invalid operands"));
}

#[test]
fn struct_assignment_and_return() {
    clean("struct S { int a; double b; }; struct S g(struct S s) { struct S t = s; t = s; return t; }");
    assert!(errors("struct S {int a;}; struct T {int a;}; void f(struct S s, struct T t) { s = t; }")[0]
        .contains("assigning to 'struct S' from incompatible type 'struct T'"));
}

// ───────────────────────────── conversions & warnings ─────────────────────────────

#[test]
fn narrowing_warnings() {
    assert!(warnings("void f(long l) { int i = l; (void)i; }")[0]
        .contains("implicit conversion loses integer precision: 'long' to 'int'"));
    assert!(warnings("void f(int i) { char c = i; (void)c; }")[0].contains("loses integer precision: 'int' to 'char'"));
    assert!(warnings("void f(char c) { c = c + 1; }")[0].contains("loses integer precision: 'int' to 'char'"));
    assert!(warnings("void f(void) { char c = 300; (void)c; }")[0].contains("changes value from 300 to 44"));
    assert!(warnings("void f(void) { unsigned char c = -1; (void)c; }")[0].contains("changes value from -1 to 255"));
    assert!(
        warnings("void f(double d) { int i = d; (void)i; }")[0].contains("turns floating-point number into integer")
    );
    assert!(warnings("void f(void) { int i = 3.5; (void)i; }")[0].contains("changes value from 3.5 to 3"));
    assert!(warnings("void f(double d) { float x = d; (void)x; }")[0].contains("loses floating-point precision"));
    assert!(warnings("void f(void) { int i; i = 3000000000u; (void)i; }")[0]
        .contains("changes value from 3000000000 to -1294967296"));
}

#[test]
fn conversions_that_do_not_warn() {
    let none = |s: &str| {
        let w = warnings(s);
        assert!(w.is_empty(), "{s}: {:?}", w);
    };
    none("void f(void) { char c = 100; unsigned char u = 255; short s = -5; (void)c; (void)u; (void)s; }");
    none("void f(int i) { unsigned char c = i & 0xff; (void)c; }");
    none("void f(unsigned u) { unsigned char c = u % 10; (void)c; }");
    none("void f(char a, char b) { int x = a + b; (void)x; }");
    none("void f(int i) { long l = i; double d = i; (void)l; (void)d; }");
    none("void f(void) { float x = 0.1; double d = 1; int i = 3.0; (void)x; (void)d; (void)i; }");
    none("void f(int i) { char c = (char)i; _Bool b = i; (void)c; (void)b; }");
    none("void f(int i) { unsigned char c = (i > 0) ? 1 : 0; (void)c; }");
    none("struct B { int a : 4; }; void f(struct B b) { char c = b.a; (void)c; }");
    none("void f(int i) { char c = i >> 24; (void)c; }");
    none("void f(unsigned char a) { char c = a >> 1; (void)c; }");
}

#[test]
fn pointer_conversion_diagnostics() {
    assert!(warnings("void f(char *c) { int *p = c; (void)p; }")[0]
        .contains("incompatible pointer types initializing 'int *' with an expression of type 'char *'"));
    assert!(warnings("void f(int i) { int *p = i; (void)p; }")[0]
        .contains("incompatible integer to pointer conversion initializing 'int *' with an expression of type 'int'"));
    assert!(warnings("void f(int *p) { int i = p; (void)i; }")[0]
        .contains("incompatible pointer to integer conversion initializing 'int' with an expression of type 'int *'"));
    assert!(warnings("void f(const int *c) { int *p = c; (void)p; }")[0]
        .contains("initializing 'int *' with an expression of type 'const int *' discards qualifiers"));
    assert!(warnings("void f(int *p, char *c) { p = c; }")[0]
        .contains("incompatible pointer types assigning to 'int *' from 'char *'"));
    assert!(warnings("int *f(char *c) { return c; }")[0]
        .contains("incompatible pointer types returning 'char *' from a function with result type 'int *'"));
    assert!(warnings("void g(int *); void f(char *c) { g(c); }")[0]
        .contains("incompatible pointer types passing 'char *' to parameter of type 'int *'"));
    // all fine
    let w = warnings("void f(void *v, int *p) { int *a = v; void *b = p; int *c = 0; int *d = (void *)0; const int *e = p; (void)a; (void)b; (void)c; (void)d; (void)e; }");
    assert!(w.is_empty(), "{w:?}");
}

#[test]
fn pointer_comparisons() {
    assert!(warnings("int f(int *p, char *q) { return p == q; }")[0]
        .contains("comparison of distinct pointer types ('int *' and 'char *')"));
    assert!(warnings("int f(int *p, int i) { return p == i; }")[0].contains("comparison between pointer and integer"));
    assert!(warnings("int f(int *p, void *v) { return p == v || p == 0 || p < v; }").is_empty());
}

#[test]
fn incompatible_types_are_errors() {
    assert!(errors("struct S {int a;} s; void f(void) { int i = s; }")[0]
        .contains("initializing 'int' with an expression of incompatible type 'struct S'"));
    assert!(errors("void f(void) { double d = (void)0; }")[0].len() > 5);
    assert!(errors("struct S {int a;}; void f(struct S s) { int i; i = s; }")[0]
        .contains("assigning to 'int' from incompatible type 'struct S'"));
    assert!(errors("struct S {int a;}; void g(int); void f(struct S s) { g(s); }")[0]
        .contains("passing 'struct S' to parameter of incompatible type 'int'"));
    assert!(errors("struct S {int a;}; int f(struct S s) { return s; }")[0]
        .contains("returning 'struct S' from a function with incompatible result type 'int'"));
}

// ───────────────────────────── calls ─────────────────────────────

#[test]
fn calls() {
    assert_eq!(type_of("int f(int, char *);", "f(1, \"x\")"), "int");
    assert_eq!(type_of("int (*fp)(void);", "fp()"), "int");
    assert_eq!(type_of("int (*fp)(void);", "(*fp)()"), "int");
    assert_eq!(type_of("void f(void);", "f()"), "void");
    assert_eq!(type_of("struct S {int a;} g(void);", "g()"), "struct S");
    assert_eq!(type_of("int printf(const char *, ...);", "printf(\"%d\", 1)"), "int");
}

#[test]
fn argument_count_errors() {
    let e = errors("int f(int, int); void g(void) { f(1); }");
    assert!(e[0].contains("too few arguments to function call, expected 2, have 1"), "{e:?}");
    let e = errors("int f(int); void g(void) { f(1, 2); }");
    assert!(e[0].contains("too many arguments to function call, expected 1, have 2"), "{e:?}");
    let e = errors("int f(void); void g(void) { f(1); }");
    assert!(e[0].contains("too many arguments"), "{e:?}");
    // the note points at the declaration
    let r = run("int f(int); void g(void) { f(); }");
    let d = &r.sess.diags.diagnostics()[0];
    assert!(d.notes[0].message.contains("'f' declared here"));
}

#[test]
fn variadic_calls_promote_arguments() {
    let d = dump("int printf(const char *, ...); void f(char c, float x, short s) { printf(\"\", c, x, s); }");
    assert!(d.contains("Cast FloatToFloat : 'double'"), "{d}");
    assert!(d.contains("Cast IntToInt : 'int'"), "{d}");
}

#[test]
fn implicit_function_declaration() {
    let w = warnings("int f(void) { return undeclared_fn(1, 2); }");
    assert!(w[0].contains("call to undeclared function 'undeclared_fn'"), "{w:?}");
    // declared once: the second call does not warn again
    let w = warnings("void f(void) { g(); g(); }");
    assert_eq!(w.len(), 1);
}

#[test]
fn call_errors() {
    assert!(
        errors("void f(int i) { i(); }")[0].contains("called object type 'int' is not a function or function pointer")
    );
}

#[test]
fn undeclared_identifiers_suggest_similar_names() {
    let r = run("int counter; int f(void) { return countr; }");
    let m = &r.errors[0];
    assert!(m.contains("use of undeclared identifier 'countr'; did you mean 'counter'?"), "{m}");
    let d = &r.sess.diags.diagnostics()[0];
    assert_eq!(d.fixit.as_ref().unwrap().text, "counter");
    let e = errors("int f(void) { return zzz; }");
    assert_eq!(e[0], "use of undeclared identifier 'zzz'");
    // an adjacent transposition is a single edit
    let e = errors("int count; int f(void) { return cuont; }");
    assert!(e[0].contains("did you mean 'count'?"), "{e:?}");
}

#[test]
fn typedef_name_used_as_expression() {
    assert!(errors("typedef int T; int x = T;")[0].contains("unexpected type name 'T'"));
}

// ───────────────────────────── statements ─────────────────────────────

#[test]
fn break_and_continue_placement() {
    assert!(errors("void f(void) { break; }")[0].contains("'break' statement not in loop or switch statement"));
    assert!(errors("void f(void) { continue; }")[0].contains("'continue' statement not in loop statement"));
    assert!(
        errors("void f(int x) { switch (x) { case 1: continue; } }")[0].contains("'continue' statement not in loop")
    );
    clean("void f(int x) { while (x) { switch (x) { case 1: break; default: continue; } } }");
}

#[test]
fn switch_checks() {
    assert!(errors("void f(int x) { switch (x) { case 1: break; case 1: break; } }")[0]
        .contains("duplicate case value '1'"));
    assert!(errors("void f(void) { case 1: ; }")[0].contains("'case' statement not in switch statement"));
    assert!(errors("void f(void) { default: ; }")[0].contains("'default' statement not in switch statement"));
    assert!(errors("void f(int x) { switch (x) { default: break; default: break; } }")[0]
        .contains("multiple default labels"));
    assert!(errors("void f(int x, int y) { switch (x) { case y: break; } }")[0]
        .contains("not an integer constant expression"));
    assert!(errors("void f(double d) { switch (d) { } }")[0]
        .contains("statement requires expression of integer type ('double' invalid)"));
    clean("enum E { A, B }; void f(enum E e) { switch (e) { case A: break; case B: break; } }");
    clean("void f(char c) { switch (c) { case 'a': case 'b': break; case 'a' + 2: break; } }");
}

#[test]
fn switch_case_values_use_the_promoted_type() {
    // on an unsigned char switch, case -1 becomes 4294967295? no: promoted type is int, so -1 stays -1
    let d = dump("void f(unsigned char c) { switch (c) { case 255: break; case -1: break; } }");
    assert!(d.contains("255->case0"), "{d}");
    assert!(d.contains("-1->case1"), "{d}");
}

#[test]
fn goto_and_labels() {
    clean("void f(void) { goto end; end: return; }");
    assert!(errors("void f(void) { goto nowhere; }")[0].contains("use of undeclared label 'nowhere'"));
    assert!(errors("void f(void) { a: ; a: ; }")[0].contains("redefinition of label 'a'"));
    assert!(warnings("void f(void) { unused: ; }")[0].contains("unused label 'unused'"));
    // labels are per function
    assert!(errors("void f(void) { l: ; } void g(void) { goto l; }")[0].contains("use of undeclared label 'l'"));
}

#[test]
fn return_checks() {
    assert!(warnings("int f(void) { return; }")[0].contains("non-void function 'f' should return a value"));
    assert!(warnings("void f(void) { return 1; }")[0].contains("void function 'f' should not return a value"));
    assert!(errors("struct S {int a;}; struct S f(void) { return 1; }")[0]
        .contains("returning 'int' from a function with incompatible result type 'struct S'"));
    assert!(warnings("int *f(void) { int x = 0; return &x; }")[0]
        .contains("address of stack memory associated with local variable 'x' returned"));
    assert!(warnings("char *f(void) { char b[4] = {0}; return b; }")[0]
        .contains("address of stack memory associated with local variable 'b' returned"));
    clean("void f(void) { return; } int g(void) { return 1; } void h(void) { return (void)0; }");
}

#[test]
fn condition_must_be_scalar() {
    assert!(errors("struct S {int a;} s; void f(void) { if (s) ; }")[0]
        .contains("statement requires expression of scalar type ('struct S' invalid)"));
    assert!(errors("struct S {int a;} s; void f(void) { while (s) ; }")[0].contains("scalar type"));
    clean("void f(int *p, double d, long l) { if (p) ; while (d) ; for (; l; ) break; do ; while (p); }");
}

#[test]
fn parentheses_warning_on_assignment_in_condition() {
    let r = run("void f(int a, int b) { if (a = b) ; }");
    assert!(
        r.warnings[0].contains("using the result of an assignment as a condition without parentheses"),
        "{:?}",
        r.warnings
    );
    assert!(warnings("void f(int a, int b) { if ((a = b)) ; while ((a = b) != 0) ; }").is_empty());
}

#[test]
fn unused_value_warning() {
    assert!(warnings("void f(int a, int b) { a + b; }")[0].contains("expression result unused"));
    assert!(warnings("void f(int a) { a; }")[0].contains("expression result unused"));
    assert!(warnings("void f(int a, int b) { a == b; }")[0].contains("expression result unused"));
    assert!(warnings("void f(int *p) { *p; }")[0].contains("expression result unused"));
    let none =
        warnings("int g(void); void f(int a, int *p) { a = 1; a++; g(); (void)a; (void)(a + 1); *p = 2; a += 3; }");
    assert!(none.is_empty(), "{none:?}");
}

#[test]
fn unused_variables_and_functions() {
    assert!(warnings("void f(void) { int unused; }")[0].contains("unused variable 'unused'"));
    assert!(warnings("static void helper(void) { }")[0].contains("unused function 'helper'"));
    assert!(warnings("static int counter;")[0].contains("unused variable 'counter'"));
    // parameters are not reported, used variables are not reported
    let none = warnings("void f(int param) { int used = 1; (void)used; }");
    assert!(none.is_empty(), "{none:?}");
    // a variable that is only assigned counts as used (like Clang's -Wunused-variable)
    assert!(warnings("void f(void) { int x; x = 1; }").is_empty());
    // static function that is used
    assert!(warnings("static int h(void) { return 1; } int g(void) { return h(); }").is_empty());
    // __attribute__((unused))
    assert!(warnings("void f(void) { int x __attribute__((unused)); }").is_empty());
}

#[test]
fn static_locals_become_globals() {
    let m = clean("int next(void) { static int n = 10; return n++; }");
    let s = m.syms.iter().find(|s| s.name.as_str().starts_with("next.n.")).unwrap();
    assert_eq!(s.linkage, Linkage::Internal);
    assert!(s.init.is_some());
}

#[test]
fn static_assert() {
    clean("_Static_assert(sizeof(int) == 4, \"int\"); void f(void) { _Static_assert(1, \"ok\"); }");
    assert!(errors("_Static_assert(sizeof(int) == 5, \"int must be 5\");")[0]
        .contains("static assertion failed: int must be 5"));
    assert!(errors("int n; _Static_assert(n, \"x\");")[0].contains("not an integral constant expression"));
}

#[test]
fn generic_selection() {
    assert_eq!(type_of("int i; long l; double d;", "_Generic(i, int: 1, long: 2L, default: 3.0)"), "int");
    assert_eq!(type_of("int i; long l; double d;", "_Generic(l, int: 1, long: 2L, default: 3.0)"), "long");
    assert_eq!(type_of("int i; long l; double d;", "_Generic(d, int: 1, long: 2L, default: 3.0)"), "double");
    // lvalue conversion: a const int variable selects `int`
    assert_eq!(type_of("const int c;", "_Generic(c, int: 1, default: 2L)"), "int");
    // arrays decay
    assert_eq!(type_of("char s[4];", "_Generic(s, char *: 1, default: 2L)"), "int");
    assert!(
        errors("void f(int i) { _Generic(i, char: 1); }")[0].contains("not compatible with any generic association")
    );
    // only the selected association is checked
    clean("void f(int i) { int r = _Generic(i, int: 1, char *: undefined_function_never_checked(i)); (void)r; }");
}

#[test]
fn compound_literals() {
    clean("struct P {int x, y;}; int f(void) { struct P *p = &(struct P){1, 2}; int *a = (int[]){1, 2, 3}; return p->x + a[2]; }");
    let m = clean("int *p = (int[]){1, 2, 3}; struct S {int a;} *q = &(struct S){5};");
    assert_eq!(ty_of(&m, "p"), "int *");
}

#[test]
fn variadic_functions_and_builtins() {
    clean("#include <stdarg.h>\nint sum(int n, ...) { va_list ap; va_start(ap, n); int t = 0; for (int i = 0; i < n; i++) t += va_arg(ap, int); va_end(ap); return t; }");
    assert!(errors("#include <stdarg.h>\nint f(int n) { va_list ap; va_start(ap, n); return 0; }")[0]
        .contains("'va_start' used in function with fixed args"));
    assert!(errors("int g(int n, ...) { return __builtin_va_arg(n, int); }")[0]
        .contains("first argument to 'va_arg' is of type 'int' and not 'va_list'"));
    assert!(errors("void f(void) { __builtin_nosuch(); }")[0].contains("use of unknown builtin"));
    clean("#include <stdarg.h>\nvoid h(const char *f, va_list ap); void g(const char *f, ...) { va_list ap, bp; va_start(ap, f); va_copy(bp, ap); h(f, bp); va_end(bp); va_end(ap); }");
}

#[test]
fn func_name_is_available() {
    clean("const char *f(void) { return __func__; }");
}

// ───────────────────────────── initializers ─────────────────────────────

/// "offset:value" for every constant entry of a global's initializer.
fn entries(m: &HirModule, name: &str) -> Vec<String> {
    let p = sym(m, name).init.as_ref().unwrap();
    p.entries
        .iter()
        .map(|e| {
            let v = match &e.value {
                InitValue::Const(ConstVal::Int(v)) => format!("{}", *v as i64),
                InitValue::Const(ConstVal::Float(f)) => format!("{}", f),
                InitValue::Const(ConstVal::Addr { offset, .. }) => format!("&+{}", offset),
                InitValue::Bytes(b) => format!("{:?}", String::from_utf8_lossy(b)),
                InitValue::Expr(_) => "expr".to_string(),
            };
            format!("{}:{}", e.offset, v)
        })
        .collect()
}

#[test]
fn flat_and_nested_initializers() {
    let m = clean("int a[3] = {1, 2, 3}; struct P {int x; int y;} p = {4, 5}; int m[2][2] = {{1, 2}, {3, 4}};");
    assert_eq!(entries(&m, "a"), ["0:1", "4:2", "8:3"]);
    assert_eq!(entries(&m, "p"), ["0:4", "4:5"]);
    assert_eq!(entries(&m, "m"), ["0:1", "4:2", "8:3", "12:4"]);
}

#[test]
fn brace_elision() {
    let m = clean("int m[2][2] = {1, 2, 3, 4}; struct Q {int a[2]; int b;} q = {1, 2, 3}; struct R {struct { int a, b; } in; int c;} r = {1, 2, 3};");
    assert_eq!(entries(&m, "m"), ["0:1", "4:2", "8:3", "12:4"]);
    assert_eq!(entries(&m, "q"), ["0:1", "4:2", "8:3"]);
    assert_eq!(entries(&m, "r"), ["0:1", "4:2", "8:3"]);
    // partial elision: braces for the first row only
    let m = clean("int m[2][2] = {{1, 2}, 3, 4};");
    assert_eq!(entries(&m, "m"), ["0:1", "4:2", "8:3", "12:4"]);
}

#[test]
fn designated_initializers() {
    let m = clean("struct P {int x, y, z;} p = {.z = 9, .x = 1}; int a[5] = {[3] = 7, [1] = 2, 5}; struct W {int a[2]; struct P p;} w = {.p.y = 3, .a[1] = 4};");
    assert_eq!(entries(&m, "p"), ["0:1", "8:9"]);
    // after `[1] = 2` the next positional initializer is for element 2
    assert_eq!(entries(&m, "a"), ["4:2", "8:5", "12:7"]);
    assert_eq!(entries(&m, "w"), ["4:4", "12:3"]);
    // later designators override earlier ones
    let m = clean("int a[2] = {[0] = 1, [0] = 2};");
    assert_eq!(entries(&m, "a"), ["0:2"]);
}

#[test]
fn designators_continue_positionally() {
    let m = clean("struct P {int a, b, c, d;} p = {.b = 2, 3, 4};");
    assert_eq!(entries(&m, "p"), ["4:2", "8:3", "12:4"]);
}

#[test]
fn union_initializers() {
    let m = clean("union U {int i; float f;} u1 = {5}; union U u2 = {.f = 1.5f};");
    assert_eq!(entries(&m, "u1"), ["0:5"]);
    assert_eq!(entries(&m, "u2"), ["0:1.5"]);
    assert!(warnings("union U {int i; float f;} u = {1, 2};")[0].contains("excess elements in union initializer"));
}

#[test]
fn string_initializers() {
    let m = clean("char a[8] = \"abc\"; char b[3] = \"abc\"; char c[] = \"xy\"; const char *p = \"hi\"; char n[2][4] = {\"ab\", \"cd\"};");
    assert_eq!(entries(&m, "a"), [r#"0:"abc\0""#]);
    assert_eq!(entries(&m, "b"), [r#"0:"abc""#]);
    assert_eq!(entries(&m, "c"), [r#"0:"xy\0""#]);
    assert_eq!(entries(&m, "p"), ["0:&+0"]);
    assert_eq!(entries(&m, "n"), [r#"0:"ab\0""#, r#"4:"cd\0""#]);
    assert!(warnings("char s[2] = \"abc\";")[0].contains("initializer-string for char array is too long"));
    clean("char s[] = {\"braced\"}; unsigned char u[] = \"u\";");
}

#[test]
fn wide_string_initializers() {
    let m = clean("int w[] = L\"ab\";");
    assert_eq!(ty_of(&m, "w"), "int [3]");
    assert!(errors("char s[4] = L\"a\";")[0].contains("wide char array"));
}

#[test]
fn bitfield_initializers() {
    let m = clean("struct B {unsigned a : 3, b : 5; int c;} b = {5, 17, 9};");
    let p = sym(&m, "b").init.as_ref().unwrap();
    assert_eq!(p.entries[0].bit.unwrap().bit_offset, 0);
    assert_eq!(p.entries[1].bit.unwrap().bit_offset, 3);
    assert!(p.entries[2].bit.is_none());
}

#[test]
fn zero_fill_detection() {
    let m = clean("int a[3] = {1, 2, 3}; int b[3] = {1}; struct S {char c; int i;} s = {1, 2};");
    assert!(!sym(&m, "a").init.as_ref().unwrap().needs_zero);
    assert!(sym(&m, "b").init.as_ref().unwrap().needs_zero);
    assert!(sym(&m, "s").init.as_ref().unwrap().needs_zero, "padding bytes");
}

#[test]
fn excess_and_scalar_initializers() {
    assert!(warnings("int a[2] = {1, 2, 3};")[0].contains("excess elements in array initializer"));
    assert!(warnings("struct S {int a;} s = {1, 2};")[0].contains("excess elements in struct initializer"));
    assert!(warnings("int x = {1, 2};")[0].contains("excess elements in scalar initializer"));
    clean("int x = {5}; int y = {};");
}

#[test]
fn initializer_errors() {
    assert!(errors("int a[2] = 5;")[0].contains("array initializer must be an initializer list or string literal"));
    assert!(errors("struct S {int a;} s = 5;")[0]
        .contains("initializing 'struct S' with an expression of incompatible type 'int'"));
    assert!(errors("struct S {int a;} s = {.nope = 1};")[0]
        .contains("field designator 'nope' does not refer to any field in type 'struct S'"));
    assert!(errors("int a[2] = {[5] = 1};")[0].contains("array designator value is out of bounds"));
    assert!(errors("int n = 3; int a[2] = {[n] = 1};")[0].contains("not an integer constant expression"));
    assert!(errors("int x = 1; int y = x;")[0].contains("initializer element is not a compile-time constant"));
    assert!(errors("int f(void); int y = f();")[0].contains("initializer element is not a compile-time constant"));
}

#[test]
fn static_initializers_may_use_addresses() {
    let m = clean("int x; int arr[4]; int *p = &x; int *q = &arr[2]; int *r = arr + 1; char *s = \"lit\"; struct S {int a, b;} st; int *f = &st.b; long off = (long)&((struct S *)0)->b; void (*fp)(void);");
    assert_eq!(entries(&m, "p"), ["0:&+0"]);
    assert_eq!(entries(&m, "q"), ["0:&+8"]);
    assert_eq!(entries(&m, "r"), ["0:&+4"]);
    assert_eq!(entries(&m, "f"), ["0:&+4"]);
    assert_eq!(entries(&m, "off"), ["0:4"]);
}

#[test]
fn a_variable_is_in_scope_in_its_own_initializer() {
    clean("#include <stdlib.h>\nstruct N { int v; }; void f(void) { struct N *n = malloc(sizeof *n); int a[3] = {0}; unsigned long z = sizeof a; free(n); (void)z; }");
    let m = clean("int p_target; int *p = &p_target; int self_size = sizeof(self_size); static int *q = (int *)&q;");
    assert_eq!(init_const(&m, "self_size").0, 4);
}

#[test]
fn local_initializers_may_be_non_constant() {
    clean("int f(int x) { int a[3] = {x, x + 1, 3}; struct {int p, q;} s = {.q = x}; return a[0] + s.q; }");
}

#[test]
fn local_struct_copy_initialization() {
    clean("struct S {int a; char b[8];}; void f(struct S x) { struct S y = x; struct S z[2] = {x, y}; (void)z; }");
}

#[test]
fn float_initializers() {
    let m = clean("double d = 1; float f = 2.5; double e = 1.0f / 3;");
    let p = sym(&m, "d").init.as_ref().unwrap();
    assert!(matches!(p.entries[0].value, InitValue::Const(ConstVal::Float(v)) if v == 1.0));
}

// ───────────────────────────── whole-program checks ─────────────────────────────

#[test]
fn every_bundled_header_analyzes_cleanly() {
    for name in crate::headers::BUNDLED_NAMES {
        let r = run(&format!("#include <{}>\n", name));
        assert!(r.errors.is_empty(), "{name}: {:?}", r.errors);
        assert!(r.warnings.is_empty(), "{name}: {:?}", r.warnings);
    }
}

#[test]
fn libc_prototypes_and_usage() {
    clean(
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <ctype.h>
#include <errno.h>
#include <assert.h>
#include <stdint.h>
#include <stdbool.h>
#include <time.h>

int cmp(const void *a, const void *b) { return *(const int *)a - *(const int *)b; }

int main(int argc, char **argv) {
    int v[4] = {3, 1, 2, 0};
    qsort(v, 4, sizeof v[0], cmp);
    char buf[32];
    snprintf(buf, sizeof buf, "%d %s %f", v[0], argc > 1 ? argv[1] : "none", sqrt(2.0));
    puts(buf);
    char *dup = strdup(buf);
    if (!dup) { perror("strdup"); return EXIT_FAILURE; }
    size_t n = strlen(dup);
    for (size_t i = 0; i < n; i++) dup[i] = (char)toupper((unsigned char)dup[i]);
    free(dup);
    assert(n > 0);
    errno = 0;
    uint64_t big = UINT64_MAX;
    bool ok = big > 0 && M_PI > 3.0;
    div_t d = div(7, 2);
    clock_t t = clock();
    (void)t;
    return ok ? d.rem : 1;
}
"#,
    );
}

#[test]
fn realistic_program_with_structs_and_function_pointers() {
    clean(
        r#"
typedef struct node { int value; struct node *next; } node_t;
typedef int (*binop)(int, int);
static int add(int a, int b) { return a + b; }
static int mul(int a, int b) { return a * b; }
static binop table[] = { add, mul };
static node_t *push(node_t *head, node_t *n) { n->next = head; return n; }
int fold(const node_t *l, binop f, int init) {
    int acc = init;
    for (; l; l = l->next) acc = f(acc, l->value);
    return acc;
}
int run(void) {
    node_t a = {1, 0}, b = {2, 0}, c = {3, 0};
    node_t *list = push(push(push(0, &a), &b), &c);
    return fold(list, table[0], 0) + fold(list, table[1], 1);
}
"#,
    );
}

#[test]
fn errors_do_not_cascade() {
    // one undeclared identifier -> exactly one error, even though it feeds arithmetic and a call
    let e = errors("int g(int); int f(void) { return g(undefined_var + 1) * 2; }");
    assert_eq!(e.len(), 1, "{e:?}");
    // independent errors are all reported
    let e = errors("void f(void) { a = 1; b = 2; int c = d; }");
    assert_eq!(e.len(), 3, "{e:?}");
}

#[test]
fn hir_dump_shape() {
    let d = dump("int add(int a, int b) { return a + b; }");
    assert!(d.starts_with("Module\n"), "{d}");
    assert!(d.contains("Function add : 'int (int, int)'"), "{d}");
    assert!(d.contains("Param a#0 : 'int'"), "{d}");
    assert!(d.contains("Binary '+' : 'int'"), "{d}");
    assert!(d.contains("Return"), "{d}");
}

// ───────────────────────────── uninitialized variables ─────────────────────────────

fn uninit(src: &str) -> Vec<String> {
    let r = run(src);
    assert!(r.errors.is_empty(), "errors for {src:?}: {:?}", r.errors);
    r.warnings.into_iter().filter(|w| w.contains("uninitialized")).collect()
}

#[test]
fn uninitialized_read_is_reported_once() {
    let w = uninit("int f(void) { int x; int y = x + 1; return y + x; }");
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].contains("variable 'x' is uninitialized when used here"), "{w:?}");
}

#[test]
fn partially_assigned_variables_may_be_uninitialized() {
    let w = uninit("int f(int c) { int x; if (c) x = 1; return x; }");
    assert!(w.len() == 1 && w[0].contains("may be uninitialized"), "{w:?}");
    let w = uninit("int f(int n) { int x; for (int i = 0; i < n; i++) x = i; return x; }");
    assert!(w.len() == 1 && w[0].contains("may be uninitialized"), "{w:?}");
    let w = uninit("int f(int k) { int x; switch (k) { case 0: x = 1; break; case 1: x = 2; break; } return x; }");
    assert!(w.len() == 1 && w[0].contains("may be uninitialized"), "{w:?}");
}

#[test]
fn definitely_assigned_variables_do_not_warn() {
    assert!(uninit("int f(int c) { int x; if (c) x = 1; else x = 2; return x; }").is_empty());
    assert!(uninit("int f(int k) { int x; switch (k) { case 0: x = 1; break; default: x = 2; } return x; }").is_empty());
    assert!(uninit("int f(void) { int x; do { x = 3; } while (x < 0); return x; }").is_empty());
    assert!(uninit("int f(void) { int x; while (1) { x = 4; break; } return x; }").is_empty());
    assert!(uninit("int f(int a) { int x; if (a && (x = 5)) return x; return 0; }").is_empty());
    assert!(uninit("int f(int a) { int x; if (a || (x = 5)) return 0; return x; }").is_empty());
    assert!(uninit("int f(int a) { int x = a ? (a, 1) : 2; return x; }").is_empty());
}

#[test]
fn taking_the_address_counts_as_initializing() {
    assert!(uninit("int scanf(const char *, ...); int f(void) { int x; scanf(\"%d\", &x); return x; }").is_empty());
    assert!(uninit("void g(int *); int f(void) { int x; g(&x); return x; }").is_empty());
}

#[test]
fn noreturn_calls_end_a_path() {
    let src = "void abort(void) __attribute__((noreturn)); int f(int k) { int x; switch (k) { case 0: x = 1; break; default: abort(); } return x; }";
    assert!(uninit(src).is_empty(), "{:?}", uninit(src));
    assert!(uninit("#include <stdlib.h>\nint f(int k) { int r; if (k) r = 1; else exit(2); return r; }").is_empty());
}

#[test]
fn aggregates_params_and_gotos_are_not_tracked() {
    assert!(uninit("int f(int p) { return p; }").is_empty());
    assert!(uninit("int f(void) { int a[3]; a[0] = 1; return a[0]; }").is_empty());
    assert!(uninit("struct S { int a; }; int f(void) { struct S s; s.a = 1; return s.a; }").is_empty());
    assert!(uninit("int f(int c) { int x; if (c) goto set; return 0; set: x = 1; return x; }").is_empty());
}

#[test]
fn self_initialization_and_compound_assignment_read_the_variable() {
    let w = uninit("int f(void) { int x = x + 1; return x; }");
    assert!(w.len() == 1 && w[0].contains("'x'"), "{w:?}");
    let w = uninit("int f(void) { int x; x += 2; return x; }");
    assert!(w.len() == 1, "{w:?}");
    let w = uninit("int f(void) { int x; x++; return x; }");
    assert!(w.len() == 1, "{w:?}");
}

#[test]
fn a_variable_declared_in_a_loop_is_fresh_each_iteration() {
    let w =
        uninit("int f(int n) { int s = 0; for (int i = 0; i < n; i++) { int t; if (i) s += t; t = i; } return s; }");
    assert_eq!(w.len(), 1, "{w:?}");
}

// ───────────────────────────── -Wextra / -Wshadow / -Wsign-conversion ─────────────────────────────

fn extra_warnings(src: &str) -> Vec<String> {
    let r = run_with(src, |c| c.enable_everything());
    assert!(r.errors.is_empty(), "errors for {src:?}: {:?}", r.errors);
    r.warnings
}

#[test]
fn unused_parameters_are_reported_only_with_wextra() {
    let src = "int f(int used, int unused) { return used; }";
    assert!(warnings(src).is_empty(), "off by default and in -Wall");
    let w = extra_warnings(src);
    assert!(w.len() == 1 && w[0] == "unused parameter 'unused'", "{w:?}");
    assert!(extra_warnings("int f(int) { return 0; }").is_empty(), "unnamed parameters are never unused");
}

#[test]
fn sign_compare_flags_possibly_negative_operands() {
    let w = extra_warnings("int f(int a, unsigned b) { return a < b; }");
    assert!(
        w.iter().any(|m| m.contains("comparison of integers of different signs: 'int' and 'unsigned int'")),
        "{w:?}"
    );
    // fine: constants that are not negative, same signedness, promotion of unsigned char, wider signed type
    for ok in [
        "int f(unsigned b) { return b < 10; }",
        "int f(unsigned a, unsigned b) { return a < b; }",
        "int f(unsigned char c, int i) { return c < i; }",
        "int f(long a, unsigned b) { return a < b; }",
        "int f(int a, int b) { return a == b; }",
    ] {
        assert!(!extra_warnings(ok).iter().any(|m| m.contains("different signs")), "{ok}");
    }
    assert!(extra_warnings("int f(int a) { return a >= 0u; }").iter().any(|m| m.contains("different signs")));
}

#[test]
fn sign_conversion_is_off_unless_requested() {
    let src = "unsigned f(int a) { unsigned u = a; return u; }";
    assert!(warnings(src).is_empty());
    let w = extra_warnings(src);
    assert!(w.iter().any(|m| m.contains("implicit conversion changes signedness: 'int' to 'unsigned int'")), "{w:?}");
    assert!(!extra_warnings("unsigned f(void) { unsigned u = 5; return u; }").iter().any(|m| m.contains("signedness")));
}

#[test]
fn shadowing_locals_and_globals() {
    let src = "int g; int f(int p) { int x = 1; { int x = 2; p += x; } { int g = 3; p += g; } return x + p; }";
    assert!(warnings(src).is_empty());
    let w = extra_warnings(src);
    assert_eq!(w.iter().filter(|m| m.contains("declaration shadows a local variable")).count(), 1, "{w:?}");
    assert_eq!(w.iter().filter(|m| m.contains("shadows a variable in the global scope")).count(), 1, "{w:?}");
    let w = extra_warnings("int n; int f(int n) { return n; }");
    assert!(w.iter().any(|m| m.contains("shadows a variable in the global scope")), "{w:?}");
}

#[test]
fn empty_if_body() {
    let w = extra_warnings("int f(int a) { if (a); return 1; }");
    assert!(w.iter().any(|m| m == "if statement has empty body"), "{w:?}");
    assert!(!extra_warnings("int f(int a) { if (a) {} return 1; }").iter().any(|m| m.contains("empty body")));
}
