# Cinder

**A from-scratch C11 compiler for x86-64 Linux, written in Rust — with a web playground.**

Cinder reads C, runs its own preprocessor, parser, type checker, SSA optimizer and x86-64 code generator, and
writes AT&T assembly. The only external tools are the system assembler (`as`) and `cc` used purely as a *linker
driver*. It does not wrap or reuse GCC, Clang or LLVM, and the compiler crate has **no dependencies at all**.

* Clang-style diagnostics with carets, notes, fix-its and "did you mean" suggestions; real warnings
  (`-Wunused-variable`, lossy conversions, missing `return`, **uninitialized use** via a flow analysis, …)
* A 10-pass SSA optimizer (`-O0` / `-O1` / `-O2`, each pass individually toggleable, IR verified after every pass)
* A System V ABI backend (linear-scan register allocation) that interoperates with GCC-compiled code in both directions
* **432 unit tests, 243 end-to-end programs run at three optimization levels, differential testing against GCC,**
  random-program optimizer fuzzing, benchmarks
* A web playground (Monaco editor, assembly/IR/AST views, source↔output highlighting, run with stdin, share links)
  backed by a sandboxed Rust server, deployable on Render from one Dockerfile

> *Cinder* is a working title. Renaming is a mechanical find-and-replace (crate names, the binary, docs, Docker/Render config).

## Quick start

Requirements: Linux x86-64, a Rust toolchain (1.82 or newer for the compiler), and `as` + `cc` (binutils and gcc).
On Windows or macOS use the Docker dev loop below.

```bash
cargo build --release
cat > hello.c <<'EOF'
#include <stdio.h>
int main(void) { puts("hello from cinder"); return 0; }
EOF
./target/release/cinder -O2 hello.c -o hello && ./hello
```

```bash
cinder -S -O2 file.c -o -          # assembly to stdout
cinder --emit-ir -O2 file.c        # the SSA IR
cinder --emit-ast file.c           # syntax tree      (also --emit-hir for the typed tree)
cinder -E -DDEBUG=1 -Iinclude file.c   # preprocessor output
cinder -Wall -Wextra -Werror file.c    # warnings as errors
cinder -O2 -fno-licm -fno-inline file.c  # switch single passes off
```

| flag | meaning |
|------|---------|
| `-o <file>` · `-c` · `-S` · `-E` | output file · compile only · assembly · preprocess only |
| `-O0` `-O1` `-O2` | optimization level (`-O3`/`-Os` mean `-O2`) |
| `-I <dir>` `-D <n>[=v]` `-U <n>` `-isystem <dir>` `-nostdinc` | preprocessor |
| `-Wall` `-Wextra` `-W<name>` `-Wno-<name>` `-Werror[=<name>]` `-w` | warnings ([DIAGNOSTICS.md](docs/DIAGNOSTICS.md)) |
| `-f<pass>` `-fno-<pass>` | `mem2reg sccp dce cse copyprop licm strength inline tailcall simplifycfg peephole` |
| `--emit-ast` `--emit-hir` `--emit-ir` `--emit-ir-lines` | inspect each stage |
| `-fsyntax-only` · `--diagnostics-format=json` · `--color=…` · `-ferror-limit=n` | diagnostics control |
| `-fmacro-expansion-limit=n` · `--restrict-includes` | resource guards (used by the playground) |
| `-l<lib>` `-L<dir>` `-static` · `-v` · `--version` `--help` | linking, tracing, info |

### Build, test, benchmark

```bash
cargo test --workspace                       # everything below, on Linux with gcc + as
CINDER_DIFF_GCC=1 cargo test --release --test e2e         # every program also through gcc -O0 and compared
CINDER_FUZZ_N=600 cargo test --release --test fuzz_opt    # optimizer fuzzing against gcc
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check
scripts/bench.sh                             # benchmarks (needs gcc as the yardstick)
```

On Windows/macOS: `bash scripts/dev-up.sh` starts a Linux container with the repository mounted; then
`bash scripts/dx.sh "cargo test --workspace"` runs anything inside it. Details in [docs/TESTING.md](docs/TESTING.md).

## How it works

```
 source ─► lexer ─► preprocessor ─► parser ─► AST
                                               │
                         sema: names, types, constants, layout, warnings
                                               ▼
                                   typed HIR (all conversions explicit)
                                               │ lowering
                                               ▼
        SSA IR (locals are allocas) ─► mem2reg ─► sccp ─► cse ─► licm ─► inline ─► … ─► SSA IR
                                               │        (each pass verified; -O0/-O1/-O2)
                       isel ─► MIR (virtual regs) ─► linear-scan regalloc ─► peephole
                                               ▼
                                     x86-64 AT&T assembly ─► as ─► cc (link only) ─► executable
```

| stage | source | what it does | doc |
|-------|--------|--------------|-----|
| lexer, source map | `lex.rs` `source.rs` `literal.rs` | preprocessing tokens, exact spans through line splices | [FRONTEND](docs/FRONTEND.md) |
| preprocessor | `pp/` | hide-set macro expansion, `#if`, includes, bundled libc headers | [FRONTEND](docs/FRONTEND.md) |
| parser | `parse/` `ast.rs` | recursive descent, typedef-name tracking, Clang-style recovery | [FRONTEND](docs/FRONTEND.md) |
| semantic analysis | `sema/` `types.rs` `hir.rs` | scopes, type checking, conversions, SysV layout, initializers, constant evaluation, warnings | [SEMA](docs/SEMA.md) |
| IR + lowering | `ir/` `lower/` | SSA IR, verifier, dominators, SysV aggregate passing | [IR](docs/IR.md) |
| optimizer | `opt/` | 10 passes, alias analysis | [OPTIMIZER](docs/OPTIMIZER.md) |
| backend | `backend/` `abi.rs` | instruction selection, linear scan, frames, peephole, jump tables | [BACKEND](docs/BACKEND.md) |
| diagnostics | `diag.rs` | Clang-style rendering, warning groups, JSON | [DIAGNOSTICS](docs/DIAGNOSTICS.md) |
| playground | `crates/cinder-server` `web/` | HTTP API, sandboxed execution, Monaco UI | [PLAYGROUND](docs/PLAYGROUND.md) [SANDBOX](docs/SANDBOX.md) |

The same function at `-O0` and `-O2` (`cinder --emit-ir`), straight from the compiler:

```
-O0                                      -O2
%a.3 = alloca 4, align 4  (×5 slots)     entry:
store i32 %a.0, %a.3  …                    %13 = mul i32 %a.0, %b.1     ; hoisted out of the loop
for.cond.1:                              for.cond.1:
  %8 = load i32, %i.7                      %i.22 = phi i32 [ 0, entry ], [ %19, for.body.2 ]
  %9 = load i32, %n.5                      %s.21 = phi i32 [ 0, entry ], [ %17, for.body.2 ]
  %10 = icmp slt i32 %8, %9                %10 = icmp slt i32 %i.22, %n.2
  condbr %10, for.body.2, for.end.4        condbr %10, for.body.2, for.end.3
for.body.2:  (loads/stores around each op)  for.body.2:  (3 adds, no memory traffic)
```

## What is supported

**Language (C11):** all integer types including `long long` and `_Bool`; `float` and `double`; pointers, arrays,
**variable length arrays**; structs and unions with **bit-fields**, flexible array members and anonymous members;
enums, typedefs, function pointers; **variadic functions**; designated initializers with brace elision, compound
literals; `_Generic`, `_Static_assert`, `_Alignas`/`_Alignof`; `inline`, `restrict`, `_Noreturn`; `switch` (including
Duff's device) with jump tables for dense cases; `goto`; wide and Unicode string/char literals; `__attribute__((packed,
aligned))`, `#pragma pack`; `__builtin_va_*`, `__builtin_expect/unreachable/trap/inf/nan`.
**ABI:** System V x86-64 — small structs in registers by eightbyte class, large ones via `sret`/`byval`, varargs with
`al`, 16-byte stack alignment; verified against GCC both ways by generated tests.
**Preprocessor:** object/function-like macros, `#`, `##`, variadics and `__VA_OPT__`, `#if` expressions, `defined`,
`__has_include`, `#include` search paths, `#pragma once`, `_Pragma`, `__COUNTER__`, predefined macros.
**Libc:** compiles against bundled headers (`assert ctype errno float inttypes iso646 limits math stdalign stdarg stdbool
stddef stdint stdio stdlib stdnoreturn string time unistd sys/types`) and links with the system glibc.
**Optimizations:** mem2reg, sparse conditional constant propagation, strength reduction, copy propagation, CSE with
redundant-load elimination, DCE + dead stores, CFG simplification, LICM, inlining, tail-call elimination (self recursion
→ loops, sibling calls → `jmp`). See [docs/OPTIMIZER.md](docs/OPTIMIZER.md).

## Benchmarks

Linux x86-64 container (Debian 12, gcc 12.2), best of 3, seconds. Every binary's output is checked against `gcc -O0`.

| benchmark | cinder -O0 | cinder -O2 | -O0 → -O2 | gcc -O0 | gcc -O2 | cinder -O2 / gcc -O2 |
|-----------|-----------:|-----------:|----------:|--------:|--------:|---------------------:|
| fib | 0.221 | 0.151 | 1.46× | 0.224 | 0.051 | 2.96× |
| mandelbrot | 0.116 | 0.061 | 1.90× | 0.112 | 0.055 | 1.11× |
| matmul | 0.265 | 0.088 | 3.01× | 0.275 | 0.035 | 2.51× |
| nbody | 0.360 | 0.251 | 1.43× | 0.557 | 0.249 | 1.01× |
| sieve | 1.471 | 0.625 | 2.35× | 0.916 | 0.484 | 1.29× |
| sort | 0.643 | 0.587 | 1.10× | 0.703 | 0.359 | 1.64× |
| spectral | 0.151 | 0.077 | 1.96× | 0.132 | 0.070 | 1.10× |
| strings | 0.392 | 0.344 | 1.14× | 0.569 | 0.387 | 0.89× |
| vm | 1.552 | 0.899 | 1.73× | 1.454 | 0.806 | 1.12× |

The optimizer buys 1.1×–3.0×; Cinder's `-O2` is within ~1.0–1.3× of GCC's on five of nine programs and 1.6–3× slower
on `sort`, `matmul` and `fib`. What GCC does that Cinder does not (induction-variable strength reduction, phi
coalescing, inline `sqrt`, live-range splitting, vectorization) is listed in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## Diagnostics

```
uninitialized.c:14:12: warning: variable 'value' may be uninitialized when used here [-Wmaybe-uninitialized]
  14 |     return value * 2;
     |            ^~~~~
uninitialized.c:11:9: note: variable 'value' declared here
  11 |     int value;
     |         ^~~~~

type_errors.c:18:17: error: use of undeclared identifier 'coutn'; did you mean 'count'?
  18 |     int total = coutn + 1;
     |                 ^~~~~
     |                 count
```

Parsing and analysis recover after errors, so one run reports many independent problems. Warning flags and the
uninitialized-variable analysis are described in [docs/DIAGNOSTICS.md](docs/DIAGNOSTICS.md).

## The playground

```bash
docker build -t cinder-playground .
docker run --rm -p 8080:8080 cinder-playground      # http://localhost:8080
```

* **Editor:** Monaco, with assembly/IR/AST/typed-tree/preprocessed views, `-O0/-O1/-O2`, 18 examples, diagnostics as
  squiggles, source↔output line highlighting, stdin, shareable links (code lives in the URL), dark/light, mobile layout.
* **API:** `POST /api/compile {code, optLevel, emit}` · `POST /api/run {code, optLevel, stdin}` · `GET /api/health`
  ([docs/PLAYGROUND.md](docs/PLAYGROUND.md)).
* **Running strangers' code is the dangerous part**, so execution is layered: per-slot unprivileged uid, `chroot` into an
  empty directory holding only the program, rlimits (CPU 2 s, 128 MB, 16 processes, 64 KiB output), a seccomp-BPF filter
  (no network, no `ptrace`/`mount`/namespaces/identity changes), a memory watchdog over *all* of a program's processes,
  killing of everything the program started, a concurrency limit and per-IP rate limits. The server self-tests the sandbox at
  start-up and refuses to run code without the isolation it was configured to require. 26 integration tests run hostile
  programs against it. [docs/SANDBOX.md](docs/SANDBOX.md) has the threat model and the limitations.

### Deploying on Render

The repository ships a `Dockerfile` and a Blueprint, [`render.yaml`](render.yaml): in the Render dashboard choose
**New → Blueprint**, select the repository and apply. The step-by-step guide (also for creating the Web Service by hand),
all environment variables, how to check that the sandbox is fully active, and troubleshooting are in
[docs/DEPLOY.md](docs/DEPLOY.md). The deploy has not been exercised on Render from this repository; the guide says how to
verify it in a minute.

## Known limitations

Anything not implemented is **rejected with an error** (mostly `error: not yet supported: …`), never miscompiled:

* `long double` (x87), `_Complex`, `_Atomic`, thread-local storage (`_Thread_local`), `typeof`, `__int128`, `_Float16`
* inline `asm` and assembler labels on declarations, K&R function definitions, computed `goto` / address-of-label,
  GNU statement expressions, case ranges (`case 1 ... 5`), nested functions, VLA compound literals

Other limits:

* **Platform:** x86-64 Linux, System V ABI only. Needs the system `as` and `cc` for assembling and linking. Position-dependent
  code (`-no-pie`); `-fPIC` is accepted and ignored, so no shared libraries.
* **Parallel programming:** no OpenMP (`<omp.h>` is not found, and `#pragma omp` lines are ignored with
  `-Wunknown-pragmas`), no MPI (`<mpi.h>`), no threads or atomics (`<pthread.h>`, `<threads.h>`, `<stdatomic.h>`).
  A missing header is reported with a note saying so. The playground additionally runs programs without network access
  and with at most 16 processes, so MPI-style programs could not run there anyway.
* **Libc surface:** only the headers listed above are bundled (no `signal.h`, `setjmp.h`, `wchar.h`, `locale.h`,
  `complex.h`, `stdatomic.h`, `threads.h`, `fenv.h`, …). `-isystem /usr/include` can be tried, but glibc's headers use many
  GNU extensions and are not guaranteed to parse.
* **Builtins:** only `__builtin_va_*`, `expect`, `unreachable`, `trap`, `inf`/`huge_val`, `nan` (no `popcount`, `clz`, `memcpy`, …).
* **`goto`** out of a block with a variable length array does not restore the stack pointer (the space is reclaimed at
  function exit); `break`, `continue` and normal exits do.
* **Preprocessor:** `#line` is accepted but does not renumber; diagnostics inside macro expansions have no "in expansion
  of" backtrace; macro expansion is capped at 1,000,000 tokens per translation unit (`-fmacro-expansion-limit=n`).
* **Debug info:** `.file`/`.loc` line tables only, no DWARF type information.
* **Optimizer:** no loop unrolling, vectorization, induction-variable optimization or interprocedural analysis beyond
  inlining; the register allocator has no live-range splitting (floating-point values live across calls are spilled).
  `-O3` and `-Os` behave like `-O2`. Optimizations are validated by testing (e2e at three levels, GCC differential,
  fuzzing), not proven.
* **Playground sandbox:** a seccomp *denylist* inside a container, not a hardware-isolated VM; see
  [docs/SANDBOX.md](docs/SANDBOX.md) before exposing it to hostile users at scale.

## Repository layout

```
crates/cinder/            the compiler (library + `cinder` binary), no dependencies; src/include = bundled libc headers
crates/cinder-server/     axum HTTP API + sandboxed runner
web/                      playground front end (no build step)
tests/e2e/*.cases         243 end-to-end programs      tests/diag/   diagnostic goldens      tests/bench/  benchmarks
tests/gen/abi.py          ABI interoperability test generator
scripts/                  Docker dev loop, benchmark driver
docs/                     one page per stage (+ sandbox, deploy, testing, roadmap)
Dockerfile  render.yaml
```

There is no CI workflow in the repository. The checks it would run are plain commands, all listed in
[docs/TESTING.md](docs/TESTING.md): build + test, fmt + clippy, differential testing against GCC, fuzzing, benchmarks,
a Rust 1.82 build of the compiler crate, and building and probing the playground image.

## Documentation

[Front end](docs/FRONTEND.md) · [Semantic analysis](docs/SEMA.md) · [IR and lowering](docs/IR.md) ·
[Optimizer](docs/OPTIMIZER.md) · [Backend](docs/BACKEND.md) · [Diagnostics](docs/DIAGNOSTICS.md) ·
[Benchmarks](docs/BENCHMARKS.md) · [Testing](docs/TESTING.md) · [Playground](docs/PLAYGROUND.md) ·
[Sandbox](docs/SANDBOX.md) · [Deploying on Render](docs/DEPLOY.md) · [Roadmap and milestones](docs/ROADMAP.md)
