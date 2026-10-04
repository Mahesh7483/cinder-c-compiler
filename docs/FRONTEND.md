# Front end: source map, lexer, preprocessor, parser

```
bytes ─► SourceMap ─► lexer ─► preprocessor ─► parser ─► AST
```

Source: `source.rs`, `lex.rs`, `literal.rs`, `intern.rs`, `headers.rs`, `pp/`, `parse/`, `ast.rs`.
Try it: `cinder -E file.c` (preprocessed text) and `cinder --emit-ast file.c`.

## Source map (`source.rs`)

Every file is kept twice. `orig` is the file exactly as read; it is what diagnostics print and
what physical line numbers refer to. `text` is the *logical* text after translation phases 1–2
(UTF-8 BOM dropped, `\r\n` and `\r` folded to `\n`, backslash-newline splices removed). The lexer
works on `text`, and every `Span` is a byte range into it. A table of removed byte ranges maps a
logical offset back to a physical line and column in O(log n), so a diagnostic on a line that was
continued with `\` still points at the right place.

## Lexer (`lex.rs`, `literal.rs`, `intern.rs`)

The lexer produces *preprocessing tokens*: identifiers, pp-numbers, character and string
literals, punctuators, and flags for "preceded by whitespace" and "first on its line" (the
preprocessor needs both: `#` only starts a directive at the beginning of a line, and `-E` output
must not fuse tokens).

* It is deliberately infallible. A malformed literal becomes a `BadLiteral` token and a stray
  character becomes `Other`; the preprocessor or parser reports them **only if they survive into
  live code**. This matters for `#if 0` blocks, which may legitimately contain a lone `'`.
  The one hard lexer error is an unterminated `/* ... */`.
* `literal.rs` turns spellings into values once, for both the preprocessor (`#if` arithmetic) and
  the parser: integer constants with their C11 type (`int`, `unsigned`, `long`, … chosen by suffix,
  base and value), floating constants, character constants and string literals with all C11 escape
  sequences (octal, hex, `\u`/`\U`, with range checks).
* Identifiers are interned (`intern.rs`) so macro lookups, hide-set operations and keyword tests
  are `u32` comparisons. Interned strings are leaked on purpose: the compiler is a short-lived
  process.

## Preprocessor (`pp/`)

Tokens come from a stack of file frames plus a `pending` stack of tokens produced by macro
expansion, which are rescanned before the file continues.

* **Macro expansion** follows Prosser's hide-set algorithm, so self-referential and mutually
  recursive macros terminate exactly as the standard requires (the C11 6.10.3.5 example
  `#define f(a) a*g` / `#define g(a) f(a)` makes `f(2)(9)` expand to `2*9*g`).
* **Supported:** `#include` (quoted, angled, macro-computed, `__has_include`), object-like and
  function-like `#define` with `#`, `##`, variadics, GNU named variadics, `, ## __VA_ARGS__` and
  `__VA_OPT__`; `#undef`; `#if/#ifdef/#ifndef/#elif/#else/#endif` with `defined` (expression
  evaluation in `pp/expr.rs` uses 64-bit `intmax_t`/`uintmax_t` with the usual conversions);
  `#error`, `#warning`, `#pragma` (`once` is handled, the rest forwarded), `_Pragma`, and the
  predefined macros `__FILE__ __LINE__ __COUNTER__ __INCLUDE_LEVEL__ __DATE__ __TIME__` plus the
  usual `__STDC__`, `__x86_64__`, `__linux__`, `__SIZEOF_*__` family.
* **Include search:** quoted includes look next to the including file first; then the `-I`
  directories, then the bundled headers, then any `-isystem` directories. Angled includes skip
  the first step. Because `-I` comes before the bundle, a project can override a bundled header
  by shipping its own. With
  `--restrict-includes` (used by the playground) absolute paths, `..` components and the `-I` and
  system directories are refused, so untrusted source cannot read host files at compile time.
* **Bundled libc headers** (`include/`, embedded with `include_str!` by `headers.rs`) declare
  glibc's ABI with plain C11: `assert ctype errno float inttypes iso646 limits math stdalign
  stdarg stdbool stddef stdint stdio stdlib stdnoreturn string time unistd sys/types`. glibc's own
  headers need dozens of GNU extensions, and bundling keeps behaviour identical on every host and
  the compiler a single file. `-isystem /usr/include` opts into real system headers.
* `-E` output (`pp/output.rs`) keeps the line structure and prints `# <line> "<file>"` markers.
* Known limitations: `#line` is accepted but does not renumber anything, and diagnostics inside a
  macro expansion point at the macro body without a "in expansion of macro" backtrace.

## Parser (`parse/`, `ast.rs`)

A hand-written recursive-descent parser for C11 with precedence climbing for expressions
(`parse/expr.rs`), declarators handled the way the standard describes them (inside-out, with
abstract declarators for casts and `sizeof`), and a scoped typedef-name table — the classic
"lexer hack" — so `T * x;` is a declaration when `T` names a type and an expression otherwise.

The AST is purely syntactic: specifiers and declarators are kept as written, typedef names are not
resolved, no types are computed. Semantic analysis does that.

**Error recovery follows Clang's lead.** A missing `;` or `)` is reported ("expected ';' after
expression", with a fix-it) and parsing continues as if it were there. Otherwise the parser
resynchronizes at the next `;` or `}` so several independent errors appear in one run. Errors are
limited by `-ferror-limit` (default 20), after which the compiler stops with
`too many errors emitted`.

`not yet supported` constructs are *recognized* by the parser and rejected with a clear message
rather than mis-parsed: `_Complex`, `typeof`, K&R function definitions, computed goto and
address-of-label, GNU statement expressions, case ranges, inline and top-level `asm`, assembler
labels on declarations (see the README for the full list).

### Example

```c
int sum(int a, int b, int n) {
    int s = 0;
    for (int i = 0; i < n; i++)
        s += a * b + i;
    return s;
}
```

`cinder --emit-ast` (trimmed):

```
TranslationUnit
`-FunctionDef sum <3:1-8:2>
  |-Specs 'int'
  |-Declarator 'sum(int a, int b, int n)'
  `-CompoundStmt <3:30-8:2>
    |-Declaration <4:5-15>
    | `-InitDeclarator 's'
    |   `-IntLiteral 0 <4:13-14>
    |-ForStmt <5:5-6:24>
    | |-Init ... |-Cond ... |-Step ...
    | `-ExprStmt <6:9-24>
    |   `-AssignOp '+=' <6:9-23>
    |     |-Ident s <6:9-10>
    |     `-BinaryOp '+' <6:14-23>
    |       |-BinaryOp '*' <6:14-19> ...
    |       `-Ident i <6:22-23>
    `-ReturnStmt <7:5-14>
```

## Tests

* `lex.rs` / `pp/tests.rs` — token streams, every directive, macro corner cases (recursion,
  stringizing, pasting, variadics), `#if` arithmetic, include search and `--restrict-includes`.
* `parse/tests.rs` — declarator torture tests (`int (*(*f)(int))[3]`), every statement and expression
  form, and multi-error recovery (one run, several independent diagnostics, no cascades).
* `tests/diag/*.c` — golden diagnostics for `preprocessor`, `recovery` and the other stages
  (`crates/cinder/tests/diag.rs`).
