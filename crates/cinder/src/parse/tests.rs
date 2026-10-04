use super::*;
use crate::ast_dump::{declarator_text, sexp, specs_text, type_name_text};
use crate::diag::Level;
use crate::pp::{self, PpOptions};

struct P {
    tu: TranslationUnit,
    errors: Vec<String>,
    warnings: Vec<String>,
    sess: Session,
}

fn p(src: &str) -> P {
    let mut sess = Session::new();
    sess.diags.config.enable_all();
    let id = sess.sources.add_file("t.c", None, src.to_string());
    let toks = pp::preprocess(&mut sess, id, &PpOptions::default());
    let tu = parse(&mut sess, toks);
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for d in sess.diags.diagnostics() {
        match d.level {
            Level::Error | Level::Fatal => errors.push(d.message.clone()),
            Level::Warning => warnings.push(d.message.clone()),
            Level::Note => {}
        }
    }
    P { tu, errors, warnings, sess }
}

fn ok(src: &str) -> P {
    let r = p(src);
    assert!(r.errors.is_empty(), "unexpected errors for {src:?}: {:?}", r.errors);
    r
}

/// S-expression of the initializer of `int x = <e>;` (with `typedef int T;` in scope).
fn ex(e: &str) -> String {
    let r = ok(&format!("typedef int T; struct S {{ int a, b; }}; int x = {};", e));
    for d in &r.tu.decls {
        if let ExternalDecl::Decl(decl) = d {
            if let Some(id) = decl.declarators.first() {
                if id.declarator.name().is_some_and(|n| n.name.as_str() == "x") {
                    return match id.init.as_ref().unwrap() {
                        Initializer::Expr(e) => sexp(e),
                        Initializer::List(l) => crate::ast_dump::init_text(l),
                    };
                }
            }
        }
    }
    panic!("no x");
}

/// "specs | declarator" for each declarator of the last top-level declaration.
fn decls(src: &str) -> Vec<String> {
    let r = ok(src);
    let ExternalDecl::Decl(d) = r.tu.decls.last().unwrap() else { panic!("not a declaration") };
    d.declarators.iter().map(|i| format!("{} | {}", specs_text(&d.specs), declarator_text(&i.declarator))).collect()
}

fn first_decl(src: &str) -> String {
    decls(src).remove(0)
}

fn func_body(src: &str) -> Vec<BlockItem> {
    let r = ok(src);
    for d in r.tu.decls {
        if let ExternalDecl::Func(f) = d {
            if let StmtKind::Compound(items) = f.body.kind {
                return items;
            }
        }
    }
    panic!("no function");
}

fn stmt_of(src: &str) -> Stmt {
    let items = func_body(&format!("typedef int T; void f(void) {{ {} }}", src));
    match items.into_iter().next().unwrap() {
        BlockItem::Stmt(s) => s,
        BlockItem::Decl(_) => panic!("declaration"),
    }
}

// ───────────────────────────── expressions ─────────────────────────────

#[test]
fn precedence_and_associativity() {
    assert_eq!(ex("1 + 2 * 3"), "(+ 1 (* 2 3))");
    assert_eq!(ex("1 * 2 + 3"), "(+ (* 1 2) 3)");
    assert_eq!(ex("1 - 2 - 3"), "(- (- 1 2) 3)");
    assert_eq!(ex("1 << 2 + 3"), "(<< 1 (+ 2 3))");
    assert_eq!(ex("1 < 2 == 3 < 4"), "(== (< 1 2) (< 3 4))");
    assert_eq!(ex("1 & 2 ^ 3 | 4"), "(| (^ (& 1 2) 3) 4)");
    assert_eq!(ex("1 || 2 && 3"), "(|| 1 (&& 2 3))");
    assert_eq!(ex("1 == 2 & 3"), "(& (== 1 2) 3)");
    assert_eq!(ex("(1 + 2) * 3"), "(* (+ 1 2) 3)");
}

#[test]
fn assignment_and_conditional() {
    assert_eq!(ex("(a = b = c)"), "(= a (= b c))");
    assert_eq!(ex("(a += b *= 2)"), "(+= a (*= b 2))");
    assert_eq!(ex("(a <<= 1)"), "(<<= a 1)");
    assert_eq!(ex("a ? b : c ? d : e"), "(?: a b (?: c d e))");
    assert_eq!(ex("a ? b ? c : d : e"), "(?: a (?: b c d) e)");
    assert_eq!(ex("a ? (b, c) : d"), "(?: a (, b c) d)");
    assert_eq!(ex("a ? b, c : d"), "(?: a (, b c) d)");
    assert_eq!(ex("(a, b, c)"), "(, (, a b) c)");
    assert_eq!(ex("a || b ? c : d"), "(?: (|| a b) c d)");
}

#[test]
fn unary_and_postfix() {
    assert_eq!(ex("-a * b"), "(* (neg a) b)");
    assert_eq!(ex("!a && b"), "(&& (! a) b)");
    assert_eq!(ex("*p++"), "(deref (post++ p))");
    assert_eq!(ex("&*p"), "(addr (deref p))");
    assert_eq!(ex("- -a"), "(neg (neg a))");
    assert_eq!(ex("a+++b"), "(+ (post++ a) b)");
    assert_eq!(ex("++a + b++"), "(+ (pre++ a) (post++ b))");
    assert_eq!(ex("~a"), "(~ a)");
    assert_eq!(ex("a[1][2]"), "([] ([] a 1) 2)");
    assert_eq!(ex("p->x.y->z"), "(-> (. (-> p x) y) z)");
    assert_eq!(ex("f(1, 2)(3)"), "(call (call f 1 2) 3)");
    assert_eq!(ex("f()"), "(call f)");
    assert_eq!(ex("f(a = 1, b)"), "(call f (= a 1) b)");
    assert_eq!(ex("a.b++"), "(post++ (. a b))");
}

#[test]
fn sizeof_and_casts() {
    assert_eq!(ex("sizeof x + 1"), "(+ (sizeof x) 1)");
    assert_eq!(ex("sizeof(int) * 2"), "(* (sizeof <int>) 2)");
    assert_eq!(ex("sizeof (T)"), "(sizeof <T>)");
    assert_eq!(ex("sizeof(x)"), "(sizeof x)");
    assert_eq!(ex("sizeof x[0]"), "(sizeof ([] x 0))");
    assert_eq!(ex("(int)3.5"), "(cast <int> 3.5)");
    assert_eq!(ex("(T)x"), "(cast <T> x)");
    assert_eq!(ex("(T*)p"), "(cast <T *> p)");
    assert_eq!(ex("(unsigned long)-1"), "(cast <unsigned long> (neg 1))");
    assert_eq!(ex("(char (*)[4])p"), "(cast <char (*)[4]> p)");
    assert_eq!(ex("(void (*)(int))p"), "(cast <void (*)(int)> p)");
    assert_eq!(ex("(int)(char)x"), "(cast <int> (cast <char> x))");
    assert_eq!(ex("_Alignof(double)"), "(alignof <double>)");
}

#[test]
fn cast_versus_parenthesized_expression() {
    // `x` is not a typedef: this is a parenthesised expression plus 1
    assert_eq!(ex("(x) + 1"), "(+ x 1)");
    // `T` is a typedef: this is a cast applied to unary +1
    assert_eq!(ex("(T) + 1"), "(cast <T> (pos 1))");
    assert_eq!(ex("(T)(x)"), "(cast <T> x)");
    assert_eq!(ex("(x)(y)"), "(call x y)");
    assert_eq!(ex("(*f)(1)"), "(call (deref f) 1)");
}

#[test]
fn compound_literals() {
    assert_eq!(ex("(int[]){1, 2, 3}[1]"), "([] (complit <int []> {1, 2, 3}) 1)");
    assert_eq!(ex("((struct S){1, 2}).b"), "(. (complit <struct S> {1, 2}) b)");
    assert_eq!(ex("&(struct S){.a = 1, .b = 2}"), "(addr (complit <struct S> {.a=1, .b=2}))");
    assert_eq!(ex("(T){0}"), "(complit <T> {0})");
}

#[test]
fn generic_selection() {
    assert_eq!(ex("_Generic(1, int: 10, long: 20, default: 30)"), "(_Generic 1 <int>:10 <long>:20 default:30)");
}

#[test]
fn builtins() {
    assert_eq!(ex("__builtin_va_arg(ap, int)"), "(va_arg ap <int>)");
    assert_eq!(ex("__builtin_va_arg(ap, char *)"), "(va_arg ap <char *>)");
    assert_eq!(ex("__builtin_offsetof(struct S, b)"), "(offsetof <struct S> .b)");
    assert_eq!(ex("__builtin_offsetof(struct S, b[2].c)"), "(offsetof <struct S> .b [2] .c)");
}

#[test]
fn literals_and_string_concatenation() {
    assert_eq!(ex("42"), "42");
    assert_eq!(ex("0x1Fu"), "0x1Fu");
    assert_eq!(ex("1.5e3f"), "1.5e3f");
    assert_eq!(ex("'a'"), "'a'");
    assert_eq!(ex("\"ab\" \"cd\""), "\"abcd\"");
    assert_eq!(ex("\"a\\n\" \"b\""), "\"a\\nb\"");
    assert_eq!(ex("L\"a\" \"b\""), "L\"ab\"");
    assert_eq!(ex("\"\\x41\\102\""), "\"AB\"");
}

#[test]
fn string_literal_escape_warning() {
    let r = ok("char *s = \"a\\qb\";");
    assert_eq!(r.warnings, ["unknown escape sequence '\\q'"]);
}

#[test]
fn string_concat_kind_mismatch_is_an_error() {
    let r = p("void *s = L\"a\" u\"b\";");
    assert!(r.errors[0].contains("non-standard concatenation"), "{:?}", r.errors);
}

// ───────────────────────────── declarations ─────────────────────────────

#[test]
fn declaration_specifier_combinations() {
    assert_eq!(first_decl("int x;"), "int | x");
    assert_eq!(first_decl("unsigned x;"), "unsigned int | x");
    assert_eq!(first_decl("long unsigned int x;"), "unsigned long | x");
    assert_eq!(first_decl("int long long unsigned x;"), "unsigned long long | x");
    assert_eq!(first_decl("short int x;"), "short | x");
    assert_eq!(first_decl("signed char x;"), "signed char | x");
    assert_eq!(first_decl("char x;"), "char | x");
    assert_eq!(first_decl("long double x;"), "long double | x");
    assert_eq!(first_decl("long long x;"), "long long | x");
    assert_eq!(first_decl("static const volatile int x;"), "static const volatile int | x");
    assert_eq!(first_decl("const static int x;"), "static const int | x");
    assert_eq!(first_decl("extern _Bool b;"), "extern _Bool | b");
    assert_eq!(first_decl("_Thread_local int t;"), "_Thread_local int | t");
    assert_eq!(first_decl("static inline int f(void);"), "static inline int | f(void)");
}

#[test]
fn invalid_specifier_combinations() {
    assert!(p("int float x;").errors[0].contains("cannot combine"));
    assert!(p("long long long x;").errors[0].contains("cannot combine"));
    assert!(p("unsigned signed x;").errors[0].contains("cannot combine"));
    assert!(p("short long x;").errors[0].contains("cannot combine"));
    assert!(p("char int x;").errors[0].contains("cannot combine"));
    assert!(p("static extern int x;").errors[0].contains("cannot combine with previous 'static'"));
    assert!(p("_Complex double z;").errors[0].contains("not yet supported"));
}

#[test]
fn declarator_shapes() {
    assert_eq!(first_decl("int *p;"), "int | *p");
    assert_eq!(first_decl("int **p;"), "int | **p");
    assert_eq!(first_decl("char *const *p;"), "char | *const *p");
    assert_eq!(first_decl("int a[3];"), "int | a[3]");
    assert_eq!(first_decl("int a[2][3];"), "int | a[2][3]");
    assert_eq!(first_decl("int a[];"), "int | a[]");
    assert_eq!(first_decl("int *a[3];"), "int | *a[3]");
    assert_eq!(first_decl("int (*a)[3];"), "int | (*a)[3]");
    assert_eq!(first_decl("int f(int, char);"), "int | f(int, char)");
    assert_eq!(first_decl("int f(int a, char *b);"), "int | f(int a, char *b)");
    assert_eq!(first_decl("int (*fp)(int, char);"), "int | (*fp)(int, char)");
    assert_eq!(first_decl("int *(*fp)(void);"), "int | *(*fp)(void)");
    assert_eq!(first_decl("int (*arr[5])(void);"), "int | (*arr[5])(void)");
    assert_eq!(first_decl("int (*(*f)(int))[3];"), "int | (*(*f)(int))[3]");
    assert_eq!(first_decl("int f(int, ...);"), "int | f(int, ...)");
    assert_eq!(first_decl("int f();"), "int | f()");
    assert_eq!(
        first_decl("int f(int a[static 3], int b[*], int c[const 2]);"),
        "int | f(int a[static 3], int b[*], int c[const 2])"
    );
}

#[test]
fn the_classic_signal_declaration() {
    let d = first_decl("void (*signal(int sig, void (*handler)(int)))(int);");
    assert_eq!(d, "void | (*signal(int sig, void (*handler)(int)))(int)");
}

#[test]
fn abstract_declarators_in_parameters() {
    assert_eq!(first_decl("void f(int (*)(void), char *, int [3]);"), "void | f(int (*)(void), char *, int [3])");
    assert_eq!(first_decl("void g(int (int), void (void));"), "void | g(int (int), void (void))");
}

#[test]
fn multiple_declarators() {
    assert_eq!(decls("int x, *y, z[3], (*w)(void);"), ["int | x", "int | *y", "int | z[3]", "int | (*w)(void)"]);
    let r = ok("int a = 1, b = 2;");
    let ExternalDecl::Decl(d) = &r.tu.decls[0] else { panic!() };
    assert!(d.declarators.iter().all(|x| x.init.is_some()));
}

#[test]
fn typedef_disambiguation() {
    // `T * x;` is a declaration when T is a typedef...
    let items = func_body("typedef int T; void f(void) { T * x; }");
    assert!(matches!(items[0], BlockItem::Decl(_)));
    // ...and an expression otherwise
    let items = func_body("int T; void f(void) { T * x; }");
    assert!(matches!(items[0], BlockItem::Stmt(_)));
}

#[test]
fn typedef_shadowing_in_inner_scope() {
    let items = func_body("typedef int T; void f(void) { { int T = 3; T * 2; } T * y; }");
    let BlockItem::Stmt(Stmt { kind: StmtKind::Compound(inner), .. }) = &items[0] else { panic!() };
    assert!(matches!(inner[0], BlockItem::Decl(_)));
    assert!(matches!(inner[1], BlockItem::Stmt(_))); // `T * 2;` — T is a variable here
    assert!(matches!(items[1], BlockItem::Decl(_))); // outside, T is a type again
}

#[test]
fn typedef_declarations() {
    assert_eq!(first_decl("typedef unsigned long size_t;"), "typedef unsigned long | size_t");
    assert_eq!(
        first_decl("typedef int (*cmp_t)(const void *, const void *);"),
        "typedef int | (*cmp_t)(const void *, const void *)"
    );
    // typedef names can be used straight away
    let r = ok("typedef struct node node_t; struct node { node_t *next; }; node_t *head;");
    assert_eq!(r.tu.decls.len(), 3);
}

#[test]
fn enumerators_hide_typedef_names() {
    let r = ok("typedef int A; enum { A = 1 }; int x = A;");
    assert_eq!(r.tu.decls.len(), 3);
}

#[test]
fn struct_and_union_definitions() {
    let r = ok("struct P { int x, y; char *name; } p; union U { int i; float f; } u;");
    let ExternalDecl::Decl(d) = &r.tu.decls[0] else { panic!() };
    let TypeSpecKind::Record(rec) = &d.specs.ty.as_ref().unwrap().kind else { panic!() };
    assert!(!rec.is_union);
    assert_eq!(rec.tag.unwrap().name.as_str(), "P");
    let members = rec.members.as_ref().unwrap();
    assert_eq!(members.len(), 2);
    let ExternalDecl::Decl(d) = &r.tu.decls[1] else { panic!() };
    let TypeSpecKind::Record(rec) = &d.specs.ty.as_ref().unwrap().kind else { panic!() };
    assert!(rec.is_union);
}

#[test]
fn bitfields_and_anonymous_members() {
    let r =
        ok("struct B { unsigned a : 3, b : 5; int : 0; int c; struct { int x; }; union { int u; float f; } named; };");
    let ExternalDecl::Decl(d) = &r.tu.decls[0] else { panic!() };
    let TypeSpecKind::Record(rec) = &d.specs.ty.as_ref().unwrap().kind else { panic!() };
    let members = rec.members.as_ref().unwrap();
    assert_eq!(members.len(), 5);
    let MemberDecl::Field { declarators, .. } = &members[0] else { panic!() };
    assert_eq!(declarators.len(), 2);
    assert!(declarators.iter().all(|d| d.bit_width.is_some()));
    let MemberDecl::Field { declarators, .. } = &members[1] else { panic!() };
    assert!(declarators[0].declarator.is_none()); // `int : 0;`
    let MemberDecl::Field { declarators, .. } = &members[3] else { panic!() };
    assert!(declarators.is_empty()); // anonymous struct member
}

#[test]
fn struct_references_and_incomplete_types() {
    let r = ok("struct S; struct S *p; struct S { struct S *next; int v; };");
    assert_eq!(r.tu.decls.len(), 3);
}

#[test]
fn enum_definitions() {
    let r = ok("enum Color { RED, GREEN = 5, BLUE, LAST = BLUE + 10, };");
    let ExternalDecl::Decl(d) = &r.tu.decls[0] else { panic!() };
    let TypeSpecKind::Enum(e) = &d.specs.ty.as_ref().unwrap().kind else { panic!() };
    let list = e.enumerators.as_ref().unwrap();
    assert_eq!(list.len(), 4);
    assert!(list[0].value.is_none());
    assert_eq!(sexp(list[1].value.as_ref().unwrap()), "5");
    assert_eq!(sexp(list[3].value.as_ref().unwrap()), "(+ BLUE 10)");
}

#[test]
fn initializers() {
    assert_eq!(ex("{1, 2}"), "{1, 2}");
    assert_eq!(ex("{.a = 1, .b = {2, 3}}"), "{.a=1, .b={2, 3}}");
    assert_eq!(ex("{[2] = 5, [4] = 6, 7}"), "{[2]=5, [4]=6, 7}");
    assert_eq!(ex("{.a.b = 1, [1].c = 2}"), "{.a.b=1, [1].c=2}");
    assert_eq!(ex("{1, 2,}"), "{1, 2}");
    assert_eq!(ex("{}"), "{}");
    let r = ok("char s[] = \"hi\"; int a[2][2] = {{1,2},{3,4}};");
    assert_eq!(r.tu.decls.len(), 2);
}

#[test]
fn static_assert_and_alignas() {
    let r = ok(
        "_Static_assert(sizeof(int) == 4, \"int is 4 bytes\"); _Alignas(16) char buf[32]; _Alignas(double) char b2[8];",
    );
    let ExternalDecl::StaticAssert(sa) = &r.tu.decls[0] else { panic!() };
    assert_eq!(sa.message.as_deref(), Some("int is 4 bytes"));
    let ExternalDecl::Decl(d) = &r.tu.decls[1] else { panic!() };
    assert!(matches!(d.specs.align, Some((AlignSpec::Expr(_), _))));
    let ExternalDecl::Decl(d) = &r.tu.decls[2] else { panic!() };
    assert!(matches!(d.specs.align, Some((AlignSpec::Type(_), _))));
}

#[test]
fn attributes_are_parsed() {
    let r = ok("int x __attribute__((unused)); __attribute__((aligned(16))) int y; struct __attribute__((packed)) S { char c; int i; };");
    assert_eq!(r.tu.decls.len(), 3);
    let ExternalDecl::Decl(d) = &r.tu.decls[1] else { panic!() };
    assert_eq!(d.specs.attrs[0].name.name.as_str(), "aligned");
    assert_eq!(sexp(&d.specs.attrs[0].args[0]), "16");
    let ExternalDecl::Decl(d) = &r.tu.decls[2] else { panic!() };
    let TypeSpecKind::Record(rec) = &d.specs.ty.as_ref().unwrap().kind else { panic!() };
    assert_eq!(rec.attrs[0].name.name.as_str(), "packed");
}

#[test]
fn pragma_pack_is_recorded_on_structs() {
    let r = ok("struct A {int a;};\n#pragma pack(push, 1)\nstruct B {char c; int i;};\n#pragma pack(pop)\nstruct C {int a;};\n#pragma pack(2)\nstruct D {int a;};\n#pragma pack()\nstruct E {int a;};\n");
    let pack = |i: usize| -> Option<u32> {
        let ExternalDecl::Decl(d) = &r.tu.decls[i] else { panic!() };
        let TypeSpecKind::Record(rec) = &d.specs.ty.as_ref().unwrap().kind else { panic!() };
        rec.pack
    };
    assert_eq!(pack(0), None);
    assert_eq!(pack(1), Some(1));
    assert_eq!(pack(2), None);
    assert_eq!(pack(3), Some(2));
    assert_eq!(pack(4), None);
}

#[test]
fn unknown_pragma_warns() {
    let r = ok("#pragma frobnicate on\nint x;");
    assert_eq!(r.warnings, ["unknown pragma ignored: 'frobnicate'"]);
    assert!(ok("#pragma GCC diagnostic push\nint y;").warnings.is_empty());
}

// ───────────────────────────── statements ─────────────────────────────

#[test]
fn if_else_and_dangling_else() {
    let s = stmt_of("if (a) if (b) x(); else y();");
    let StmtKind::If { then, els, .. } = s.kind else { panic!() };
    assert!(els.is_none(), "else binds to the inner if");
    let StmtKind::If { els, .. } = then.kind else { panic!() };
    assert!(els.is_some());
}

#[test]
fn loops() {
    assert!(matches!(stmt_of("while (1) ;").kind, StmtKind::While { .. }));
    assert!(matches!(stmt_of("do x(); while (0);").kind, StmtKind::DoWhile { .. }));
    let StmtKind::For { init, cond, step, .. } = stmt_of("for (;;) ;").kind else { panic!() };
    assert!(matches!(init, ForInit::None) && cond.is_none() && step.is_none());
    let StmtKind::For { init, .. } = stmt_of("for (i = 0; i < 3; i++) ;").kind else { panic!() };
    assert!(matches!(init, ForInit::Expr(_)));
    let StmtKind::For { init, .. } = stmt_of("for (int i = 0, j = 1; i < 3; i++) ;").kind else { panic!() };
    let ForInit::Decl(d) = init else { panic!() };
    assert_eq!(d.declarators.len(), 2);
    // a `for` declaration of a typedef-named variable: scope ends with the loop
    let items = func_body("typedef int T; void f(void) { for (int T = 0; T < 3; T++) ; T * q; }");
    assert!(matches!(items[1], BlockItem::Decl(_)));
}

#[test]
fn switch_case_default_goto_label() {
    let s = stmt_of("switch (x) { case 1: a(); break; case 2: default: b(); }");
    let StmtKind::Switch { body, .. } = s.kind else { panic!() };
    let StmtKind::Compound(items) = body.kind else { panic!() };
    assert_eq!(items.len(), 3);
    assert!(matches!(stmt_of("goto done;").kind, StmtKind::Goto(_)));
    let s = stmt_of("done: return;");
    let StmtKind::Label { name, body } = s.kind else { panic!() };
    assert_eq!(name.name.as_str(), "done");
    assert!(matches!(body.kind, StmtKind::Return(None)));
    // label before a closing brace gets an empty statement
    let _ = stmt_of("{ end: }");
}

#[test]
fn label_with_typedef_name() {
    // labels live in their own namespace
    let s = stmt_of("T: ;");
    assert!(matches!(s.kind, StmtKind::Label { .. }));
}

#[test]
fn return_forms() {
    assert!(matches!(stmt_of("return;").kind, StmtKind::Return(None)));
    assert!(matches!(stmt_of("return 1 + 2;").kind, StmtKind::Return(Some(_))));
    assert!(matches!(stmt_of(";").kind, StmtKind::Empty));
}

#[test]
fn mixed_declarations_and_statements() {
    let items = func_body("void f(void) { int a = 1; a++; int b = a; { int c; } }");
    assert!(matches!(items[0], BlockItem::Decl(_)));
    assert!(matches!(items[1], BlockItem::Stmt(_)));
    assert!(matches!(items[2], BlockItem::Decl(_)));
    assert!(matches!(items[3], BlockItem::Stmt(_)));
}

// ───────────────────────────── functions ─────────────────────────────

#[test]
fn function_definitions() {
    let r = ok("int main(void) { return 0; }\nstatic int add(int a, int b) { return a + b; }\nvoid g(int n, ...) { }");
    assert_eq!(r.tu.decls.len(), 3);
    for d in &r.tu.decls {
        assert!(matches!(d, ExternalDecl::Func(_)));
    }
    let ExternalDecl::Func(f) = &r.tu.decls[1] else { panic!() };
    assert_eq!(declarator_text(&f.declarator), "add(int a, int b)");
}

#[test]
fn function_returning_function_pointer() {
    let r = ok("int (*pick(int which))(int) { return 0; }");
    let ExternalDecl::Func(f) = &r.tu.decls[0] else { panic!() };
    assert_eq!(f.declarator.name().unwrap().name.as_str(), "pick");
}

#[test]
fn param_shadows_typedef_inside_body() {
    // inside the body `T` is the parameter, so `T * 2` is an expression
    let items = func_body("typedef int T; int f(int T) { T * 2; return T; }");
    assert!(matches!(items[0], BlockItem::Stmt(_)));
}

#[test]
fn function_prototype_with_unnamed_void() {
    let d = first_decl("int f(void);");
    assert_eq!(d, "int | f(void)");
}

// ───────────────────────────── error recovery ─────────────────────────────

fn msgs(src: &str) -> Vec<String> {
    p(src).errors
}

#[test]
fn missing_semicolons_recover() {
    let r = p("int f(void) {\n    int x = 1\n    int y = 2\n    return x + y\n}\n");
    assert_eq!(r.errors.len(), 3, "{:?}", r.errors);
    assert!(r.errors[0].contains("expected ';' at end of declaration"));
    assert!(r.errors[2].contains("expected ';' after return statement"));
    // the caret sits right after the last token on the line
    let d = &r.sess.diags.diagnostics()[0];
    let loc = r.sess.sources.loc(d.span).unwrap();
    assert_eq!((loc.line, loc.col), (2, 14));
    assert!(d.fixit.is_some());
}

#[test]
fn missing_semicolon_after_tag_definition() {
    let r = p("struct S { int a; }\nint main(void) { return 0; }\n");
    assert_eq!(r.errors, ["expected ';' after struct"]);
    assert!(r.tu.decls.iter().any(|d| matches!(d, ExternalDecl::Func(_))), "main must still parse");
    assert_eq!(p("enum E { A, B }\nint x;").errors, ["expected ';' after enum"]);
    assert_eq!(p("union U { int a; }\nstatic int y;").errors, ["expected ';' after union"]);
    // block scope too
    let r = p("void f(void) { struct T { int a; } int z; }");
    assert_eq!(r.errors, ["expected ';' after struct"]);
    // but a declarator after the tag is fine
    ok("struct S { int a; } s, *p; enum E { A } e;");
}

#[test]
fn expected_expression() {
    let r = p("int x = ;");
    assert_eq!(r.errors, ["expected expression"]);
    assert!(msgs("void f(void) { x = ; }")[0].contains("expected expression"));
    assert!(msgs("int x = 1 + ;")[0].contains("expected expression"));
}

#[test]
fn unbalanced_parens_note_the_opener() {
    let r = p("int f(void) { return (1 + 2; }");
    assert!(r.errors[0].contains("expected ')'"), "{:?}", r.errors);
    let d = &r.sess.diags.diagnostics()[0];
    assert!(d.notes[0].message.contains("to match this '('"));
    assert!(msgs("int x[3;")[0].contains("expected ']'"));
}

#[test]
fn multiple_independent_errors_are_all_reported() {
    let r = p("int a = ;\nint b = ;\nint c = 1;\nvoid f(void) { int x = ; int y = ; }\nint d = ;\n");
    assert_eq!(r.errors.len(), 5, "{:?}", r.errors);
}

#[test]
fn errors_do_not_hide_later_functions() {
    let r = p("int f(void) { return ; ; ; 1 +; }\nint g(void) { return 2 }\nint h(void) { return 3; }\n");
    assert!(r.errors.len() >= 2, "{:?}", r.errors);
    assert!(r.errors.iter().any(|e| e.contains("expected ';' after return statement")));
    // the third function still parsed
    assert!(r
        .tu
        .decls
        .iter()
        .any(|d| matches!(d, ExternalDecl::Func(f) if f.declarator.name().unwrap().name.as_str() == "h")));
}

#[test]
fn unknown_type_name() {
    let r = p("foo x;\nint ok;");
    assert_eq!(r.errors, ["unknown type name 'foo'"]);
    let r = p("void f(void) { bar y; int z; }");
    assert_eq!(r.errors, ["unknown type name 'bar'"]);
    let r = p("void f(void) { baz *y; }");
    // `baz * y;` is a valid expression statement as far as the parser can tell
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(msgs("void f(foo x) {}"), ["unknown type name 'foo'"]);
}

#[test]
fn stray_tokens_at_file_scope() {
    assert!(msgs("}\nint x;")[0].contains("extraneous closing brace"));
    let r = p("+ int x;\nint y;");
    assert!(!r.errors.is_empty());
    assert!(r.tu.decls.iter().any(|d| matches!(d, ExternalDecl::Decl(_))));
}

#[test]
fn nested_function_definition_is_rejected() {
    let r = p("void f(void) { int g(void) { return 1; } }");
    assert!(r.errors[0].contains("function definition is not allowed here"), "{:?}", r.errors);
}

#[test]
fn unterminated_block_reports_the_opening_brace() {
    let r = p("void f(void) {\n  int x;\n");
    assert!(r.errors[0].contains("expected '}'"));
    let d = &r.sess.diags.diagnostics()[0];
    assert!(d.notes[0].message.contains("to match this '{'"));
}

#[test]
fn control_flow_syntax_errors() {
    assert!(msgs("void f(void) { if x) ; }")[0].contains("expected '(' after 'if'"));
    assert!(!msgs("void f(void) { while (1) }").is_empty());
    assert!(msgs("void f(void) { do ; (1); }")[0].contains("expected 'while'"));
    assert!(!msgs("void f(void) { for (;;) }").is_empty());
    assert!(msgs("void f(void) { x = a ? b; }")[0].contains("expected ':'"));
}

#[test]
fn unsupported_features_are_diagnosed_not_ignored() {
    assert!(msgs("int f(a, b) int a; int b; { return 0; }")[0].contains("K&R"));
    assert!(msgs("void f(void) { __asm__(\"nop\"); }")[0].contains("not yet supported: inline assembly"));
    assert!(msgs("void f(void) { void *p = &&l; l: ; }")[0].contains("computed goto"));
    assert!(msgs("int x = ({ 1; });")[0].contains("statement expressions"));
    assert!(msgs("void f(int x) { switch (x) { case 1 ... 3: break; } }")[0].contains("case ranges"));
}

#[test]
fn lexical_errors_surface_through_the_parser() {
    let r = p("int x = 'a;\nint y = 1;");
    assert!(r.errors[0].contains("missing terminating '"), "{:?}", r.errors);
    let r = p("int x = 1 @ 2;");
    assert!(r.errors[0].contains("stray '@'"), "{:?}", r.errors);
}

#[test]
fn parser_always_terminates() {
    // adversarial garbage must neither loop nor panic
    let junk = [
        "((((((((((",
        "}}}}}}",
        "int int int",
        "struct struct {",
        "{ { { {",
        "int (",
        "int [",
        "= = = =",
        "case case",
        "else else",
        "typedef typedef",
        "enum { , , }",
        "int a[",
        "f(",
        "void f(int a,",
        "_Generic(",
        "(int){",
        "{ .x = }",
        "[1] = 2",
        "int x = {1, 2",
        "struct S { int a; ",
        "union { int",
        "static static",
        "\"abc",
        "'",
        "int f(void) { if",
        "x ? : ;",
    ];
    for j in junk {
        let r = p(j);
        assert!(!r.errors.is_empty() || j.is_empty(), "no error for {j:?}");
    }
}

// ───────────────────────────── whole files ─────────────────────────────

#[test]
fn every_bundled_header_parses() {
    for name in crate::headers::BUNDLED_NAMES {
        let r = p(&format!("#include <{}>\n", name));
        assert!(r.errors.is_empty(), "{name}: {:?}", r.errors);
        assert!(r.warnings.is_empty(), "{name}: {:?}", r.warnings);
    }
}

#[test]
fn all_headers_together_parse() {
    let mut src = String::new();
    for name in crate::headers::BUNDLED_NAMES {
        src.push_str(&format!("#include <{}>\n", name));
    }
    src.push_str("int main(void) { printf(\"%d\\n\", 1); return 0; }\n");
    let r = ok(&src);
    assert!(r.tu.decls.len() > 200, "{}", r.tu.decls.len());
}

#[test]
fn realistic_program() {
    let src = r#"
#include <stdio.h>
#include <stdlib.h>

typedef struct node {
    int value;
    struct node *next;
} node_t;

static node_t *push(node_t *head, int v) {
    node_t *n = malloc(sizeof *n);
    if (!n) { perror("malloc"); exit(1); }
    n->value = v;
    n->next = head;
    return n;
}

int main(int argc, char **argv) {
    node_t *list = NULL;
    for (int i = 0; i < 10; i++)
        list = push(list, i * i);
    int sum = 0;
    for (node_t *p = list; p; p = p->next)
        sum += p->value;
    switch (sum % 3) {
    case 0: puts("zero"); break;
    case 1: puts("one"); break;
    default: puts("two");
    }
    printf("%d %s\n", sum, argc > 1 ? argv[1] : "none");
    return sum > 100 ? 0 : 1;
}
"#;
    let r = ok(src);
    assert_eq!(r.sess.diags.warning_count(), 0);
    assert!(r
        .tu
        .decls
        .iter()
        .any(|d| matches!(d, ExternalDecl::Func(f) if f.declarator.name().unwrap().name.as_str() == "main")));
}

#[test]
fn ast_dump_has_expected_shape() {
    let r = ok("int add(int a, int b) { return a + b * 2; }\n");
    let text = crate::ast_dump::dump(&r.sess.sources, &r.tu);
    assert!(text.starts_with("TranslationUnit\n`-FunctionDef add"), "{text}");
    assert!(text.contains("Declarator 'add(int a, int b)'"), "{text}");
    assert!(text.contains("BinaryOp '+'"), "{text}");
    assert!(text.contains("BinaryOp '*'"), "{text}");
    assert!(text.contains("IntLiteral 2"), "{text}");
}

#[test]
fn spans_cover_whole_constructs() {
    let src = "int f(void) {\n  return 1 +\n    2;\n}\n";
    let r = ok(src);
    let ExternalDecl::Func(f) = &r.tu.decls[0] else { panic!() };
    let StmtKind::Compound(items) = &f.body.kind else { panic!() };
    let BlockItem::Stmt(s) = &items[0] else { panic!() };
    let (lo, hi) = (s.span.lo as usize, s.span.hi as usize);
    assert_eq!(&r.sess.sources.text(0)[lo..hi], "return 1 +\n    2;");
    let _ = type_name_text;
}
