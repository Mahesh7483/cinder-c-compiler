# Benchmarks

`scripts/bench.sh` compiles every program in `tests/bench/` with Cinder at `-O0`, `-O1`
and `-O2` and with GCC (`-O0` and `-O2`, as a yardstick), runs each binary three times
and reports the best wall-clock time. **Every binary must print exactly what the
`gcc -O0` build prints** — a wrong answer shows up as `FAIL` instead of a time — so
the table is also a correctness check of the optimizer on larger programs.

```
cargo build --release
scripts/bench.sh                 # all benchmarks, best of 3
scripts/bench.sh -n 5 sort vm    # chosen benchmarks, best of 5
scripts/bench.sh -o results.md   # also write the Markdown table
```

## Results

Linux x86-64 in a Docker container (Debian 12, gcc 12.2), best of 3, seconds:

| benchmark | cinder -O0 | cinder -O1 | cinder -O2 | -O0 → -O2 | gcc -O0 | gcc -O2 | cinder -O2 / gcc -O2 |
|-----------|-----------:|-----------:|-----------:|----------:|--------:|--------:|---------------------:|
| fib | 0.221 s | 0.139 s | 0.151 s | 1.46× | 0.224 s | 0.051 s | 2.96× |
| mandelbrot | 0.116 s | 0.060 s | 0.061 s | 1.90× | 0.112 s | 0.055 s | 1.11× |
| matmul | 0.265 s | 0.258 s | 0.088 s | 3.01× | 0.275 s | 0.035 s | 2.51× |
| nbody | 0.360 s | 0.228 s | 0.251 s | 1.43× | 0.557 s | 0.249 s | 1.01× |
| sieve | 1.471 s | 0.678 s | 0.625 s | 2.35× | 0.916 s | 0.484 s | 1.29× |
| sort | 0.643 s | 0.567 s | 0.587 s | 1.10× | 0.703 s | 0.359 s | 1.64× |
| spectral | 0.151 s | 0.121 s | 0.077 s | 1.96× | 0.132 s | 0.070 s | 1.10× |
| strings | 0.392 s | 0.345 s | 0.344 s | 1.14× | 0.569 s | 0.387 s | 0.89× |
| vm | 1.552 s | 1.103 s | 0.899 s | 1.73× | 1.454 s | 0.806 s | 1.12× |

| benchmark | what it stresses |
|-----------|------------------|
| `fib` | recursion: calls, returns, callee-saved registers |
| `sort` | quicksort, mergesort and insertion sort on 3M / 1.5M / 20k pseudo-random ints |
| `matmul` | 360×360 `double` matrix multiply (3 repetitions): loop nests, array indexing, FP |
| `nbody` | 3M steps of the five-body simulation: FP arithmetic, `sqrt`, struct fields |
| `sieve` | sieve of Eratosthenes to 30M plus a Collatz scan |
| `vm` | a bytecode interpreter running 40M loop iterations: dense `switch` dispatch |
| `strings` | `memcpy`, FNV hashing, `memchr`, `snprintf` over 4 MiB buffers (mostly libc) |
| `mandelbrot` | 700×700 escape-time iteration: tight FP loops with data-dependent exits |
| `spectral` | spectral norm, n = 1200: divisions and small calls in hot loops |

## Reading the numbers

* `-O0 → -O2` is the speed-up from Cinder's own optimizer, 1.1× to 3.0×. It is large where
  `mem2reg` + CSE + LICM remove memory traffic from tight loops (`matmul`, `sieve`,
  `spectral`) and small where the time is spent in libc (`strings`) or in recursion
  and unpredictable branches (`fib`, `sort`).
* Against GCC `-O2`, Cinder `-O2` is within about 1.0–1.3× on five of the nine programs
  and about 1.6–3× slower on `sort`, `matmul` and `fib`. Two things are *better* than
  GCC here only because GCC's `-O0` is slower than ours on floating point (`nbody`:
  GCC `-O0` 0.56 s vs Cinder `-O0` 0.36 s) and because `strings` is dominated by `glibc`.
* Two code-generation fixes came straight out of this table:
  1. `cvtsi2sd`/`cvtss2sd` write only the low lane of an XMM register, so each
     conversion silently depended on the register's previous value and serialized the
     loop (`spectral` took 0.33 s). Clearing the destination first (`xorps`, as GCC does)
     made it 4.3× faster.
  2. Loops were laid out test-first (a conditional *and* an unconditional jump per
     iteration); they are now rotated so the test is at the bottom. Dense `switch`
     statements compile to a bounds check and an indirect jump through a table
     (`vm`: 1.03 s → 0.85 s).
* A hoisted address computation (`&A[i]` out of the inner loop) used to be re-folded into
  every use by instruction selection, undoing the hoist; address arithmetic used outside
  its defining block is now computed once into a register.

## Known gaps (what GCC does that Cinder does not)

* **No induction-variable strength reduction**: `B[k][j]` still multiplies by the row
  size each iteration (the `matmul` gap), and the loop counter is re-sign-extended.
* **No copy coalescing across phis**: loop-carried values go through two extra register
  moves per iteration (cheap on a modern core, but visible in the instruction count).
* **No inlining of recursion / tail-merging** like GCC's `fib` (which also unrolls one
  level), and no vectorization or loop unrolling.
* `sqrt`, `fabs`, ... are ordinary libm calls (GCC emits `sqrtsd`).
* The register allocator is linear scan with hole-free intervals and no live-range
  splitting: a value that lives across a call is kept in a callee-saved general-purpose
  register or, for floating point (the ABI has no callee-saved XMM registers), spilled
  for its whole lifetime.
