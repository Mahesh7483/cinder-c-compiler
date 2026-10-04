# The optimizer (`crates/cinder/src/opt/`)

Cinder lowers every local variable to a stack slot (`alloca` + `load`/`store`)
and leaves it to the optimizer to build SSA form. At `-O0` nothing runs, so the
generated code is a faithful, easy-to-debug transcription of the source; at
`-O1`/`-O2` the passes below turn it into registers, folded constants and
loops without redundant work.

```
cinder -O2 --emit-ir prog.c      # the IR the backend will see, after optimization
cinder -O2 -fno-licm prog.c      # switch a single pass off
cinder -O0 -fmem2reg prog.c      # ... or one on (mem2reg only)
```

`-fno-<pass>` / `-f<pass>` take effect in command-line order and may name any
pass in the table. Unknown `-f` flags are accepted and ignored (GCC habit).

## Passes

| pass          | level | what it does |
|---------------|-------|--------------|
| `mem2reg`     | -O1   | Promotes stack slots whose address is only used by plain loads and stores of one scalar type to SSA values (Cytron et al.: phis at the iterated dominance frontier of the storing blocks, renaming along the dominator tree). Reads before any store become `undef`. |
| `sccp`        | -O1   | Sparse conditional constant propagation. Folds constants *and* prunes branches whose condition becomes constant, so dead arms no longer pollute phis. |
| `strength`    | -O1   | Local algebraic rewrites: identities (`x+0`, `x*1`, `x^x`), constants to the right, `x-c` → `x+(-c)`, `x*2^k` → shift, unsigned `/ %` by `2^k` → shift / mask, signed `/ %` by `2^k` → bias-and-shift sequence, cast chains, `(a<b) != 0` → `a<b`, unsigned compares against 0, `(x+c1)+c2`, `ptradd` chains. |
| `copyprop`    | -O1   | Removes trivial phis (all inputs the same value). |
| `cse`         | -O1   | Value numbering of pure instructions over the dominator tree (commutative operands in canonical order) plus *block-local load forwarding*: a load of an address that was just loaded or stored reuses that value until something may write the location. |
| `dce`         | -O1   | Mark-and-sweep from side effects (also kills dead phi cycles), removal of write-only stack slots (including through `ptradd`, `memset`, `memcpy` destinations), and block-local dead-store elimination. |
| `simplifycfg` | -O1   | Folds constant/identical-target branches and switches, deletes unreachable blocks, merges a block into its only predecessor, bypasses empty forwarding blocks, copies tiny `phi`+`ret` blocks into their predecessors (this is what exposes `return c ? a : f(x)` as a tail call), and lays blocks out in reverse post-order with the first successor adjacent. |
| `licm`        | -O2   | Gives each natural loop a preheader and hoists loop-invariant instructions into it, innermost loops first. |
| `inline`      | -O2   | Inlines direct calls to small, non-recursive, non-variadic functions, callees first. Deletes internal functions that end up unreferenced. |
| `tailcall`    | -O2   | Self tail recursion becomes a loop; other calls in tail position are marked and emitted as `jmp`. |
| `peephole`    | all   | Backend clean-ups on allocated machine code (on at `-O0` too; `-fno-peephole` turns it off). |

### Pipeline order

1. `simplifycfg`, `mem2reg`, then up to six rounds of
   `sccp → strength → copyprop → cse → licm → dce → simplifycfg` until nothing changes
   (every function);
2. `tailcall` (self recursion → loops), then the scalar pipeline again on the functions it changed.
   Doing this *before* inlining turns such functions non-recursive, so they can be inlined;
3. `inline`, then the scalar pipeline again on the functions that grew;
4. sibling tail calls are marked last, because nothing may be inserted after a marked call.

## Rules that keep optimizations from changing behaviour

* **No speculation of anything that can fault.** `sdiv/udiv/srem/urem` are only
  hoisted by LICM when the divisor is a constant other than `0` and `-1`; a
  load is hoisted only when nothing in the loop can write that location *and*
  it cannot fault (an in-bounds stack slot or global, or a load in the loop
  header, which runs whenever the preheader does).
* **Constant folding mirrors the hardware.** Integer folds wrap at the width of
  the type; division by zero, `INT_MIN / -1`, oversized shifts and out-of-range
  float→int conversions are *left for run time*; int→float is rounded once
  (never via an intermediate `double`).
* **NaN-correct comparisons.** `(a == b) == 0` becomes `a != b`, but ordered
  float predicates (`<`, `<=`, ...) are never negated.
* **Memory is conservative.** Two accesses may alias unless they hit different
  stack slots/globals or disjoint constant ranges of one object
  (`opt/alias.rs`). A call can only touch a stack slot whose address escaped.
  Volatile accesses are never merged, forwarded, hoisted or dropped.
* **Tail calls need a private frame.** They are only formed in functions whose
  stack slots never have their address observed (so nothing can read the frame
  after the jump), only when every argument travels in registers, and never for
  variadic callees or when the call's results are not exactly what is returned.
* **Inlined by-value aggregates are copied** (the callee may modify its
  parameter); inlined allocas move to the caller's entry block.

## How it is tested

* **Verifier after every pass.** `ir::verify` checks SSA dominance, phi shape
  (one entry per predecessor), operand types and structure. It runs after each
  pass in debug builds and tests (and once on the final IR in release builds);
  a failure panics with the name of the pass and the offending IR.
* **Unit tests** (`opt/tests.rs`, ~85): each pass on small C functions with only
  that pass enabled, asserting on the resulting IR (including the *negative*
  cases: no hoisting past stores/calls, no merging across sibling branches, no
  tail call with stack arguments, ...).
* **End-to-end** (`tests/e2e/*.cases`, run at -O0, -O1 and -O2): every program must print the
  same output and exit status at every level, which is also the GCC-blessed
  result. `tests/e2e/optimizer.cases` targets this directory specifically
  (signed division edge cases, tail recursion 50 million deep, mutual and
  indirect tail calls, aliasing, by-value structs, NaN comparisons, ...); cases
  marked `min-opt: 2` rely on tail calls to fit in the stack.
* **Differential fuzzing** (`crates/cinder/tests/fuzz_opt.rs`): random,
  terminating, UB-free programs (wrapping arithmetic, masked shifts and
  indices, local arrays of `unsigned`/`unsigned char`/`short`, pointer aliasing,
  loops, `switch`, helper calls, tail recursion) are compiled by `gcc -O0` and by
  cinder at every level and compared. On a mismatch it reports the seed, keeps
  the program, and *bisects the culprit* by re-running with each pass disabled.
  `CINDER_FUZZ_N=2000 cargo test --release --test fuzz_opt` is a soak run.
  The harness is itself validated by mutation: breaking the signed-division
  bias in `strength.rs` is caught in 55 of 200 programs and attributed to
  `strength`.

## Known limitations

* CSE forwards loads only inside a basic block; there is no global value
  numbering of memory, no store sinking, no loop unrolling or vectorization.
* Division by a constant is a real `idiv`/`div` (no magic-number
  multiplication), except for powers of two.
* The inliner uses a size threshold only (40 instructions, 100 for `inline`
  functions, 400 for an internal function with a single call site).
* Sibling tail calls are limited to register-only argument lists.
