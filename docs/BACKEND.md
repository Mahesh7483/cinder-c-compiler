# x86-64 backend

```
IR ─► isel ─► MIR (virtual regs) ─► linear-scan regalloc ─► peephole ─► frame layout ─► AT&T text
                                                                                          │
                                                                              as ─► cc (link only) ─► executable
```

Source: `backend/{isel,isel_inst,mir,regalloc,peephole,frame,emit}.rs`, `abi.rs`.
Try it: `cinder -S -O2 file.c -o -` prints the assembly (with `.loc` directives, so the line
mapping in the playground comes straight from this output). The assembler is the system `as`;
`cc` is used **only as a linker driver** (to find `crt1.o`, libc and `ld`). No GCC, Clang or LLVM
code generation is involved anywhere.

Target: x86-64 Linux, System V ABI, AT&T syntax, position-dependent code (`cc -no-pie`).

## Machine IR (`mir.rs`)

x86-64 instructions over *virtual or physical registers*, operands in AT&T order (`src, dst`), with
addressing modes `disp(base, index, scale)` where the base may be a register, a stack slot, a
symbol (`%rip`-relative), or the top of the outgoing-argument area. Instruction selection produces
MIR with virtual registers; the allocator rewrites them; emission prints text.

## Instruction selection (`isel.rs`, `isel_inst.rs`)

* **Addressing-mode folding.** `alloca`, `ptradd` and symbol addresses are folded into x86 addressing
  modes at their `load`/`store`/`memcpy` uses, so `a[i]` is one instruction with `(base,index,4)`.
  An address used *outside* the block that computes it (a hoisted `&A[i]`) is computed once into a
  register instead, so instruction selection does not undo loop-invariant code motion.
* **Compare-and-branch fusion.** An `icmp`/`fcmp` whose only use is the block's `condbr` becomes
  `cmp` + `jcc` (`ucomisd` + `jcc`, with an extra parity check so NaN compares false) with no boolean
  in a register.
* **Phi elimination.** Critical edges are split first, then each phi becomes parallel copies on the
  incoming edges with parallel-copy semantics: a source that is also the destination of another copy in
  the group is first saved in a temporary virtual register, so swaps and rotations are correct.
* **Switch.** Sparse switches compile to a compare chain; a switch with at least four cases whose value
  range is at most `3 × cases + 10` (and 2048) compiles to a bounds check and `jmp *table(,%idx,8)` with
  the table emitted into `.rodata` (`.LJT…`). The bytecode-VM benchmark gained ~17% from this.
* **Calls** use the shared argument assignment in `abi.rs` (integer args in `rdi rsi rdx rcx r8 r9`,
  floats in `xmm0–7`, the rest on the stack, `al` = number of vector registers for variadic callees,
  `sret`/`byval` for large structs, `rax:rdx` / `xmm0:xmm1` for two-register results).
  Calls marked `tail` by the optimizer become `jmp` after the frame is torn down (indirect ones via `r11`).
* **Block copies.** `memcpy`/`memset` of at most 128 bytes are unrolled with 16-, 8-, 4-, 2- and 1-byte
  moves (SSE `movups` for the wide part); larger ones use `rep movsb`/`rep stosb`.
* **Floating point** uses scalar SSE (`addsd`, `mulss`, `cvtsi2sd`, `ucomisd`, …). `cvtsi2sd`,
  `cvtss2sd` and `cvtsd2ss` write only the low lane, so the destination is cleared with `xorps`
  first — otherwise every conversion carries a false dependency on the register's previous value (this
  alone made the `spectral` benchmark 4.3× faster; see [BENCHMARKS.md](BENCHMARKS.md)).
* **Integer details.** 32-bit operations rely on implicit zero-extension; `idiv`/`div` use the fixed
  `rax`/`rdx` pair with `cdq`/`cqo` or a zeroed `edx`; shifts use `cl`; division and remainder by a
  constant power of two are handled earlier by the optimizer's `strength` pass.
* **Variable length arrays.** `dynalloca` subtracts the (16-byte-rounded) size from `rsp` and addresses the
  new object just *above* the fixed-size outgoing-argument area (`Base::OutgoingTop`), so calls still find
  their arguments at `rsp` and `rsp` stays 16-byte aligned. `stacksave`/`stackrestore` copy `rsp` in and out;
  lowering emits them around blocks that declare a VLA, and for `break`/`continue` out of such blocks.

## Register allocation (`regalloc.rs`)

A classic linear-scan allocator with spilling, extended to honour the fixed-register constraints of the
ABI and of x86 instructions:

1. number instructions in layout order and compute block successors;
2. compute liveness of virtual registers by iterative dataflow;
3. build one live interval per virtual register (a single `[start, end]` range, no holes);
4. compute *busy ranges* for physical registers that appear explicitly in the code — argument and
   result registers around calls, `rax`/`rdx` for division, `rcx` for shifts, caller-saved registers
   clobbered by calls. A virtual register may not take a physical register whose busy range overlaps its
   interval. This one rule implements every fixed-register constraint without special cases;
5. scan intervals by start position, preferring the register of a copy-related interval (a *coalescing
   hint*, so `mov` between related values usually disappears), and when no register is free spill the
   interval that **ends furthest away**;
6. rewrite the code, replacing virtual registers, and wrap spilled values in loads and stores through
   reserved scratch registers (`r10`/`r11`, `xmm14`/`xmm15`).

Allocatable: 12 general-purpose registers (`rax rcx rdx rbx rsi rdi r8 r9 r12–r15`; `r10`/`r11` are the
scratch pair, `rsp`/`rbp` belong to the frame) and 14 vector registers (`xmm0–xmm13`). Intervals that cross a call may only use callee-saved registers
(`rbx r12–r15`); the ABI has no callee-saved XMM registers, so a floating-point value live across a call
is spilled. Used callee-saved registers are saved in the prologue and restored in the epilogue.

Known limits (also in the README): no live-range splitting, so a long interval with one hot loop and a
cold call still lives in memory when pressure is high; no coalescing across phis.

## Frame layout (`frame.rs`)

```
high addresses
  incoming stack arguments      rbp + 16 ...
  return address                rbp + 8
  saved rbp                     rbp
  callee-saved registers        rbp - 8 ... rbp - 8k
  locals and spill slots        below that, each at its own alignment (over-aligned locals supported)
  outgoing argument area        at rsp (bottom of the frame)
low addresses
```

The total below `rbp` is a multiple of 16, so `rsp` is 16-byte aligned at every call. Every function
keeps a frame pointer, which makes `gdb` backtraces and the playground's assembly easy to read.

## Peephole (`peephole.rs`)

Runs on allocated MIR; toggle with `-fno-peephole`:

* delete `mov %r, %r` (64-bit) and vector self-moves, and adds/subs/shifts by zero;
* `cmp $0, %r` → `test %r, %r`; `lea (%r), %d` → `mov %r, %d`;
* delete a `jmp` to the next block; turn `jcc L1; jmp L2` (with `L1` next) into `jncc L2`;
* **loop rotation**: a loop laid out test-first (`jmp test; body: …; test: cmp; jcc body`) keeps one
  conditional jump per iteration instead of a conditional plus an unconditional one.

## Example

`-O2` output for the `sum` function used in [IR.md](IR.md):

```asm
sum:
	pushq %rbp
	movq %rsp, %rbp
	movl %edi, %edi          # normalize 32-bit parameters (zero upper halves)
	movl %esi, %esi
	movl %edx, %edx
	.loc 1 6
	imull %esi, %edi         # a * b, hoisted out of the loop
	.loc 1 5
	movl $0, %r8d            # i
	movl $0, %r9d            # s
	jmp .Lsum_1
.Lsum_2:                     # loop body
	leal (%rdi,%r8,1), %esi  # a*b + i
	leal (%r9,%rsi,1), %esi  # s + (a*b + i)
	leal 1(%r8), %ecx        # i + 1
	movl %ecx, %r8d
	movl %esi, %r9d
.Lsum_1:
	cmpl %edx, %r8d          # i < n   (rotated: the test is at the bottom)
	jl .Lsum_2
	movl %r9d, %eax
	leave
	ret
```

The two `mov`s at the bottom of the loop are the phi copies the allocator could not coalesce (a listed
gap); everything else is what you would hope for.

## Tests

* `crates/cinder/tests/backend_ir.rs` — hand-built IR for paths `-O0` never reaches: register pressure
  and spilling, values live across calls, phi copy cycles, fixed-register instructions.
* The end-to-end suite compiles and runs 243 programs at `-O0`, `-O1` and `-O2` and compares with
  the expected output (blessed from GCC); with `CINDER_DIFF_GCC=1` it also compiles each program with
  `gcc -O0` and compares live. `tests/e2e/lang_abi.cases` checks calling-convention interoperability
  with GCC in both directions.
* `crates/cinder/tests/fuzz_opt.rs` generates random UB-free programs and compares Cinder at every
  level with `gcc -O0`; a failure names the first pass whose output diverges.
