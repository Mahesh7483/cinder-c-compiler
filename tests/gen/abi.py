#!/usr/bin/env python3
"""Generates tests/e2e/lang_abi.cases: System V calling-convention tests.

Every case is run in three arrangements, so the same program checks both
self-consistency and interoperability with GCC (the reference ABI):

  * `abi_*_cinder_calls_gcc`  main compiled by cinder, callees by gcc
  * `abi_*_gcc_calls_cinder`  callees compiled by cinder, main by gcc
  * `abi_*_all_cinder`        everything compiled by cinder

Expected output is produced by `CINDER_BLESS=1 cargo test --test e2e` (gcc).
Re-run this script, then bless, after editing it.
"""
import os

# (name, "struct"|"union", [(ctype, field, array_len|None, bit_width|None)])
TYPES = [
    ("S_c", "struct", [("char", "a", None, None)]),
    ("S_cc", "struct", [("char", "a", None, None), ("char", "b", None, None)]),
    ("S_c3", "struct", [("char", "a", 3, None)]),
    ("S_i", "struct", [("int", "a", None, None)]),
    ("S_l", "struct", [("long", "a", None, None)]),
    ("S_ii", "struct", [("int", "a", None, None), ("int", "b", None, None)]),
    ("S_ff", "struct", [("float", "a", None, None), ("float", "b", None, None)]),
    ("S_d", "struct", [("double", "a", None, None)]),
    ("S_iii", "struct", [("int", "a", None, None), ("int", "b", None, None), ("int", "c", None, None)]),
    ("S_fff", "struct", [("float", "a", None, None), ("float", "b", None, None), ("float", "c", None, None)]),
    ("S_ll", "struct", [("long", "a", None, None), ("long", "b", None, None)]),
    ("S_dd", "struct", [("double", "a", None, None), ("double", "b", None, None)]),
    ("S_ld", "struct", [("long", "a", None, None), ("double", "b", None, None)]),
    ("S_dl", "struct", [("double", "a", None, None), ("long", "b", None, None)]),
    ("S_ifd", "struct", [("int", "a", None, None), ("float", "b", None, None), ("double", "c", None, None)]),
    ("S_lll", "struct", [("long", "a", None, None), ("long", "b", None, None), ("long", "c", None, None)]),
    ("S_dddd", "struct", [("double", "a", 4, None)]),
    ("S_c9", "struct", [("char", "a", 9, None)]),
    ("S_c17", "struct", [("char", "a", 17, None)]),
    ("S_nested", "struct", [("struct { int a, b; }", "x", None, None), ("double", "d", None, None)]),
    ("U_if", "union", [("int", "i", None, None)]),
    ("U_ld", "union", [("long", "l", None, None)]),
    ("S_f4", "struct", [("float", "a", 4, None)]),
    ("S_df", "struct", [("double", "a", None, None), ("float", "b", None, None)]),
    ("S_sss", "struct", [("short", "a", None, None), ("short", "b", None, None), ("short", "c", None, None)]),
    ("S_fi", "struct", [("float", "a", None, None), ("int", "b", None, None)]),
    ("S_bits", "struct", [("unsigned", "x", None, 5), ("unsigned", "y", None, 11), ("int", "z", None, None)]),
    ("S_arr2", "struct", [("int", "a", 2, None), ("float", "f", 2, None)]),
    ("S_cd", "struct", [("char", "c", None, None), ("double", "d", None, None)]),
    ("S_i3f", "struct", [("int", "a", 3, None), ("float", "f", None, None)]),
]

HEADER = """#include <stdio.h>
#include <string.h>
"""


def is_float(t):
    return t in ("float", "double")


def type_defs():
    out = []
    for name, kind, fields in TYPES:
        body = []
        for ct, f, n, bits in fields:
            if n:
                body.append("%s %s[%d];" % (ct, f, n))
            elif bits:
                body.append("%s %s : %d;" % (ct, f, bits))
            else:
                body.append("%s %s;" % (ct, f))
        # unions in the table list just the first member plus an aliasing double/float below
        if kind == "union" and name == "U_if":
            body.append("float f;")
        if kind == "union" and name == "U_ld":
            body.append("double d;")
        out.append("typedef %s { %s } %s;" % (kind, " ".join(body), name))
    return "\n".join(out) + "\n"


def prototypes():
    out = []
    for name, _, _ in TYPES:
        out.append("%s mk_%s(long s);" % (name, name))
        out.append("long take_%s(%s v);" % (name, name))
        out.append("long take2_%s(int p, %s v, double d, %s w, long z);" % (name, name, name))
        out.append("%s echo_%s(%s v);" % (name, name, name))
    return "\n".join(out) + "\n"


def impls():
    out = []
    for name, kind, fields in TYPES:
        mk = ["%s mk_%s(long s) {" % (name, name), "    %s r;" % name, "    memset(&r, 0, sizeof r);"]
        take = ["long take_%s(%s v) {" % (name, name), "    unsigned long h = 17;"]
        k = 0
        for ct, f, n, bits in fields:
            k += 1
            idxs = range(n) if n else [None]
            for i in idxs:
                lhs = "r.%s" % f + ("[%d]" % i if i is not None else "")
                rhs = "v.%s" % f + ("[%d]" % i if i is not None else "")
                seed = "s * %d + %d" % (k * 3 + 1, (i or 0) + k)
                if is_float(ct):
                    mk.append("    %s = (%s)(%s) * 0.5%s;" % (lhs, ct, seed, "f" if ct == "float" else ""))
                    take.append("    h = h * 31 + (unsigned long)(long)(%s * 4);" % rhs)
                elif bits:
                    mk.append("    %s = (%s)((%s) & %d);" % (lhs, ct, seed, (1 << bits) - 1))
                    take.append("    h = h * 31 + (unsigned long)(long)%s;" % rhs)
                elif ct.startswith("struct"):
                    mk.append("    %s.a = (int)(%s); %s.b = (int)(s * 5 + %d);" % (lhs, seed, lhs, k))
                    take.append("    h = h * 31 + (unsigned long)(long)%s.a; h = h * 31 + (unsigned long)(long)%s.b;" % (rhs, rhs))
                else:
                    mk.append("    %s = (%s)(%s);" % (lhs, ct, seed))
                    take.append("    h = h * 31 + (unsigned long)(long)%s;" % rhs)
        mk += ["    return r;", "}"]
        take += ["    return (long)h;", "}"]
        take2 = [
            "long take2_%s(int p, %s v, double d, %s w, long z) {" % (name, name, name),
            "    return (long)((unsigned long)(long)p * 7 + (unsigned long)take_%s(v) * 3 + (unsigned long)(long)(d * 2) + (unsigned long)take_%s(w) + (unsigned long)z);"
            % (name, name),
            "}",
        ]
        echo = ["%s echo_%s(%s v) { return v; }" % (name, name, name)]
        out += mk + take + take2 + echo + [""]
    return "\n".join(out)


def main_body():
    lines = ["int main(void) {"]
    for i, (name, _, _) in enumerate(TYPES):
        lines.append(
            '    printf("%s %%ld %%ld %%ld %%ld\\n", take_%s(mk_%s(5)), take2_%s(1, mk_%s(2), 3.5, mk_%s(4), 9), '
            "take_%s(echo_%s(mk_%s(7))), take_%s(mk_%s(-3)));"
            % (name, name, name, name, name, name, name, name, name, name, name)
        )
    lines.append("    return 0;")
    lines.append("}")
    return "\n".join(lines) + "\n"


# ---- scalars: many arguments, narrow types, all register/stack combinations ----
SCALAR_DEFS = """long f_ints(long a, int b, short c, char d, unsigned char e, unsigned short f, unsigned g, long h, long i, long j);
double f_dbls(double a, double b, double c, double d, double e, double f, double g, double h, double i, double j);
double f_mix(int a, double b, char c, float d, long e, double f, short g, float h, int i, double j, long k, double l, int m, double n, double o, double p, double q, float r, long t, int u);
signed char r_sc(int x);
unsigned char r_uc(int x);
short r_ss(int x);
unsigned short r_us(int x);
_Bool r_b(int x);
float r_f(int x);
double r_d(int x);
long r_l(int x);
const char *r_p(int x);
int narrow_args(signed char a, unsigned char b, short c, unsigned short d, _Bool e, char f);
long apply(long (*fn)(long, long), long a, long b);
long add_l(long a, long b);
double h_fp(float a, double b, float c);
"""

SCALAR_IMPLS = """long f_ints(long a, int b, short c, char d, unsigned char e, unsigned short f, unsigned g, long h, long i, long j) {
    return a * 1 + b * 2 + c * 3 + d * 4 + e * 5 + f * 6 + (long)g * 7 + h * 8 + i * 9 + j * 10;
}
double f_dbls(double a, double b, double c, double d, double e, double f, double g, double h, double i, double j) {
    return a + b * 2 + c * 3 + d * 4 + e * 5 + f * 6 + g * 7 + h * 8 + i * 9 + j * 10;
}
double f_mix(int a, double b, char c, float d, long e, double f, short g, float h, int i, double j, long k, double l, int m, double n, double o, double p, double q, float r, long t, int u) {
    return a + b * 2 + c * 3 + d * 4 + e * 5 + f * 6 + g * 7 + h * 8 + i * 9 + j * 10 + k * 11 + l * 12 + m * 13 + n * 14 + o * 15 + p * 16 + q * 17 + r * 18 + t * 19 + u * 20;
}
signed char r_sc(int x) { return (signed char)(x * 3); }
unsigned char r_uc(int x) { return (unsigned char)(x * 3); }
short r_ss(int x) { return (short)(x * 300); }
unsigned short r_us(int x) { return (unsigned short)(x * 300); }
_Bool r_b(int x) { return x > 2; }
float r_f(int x) { return x * 0.25f; }
double r_d(int x) { return x * 0.125; }
long r_l(int x) { return (long)x * 4000000000L; }
const char *r_p(int x) { return x ? "yes" : "no"; }
int narrow_args(signed char a, unsigned char b, short c, unsigned short d, _Bool e, char f) {
    return a + b * 2 + c * 3 + d * 4 + e * 5 + f * 6;
}
long apply(long (*fn)(long, long), long a, long b) { return fn(a, b) + fn(b, a) * 2; }
long add_l(long a, long b) { return a * 10 + b; }
double h_fp(float a, double b, float c) { return a + b * 2 + c * 3; }
"""

SCALAR_MAIN = """int main(void) {
    printf("%ld\\n", f_ints(1, -2, -3, -4, 250, 65000, 4000000000u, 5, 6, 7));
    printf("%.3f\\n", f_dbls(1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5, 9.5, 10.5));
    printf("%.3f\\n", f_mix(1, 2.5, -3, 4.25f, 5, 6.5, -7, 8.25f, 9, 10.5, 11, 12.5, 13, 14.5, 15.5, 16.5, 17.5, 18.25f, 19, 20));
    for (int i = -70; i <= 130; i += 40)
        printf("%d %d %d %d %d %.2f %.3f %ld %s\\n", r_sc(i), r_uc(i), r_ss(i), r_us(i), r_b(i), r_f(i), r_d(i), r_l(i), r_p(i));
    printf("%d\\n", narrow_args(-100, 200, -30000, 60000, 1, -5));
    printf("%d\\n", narrow_args(127, 255, 32767, 65535, 0, 'z'));
    printf("%ld %.2f\\n", apply(add_l, 3, 4), h_fp(1.5f, 2.25, 3.125f));
    return 0;
}
"""

# ---- stack pressure: structs competing for registers with scalars ----
PRESSURE_TYPES = ["S_ii", "S_ll", "S_ff", "S_ld", "S_dd", "S_iii", "S_fff", "S_lll", "S_dddd", "S_nested"]

PRESSURE_DEFS = (
    "long press1(S_ii a, S_ll b, S_ff c, S_ld d, S_dd e, S_iii f, S_fff g, S_lll h, int i, double j);\n"
    "long press2(long a, long b, long c, long d, long e, S_ll f, long g, S_ld h, double i, S_dd j, S_c9 k, S_i l);\n"
    "long press3(double a, double b, double c, double d, double e, double f, double g, S_dd h, S_ff i, double j, S_f4 k);\n"
    "S_dddd press4(S_dddd a, S_lll b, int n);\n"
)

PRESSURE_IMPLS = """long press1(S_ii a, S_ll b, S_ff c, S_ld d, S_dd e, S_iii f, S_fff g, S_lll h, int i, double j) {
    return take_S_ii(a) + take_S_ll(b) * 3 + take_S_ff(c) * 5 + take_S_ld(d) * 7 + take_S_dd(e) * 11 + take_S_iii(f) * 13 + take_S_fff(g) * 17 + take_S_lll(h) * 19 + i + (long)(j * 8);
}
long press2(long a, long b, long c, long d, long e, S_ll f, long g, S_ld h, double i, S_dd j, S_c9 k, S_i l) {
    return a + b * 2 + c * 3 + d * 4 + e * 5 + take_S_ll(f) * 6 + g * 7 + take_S_ld(h) * 8 + (long)(i * 9) + take_S_dd(j) * 10 + take_S_c9(k) * 11 + take_S_i(l) * 12;
}
long press3(double a, double b, double c, double d, double e, double f, double g, S_dd h, S_ff i, double j, S_f4 k) {
    return (long)(a + b * 2 + c * 3 + d * 4 + e * 5 + f * 6 + g * 7 + j * 8) + take_S_dd(h) * 9 + take_S_ff(i) * 10 + take_S_f4(k) * 11;
}
S_dddd press4(S_dddd a, S_lll b, int n) {
    S_dddd r = a;
    for (int i = 0; i < 4; i++) r.a[i] += (double)(take_S_lll(b) % 100) + n;
    return r;
}
"""

PRESSURE_MAIN = """int main(void) {
    printf("%ld\\n", press1(mk_S_ii(1), mk_S_ll(2), mk_S_ff(3), mk_S_ld(4), mk_S_dd(5), mk_S_iii(6), mk_S_fff(7), mk_S_lll(8), 9, 10.5));
    printf("%ld\\n", press2(1, 2, 3, 4, 5, mk_S_ll(6), 7, mk_S_ld(8), 9.5, mk_S_dd(10), mk_S_c9(11), mk_S_i(12)));
    printf("%ld\\n", press3(1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, mk_S_dd(8), mk_S_ff(9), 10.5, mk_S_f4(11)));
    S_dddd r = press4(mk_S_dddd(3), mk_S_lll(4), 5);
    printf("%.2f %.2f %.2f %.2f\\n", r.a[0], r.a[1], r.a[2], r.a[3]);
    return 0;
}
"""

# ---- variadics ----
VARIADIC_DEFS = """long sum_ints(int n, ...);
double sum_doubles(int n, ...);
double sum_mixed(const char *fmt, ...);
long sum_after_many(int a, int b, int c, int d, int e, int n, ...);
"""

VARIADIC_IMPLS = """#include <stdarg.h>
long sum_ints(int n, ...) {
    va_list ap;
    va_start(ap, n);
    long s = 0;
    for (int i = 0; i < n; i++) s += va_arg(ap, int) * (i + 1);
    va_end(ap);
    return s;
}
double sum_doubles(int n, ...) {
    va_list ap;
    va_start(ap, n);
    double s = 0;
    for (int i = 0; i < n; i++) s += va_arg(ap, double) * (i + 1);
    va_end(ap);
    return s;
}
double sum_mixed(const char *fmt, ...) {
    va_list ap, ap2;
    va_start(ap, fmt);
    va_copy(ap2, ap);
    double s = 0;
    for (const char *p = fmt; *p; p++) {
        switch (*p) {
        case 'i': s += va_arg(ap, int); break;
        case 'l': s += va_arg(ap, long); break;
        case 'd': s += va_arg(ap, double); break;
        case 's': s += (double)strlen(va_arg(ap, const char *)); break;
        }
    }
    for (const char *p = fmt; *p; p++) {
        switch (*p) {
        case 'i': s += va_arg(ap2, int); break;
        case 'l': s += va_arg(ap2, long); break;
        case 'd': s += va_arg(ap2, double); break;
        case 's': (void)va_arg(ap2, const char *); break;
        }
    }
    va_end(ap);
    va_end(ap2);
    return s;
}
long sum_after_many(int a, int b, int c, int d, int e, int n, ...) {
    va_list ap;
    va_start(ap, n);
    long s = a + b * 2 + c * 3 + d * 4 + e * 5;
    for (int i = 0; i < n; i++) s += va_arg(ap, long) * (i + 7);
    va_end(ap);
    return s;
}
"""

VARIADIC_MAIN = """int main(void) {
    printf("%ld\\n", sum_ints(5, 1, 2, 3, 4, 5));
    printf("%ld\\n", sum_ints(12, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12));
    printf("%.2f\\n", sum_doubles(3, 1.5, 2.5, 3.5));
    printf("%.2f\\n", sum_doubles(11, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0));
    printf("%.2f\\n", sum_mixed("ildsiddl", 1, 2L, 3.5, "four", 5, 6.5, 7.25, 8L));
    printf("%ld\\n", sum_after_many(1, 2, 3, 4, 5, 4, 10L, 20L, 30L, 40L));
    return 0;
}
"""


def case(name, prog, gcc_files=(), cinder_files=()):
    t = "//// case: %s\n%s" % (name, prog)
    for fname, text in cinder_files:
        t += "//// file: %s\n%s" % (fname, text)
    for fname, text in gcc_files:
        t += "//// gcc-file: %s\n%s" % (fname, text)
    return t


def three_ways(tag, defs, impls, main):
    """prog = (defs + main) with gcc callees; (defs + impls) with gcc main; everything cinder."""
    return [
        case("abi_%s_cinder_calls_gcc" % tag, defs + main, gcc_files=[("impl.c", HEADER + defs + impls)]),
        case("abi_%s_gcc_calls_cinder" % tag, HEADER + defs + impls, gcc_files=[("main.c", HEADER + defs + main)]),
        case("abi_%s_all_cinder" % tag, HEADER + defs + impls + main),
    ]


def with_header(defs, main):
    return HEADER + defs + main


def main():
    cases = []
    sdefs = type_defs() + prototypes()
    # struct cases: callee definitions need their own header
    cases += [
        case("abi_structs_cinder_calls_gcc", HEADER + sdefs + main_body(), gcc_files=[("impl.c", HEADER + sdefs + impls())]),
        case("abi_structs_gcc_calls_cinder", HEADER + sdefs + impls(), gcc_files=[("main.c", HEADER + sdefs + main_body())]),
        case("abi_structs_all_cinder", HEADER + sdefs + impls() + main_body()),
    ]
    cases += [
        case("abi_scalars_cinder_calls_gcc", HEADER + SCALAR_DEFS + SCALAR_MAIN, gcc_files=[("impl.c", HEADER + SCALAR_DEFS + SCALAR_IMPLS)]),
        case("abi_scalars_gcc_calls_cinder", HEADER + SCALAR_DEFS + SCALAR_IMPLS, gcc_files=[("main.c", HEADER + SCALAR_DEFS + SCALAR_MAIN)]),
        case("abi_scalars_all_cinder", HEADER + SCALAR_DEFS + SCALAR_IMPLS + SCALAR_MAIN),
    ]
    pdefs = type_defs() + prototypes() + PRESSURE_DEFS
    pimpl = impls() + PRESSURE_IMPLS
    cases += [
        case("abi_pressure_cinder_calls_gcc", HEADER + pdefs + PRESSURE_MAIN, gcc_files=[("impl.c", HEADER + pdefs + pimpl)]),
        case("abi_pressure_gcc_calls_cinder", HEADER + pdefs + pimpl, gcc_files=[("main.c", HEADER + pdefs + PRESSURE_MAIN)]),
        case("abi_pressure_all_cinder", HEADER + pdefs + pimpl + PRESSURE_MAIN),
    ]
    cases += [
        case("abi_variadics_cinder_calls_gcc", HEADER + VARIADIC_DEFS + VARIADIC_MAIN, gcc_files=[("impl.c", HEADER + VARIADIC_DEFS + VARIADIC_IMPLS)]),
        case("abi_variadics_gcc_calls_cinder", HEADER + VARIADIC_DEFS + VARIADIC_IMPLS, gcc_files=[("main.c", HEADER + VARIADIC_DEFS + VARIADIC_MAIN)]),
        case("abi_variadics_all_cinder", HEADER + VARIADIC_DEFS + VARIADIC_IMPLS + VARIADIC_MAIN),
    ]
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "e2e", "lang_abi.cases")
    with open(out, "w", encoding="utf-8", newline="\n") as f:
        f.write("".join(cases))
    print("wrote", os.path.normpath(out), len(cases), "cases")


if __name__ == "__main__":
    main()
