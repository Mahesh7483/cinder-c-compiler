# The SSA IR and lowering

```
HIR ─► lowering ─► SSA IR (locals as allocas) ─► optimizer ─► SSA IR ─► backend
```

Source: `ir/{mod,cfg,verify,print}.rs`, `lower/{mod,expr,stmt,data,abi}.rs`, `abi.rs`.
Try it: `cinder --emit-ir file.c` (add `-O2` to see the optimized form,
`--emit-ir-lines` for `; L<n>` source-line comments on every instruction).

## Shape of the IR

A `Module` holds symbols (functions, data, string literals) and function bodies. A `Func` is a
control-flow graph of basic blocks; instructions live in an arena and blocks list them by id.
**Every value is defined exactly once** (SSA), either by an instruction or as a parameter, and
`phi` instructions merge values at joins. Syntax is LLVM-flavoured but much smaller.

**Types.** Only scalars have IR types: `i8 i16 i32 i64 ptr f32 f64`. Signedness is not part of a type;
it is carried by the operation (`sdiv`/`udiv`, `slt`/`ult`, `sext`/`zext`). Structs, unions and arrays
never appear as values: they live in memory and are copied with `memcpy`.

**Instructions.**

| group | instructions |
|-------|--------------|
| memory | `alloca size, align`, `load`, `store` (both with a `volatile` flag), `memcpy`, `memset`, `ptradd base, byteoffset` |
| integer | `add sub mul sdiv udiv srem urem and or xor shl lshr ashr`, `neg not` |
| float | `fadd fsub fmul fdiv`, `fneg` |
| compare / select | `icmp` (`eq ne slt sle sgt sge ult ule ugt uge`), `fcmp` (`oeq une olt ole ogt oge`), `select` |
| conversion | `trunc zext sext fptrunc fpext fptosi fptoui sitofp uitofp ptrtoint inttoptr` |
| calls | `call` (direct or indirect, with `variadic` and `tail` flags, up to two results for register-returned structs) |
| SSA | `phi` |
| stack | `dynalloca`, `stacksave`, `stackrestore` (variable length arrays) |
| varargs | `va_reg_save_area`, `va_stack_args` (the register save area and stack-argument pointer used by `va_start`), `trap` |

**Terminators:** `br`, `condbr`, `switch` (value, cases, default), `ret` (zero, one or two values),
`unreachable`.

Every instruction records the source line it came from. The backend turns that into `.loc`
directives (so `gdb` and the playground can map assembly back to C) and `--emit-ir-lines` prints it.

## Lowering (`lower/`)

Lowering follows the same strategy as Clang at `-O0`, which keeps it simple and trustworthy:

* every local variable (and every parameter) gets an `alloca` slot; reads are `load`, writes are `store`.
  Promotion to SSA is the optimizer's job (`mem2reg`), so `-O0` is a faithful, debuggable translation
  and `-O0` vs `-O2` in the benchmarks measures the optimizer honestly;
* `&&`, `||`, `?:` and `!` become control flow with `phi`s only where a value must merge;
* `switch` becomes the IR `switch` terminator;
* aggregates are always handled by address: assignment is `memcpy`, a call returning a big struct passes a
  hidden `sret` pointer, a `byval` argument is copied into the outgoing area;
* **System V aggregate passing.** A struct of at most 16 bytes is classified per eightbyte (INTEGER or
  SSE, e.g. `{double, long}` → `xmm0` + `rax`) and travels in registers as scalar pieces; larger structs go
  to memory, as does anything the classification rules send there (e.g. packed or unaligned fields). The classification lives in `lower/abi.rs`, the register
  assignment in `abi.rs`, which both the lowering (for `va_start` offsets) and the backend (to place
  arguments) use so they cannot disagree. The `tests/gen/abi.py` generator checks this against GCC in
  both directions (Cinder calling GCC-compiled code and the reverse) over generated signatures that mix
  integers, floats, small and large structs, unions and bit-fields (`tests/e2e/lang_abi.cases`);
* variadic functions spill the argument registers into a register save area and `va_arg` walks it exactly
  as the ABI's `va_list` (24 bytes) prescribes, so Cinder code interoperates with `printf`, `vsnprintf`, etc.;
* static data (`lower/data.rs`): globals with their flattened initializer entries, string literals
  (deduplicated), and relocations such as `&global + 8` inside initializers.

## Verifier (`ir/verify.rs`)

Run after lowering and — in test and debug builds — **after every optimization pass**, so a buggy pass is
caught at the pass that broke the invariant (the panic names the pass and prints the function). It checks:

* every value is defined once, and every use is dominated by its definition (SSA dominance, computed
  with `ir/cfg.rs`) — no use of undefined or removed values;
* phi nodes come first in their block and have exactly one incoming value per predecessor;
* operand types match instruction types (`add i32` takes two `i32`s, `load` takes a `ptr`, branch
  conditions are integers, `ret` matches the function's result types, switch values match their type);
* every block ends in a terminator, branch targets exist, switch cases are unique;
* `alloca` appears only in the entry block, calls name valid symbols.

`ir/cfg.rs` provides predecessors, reverse post-order, dominators (Cooper–Harvey–Kennedy), dominance
frontiers, natural loops and unreachable-block removal; mem2reg, LICM and the verifier share it.

## Example

```c
int sum(int a, int b, int n) { int s = 0; for (int i = 0; i < n; i++) s += a * b + i; return s; }
```

`-O0` — straight from lowering, locals in memory (trimmed):

```
define i32 @sum(i32 %a.0, i32 %b.1, i32 %n.2) {
entry:
  %a.3 = alloca 4, align 4          ; one slot per parameter and local
  %s.6 = alloca 4, align 4
  %i.7 = alloca 4, align 4
  store i32 %a.0, %a.3
  store i32 0, %s.6
  store i32 0, %i.7
  br for.cond.1
for.cond.1:
  %8 = load i32, %i.7
  %9 = load i32, %n.5
  %10 = icmp slt i32 %8, %9
  condbr %10, for.body.2, for.end.4
for.body.2:
  %11 = load i32, %a.3
  %12 = load i32, %b.4
  %13 = mul i32 %11, %12
  ...
```

`-O2` — mem2reg made everything SSA, and LICM hoisted `a * b` out of the loop:

```
define i32 @sum(i32 %a.0, i32 %b.1, i32 %n.2) {
entry:
  %13 = mul i32 %a.0, %b.1               ; hoisted: loop invariant
  br for.cond.1
for.cond.1:
  %i.22 = phi i32 [ 0, entry ], [ %19, for.body.2 ]
  %s.21 = phi i32 [ 0, entry ], [ %17, for.body.2 ]
  %10 = icmp slt i32 %i.22, %n.2
  condbr %10, for.body.2, for.end.3
for.body.2:
  %15 = add i32 %13, %i.22
  %17 = add i32 %s.21, %15
  %19 = add i32 %i.22, 1
  br for.cond.1
for.end.3:
  ret i32 %s.21
}
```

## Tests

`lower/tests.rs` pins the shape of the IR for each language construct (golden fragments, including the
`; L<n>` line annotations), `ir/verify.rs` is exercised both directly and by every optimizer test, and
`crates/cinder/tests/backend_ir.rs` feeds hand-written IR straight to the backend. See
[OPTIMIZER.md](OPTIMIZER.md) for the passes.
