# The web playground

```
browser (Monaco, web/)  ──HTTP/JSON──►  cinder-server (axum)  ──spawns──►  cinder  ─► as ─► cc/ld
                                              │                                           │
                                              └── sandboxed run of the program ◄──────────┘   (see SANDBOX.md)
```

One Docker image serves everything: the Rust API (`crates/cinder-server`), the static front end (`web/`)
and the compiler binary it shells out to. Run it locally with

```bash
docker build -t cinder-playground .
docker run --rm -p 8080:8080 cinder-playground      # http://localhost:8080
```

or without Docker (unprivileged, so the weaker `seccomp+rlimits` sandbox mode; Linux only):

```bash
cargo build --release
WEB_DIR=web ./target/release/cinder-server          # http://localhost:8080
```

## HTTP API

All bodies are JSON; errors are `{"error": "<message>"}` with a matching status
(`400` malformed, `413` too large, `429` rate limited with `Retry-After`, `503` busy or running disabled).

### `GET /api/health`

```json
{"status":"ok","version":"0.1.0","sandbox":"chroot+uid+seccomp+rlimits","runEnabled":true}
```

Used by Render's health check and the Docker `HEALTHCHECK`. `sandbox` is the isolation mode the start-up
self-test settled on ([SANDBOX.md](SANDBOX.md)).

### `POST /api/compile`

```json
{"code": "int main(void) { return 0; }", "optLevel": 2, "emit": "asm"}
```

`emit` is one of `asm` (default), `ir`, `ast`, `hir`, `pp` (the preprocessor's output). `optLevel` is 0–2.
The program is compiled with `-Wall`; **diagnostics are returned even when compilation succeeds**.

```json
{
  "ok": true,
  "emit": "asm",
  "output": "\t.text\n\t.globl main\n...",
  "lineMap": [0, 0, 1, 1, 2, ...],
  "diagnostics": [ {"level":"warning","message":"unused variable 'x'","flag":"-Wunused-variable",
                     "file":"main.c","line":2,"col":6,"endLine":2,"endCol":7} ],
  "errors": 0, "warnings": 1,
  "stderr": "",
  "timeMs": 12, "timedOut": false
}
```

`lineMap[i]` is the 1-based **source line that output line `i` came from** (0 = none). For `asm` it is derived
from the `.loc` directives (which are then stripped from the text); for `ir` from the `; L<n>` comments of
`--emit-ir-lines`. The front end uses it to highlight the matching lines in both panes.

### `POST /api/run`

```json
{"code": "...", "optLevel": 2, "stdin": "input for the program"}
```

Compiles (statically linked) and runs the program under the sandbox limits:

```json
{
  "ok": true, "compiled": true,
  "diagnostics": [], "errors": 0, "warnings": 0, "compileStderr": "",
  "stdout": "hello\n", "stderr": "", "exitCode": 0, "signal": null,
  "timedOut": false, "memoryExceeded": false, "truncated": false,
  "timeMs": 3, "compileMs": 41, "sandbox": "chroot+uid+seccomp+rlimits"
}
```

If compilation fails, `compiled` is `false`, nothing runs, and `diagnostics` explains why. A crash reports
`signal` (`"SIGSEGV"`), a timeout `timedOut`, a flood of output `truncated`, a memory hog `memoryExceeded`.

## Front end (`web/`)

Plain HTML, CSS and ES modules, **no build step**; Monaco is loaded from jsDelivr (with a textarea editor as
fallback if the CDN is blocked).

* **Split panes** with draggable splitters: the C source on the left, the compiler's output on the right, and a
  console below. The output pane switches between **Assembly**, **IR**, **AST**, **Typed** (HIR) and
  **Preprocessed**; custom syntax highlighting for AT&T assembly and the Cinder IR.
* **Optimization selector** `-O0 / -O1 / -O2`: the output recompiles as you type (debounced) and whenever you
  change it, so you can watch `mem2reg` turn loads and stores into phi nodes or LICM hoist a multiply.
* **Source ↔ output linking**: put the cursor on a C line and the instructions it produced are highlighted in the
  output (and the reverse: a cursor in the output highlights its source line), driven by `lineMap`.
* **Diagnostics** become Monaco markers (squiggles) and a clickable list with notes; warnings show even when the
  build succeeds.
* **Run** (Ctrl/Cmd+Enter) executes the program with the **Input** tab as stdin and shows stdout, stderr (in red),
  the exit code or terminating signal, and the time; limit violations are explained in plain words.
* **18 example programs** in four groups (basics, language features, a tour of the optimizer, algorithms),
  including ones designed to show a specific pass at `-O2`.
* **Share**: the link encodes source, optimization level, output view and stdin in the URL fragment
  (`#s=` = deflate-raw + base64url via `CompressionStream`, with a plain `#j=` fallback). Nothing is stored on the
  server, so links never expire and cost nothing.
* **Dark / light theme** (follows the system, switchable, remembered), **mobile layout** below 820 px: panes become
  tabs (Source / Output / Console) with a bottom tab bar, controls scroll rather than overflow.
* Keyboard: Ctrl/Cmd+Enter runs; the splitters are focusable and move with the arrow keys.

## Layout of the server crate

| file | role |
|------|------|
| `main.rs` | start-up: check the compiler, run the sandbox self-test, listen, graceful shutdown, `--healthcheck` |
| `lib.rs` | router, security headers (CSP, `nosniff`, no framing), request log, static files (an unknown path answers `404` with the page as its body) |
| `config.rs` | every environment variable and its default (documented in the module header) |
| `api.rs` | handlers, slot pool, sandbox modes, self-test, client address, JSON shapes |
| `sandbox.rs` | child setup (chroot, uid, rlimits, seccomp), supervision, output caps, memory watchdog, process killing |
| `ratelimit.rs` | per-client token buckets |
| `output.rs` | turns compiler output into display text plus `lineMap` |

Environment variables are listed in `crates/cinder-server/src/config.rs` and in [DEPLOY.md](DEPLOY.md).
