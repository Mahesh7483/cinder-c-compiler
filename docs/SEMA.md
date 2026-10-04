# Semantic analysis and the typed HIR

```
AST ─► sema ─► HIR (typed, explicit conversions) ─► lowering
```

Source: `types.rs`, `sema/{mod,expr,stmt,init,consteval,uninit}.rs`, `hir.rs`, `hir_dump.rs`.
Try it: `cinder --emit-hir file.c` prints the typed tree; `cinder -fsyntax-only -Wall file.c`
runs the front end and reports diagnostics only.

## What sema does

* **Name resolution** with real C scopes: ordinary identifiers, struct/union/enum tags and labels
  live in separate namespaces; block, function-prototype, file and `for`-init scopes nest correctly;
  redeclarations are checked for compatibility (`conflicting types for 'f'`) and tentative
  definitions are merged.
* **Type checking** of every expression and statement against C11 rules, with Clang-style messages
  (`invalid operands to binary expression ('int *' and 'double')`, `called object type 'int' is not a
  function`, …) and "did you mean" suggestions for misspelled identifiers.
* **Conversion insertion.** Every implicit conversion becomes an explicit `Cast` node:
  lvalue-to-rvalue, array and function decay, integer promotions, the usual arithmetic conversions,
  assignment and argument conversions, `_Bool` normalization. After sema, both operands of a binary
  operator have the same type, pointer arithmetic is a separate node with the element size
  attached, and compound assignments, `++`/`--` and the conditional operator carry their working type.
  Lowering is therefore nearly mechanical.
* **Constant evaluation** (`consteval.rs`) for array sizes, `case` labels, enumerators, bit-field
  widths, `_Static_assert`, and the address constants
  allowed in static initializers (`&global`, `&arr[3]`, `(char *)&s + 4`, string literals).
* **Struct/union layout** (`types.rs`) follows the System V x86-64 ABI exactly: natural alignment,
  trailing padding, bit-field packing (a bit-field never straddles its storage unit, zero-width
  fields force realignment, unnamed bit-fields), flexible array members, `_Alignas`/`alignas`, and the
  GNU `packed`/`aligned` attributes. Sizes are cross-checked in the unit tests against known SysV
  values, and the ABI test suite checks layout and passing against GCC byte for byte.
* **Initializers** (`init.rs`) flatten `{ ... }` lists — brace elision, designators in any order,
  `.a.b[2] = x`, overriding earlier entries, string literals initializing `char` arrays, incomplete
  array sizing — into a sorted list of `(byte offset, value)` entries. For static storage every
  entry must be a constant expression. Lowering can then emit either data or stores without redoing
  any C11 6.7.9 logic. Bit-field entries cover only the bytes their bits span.
* **Variable length arrays.** Each VLA declarator gets a hidden length local; `sizeof` of a VLA type is
  a runtime node; pointers to VLAs scale arithmetic by the runtime row size; parameter VLA sizes are
  evaluated at function entry. Block exit, `break` and `continue` restore the stack pointer (`goto`
  does not — see the README's limitations).
* **`_Generic`**, compound literals, `_Static_assert`, `_Noreturn`, `inline`, `restrict`, `_Bool`,
  `long long`, designated initializers, variadic functions, function pointers and
  `__builtin_{va_start,va_end,va_copy,expect,unreachable,trap,huge_val,inf,nan}` are supported. Other
  GNU attributes (`unused`, `noreturn`, `format`, …) are accepted and ignored.
* **Unsupported constructs** produce `error: not yet supported: <thing>` at the use site: `long double`,
  `_Complex`, `_Atomic`, thread-local storage, `typeof`, inline assembly, K&R definitions, computed
  goto, statement expressions, case ranges and VLA compound literals.

Errors never abort the pass. A failed expression becomes `HExprKind::Error` of type `int`, so one
mistake yields one diagnostic rather than a cascade, and unrelated errors elsewhere are still found.

## Warnings

Sema also issues the warnings (full list and semantics: [DIAGNOSTICS.md](DIAGNOSTICS.md)). Two are
analyses rather than local checks:

* **missing return** (`-Wreturn-type`): a flow check that the end of a non-void function is
  *reachable* — `for (;;) {}`, a trailing `abort()` or a `switch` whose every path returns do not trigger it.
* **uninitialized variables** (`sema/uninit.rs`): a definite-assignment analysis with `def` and
  `maybe` sets, described in DIAGNOSTICS.md.

## The HIR (`hir.rs`)

Every identifier refers to a local (`Local name#id`) or global entity, every expression has a type,
and initializers are byte-offset entries. `--emit-hir` of the function from the front-end page
(trimmed) shows how much is made explicit:

```
Function sum : 'int (int, int, int)' [external]
|-Param a#0 : 'int'   |-Param b#1 : 'int'   |-Param n#2 : 'int'
`-Block
  |-Decl s#3 : 'int'
  | `-Init size=4
  |   `-@0 'int'
  |     `-IntLiteral 0 : 'int'
  |-For
  | |-Cond
  | | `-Binary '<' : 'int'
  | |   |-Cast LValueToRValue : 'int'  <- Local i#4
  | |   `-Cast LValueToRValue : 'int'  <- Local n#2
  | `-ExprStmt
  |   `-CompoundAssign '+=' in 'int' : 'int'
  |     |-Local s#3 : 'int'
  |     `-Binary '+' : 'int'  (a * b) + i, each operand an LValueToRValue cast
  `-Return
    `-Cast LValueToRValue : 'int' <- Local s#3
```

## Tests

`sema/tests.rs` (98 tests): layout against known SysV sizes, every conversion rule, every
diagnostic and warning (positive and negative cases, so a warning that fires too often fails too),
initializer flattening including the bit-field regression, VLA typing, constant evaluation, and the
uninitialized-variable analysis. `tests/diag/*.c` golden files check the rendered messages, and the
end-to-end suite checks that what sema accepts actually runs correctly.
