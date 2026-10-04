# Running untrusted code: the sandbox

The playground compiles and **runs C programs written by strangers** on a shared server. That is the
most dangerous thing in this repository, so it is designed as layers: each one assumes the one before
it has failed. The implementation is `crates/cinder-server/src/sandbox.rs` and `api.rs`; the behaviour
is pinned by 26 integration tests that run hostile programs (`crates/cinder-server/tests/api.rs`).

## What is promised

| a program cannot… | stopped by |
|-------------------|------------|
| run for more than ~2 s of CPU | `RLIMIT_CPU` (the program gets `SIGXCPU`), plus a wall-clock kill after `RUN_WALL_SECS` |
| sleep or block forever | the wall-clock limit kills the whole process group |
| use more than 128 MB | `RLIMIT_AS` per process **and** a watchdog on the *total* resident memory of all its processes |
| fork-bomb the host | `RLIMIT_NPROC` (16, counted per sandbox uid), the process-group kill, and a final kill of every process of that uid |
| leave a process running after it finished | kill of the process group, then of every process owned by the sandbox uid (catches `setsid()` escapes), and `tini` as PID 1 reaping orphans |
| print without bound | stdout and stderr are each capped (64 KiB); the program is stopped when the cap is hit |
| fill the disk | `RLIMIT_FSIZE` (1 MiB per file) and an empty, root-owned filesystem it cannot write to |
| read any file on the server | `chroot` into an empty directory containing only the program; no `/proc`, `/dev`, `/etc`, libc |
| use the network | seccomp: every socket-related syscall fails with `EPERM` |
| see or signal other processes | per-slot uid, `ptrace`/`process_vm_*` denied, no `/proc` in the chroot |
| become root or change identity | not root to begin with; `setuid`-family, `capset`, `chroot`, `mount`, `unshare`, `setns`, `pivot_root` denied; `no_new_privs` set |
| load kernel modules, reboot, change the clock, … | seccomp denylist (`init_module`, `reboot`, `settimeofday`, `kexec_*`, `bpf`, `perf_event_open`, …) |
| overload the service | one compile/run slot per request (`MAX_CONCURRENT`), a wait queue that answers `503` after `QUEUE_WAIT_SECS`, per-IP token-bucket rate limits (`429` + `Retry-After`), request size caps |

## The layers, in the order a run goes through them

1. **HTTP edge** — body and field size limits (`MAX_CODE_BYTES`, `MAX_STDIN_BYTES`), NUL bytes rejected,
   per-client token bucket (`RATE_RUN_PER_MIN` = 20, burst 5; `RATE_COMPILE_PER_MIN` = 60, burst 10).
   The client address comes from `CLIENT_IP_HEADER` (a header the edge proxy *overwrites*, e.g.
   `CF-Connecting-IP`), else `X-Forwarded-For` counted `TRUST_PROXY_HOPS` entries from the **right**
   (the left of that header is client-controlled), else the socket peer.
2. **Slot** — a semaphore of `MAX_CONCURRENT` slots. Each slot owns a numeric uid (`RUN_UID_BASE + n`,
   no passwd entry) for its job's whole lifetime, so `RLIMIT_NPROC` and process kills apply to exactly
   that job.
3. **Compile step** — `cinder -static -o prog -lm` runs as the slot uid with its own rlimits, the seccomp
   filter and **`--restrict-includes`** (no absolute or `..` `#include`, no host include directories), and
   its own memory watchdog (`COMPILE_MEMORY_MB`). The compiler itself also refuses runaway inputs: macro
   expansion has a token budget and huge sparse static arrays are not materialized in memory.
   It has no inline assembly, no `__asm__` labels and no `#pragma comment`, so source text cannot reach
   the assembler or linker as directives. The program is linked **statically** so it can run alone in an
   empty directory.
4. **Chroot** — a fresh root-owned directory holds one hard link to the binary and nothing else; the child
   `chroot`s into it and `chdir`s to `/`.
5. **Identity** — in the child, after `chroot` and before `exec`: `setgroups(0)`, `setgid`, `setuid` to the slot
   uid. All capabilities are gone with the uid change; no new ones can be gained (`no_new_privs`).
6. **rlimits** — CPU, address space, processes, open files (32), file size, core dumps (0).
7. **seccomp-BPF** — a classic-BPF filter (`build_filter`): foreign architectures and the x32 ABI kill the
   process; 68 denied syscalls return `EPERM` (so programs see a clean error instead of dying); everything
   else is allowed. It is installed last so the setup calls above are still possible.
8. **Supervision** — the server reads stdout/stderr through capped pipes, waits for exit, the wall clock or the
   memory watchdog (40 ms polling of `/proc`), then `SIGKILL`s the process group and every process of the uid.
   Output reports the exit code or the signal (`SIGSEGV`, `SIGXCPU`, `SIGKILL`…), `timedOut`, `truncated`,
   `memoryExceeded`.
9. **Cleanup** — the job directory and the chroot directory are removed when the request ends (also on
   errors); a test checks that nothing is left in `WORK_DIR`.

## Modes and the start-up self-test

The server never assumes the environment supports all of this. At start-up it compiles and runs a tiny
program through the real code path (`api::probe`) and settles on the strongest mode that works:

| mode (`sandbox` in `/api/health`) | when | isolation |
|---|---|---|
| `chroot+uid+seccomp+rlimits` | root with `CAP_SYS_CHROOT` / `CAP_SETUID` (the Docker image, Render) | everything above |
| `uid+seccomp+rlimits` | root, but `chroot` fails | no filesystem isolation: the program sees the container's files with an unprivileged uid's ordinary permissions |
| `seccomp+rlimits` | not root (local development, CI runners) | the program shares the server's uid: no per-uid process limit |
| `none` | `SANDBOX=off` | nothing — only for local experiments, **never** on a public server |
| `disabled` | self-test failed, or `SANDBOX=require` could not be met | `/api/run` answers `503` with the reason; the compiler views keep working |

A **root server never falls back to running programs as root**: the weakest root mode is `uid+...` and if
that fails too, running is disabled. The Docker image sets `SANDBOX=require`, which only accepts the full
mode, so a deployment cannot silently run with less than the documented isolation. After building the image,
check that `/api/health` reports `chroot+uid+seccomp+rlimits`, that a normal program runs and that an infinite
loop is killed (the commands are in [TESTING.md](TESTING.md)).

## What the tests cover

`crates/cinder-server/tests/api.rs` (each test starts a real server on its own uid range):
infinite loop (`SIGXCPU`), sleeping program (wall clock), memory bomb (allocation fails), **fork of many
memory-hungry children (aggregate watchdog)**, output flood (truncated, stopped), fork bomb (≤ `RUN_PROCESSES`
succeed, server unaffected), **a child that calls `setsid()` does not outlive the job**, network and
dangerous syscalls (`EPERM`), no files visible and none writable, runs as an unprivileged uid, deep recursion
(clean `SIGSEGV`), `#include` of `/etc/passwd` by absolute and `../../..` path refused, oversized and malformed requests,
rate limiting (429), queueing beyond the slot count, scratch cleanup, security headers.

## Honest limitations

* The seccomp filter is a **denylist**. Anything not listed is allowed, so the surface is the whole rest of the
  Linux syscall interface: a kernel vulnerability reachable through an allowed syscall is out of scope for
  this design. An allowlist would be stronger but breaks real libc programs unpredictably; the layers above
  (uid, empty chroot, rlimits) limit what such a bug could reach.
* The outer boundary is the container. On a managed platform the container runtime (Render's) is the last line of
  defence; Cinder adds defence in depth inside it, not a replacement.
* There is no cgroup: CPU and memory are limited per job by rlimits and the watchdog, which react within tens of
  milliseconds, not instantly. A program can use a burst of memory above the cap between two polls (one
  `memset` of the 128 MB the address-space limit allows is the most a single process can do).
* No network namespace is used (Render does not allow user namespaces); network access is blocked by denying the
  syscalls, so a program cannot even create a socket (including `AF_UNIX`).
* The compile step runs *outside* the chroot (it needs the assembler, linker and libc), as an unprivileged uid with
  seccomp and rlimits. It can read world-readable files of the container but has no way to send their contents
  anywhere: source text cannot include files (`--restrict-includes`) or inject assembler directives.
