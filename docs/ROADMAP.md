# Cinder — repo structure & milestone plan

Cinder is a from-scratch C11 compiler for x86-64 Linux (System V ABI), written in
Rust with no compiler dependencies. It emits AT&T assembly and uses the system
`as` and `cc` (as a *linker driver only*) to produce executables.

The name is a working title; renaming is a mechanical find-and-replace
(`cinder` → anything) because it only appears in crate names, the CLI binary,
docs and the Docker/Render config.

## Repo layout

```
Cargo.toml                    workspace
crates/
  cinder/                     the compiler: library + `cinder` binary
    src/
      source.rs  diag.rs      source map, spans, Clang-style diagnostics engine
      lex.rs     pp/          lexer, preprocessor (macros, #if, #include)
      ast.rs     parse/       syntax tree, recursive-descent parser
      types.rs   sema/        type table, scopes, type checking, const-eval, layout
      hir.rs                  typed tree produced by sema (explicit conversions)
      ir/                     SSA IR: types, builder, printer, verifier, CFG, dominators
      lower/                  HIR -> IR (locals as allocas, SysV aggregate ABI)
      opt/                    mem2reg, sccp/fold, dce, cse, copyprop, licm,
                              strength, inline, tailcall, simplifycfg
      backend/                isel, abi, regalloc (linear scan), emit, peephole
      driver.rs  main.rs      CLI + pipeline orchestration
    include/                  bundled libc headers (embedded in the binary)
    tests/                    integration tests (e2e harness, diagnostics goldens)
  cinder-server/              axum HTTP API + sandboxed runner
web/                          Monaco-based playground (static)
tests/
  e2e/*.cases                 200+ end-to-end programs (split-file bundles)
  diag/                       diagnostic golden tests
  bench/                      fib, sort, matmul, nbody, ...
scripts/                      Docker dev loop, benchmark + diff-vs-gcc drivers
docs/                         one page per compiler stage
.github/workflows/ci.yml
Dockerfile  render.yaml  README.md
```

## Pipeline

```
source ─► lexer ─► preprocessor ─► parser ─► AST
                                              │
                                  sema (types, scopes, consteval, layout)
                                              ▼
                                   HIR (typed, explicit casts)
                                              │ lowering
                                              ▼
                  SSA IR (alloca form) ─► opt passes (mem2reg, ...) ─► SSA IR
                                              │
                    isel ─► MIR (vregs) ─► linear-scan regalloc ─► peephole
                                              ▼
                                   x86-64 AT&T asm ─► as ─► cc (link) ─► exe
```

## Key design decisions

* **Locals are allocas, SSA comes from `mem2reg`.** Lowering always emits
  alloca + load/store for locals (like Clang `-O0`). `-O0` keeps them in
  memory; `-O1+` promotes them to SSA with phis. That makes `-O0` simple and
  trustworthy, and gives an honest `-O0` vs `-O2` benchmark delta.
* **Bundled libc headers.** glibc's own headers need dozens of GNU extensions.
  Cinder ships its own `<stdio.h>`, `<stdlib.h>`, `<string.h>`, `<math.h>`,
  `<stdarg.h>`, `<stdint.h>`, ... declaring glibc's ABI, embedded in the
  binary so behaviour is identical on every host. `/usr/include` is an opt-in
  fallback (`-isystem`).
* **Source-line mapping** is emitted as real `.file`/`.loc` directives, so the
  playground maps assembly lines to source lines and `gdb` gets line info.
* **Server never trusts the compiler either.** The API spawns `cinder` as a
  subprocess under the same rlimits as user code, so a parser crash or runaway
  compile cannot take the service down.
* **Unsupported means diagnosed.** Anything not implemented gets a
  `not yet supported` error and a README entry (initial list: `long double`,
  `_Complex`, `_Atomic`, inline asm, K&R definitions, computed goto, GNU
  statement expressions).

## Milestones

Each milestone ends with: all tests green, results shown, one or more commits.

| # | Milestone | Done when |
|---|-----------|-----------|
| M0 | Scaffolding: workspace, source map, diagnostics engine, Docker dev loop, CI skeleton | `cargo test` green on Windows + Linux container |
| M1 | Lexer + preprocessor, `-E` | token/pp unit tests; `-E` output matches expectations |
| M2 | Parser + AST, error recovery, `--emit-ast` | parses full-declarator torture tests; multi-error recovery tests |
| M3 | Types + semantic analysis, warnings framework | type errors/warnings golden tests; layout tests vs. known SysV sizes |
| M4 | SSA IR, lowering, verifier, `--emit-ir` | IR golden tests; verifier passes on all corpus programs |
| M5 | Backend: isel, ABI, linear-scan regalloc, frames; first end-to-end programs | first 100 e2e tests pass at `-O0` in Linux container |
| M6 | Optimizer passes, each toggleable; `-O1`/`-O2` | e2e identical at `-O0/-O1/-O2`; per-pass IR tests |
| M7 | Language breadth: structs by value, varargs, floats, bit-fields, VLAs, `_Generic`, designated init | 200+ e2e tests, differential vs. GCC |
| M8 | Diagnostic quality + `-Wall` warnings (unused, conversion, missing return, uninitialized) | diagnostic goldens |
| M9 | Benchmarks, differential harness, CI complete | `-O0` vs `-O2` table in README |
| M10 | Server, sandbox, Monaco frontend, Dockerfile, `render.yaml`, docs, README | image builds; sandbox escape/limit tests pass |

**Deliberate deviation from the brief's ordering:** the brief lists
"optimizations" before "backend". I build the backend first (M5) and the
optimizer second (M6). An optimizer can only be shown to preserve behaviour by
running programs, so the backend has to exist first. Everything the brief
asks for is still delivered; only the order of two milestones changes.

## Environment notes (this machine)

Windows host without `gcc`/`as`/`ld`; Linux x86-64 output is built, run and
differentially tested against GCC inside a `rust:1-slim-bookworm` container
(which ships `gcc`, `as`, `ld`). Front-end and middle-end tests also run
natively on Windows.
