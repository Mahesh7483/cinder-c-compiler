use super::*;
use crate::diag::Level;
use std::sync::atomic::{AtomicU32, Ordering};

struct Out {
    toks: String,
    errors: Vec<String>,
    warnings: Vec<String>,
    sess: Session,
    raw: Vec<Token>,
}

fn run_opts(src: &str, opts: PpOptions) -> Out {
    let mut sess = Session::new();
    let id = sess.sources.add_file("t.c", None, src.to_string());
    let raw = preprocess(&mut sess, id, &opts);
    let toks =
        raw.iter().filter(|t| !matches!(t.kind, TokKind::Eof)).map(|t| t.spelling()).collect::<Vec<_>>().join(" ");
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for d in sess.diags.diagnostics() {
        match d.level {
            Level::Error | Level::Fatal => errors.push(d.message.clone()),
            Level::Warning => warnings.push(d.message.clone()),
            Level::Note => {}
        }
    }
    Out { toks, errors, warnings, sess, raw }
}

fn run(src: &str) -> Out {
    run_opts(src, PpOptions::default())
}

fn ok(src: &str) -> String {
    let o = run(src);
    assert!(o.errors.is_empty(), "unexpected errors: {:?}", o.errors);
    o.toks
}

fn tmpdir() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "cinder-pp-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ── object-like macros ──

#[test]
fn object_like() {
    assert_eq!(ok("#define N 10\nint a[N];"), "int a [ 10 ] ;");
    assert_eq!(ok("#define A B\n#define B 3\nA"), "3");
    assert_eq!(ok("#define EMPTY\nint EMPTY x;"), "int x ;");
}

#[test]
fn macro_redefinition_and_undef() {
    assert_eq!(ok("#define X 1\n#undef X\n#define X 2\nX"), "2");
    let o = run("#define X 1\n#define X 2\nX");
    assert_eq!(o.warnings, ["'X' macro redefined"]);
    assert_eq!(o.toks, "2");
    // identical redefinition is silent (whitespace amount is irrelevant)
    let o = run("#define X a  +  b\n#define X a + b\n");
    assert!(o.warnings.is_empty(), "{:?}", o.warnings);
}

// ── function-like macros ──

#[test]
fn function_like() {
    assert_eq!(ok("#define SQ(x) ((x)*(x))\nSQ(3+1)"), "( ( 3 + 1 ) * ( 3 + 1 ) )");
    assert_eq!(ok("#define ADD(a,b) a+b\nADD(1,2) ADD((1,2),3)"), "1 + 2 ( 1 , 2 ) + 3");
    assert_eq!(ok("#define F(x) x\nF(F(F(7)))"), "7");
    assert_eq!(ok("#define ZERO() 0\nZERO()"), "0");
    assert_eq!(ok("#define ONE(x) [x]\nONE()"), "[ ]");
}

#[test]
fn function_like_name_without_parens_is_plain() {
    assert_eq!(ok("#define F(x) x\nint F = 3;"), "int F = 3 ;");
    assert_eq!(ok("#define F(x) x\nF"), "F");
}

#[test]
fn invocation_may_span_lines() {
    assert_eq!(ok("#define F(a,b) a-b\nF(1,\n2)"), "1 - 2");
    assert_eq!(ok("#define F(a) a\nF\n(9)"), "9");
}

#[test]
fn arguments_are_expanded_before_substitution() {
    assert_eq!(ok("#define ONE 1\n#define ID(x) x\nID(ONE)"), "1");
    assert_eq!(ok("#define A 5\n#define F(x) x x\nF(A)"), "5 5");
}

#[test]
fn nested_calls_with_commas_in_parens() {
    assert_eq!(ok("#define F(a,b) <a|b>\nF(g(1,2),h)"), "< g ( 1 , 2 ) | h >");
}

// ── # and ## ──

#[test]
fn stringification() {
    assert_eq!(ok("#define S(x) #x\nS(hello)"), "\"hello\"");
    assert_eq!(ok("#define S(x) #x\nS(  a   +   b  )"), "\"a + b\"");
    assert_eq!(ok("#define S(x) #x\nS(\"q\\n\")"), "\"\\\"q\\\\n\\\"\"");
    assert_eq!(ok("#define S(x) #x\nS()"), "\"\"");
    // the argument is NOT macro-expanded before stringification
    assert_eq!(ok("#define N 5\n#define S(x) #x\nS(N)"), "\"N\"");
    // ...but is when routed through another macro
    assert_eq!(ok("#define N 5\n#define S(x) #x\n#define XS(x) S(x)\nXS(N)"), "\"5\"");
}

#[test]
fn token_pasting() {
    assert_eq!(ok("#define CAT(a,b) a##b\nCAT(foo,bar)"), "foobar");
    assert_eq!(ok("#define CAT(a,b) a##b\nCAT(1,2)"), "12");
    assert_eq!(ok("#define CAT(a,b) a##b\nCAT(+,=)"), "+=");
    assert_eq!(ok("#define CAT3(a,b,c) a##b##c\nCAT3(x,y,z)"), "xyz");
    assert_eq!(ok("#define CAT(a,b) a##b\nCAT(,x) CAT(x,) CAT(,)"), "x x");
    // operands of ## are not expanded first
    assert_eq!(ok("#define A 1\n#define CAT(a,b) a##b\nCAT(A,2)"), "A2");
    assert_eq!(ok("#define DECL(n) int var_##n = n;\nDECL(7)"), "int var_7 = 7 ;");
}

#[test]
fn invalid_paste_is_an_error() {
    let o = run("#define CAT(a,b) a##b\nCAT(+,/)");
    assert_eq!(o.errors.len(), 1);
    assert!(o.errors[0].contains("pasting formed '+/'"), "{:?}", o.errors);
}

// ── recursion / hide sets ──

#[test]
fn self_reference_does_not_loop() {
    assert_eq!(ok("#define foo foo\nfoo"), "foo");
    assert_eq!(ok("#define a b\n#define b a\na b"), "a b");
    assert_eq!(ok("#define x (x+1)\nx"), "( x + 1 )");
}

#[test]
fn prosser_example() {
    // The classic case from the C standard's discussion of rescanning.
    assert_eq!(ok("#define f(a) a*g\n#define g(a) f(a)\nf(2)(9)"), "2 * 9 * g");
}

#[test]
fn standard_example_6_10_3_5() {
    let src = "#define x 3\n#define f(a) f(x * (a))\n#undef x\n#define x 2\n#define g f\n#define z z[0]\n#define h g(~\n#define m(a) a(w)\n#define w 0,1\n#define t(a) a\n#define p() int\n#define q(x) x\n#define r(x,y) x ## y\n#define str(x) # x\nf(y+1) + f(f(z)) % t(t(g)(0) + t)(1);\ng(x+(3,4)-w) | h 5) & m\n(f)^m(m);\np() i[q()] = { q(1), r(2,3), r(4,), r(,5), r(,) };\nchar c[2][6] = { str(hello), str() };";
    let toks = ok(src);
    assert_eq!(
        toks,
        "f ( 2 * ( y + 1 ) ) + f ( 2 * ( f ( 2 * ( z [ 0 ] ) ) ) ) % f ( 2 * ( 0 ) ) + t ( 1 ) ; \
         f ( 2 * ( 2 + ( 3 , 4 ) - 0 , 1 ) ) | f ( 2 * ( ~ 5 ) ) & f ( 2 * ( 0 , 1 ) ) ^ m ( 0 , 1 ) ; \
         int i [ ] = { 1 , 23 , 4 , 5 , } ; \
         char c [ 2 ] [ 6 ] = { \"hello\" , \"\" } ;"
    );
}

// ── variadics ──

#[test]
fn variadic_macros() {
    assert_eq!(ok("#define LOG(f, ...) printf(f, __VA_ARGS__)\nLOG(\"%d %d\", 1, 2)"), "printf ( \"%d %d\" , 1 , 2 )");
    assert_eq!(ok("#define V(...) [__VA_ARGS__]\nV() V(a) V(a,b,c)"), "[ ] [ a ] [ a , b , c ]");
    assert_eq!(ok("#define N(args...) f(args)\nN(1,2)"), "f ( 1 , 2 )");
}

#[test]
fn gnu_comma_elision() {
    assert_eq!(
        ok("#define P(fmt, ...) pr(fmt, ## __VA_ARGS__)\nP(\"a\") P(\"a\", 1, 2)"),
        "pr ( \"a\" ) pr ( \"a\" , 1 , 2 )"
    );
}

#[test]
fn va_opt() {
    assert_eq!(ok("#define F(a, ...) f(a __VA_OPT__(,) __VA_ARGS__)\nF(1) F(1,2)"), "f ( 1 ) f ( 1 , 2 )");
}

// ── argument-count diagnostics ──

#[test]
fn wrong_argument_counts() {
    let o = run("#define F(a,b) a\nF(1)");
    assert!(o.errors[0].contains("too few arguments"), "{:?}", o.errors);
    let o = run("#define F(a,b) a\nF(1,2,3)");
    assert!(o.errors[0].contains("too many arguments"), "{:?}", o.errors);
    let o = run("#define F(a) a\nF(1");
    assert!(o.errors[0].contains("unterminated function-like macro invocation"), "{:?}", o.errors);
}

// ── conditionals ──

#[test]
fn if_else_chains() {
    assert_eq!(ok("#if 1\na\n#else\nb\n#endif"), "a");
    assert_eq!(ok("#if 0\na\n#else\nb\n#endif"), "b");
    assert_eq!(ok("#if 0\na\n#elif 1\nb\n#else\nc\n#endif"), "b");
    assert_eq!(ok("#if 0\na\n#elif 0\nb\n#else\nc\n#endif"), "c");
    // only the first true branch is taken
    assert_eq!(ok("#if 1\na\n#elif 1\nb\n#else\nc\n#endif"), "a");
}

#[test]
fn ifdef_ifndef() {
    assert_eq!(ok("#define X\n#ifdef X\nyes\n#endif\n#ifndef X\nno\n#endif"), "yes");
    assert_eq!(ok("#ifndef Y\n#define Y 1\n#endif\nY"), "1");
}

#[test]
fn nested_conditionals_and_skipping() {
    let src = "#if 0\n#if 1\nno1\n#else\nno2\n#endif\n#elif 1\nyes\n#endif";
    assert_eq!(ok(src), "yes");
    // a skipped group may contain anything that is not a directive, even a lone quote
    assert_eq!(ok("#if 0\ndon't\n'\n\"abc\n#endif\nok"), "ok");
    // a skipped group's elif is not evaluated
    assert_eq!(ok("#if 0\n#if 1/0\n#endif\n#endif\nok"), "ok");
}

#[test]
fn defined_operator() {
    assert_eq!(ok("#define A\n#if defined A && defined(A) && !defined B\nok\n#endif"), "ok");
    assert_eq!(ok("#define A\n#define D defined(A)\n#if 1\nok\n#endif"), "ok");
}

#[test]
fn if_arithmetic() {
    assert_eq!(ok("#if 1 + 2 * 3 == 7\nok\n#endif"), "ok");
    assert_eq!(ok("#if (1 << 4) - 1 == 15 && 7 / 2 == 3 && 7 % 4 == 3\nok\n#endif"), "ok");
    assert_eq!(ok("#if -1 < 0\nok\n#endif"), "ok");
    assert_eq!(ok("#if -1 < 0u\nbad\n#else\nok\n#endif"), "ok"); // unsigned comparison
    assert_eq!(ok("#if 0xFFFFFFFFFFFFFFFF == -1\nok\n#endif"), "ok");
    assert_eq!(ok("#if (2 > 1 ? 10 : 20) == 10\nok\n#endif"), "ok");
    assert_eq!(ok("#if 'a' == 97 && '\\n' == 10\nok\n#endif"), "ok");
    assert_eq!(ok("#if ~0 == -1 && !0 && !(1 == 2)\nok\n#endif"), "ok");
    assert_eq!(ok("#if UNDEFINED_THING == 0\nok\n#endif"), "ok");
    assert_eq!(ok("#if 1 || (1/0)\nok\n#endif"), "ok");
    assert_eq!(ok("#if 0 && (1/0)\nbad\n#else\nok\n#endif"), "ok");
}

#[test]
fn if_uses_macros() {
    assert_eq!(ok("#define V 3\n#if V >= 3\nok\n#endif"), "ok");
    assert_eq!(ok("#define LT(a,b) ((a)<(b))\n#if LT(1,2)\nok\n#endif"), "ok");
}

#[test]
fn if_errors() {
    let o = run("#if 1/0\n#endif");
    assert!(o.errors[0].contains("division by zero"), "{:?}", o.errors);
    let o = run("#if 1.5\n#endif");
    assert!(o.errors[0].contains("floating constant"), "{:?}", o.errors);
    let o = run("#if (1\n#endif");
    assert!(o.errors[0].contains("expected ')'"), "{:?}", o.errors);
    let o = run("#if\n#endif");
    assert!(o.errors[0].contains("#if with no expression"), "{:?}", o.errors);
}

#[test]
fn conditional_structure_errors() {
    assert!(run("#endif").errors[0].contains("#endif without #if"));
    assert!(run("#else").errors[0].contains("#else without #if"));
    assert!(run("#elif 1").errors[0].contains("#elif without #if"));
    assert!(run("#if 1\n#else\n#else\n#endif").errors[0].contains("#else after #else"));
    assert!(run("#if 1\n#else\n#elif 1\n#endif").errors[0].contains("#elif after #else"));
    assert!(run("#if 1\nx").errors[0].contains("unterminated conditional directive"));
}

#[test]
fn extra_tokens_warn() {
    let o = run("#if 1\n#endif junk");
    assert_eq!(o.warnings, ["extra tokens at end of #endif directive"]);
}

// ── predefined / builtin macros ──

#[test]
fn builtin_macros() {
    assert_eq!(ok("__LINE__\n__LINE__"), "1 2");
    assert_eq!(ok("__FILE__"), "\"t.c\"");
    assert_eq!(ok("__COUNTER__ __COUNTER__ __COUNTER__"), "0 1 2");
    assert_eq!(ok("__STDC_VERSION__ __STDC__ __STDC_HOSTED__"), "201112L 1 1");
    assert_eq!(ok("__INCLUDE_LEVEL__"), "0");
    assert_eq!(ok("#define L __LINE__\n\nL"), "3");
    assert_eq!(ok("#if defined(__x86_64__) && defined(__linux__) && __LP64__\nok\n#endif"), "ok");
}

#[test]
fn date_and_time_have_the_documented_shape() {
    let t = ok("__DATE__ __TIME__");
    let parts: Vec<&str> = t.split('"').collect();
    // `"Mmm dd yyyy" "hh:mm:ss"`
    assert_eq!(parts[1].len(), 11);
    assert_eq!(parts[3].len(), 8);
}

#[test]
fn command_line_defines() {
    let opts = PpOptions {
        defines: vec![
            MacroCmd::Define("A".into()),
            MacroCmd::Define("B=7".into()),
            MacroCmd::Define("F(x)=x+1".into()),
            MacroCmd::Define("GONE=1".into()),
            MacroCmd::Undef("GONE".into()),
        ],
        ..Default::default()
    };
    let o = run_opts("A B F(2)\n#ifdef GONE\nbad\n#endif", opts);
    assert_eq!(o.toks, "1 7 2 + 1");
}

#[test]
fn optimize_macro_follows_level() {
    let o =
        run_opts("#ifdef __OPTIMIZE__\nopt\n#else\nnoopt\n#endif", PpOptions { opt_level: 2, ..Default::default() });
    assert_eq!(o.toks, "opt");
    assert_eq!(ok("#ifdef __OPTIMIZE__\nopt\n#else\nnoopt\n#endif"), "noopt");
}

// ── #error / #warning / directives ──

#[test]
fn error_and_warning_directives() {
    let o = run("#error stop right here\nx");
    assert_eq!(o.errors, ["#error stop right here"]);
    assert_eq!(o.toks, "x");
    let o = run("#warning careful");
    assert_eq!(o.warnings, ["#warning careful"]);
    assert!(run("#error skipped\n").errors.len() == 1);
    assert!(run("#if 0\n#error skipped\n#endif").errors.is_empty());
}

#[test]
fn unknown_directive() {
    let o = run("#frobnicate x\n");
    assert!(o.errors[0].contains("invalid preprocessing directive '#frobnicate'"), "{:?}", o.errors);
    // ...but not inside a skipped group
    assert!(run("#if 0\n#frobnicate\n#endif").errors.is_empty());
}

#[test]
fn null_directive_and_comments() {
    assert_eq!(ok("#\n# /* c */ define X 4\nX"), "4");
    assert_eq!(ok("/* lead */ #define X 5\nX"), "5");
}

#[test]
fn define_errors() {
    assert!(run("#define\n").errors[0].contains("macro name missing"));
    assert!(run("#define 3 x\n").errors[0].contains("macro name must be an identifier"));
    assert!(run("#define defined 1\n").errors[0].contains("'defined' cannot be used"));
    assert!(run("#define F(a,a) a\n").errors[0].contains("duplicate macro parameter"));
    assert!(!run("#define F(a #a\n").errors.is_empty());
    assert!(run("#define F(a) #b\n").errors[0].contains("'#' is not followed by a macro parameter"));
    assert!(run("#define F(a) ## a\n").errors[0].contains("'##' cannot appear at either end"));
}

#[test]
fn object_like_macro_may_stringify_hash() {
    // `#` in an object-like macro is just a token
    assert_eq!(ok("#define H # x\nH"), "# x");
}

// ── _Pragma / pragma forwarding ──

#[test]
fn pragmas_are_forwarded() {
    let o = run("#pragma pack(push, 1)\nint x;");
    assert_eq!(o.toks, "#pragma pack(push, 1) int x ;");
    let o = run("_Pragma(\"pack(pop)\") int y;");
    assert_eq!(o.toks, "#pragma pack(pop) int y ;");
    // `#pragma once` in the main file is accepted silently
    assert_eq!(run("#pragma once\nint z;").toks, "int z ;");
}

// ── includes ──

#[test]
fn bundled_headers() {
    assert_eq!(ok("#include <limits.h>\nINT_MAX"), "2147483647");
    assert_eq!(ok("#include <stdbool.h>\nbool b = true;"), "_Bool b = 1 ;");
    let o = run("#include <stddef.h>\nsize_t n;");
    assert!(o.errors.is_empty());
    assert!(o.toks.ends_with("size_t n ;"));
    assert!(o.toks.contains("typedef unsigned long size_t ;"));
    // double inclusion is protected by the guard
    let o = run("#include <stddef.h>\n#include <stddef.h>\n");
    assert_eq!(o.toks.matches("size_t ;").count(), 1);
}

#[test]
fn every_bundled_header_preprocesses_cleanly() {
    for name in crate::headers::BUNDLED_NAMES {
        let o = run(&format!("#include <{}>\n", name));
        assert!(o.errors.is_empty(), "{name}: {:?}", o.errors);
        assert!(o.warnings.is_empty(), "{name}: {:?}", o.warnings);
    }
}

#[test]
fn nested_bundled_includes() {
    // <time.h> pulls in <stddef.h> and <sys/types.h>
    let o = run("#include <time.h>\nstruct tm t; clock_t c;");
    assert!(o.errors.is_empty(), "{:?}", o.errors);
    assert!(o.toks.contains("typedef long clock_t ;"));
}

#[test]
fn quoted_includes_relative_to_the_file() {
    let d = tmpdir();
    std::fs::write(d.join("a.h"), "#define FROM_A 11\n").unwrap();
    std::fs::create_dir_all(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/b.h"), "#include \"c.h\"\n#define FROM_B (FROM_C+1)\n").unwrap();
    std::fs::write(d.join("sub/c.h"), "#define FROM_C 20\n").unwrap();
    std::fs::write(d.join("main.c"), "#include \"a.h\"\n#include \"sub/b.h\"\nFROM_A FROM_B\n").unwrap();
    let mut sess = Session::new();
    let text = std::fs::read_to_string(d.join("main.c")).unwrap();
    let id = sess.sources.add_file(d.join("main.c").to_string_lossy(), Some(d.join("main.c")), text);
    let toks = preprocess(&mut sess, id, &PpOptions::default());
    assert!(!sess.diags.has_errors(), "{:?}", sess.diags.diagnostics().iter().map(|d| &d.message).collect::<Vec<_>>());
    let s: Vec<String> = toks.iter().filter(|t| !matches!(t.kind, TokKind::Eof)).map(|t| t.spelling()).collect();
    assert_eq!(s.join(" "), "11 ( 20 + 1 )");
}

#[test]
fn include_dirs_and_include_level() {
    let d = tmpdir();
    std::fs::write(d.join("lvl.h"), "__INCLUDE_LEVEL__\n").unwrap();
    let opts = PpOptions { include_dirs: vec![d.clone()], ..Default::default() };
    let o = run_opts("__INCLUDE_LEVEL__\n#include <lvl.h>\n#include \"lvl.h\"", opts);
    assert_eq!(o.toks, "0 1 1");
}

#[test]
fn pragma_once_and_include_guards() {
    let d = tmpdir();
    std::fs::write(d.join("once.h"), "#pragma once\nint once_decl;\n").unwrap();
    std::fs::write(d.join("guard.h"), "#ifndef G\n#define G\nint guard_decl;\n#endif\n").unwrap();
    let opts = PpOptions { include_dirs: vec![d], ..Default::default() };
    let o = run_opts("#include <once.h>\n#include <once.h>\n#include <guard.h>\n#include <guard.h>\n", opts);
    assert_eq!(o.toks.matches("once_decl").count(), 1, "{}", o.toks);
    assert_eq!(o.toks.matches("guard_decl").count(), 1, "{}", o.toks);
}

#[test]
fn missing_include_is_fatal() {
    let o = run("#include <no_such_header.h>\nx");
    assert_eq!(o.errors, ["'no_such_header.h' file not found"]);
    assert!(o.sess.diags.is_fatal());
}

#[test]
fn missing_include_explains_unsupported_runtimes() {
    for (src, needle) in [
        ("#include <omp.h>\n", "OpenMP is not supported"),
        ("#include \"mpi.h\"\n", "MPI is not supported"),
        ("#include <pthread.h>\n", "threads and atomics"),
        ("#include <wchar.h>\n", "bundles only these standard headers"),
    ] {
        let o = run(src);
        assert!(o.sess.diags.is_fatal(), "{src}");
        let notes: Vec<String> =
            o.sess.diags.diagnostics().iter().flat_map(|d| d.notes.iter().map(|n| n.message.clone())).collect();
        assert!(notes.iter().any(|n| n.contains(needle)), "{src}: {notes:?}");
    }
    // a project header that is simply missing gets no list of bundled headers
    let o = run("#include \"mine.h\"\n");
    assert!(o.sess.diags.diagnostics().iter().all(|d| d.notes.is_empty()));
}

#[test]
fn include_depth_limit() {
    let d = tmpdir();
    std::fs::write(d.join("loop.h"), "#include \"loop.h\"\n").unwrap();
    let opts = PpOptions { include_dirs: vec![d], ..Default::default() };
    let o = run_opts("#include <loop.h>\n", opts);
    assert!(o.errors.iter().any(|e| e.contains("nested too deeply")), "{:?}", o.errors);
}

#[test]
fn computed_includes() {
    assert_eq!(ok("#define HDR <limits.h>\n#include HDR\nSCHAR_MAX"), "127");
    assert_eq!(ok("#define H(x) <x>\n#include H(limits.h)\nCHAR_BIT"), "8");
}

#[test]
fn has_include() {
    assert_eq!(ok("#if __has_include(<stdio.h>)\nyes\n#endif"), "yes");
    assert_eq!(ok("#if __has_include(<does/not/exist.h>)\nno\n#else\nyes\n#endif"), "yes");
    assert_eq!(ok("#if defined(__has_include) || 1\nok\n#endif"), "ok");
}

#[test]
fn nostdinc_disables_bundled_headers() {
    let o = run_opts("#include <stdio.h>\n", PpOptions { no_bundled_headers: true, ..Default::default() });
    assert_eq!(o.errors, ["'stdio.h' file not found"]);
}

// ── spans / locations ──

#[test]
fn expansion_tokens_point_at_the_invocation() {
    let src = "#define ADD(a,b) a+b\nint x = ADD(1,\n 2);\n";
    let o = run(src);
    let plus = o.raw.iter().find(|t| t.is_punct(Punct::Plus)).unwrap();
    let loc = o.sess.sources.loc(plus.span).unwrap();
    assert_eq!((loc.line, loc.col), (2, 9)); // the `ADD` token
                                             // argument tokens keep their own positions
    let two = o.raw.iter().find(|t| t.spelling() == "2").unwrap();
    let loc = o.sess.sources.loc(two.span).unwrap();
    assert_eq!((loc.line, loc.col), (3, 2));
}

#[test]
fn line_continuation_in_define() {
    assert_eq!(ok("#define LONG(a) \\\n  a + \\\n  a\nLONG(2)"), "2 + 2");
}

// ── -E output ──

#[test]
fn e_output_basic() {
    let mut sess = Session::new();
    let id = sess.sources.add_file("t.c", None, "#define N 3\nint a[N];\n\n  int b;\n".to_string());
    let toks = preprocess(&mut sess, id, &PpOptions::default());
    let text = output::print(&sess, &toks);
    assert_eq!(text, "# 2 \"t.c\"\nint a[3];\n\n  int b;\n");
}

#[test]
fn e_output_does_not_fuse_tokens() {
    let mut sess = Session::new();
    let id = sess.sources.add_file(
        "t.c",
        None,
        "#define M -\nint x = 1 M -2;\nint y = a M M b;\nint z = -M 1;\n".to_string(),
    );
    let toks = preprocess(&mut sess, id, &PpOptions::default());
    let text = output::print(&sess, &toks);
    assert!(text.contains("1 - -2"), "{text}");
    assert!(text.contains("a - - b"), "{text}");
    // `-M` must not print as `--`
    assert!(text.contains("= - - 1"), "{text}");
    // the printed text re-lexes to the same token stream
    let again = lex::lex_snippet(&text.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n"));
    let orig: Vec<String> = toks.iter().filter(|t| !matches!(t.kind, TokKind::Eof)).map(|t| t.spelling()).collect();
    let after: Vec<String> = again.iter().map(|t| t.spelling()).collect();
    assert_eq!(orig, after);
}

#[test]
fn e_output_markers_for_includes() {
    let mut sess = Session::new();
    let id = sess.sources.add_file("t.c", None, "#include <stddef.h>\nint x;\n".to_string());
    let toks = preprocess(&mut sess, id, &PpOptions::default());
    let text = output::print(&sess, &toks);
    // a marker when entering the header, and another when returning to t.c
    assert!(text.contains("\"<cinder>/stddef.h\"\n"), "{text}");
    assert!(text.contains("# 2 \"t.c\"\nint x;"), "{text}");
}

#[test]
fn restricted_includes_refuse_paths_that_escape() {
    let opts = PpOptions { restrict_includes: true, ..Default::default() };
    for src in
        ["#include \"/etc/passwd\"\nint x;", "#include <../../etc/passwd>\nint x;", "#include \"../secret.h\"\nint x;"]
    {
        let r = run_opts(src, opts.clone());
        assert!(r.errors.iter().any(|e| e.contains("file not found")), "{src}: {:?}", r.errors);
    }
    // bundled headers still work
    let r = run_opts("#include <stdio.h>\nint x;", opts);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
}

#[test]
fn exponential_macros_hit_the_expansion_budget() {
    // 10^8 tokens if expanded in full: must stop with one clear error, quickly
    let src = "#define A(x) x x x x x x x x x x\n#define B(x) A(A(x))\n#define C(x) B(B(x))\n#define D(x) C(C(x))\nint v = D(1 +) 0;\nint after;";
    let o = run_opts(src, PpOptions { max_expansion_tokens: Some(50_000), ..PpOptions::default() });
    assert_eq!(o.errors.len(), 1, "{:?}", o.errors);
    assert!(o.errors[0].contains("macro expansion produced too many tokens"), "{:?}", o.errors);
    assert!(o.toks.contains("after"), "scanning continues after the error: {}", o.toks);
}

#[test]
fn ordinary_heavy_macro_use_stays_within_the_default_budget() {
    let mut src = String::from("#define SQ(x) ((x) * (x))\n#define QUAD(x) SQ(SQ(x))\nint s = 0;\n");
    for i in 0..1500 {
        src.push_str(&format!("int f{i}(int a) {{ return QUAD(a + {i}); }}\n"));
    }
    let o = run(&src);
    assert!(o.errors.is_empty(), "{:?}", o.errors);
}
