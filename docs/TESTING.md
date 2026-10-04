# Testing

Everything below runs with a plain `cargo test --workspace` on Linux with `gcc` and `as` installed (the
default Docker dev loop, GitHub-hosted runners and the Docker build image all have them). The repository ships no
CI workflow, so run these commands yourself (or put them in the CI system of your choice): the full list is below.

```bash
cargo test --workspace                          # unit + integration + e2e (-O0/-O1/-O2) + diagnostics + server
CINDER_DIFF_GCC=1 cargo test --release --test e2e              # every e2e program also run through gcc -O0 and compared
CINDER_FUZZ_N=600 cargo test --release --test fuzz_opt         # 600 random programs, differential against gcc
CINDER_E2E_FILTER=vla CINDER_E2E_OPTS=2 cargo test --test e2e   # one family at one level
CINDER_BLESS=1 cargo test --test e2e                            # re-record expectations from GCC (review the diff!)
scripts/bench.sh                                # benchmarks (docs/BENCHMARKS.md)
```

> **Containers and zombies.** The server tests run fork bombs and `setsid` escapes; the killed children are
> reparented to PID 1. If PID 1 never reaps them they stay zombies, which still count against their uid's process
> limit, and a later run on the same uid range fails with `EAGAIN`. `scripts/dev-up.sh` therefore starts the dev
> container with `docker run --init`, the production image uses `tini` as PID 1, and normal Linux hosts and CI
> runners have an init that reaps.

## The layers

| layer | what | where | count |
|-------|------|-------|-------|
| **unit, per stage** | lexer, preprocessor (directives, macro corner cases, `#if`), parser (declarators, recovery), sema (every conversion, every diagnostic and warning, layout vs. SysV sizes, initializers, uninitialized-variable analysis), IR lowering (shape of the IR per construct), optimizer (each pass's before/after IR), options, diagnostics rendering | `crates/cinder/src/**/tests.rs` and inline | 432 |
| **backend on hand-built IR** | register pressure, spilling, values live across calls, phi copy cycles, fixed-register instructions: paths that `-O0` code (everything in memory) never reaches | `crates/cinder/tests/backend_ir.rs` | 6 |
| **diagnostic goldens** | the *complete rendered output* (carets, underlines, notes, fix-its, summary line) of 8 programs full of mistakes | `tests/diag/*.c` + `.expected`, `crates/cinder/tests/diag.rs` | 8 files |
| **end-to-end** | compile with the real binary, link, **run**, compare stdout and exit code — at `-O0`, `-O1` **and** `-O2` | `tests/e2e/*.cases`, `crates/cinder/tests/e2e.rs` | **243 programs × 3 levels** |
| **differential vs. GCC** | the same programs through `gcc -O0`; outputs must be identical (also how the expectations are produced) | `CINDER_DIFF_GCC=1` | 243 programs |
| **ABI interoperability** | generated functions with mixed int/float/struct/union/bit-field signatures; Cinder-compiled code calls GCC-compiled code and the reverse | `tests/gen/abi.py` → `tests/e2e/lang_abi.cases` (`gcc-file:`) | 12 programs |
| **optimizer fuzzing** | random UB-free programs; Cinder at every level must agree with `gcc -O0`; a failure prints the seed and names the culprit pass by disabling passes one at a time | `crates/cinder/tests/fuzz_opt.rs` | 60 by default (`CINDER_FUZZ_N=600` for a long run) |
| **IR verifier everywhere** | the verifier runs after lowering and after **every optimization pass** in test and debug builds; a pass that breaks SSA dominance, phi shape or types panics with the pass's name | `ir/verify.rs`, `opt/mod.rs` | all of the above |
| **server** | the HTTP API and the sandbox against hostile programs (infinite loop, fork bomb, memory bombs, output flood, network, `setsid` escape, file access, includes of host files, rate limiting, queueing, cleanup) | `crates/cinder-server/src/**` (15) and `tests/api.rs` | 15 + 26 |
| **benchmarks** | 9 programs at `-O0/-O1/-O2` vs GCC; each output is verified against `gcc -O0`, so it doubles as a correctness check on larger programs | `tests/bench/*.c`, `scripts/bench.sh` | 9 |
| **container** | build the production image, start it, check that `/api/health` reports the full sandbox, run a program and kill an infinite loop through HTTP (see below) | `Dockerfile` | manual |

The e2e suite is organized by theme (`basics`, `types`, `structs`, `libc_and_varargs`, `programs*`,
`optimizer`, `lang_decls`, `lang_exprs`, `lang_stmts`, `lang_vla`, `lang_libc`, `lang_abi`). The `programs`
bundles are complete programs — n-queens, a sudoku solver, SHA-256, CRC32, a Brainfuck interpreter, a recursive
descent calculator, a bytecode VM with `switch` dispatch, Dijkstra and Floyd–Warshall, LZ/RLE round trips, game of
life, big-number arithmetic, hash tables, matrix determinant and inverse, an n-body simulation, Mandelbrot — and the
`lang_*` bundles are corner cases compilers often get wrong: usual arithmetic conversions, integer promotion of
narrow types, shifts, `switch` fallthrough and Duff's device, `goto` out of loops, short-circuit evaluation,
float conversions, division and modulo semantics, `char` signedness, VLAs, bit-fields, compound literals,
designated initializers, `_Generic`, varargs with floats, and struct passing and returning in registers vs. memory.

## Checking the playground image

```bash
docker build -t cinder-playground .
docker run -d --name pg -p 8080:8080 cinder-playground
curl -s localhost:8080/api/health        # "sandbox":"chroot+uid+seccomp+rlimits", "status":"ok"
curl -s -X POST localhost:8080/api/run -H 'content-type: application/json' \
  -d '{"code":"int main(void){for(;;);}","optLevel":0,"stdin":""}'     # "signal":"SIGXCPU" after ~2 s
docker rm -f pg
```

The minimum supported Rust version of the compiler crate (no dependencies) is checked with
`cargo +1.82 build -p cinder && cargo +1.82 test -p cinder --lib`; the server crate needs a current toolchain.

## The e2e case format

```text
//// case: add_two_numbers
//// exit: 3
#include <stdio.h>
int main(void) { printf("hi\n"); return 3; }
//// stdout
hi
```

Other directives: `stdin`, `flags:`, `expect-error:` (the compile must fail with that text),
`file:` / `gcc-file:` (extra translation units compiled by Cinder or by `gcc -O2`), `min-opt:` and `skip-gcc`.
The programs must be UB-free and independent of unspecified evaluation order — otherwise "GCC says so" is not
a valid oracle — which is why several tests sequence side effects into separate statements.

## Regression policy

A bug found by any means gets a test at the lowest layer that can show it, and — if it was observable in a
program — an e2e case as well. Recent examples: the bit-field initializer bug found while writing playground
examples (`decl_bitfield_initializers_before_ordinary_members` and a sema unit test), the sparse-initializer and
macro-bomb memory fixes (`sparse_initializers_of_huge_objects_do_not_materialize_the_gaps`,
`exponential_macros_hit_the_expansion_budget`), the sandbox escape through `setsid()`
(`a_descendant_that_leaves_the_process_group_does_not_outlive_the_job`).

## What the tests do not prove

* They show agreement with GCC on the programs tried, not correctness in general; the fuzzer's programs use a
  small, UB-free subset of C.
* The sandbox tests show the listed attacks fail on the machine that ran them. See the limitations section of
  [SANDBOX.md](SANDBOX.md) for what the design does not cover.
