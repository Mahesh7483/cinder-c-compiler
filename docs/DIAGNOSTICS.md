# Diagnostics and warnings

Cinder reports problems the way Clang does: `file:line:col: severity: message [-Wflag]`,
the source line, a caret and `~` underlines for related ranges, `note:`s pointing at
the earlier declaration or macro definition, "did you mean" suggestions with the
replacement text shown under the caret, and a summary line. Parsing and semantic
analysis recover after an error, so one run reports as many independent problems as
possible (`-ferror-limit=N` caps it). Output is sorted by source position.

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

| flag | meaning |
|------|---------|
| `--color=auto\|always\|never` | colour (auto: only on a terminal; honours `NO_COLOR`, `CLICOLOR_FORCE`) |
| `--diagnostics-format=json` | machine-readable output (used by the web playground) |
| `-fsyntax-only` | run the front end and lowering, report diagnostics, produce nothing |
| `-w` | suppress all warnings |
| `-Wall`, `-Wextra`, `-Weverything` | warning groups (below) |
| `-W<name>`, `-Wno-<name>` | enable / disable one warning |
| `-Werror`, `-Werror=<name>` | make all / one warning an error |

## Warnings

*Default* warnings are on without any flag, `-Wall` adds the *All* group, `-Wextra` the
*Extra* group; *Off* warnings need their own flag (or `-Weverything`).

| name | group | what it catches |
|------|-------|-----------------|
| `implicit-function-declaration`, `int-conversion`, `incompatible-pointer-types`, `return-type`, `division-by-zero`, `overflow`, `constant-conversion`, `shift-count-overflow`, `return-stack-address`, `excess-initializers`, ... | Default | things that are almost certainly bugs |
| `unused-variable`, `unused-function`, `unused-label`, `unused-value` | All | declared or computed but never used |
| `uninitialized`, `maybe-uninitialized` | All | read of a variable that no path / not every path assigned (below) |
| `conversion` | All | implicit conversions that lose information: `long` → `int`, `double` → `int`/`float`, `int` → `unsigned short`, ... (constants that change value are `constant-conversion`, on by default) |
| `parentheses` | All | `if (a = b)` |
| `unknown-pragmas` | All | `#pragma` Cinder does not know |
| `unused-parameter` | Extra | a named parameter that is never used |
| `sign-compare` | Extra | `int` compared with `unsigned` where the signed operand may be negative |
| `empty-body` | Extra | `if (c);` |
| `sign-conversion` | Off | implicit `int` ↔ `unsigned` conversion |
| `shadow` | Off | a local or parameter hiding a variable of an enclosing scope |

`missing return` is `return-type`; the warning is only given when the end of the
function is actually reachable, so `for (;;) {}` or a trailing `abort()` do not trigger it.

## How uninitialized-variable detection works

`sema/uninit.rs` runs a flow-sensitive *definite assignment* analysis over the typed
tree (Java's rules, adapted to C). For each scalar automatic variable declared without
an initializer it tracks the set of variables assigned on **every** path (`def`) and on
**some** path (`maybe`). A read of a variable not in `def` warns — `uninitialized` if it
is not even in `maybe`, `maybe-uninitialized` otherwise — once per variable.

It follows control flow precisely where that avoids false positives:

* conditions get separate *when true / when false* states, so `if (a && (x = f())) use(x)`
  and `while (1) { x = ...; break; }` are fine;
* loops: the body may run zero times (`for (...) x = i; return x;` warns "maybe"),
  `do ... while` always runs once;
* `switch` without `default` can skip every case; `case` labels start from the state
  at the `switch`;
* calls to `_Noreturn` functions (`exit`, `abort`, ...) end a path.

…and deliberately errs towards *not* warning where it cannot know:

* taking the address of a variable counts as initializing it (`scanf("%d", &x)`);
* after a label (a `goto` may arrive from anywhere) everything is assumed initialized;
* arrays, structs/unions, `volatile` variables and parameters are not tracked.

It produced no reports on any of the 243 programs in the end-to-end suite, which
read plenty of variables assigned in loops, branches and through pointers.

## Tests

* `crates/cinder/tests/diag.rs` compiles every `tests/diag/*.c` with `-fsyntax-only` and
  compares the complete rendered output with the neighbouring `.expected` file
  (carets, underlines, notes, fix-its, summary). `CINDER_BLESS=1 cargo test --test diag`
  rewrites them after an intentional change. A first line `// flags: ...` sets the flags.
* Unit tests in `sema/tests.rs` (warning logic), `diag.rs` (rendering), `parse/tests.rs`
  and `pp/tests.rs` (recovery, macro diagnostics).
