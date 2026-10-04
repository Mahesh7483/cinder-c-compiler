//! End-to-end tests of the HTTP API with the real compiler and the real sandbox:
//! an in-process server on an ephemeral port, driven by a tiny raw HTTP client.
//!
//! Tests that depend on the *full* sandbox (chroot + uid drop) check the mode the
//! server reports and skip those assertions when it is not running as root.
#![cfg(target_os = "linux")]

use cinder_server::api::{self, AppState, RunMode};
use cinder_server::config::Config;
use cinder_server::router;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn cinder_bin() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap().to_path_buf(); // target/<profile>
    let bin = dir.join("cinder");
    if !bin.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut cmd = std::process::Command::new(cargo);
        cmd.args(["build", "-p", "cinder"]);
        if dir.ends_with("release") {
            cmd.arg("--release");
        }
        assert!(cmd.status().unwrap().success(), "could not build the cinder binary");
    }
    bin
}

struct Server {
    port: u16,
    state: Arc<AppState>,
    _web: PathBuf,
}

static NEXT_SERVER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

async fn start(extra: &[(&str, &str)]) -> Server {
    // Every test server gets its own uid range and web directory: RLIMIT_NPROC is per uid,
    // so servers sharing uids would starve each other's processes (a fork-bomb test!).
    let n = NEXT_SERVER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let web = std::env::temp_dir().join(format!("cinder-web-test-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(web.join("index.html"), "<!doctype html><title>t</title><p>playground</p>").unwrap();
    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("CINDER_BIN".into(), cinder_bin().to_string_lossy().into());
    vars.insert("WEB_DIR".into(), web.to_string_lossy().into());
    vars.insert("RUN_UID_BASE".into(), (30000 + n * 64).to_string());
    vars.insert("RUN_CPU_SECS".into(), "1".into());
    vars.insert("RUN_WALL_SECS".into(), "3".into());
    vars.insert("RATE_RUN_PER_MIN".into(), "6000".into());
    vars.insert("RATE_COMPILE_PER_MIN".into(), "6000".into());
    for (k, v) in extra {
        vars.insert(k.to_string(), v.to_string());
    }
    let cfg = Config::from_vars(&move |k| vars.get(k).cloned());
    let state = AppState::new(cfg);
    let mode = api::probe(&state).await;
    assert!(!matches!(mode, RunMode::Disabled(_)), "sandbox self-test failed: {:?}", mode);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
    });
    Server { port, state, _web: web }
}

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

async fn http(port: u16, method: &str, path: &str, body: Option<String>) -> Reply {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let body = body.unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    // axum answers with Content-Length; decode chunked bodies too
    let body =
        if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(rest) } else { rest.to_string() };
    Reply { status, headers: head.to_string(), body }
}

fn dechunk(mut s: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = s.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 || rest.len() < n {
            break;
        }
        out.push_str(&rest[..n]);
        s = rest[n..].trim_start_matches("\r\n");
    }
    out
}

impl Server {
    async fn post(&self, path: &str, v: Value) -> Reply {
        http(self.port, "POST", path, Some(v.to_string())).await
    }
    async fn run(&self, code: &str, stdin: &str) -> Value {
        let r = self.post("/api/run", json!({"code": code, "optLevel": 1, "stdin": stdin})).await;
        assert_eq!(r.status, 200, "{}", r.body);
        r.json()
    }
    fn full(&self) -> bool {
        self.state.mode() == RunMode::Full
    }
    /// programs run under their own uid (so per-uid process limits apply)
    fn own_uid(&self) -> bool {
        matches!(self.state.mode(), RunMode::Full | RunMode::UidOnly)
    }
}

const HELLO: &str = "#include <stdio.h>\nint main(void) {\n    printf(\"hello\\n\");\n    return 0;\n}\n";

// ───────────────────────────── API basics ─────────────────────────────

#[tokio::test]
async fn health_reports_version_and_sandbox() {
    let s = start(&[]).await;
    let r = http(s.port, "GET", "/api/health", None).await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["runEnabled"], true);
    assert!(v["version"].as_str().unwrap().contains('.'));
    assert!(v["sandbox"].as_str().unwrap().contains("seccomp"), "{v}");
}

#[tokio::test]
async fn compile_returns_asm_ir_ast_hir_and_preprocessed_text() {
    let s = start(&[]).await;
    for (emit, needle) in
        [("asm", "main:"), ("ir", "define i32 @main"), ("ast", "FunctionDef"), ("hir", "main"), ("pp", "int main")]
    {
        let r = s.post("/api/compile", json!({"code": HELLO, "optLevel": 0, "emit": emit})).await;
        assert_eq!(r.status, 200, "{emit}: {}", r.body);
        let v = r.json();
        assert_eq!(v["ok"], true, "{emit}: {v}");
        let out = v["output"].as_str().unwrap();
        assert!(out.contains(needle), "{emit}: {out}");
        let lines = out.lines().count();
        assert_eq!(v["lineMap"].as_array().unwrap().len(), lines, "{emit}: one map entry per output line");
    }
}

#[tokio::test]
async fn line_maps_link_output_back_to_source_lines() {
    let s = start(&[]).await;
    for emit in ["asm", "ir"] {
        let v = s.post("/api/compile", json!({"code": HELLO, "optLevel": 0, "emit": emit})).await.json();
        let out = v["output"].as_str().unwrap();
        assert!(!out.contains(".loc") && !out.contains("; L"), "{emit}: annotations must be stripped");
        let map: Vec<u64> = v["lineMap"].as_array().unwrap().iter().map(|x| x.as_u64().unwrap()).collect();
        // the call to printf is on source line 3 and the return on line 4
        assert!(map.contains(&3), "{emit}: {map:?}");
        assert!(map.contains(&4), "{emit}: {map:?}");
        let call_line = out.lines().position(|l| l.contains("call")).unwrap();
        assert_eq!(map[call_line], 3, "{emit}: the printf call belongs to source line 3");
    }
}

#[tokio::test]
async fn optimization_level_changes_the_output() {
    let s = start(&[]).await;
    let code = "int sum(int n) { int s = 0; for (int i = 0; i < n; i++) s += i * 2; return s; }\n";
    let o0 = s.post("/api/compile", json!({"code": code, "optLevel": 0, "emit": "ir"})).await.json();
    let o2 = s.post("/api/compile", json!({"code": code, "optLevel": 2, "emit": "ir"})).await.json();
    assert!(o0["output"].as_str().unwrap().contains("alloca"));
    assert!(!o2["output"].as_str().unwrap().contains("alloca"));
    assert!(o2["output"].as_str().unwrap().contains("phi"));
}

#[tokio::test]
async fn compile_errors_come_back_with_positions() {
    let s = start(&[]).await;
    let code = "int main(void) {\n    int x = ;\n    return y;\n}\n";
    let v = s.post("/api/compile", json!({"code": code, "optLevel": 0, "emit": "asm"})).await.json();
    assert_eq!(v["ok"], false);
    assert!(v["errors"].as_u64().unwrap() >= 1);
    let d = &v["diagnostics"][0];
    assert_eq!(d["level"], "error");
    assert_eq!(d["line"], 2);
    assert!(d["col"].as_u64().unwrap() > 1);
    assert!(d["message"].as_str().unwrap().contains("expected expression"));
}

#[tokio::test]
async fn warnings_are_reported_even_when_compilation_succeeds() {
    let s = start(&[]).await;
    let code = "int f(int c) {\n    int x;\n    if (c) x = 1;\n    return x;\n}\n";
    let v = s.post("/api/compile", json!({"code": code, "optLevel": 1, "emit": "asm"})).await.json();
    assert_eq!(v["ok"], true);
    assert_eq!(v["warnings"], 1);
    assert_eq!(v["diagnostics"][0]["flag"], "-Wmaybe-uninitialized");
}

#[tokio::test]
async fn bad_requests_get_json_errors() {
    let s = start(&[("MAX_CODE_BYTES", "200")]).await;
    let r = s.post("/api/compile", json!({"code": "x".repeat(300), "emit": "asm"})).await;
    assert_eq!(r.status, 413, "{}", r.body);
    assert!(r.json()["error"].as_str().unwrap().contains("larger than"));
    let r = s.post("/api/compile", json!({"code": "int x;", "emit": "bogus"})).await;
    assert_eq!(r.status, 400);
    let r = http(s.port, "POST", "/api/compile", Some("{not json".into())).await;
    assert!((400..500).contains(&r.status), "{}", r.status);
    assert!(r.json()["error"].is_string(), "{}", r.body);
    let r = s.post("/api/compile", json!({"emit": "asm"})).await;
    assert!((400..500).contains(&r.status));
    let r = http(s.port, "GET", "/api/nothing", None).await;
    assert_eq!(r.status, 404, "unknown paths are not found");
}

#[tokio::test]
async fn includes_cannot_reach_files_outside_the_job() {
    let s = start(&[]).await;
    for inc in ["#include </etc/passwd>", "#include \"/etc/passwd\"", "#include \"../../../etc/passwd\""] {
        let v = s.post("/api/compile", json!({"code": format!("{inc}\nint x;\n"), "emit": "pp"})).await.json();
        assert_eq!(v["ok"], false, "{inc}");
        assert!(v["diagnostics"][0]["message"].as_str().unwrap().contains("file not found"), "{inc}: {v}");
        assert!(!v["output"].as_str().unwrap().contains("root:"), "{inc}: leaked a system file");
    }
}

#[tokio::test]
async fn static_files_and_security_headers() {
    let s = start(&[]).await;
    let r = http(s.port, "GET", "/", None).await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("playground"));
    let h = r.headers.to_ascii_lowercase();
    assert!(h.contains("content-security-policy"), "{h}");
    assert!(h.contains("x-content-type-options: nosniff"), "{h}");
    let api = http(s.port, "GET", "/api/health", None).await;
    assert!(api.headers.to_ascii_lowercase().contains("cache-control: no-store"));
}

#[tokio::test]
async fn rate_limiting_returns_429_with_retry_after() {
    let s = start(&[("RATE_COMPILE_PER_MIN", "5")]).await;
    let mut limited = 0;
    for _ in 0..9 {
        let r = s.post("/api/compile", json!({"code": "int x;", "emit": "pp"})).await;
        if r.status == 429 {
            limited += 1;
            assert!(r.headers.to_ascii_lowercase().contains("retry-after"), "{}", r.headers);
            assert!(r.json()["error"].as_str().unwrap().contains("too many requests"));
        }
    }
    assert!(limited >= 3, "expected the burst of 5 to be exhausted, got {limited} rejections");
}

// ───────────────────────────── running programs ─────────────────────────────

#[tokio::test]
async fn run_captures_stdout_stdin_and_exit_status() {
    let s = start(&[]).await;
    let v = s.run(HELLO, "").await;
    assert_eq!(v["compiled"], true);
    assert_eq!(v["stdout"], "hello\n");
    assert_eq!(v["exitCode"], 0);
    assert_eq!(v["ok"], true);

    let echo = "#include <stdio.h>\nint main(void) { int a, b; if (scanf(\"%d %d\", &a, &b) != 2) return 9; printf(\"%d\\n\", a * b); fprintf(stderr, \"warn\\n\"); return 7; }\n";
    let v = s.run(echo, "6 7\n").await;
    assert_eq!(v["stdout"], "42\n");
    assert_eq!(v["stderr"], "warn\n");
    assert_eq!(v["exitCode"], 7);
    assert_eq!(v["ok"], false);
    let v = s.run(echo, "x").await;
    assert_eq!(v["exitCode"], 9);
}

#[tokio::test]
async fn run_with_a_compile_error_does_not_run_anything() {
    let s = start(&[]).await;
    let v = s.run("int main(void) { return }\n", "").await;
    assert_eq!(v["compiled"], false);
    assert_eq!(v["ok"], false);
    assert_eq!(v["diagnostics"][0]["level"], "error");
    assert_eq!(v["stdout"], "");
}

#[tokio::test]
async fn run_reports_crashes_by_signal() {
    let s = start(&[]).await;
    let v = s.run("int main(void) { volatile int *p = 0; return *p; }\n", "").await;
    assert_eq!(v["signal"], "SIGSEGV", "{v}");
    let v = s.run("#include <stdlib.h>\nint main(void) { abort(); }\n", "").await;
    assert_eq!(v["signal"], "SIGABRT", "{v}");
    let v = s.run("int main(int argc, char **argv) { volatile int z = argc - 1; return 5 / z; }\n", "").await;
    assert_eq!(v["signal"], "SIGFPE", "{v}");
}

#[tokio::test]
async fn an_infinite_loop_is_stopped_by_the_cpu_limit() {
    let s = start(&[("RUN_CPU_SECS", "1"), ("RUN_WALL_SECS", "10")]).await;
    let t0 = std::time::Instant::now();
    let v = s.run("int main(void) { for (;;) {} }\n", "").await;
    assert!(t0.elapsed().as_secs() < 8, "took {:?}", t0.elapsed());
    assert_eq!(v["signal"], "SIGXCPU", "{v}");
    assert_eq!(v["ok"], false);
}

#[tokio::test]
async fn a_sleeping_program_is_stopped_by_the_wall_clock_limit() {
    let s = start(&[("RUN_CPU_SECS", "5"), ("RUN_WALL_SECS", "1")]).await;
    let t0 = std::time::Instant::now();
    let v = s.run("#include <unistd.h>\nint main(void) { sleep(30); return 0; }\n", "").await;
    assert!(t0.elapsed().as_secs() < 8, "took {:?}", t0.elapsed());
    assert_eq!(v["timedOut"], true, "{v}");
    assert_eq!(v["signal"], "SIGKILL");
}

#[tokio::test]
async fn memory_is_limited() {
    let s = start(&[("RUN_MEMORY_MB", "64")]).await;
    let code = "#include <stdio.h>\n#include <stdlib.h>\nint main(void) { void *a = malloc(16u << 20); void *b = malloc(1u << 30); printf(\"%d %d\\n\", a != 0, b != 0); return 0; }\n";
    let v = s.run(code, "").await;
    assert_eq!(v["stdout"], "1 0\n", "a small allocation works, a 1 GiB one fails: {v}");
}

#[tokio::test]
async fn memory_is_limited_across_all_processes_of_a_program() {
    // every child stays below the per-process limit, but together they hold ~10x the allowed memory
    let s = start(&[("RUN_MEMORY_MB", "64"), ("RUN_PROCESSES", "16"), ("RUN_WALL_SECS", "10")]).await;
    let code = r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int fork(void);
unsigned sleep(unsigned);
int main(void) {
    for (int i = 0; i < 12; i++) {
        if (fork() == 0) {
            char *p = malloc(50u << 20);
            if (p) memset(p, 1, 50u << 20);
            sleep(30);
            return 0;
        }
    }
    sleep(30);
    puts("survived");
    return 0;
}
"#;
    let t0 = std::time::Instant::now();
    let v = s.run(code, "").await;
    assert!(t0.elapsed().as_secs() < 8, "the watchdog acts within a fraction of a second: {v}");
    assert_eq!(v["memoryExceeded"], true, "{v}");
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["stdout"], "", "{v}");
}

#[tokio::test]
async fn output_is_capped_and_the_program_stopped() {
    let s = start(&[("OUTPUT_LIMIT_BYTES", "2048"), ("RUN_WALL_SECS", "10")]).await;
    let t0 = std::time::Instant::now();
    let v =
        s.run("#include <stdio.h>\nint main(void) { for (;;) puts(\"flooding the output with text\"); }\n", "").await;
    assert!(t0.elapsed().as_secs() < 8);
    assert_eq!(v["truncated"], true, "{}", v["stdout"].as_str().unwrap().len());
    assert!(v["stdout"].as_str().unwrap().len() <= 2048);
    assert_eq!(v["ok"], false);
}

#[tokio::test]
async fn programs_cannot_use_the_network_or_dangerous_syscalls() {
    let s = start(&[]).await;
    let code = r#"
#include <stdio.h>
#include <errno.h>
long syscall(long number, ...);
int socket(int domain, int type, int protocol);
int main(void) {
    int fd = socket(2, 1, 0);
    printf("socket %d %d\n", fd, errno);
    errno = 0;
    long r = syscall(101, 0, 0, 0, 0);   /* ptrace */
    printf("ptrace %ld %d\n", r, errno);
    errno = 0;
    r = syscall(165, 0, 0, 0, 0, 0);     /* mount */
    printf("mount %ld %d\n", r, errno);
    return 0;
}
"#;
    let v = s.run(code, "").await;
    let out = v["stdout"].as_str().unwrap();
    assert!(out.contains("socket -1 1"), "{v}");
    assert!(out.contains("ptrace -1 1"), "{v}");
    assert!(out.contains("mount -1 1"), "{v}");
}

#[tokio::test]
async fn programs_see_no_files_and_cannot_write_any() {
    let s = start(&[]).await;
    let code = r#"
#include <stdio.h>
#include <stdlib.h>
int main(void) {
    FILE *p = fopen("/etc/passwd", "r");
    FILE *w = fopen("/tmp/cinder-escape", "w");
    FILE *c = fopen("probe.txt", "w");
    printf("%d %d %d %d\n", p != NULL, w != NULL, c != NULL, getenv("PATH") != NULL);
    return 0;
}
"#;
    let v = s.run(code, "").await;
    let out = v["stdout"].as_str().unwrap();
    if s.full() {
        assert_eq!(out, "0 0 0 0\n", "chroot: nothing to read, nowhere to write, no environment: {v}");
    } else {
        assert!(out.starts_with("1 ") || out.starts_with("0 "), "{v}");
        assert!(out.trim_end().ends_with(" 0"), "environment must be empty: {v}");
    }
}

#[tokio::test]
async fn programs_run_as_an_unprivileged_user() {
    let s = start(&[]).await;
    if !s.full() {
        return;
    }
    let v = s.run("#include <stdio.h>\nunsigned getuid(void);\nunsigned getgid(void);\nint main(void) { printf(\"%u %u\\n\", getuid(), getgid()); return 0; }\n", "").await;
    let out = v["stdout"].as_str().unwrap().trim().to_string();
    let uid: u32 = out.split(' ').next().unwrap().parse().unwrap();
    assert!(uid >= 20000, "ran as uid {uid}: {v}");
    let v = s.run("int setuid(unsigned); int main(void) { return setuid(0); }\n", "").await;
    assert_ne!(v["exitCode"], 0, "setuid(0) must fail: {v}");
}

#[tokio::test]
async fn a_fork_bomb_is_contained() {
    let s = start(&[("RUN_PROCESSES", "8"), ("RUN_WALL_SECS", "4")]).await;
    let code = r#"
#include <stdio.h>
#include <unistd.h>
int main(void) {
    int ok = 0, failed = 0;
    for (int i = 0; i < 200; i++) {
        pid_t p = fork();
        if (p == 0) { for (;;) sleep(1); }
        if (p > 0) ok++; else failed++;
    }
    printf("%d %d\n", ok, failed);
    return 0;
}
"#;
    let v = s.run(code, "").await;
    let out = v["stdout"].as_str().unwrap().trim().to_string();
    let ok: u32 = out.split(' ').next().unwrap().parse().unwrap_or(999);
    if s.own_uid() {
        assert!(ok <= 8, "process limit not enforced: {v}");
    }
    // the server is still fine and nothing is left running
    let again = s.run(HELLO, "").await;
    assert_eq!(again["stdout"], "hello\n");
}

#[tokio::test]
async fn deep_recursion_and_big_stack_use_end_in_a_signal_not_a_hang() {
    let s = start(&[]).await;
    let v = s.run("int f(int n) { volatile char pad[4096]; pad[0] = (char)n; return f(n + 1) + pad[0]; }\nint main(void) { return f(0); }\n", "").await;
    assert_eq!(v["signal"], "SIGSEGV", "{v}");
}

#[tokio::test]
async fn requests_beyond_the_slot_count_queue_up_and_all_succeed() {
    let s = Arc::new(start(&[("MAX_CONCURRENT", "2")]).await);
    let mut tasks = Vec::new();
    for i in 0..5 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let code = format!("#include <stdio.h>\nint main(void) {{ printf(\"job {}\\n\"); return 0; }}\n", i);
            let v = s.run(&code, "").await;
            (i, v["stdout"].as_str().unwrap().to_string())
        }));
    }
    for t in tasks {
        let (i, out) = t.await.unwrap();
        assert_eq!(out, format!("job {}\n", i));
    }
}

#[tokio::test]
async fn scratch_directories_are_cleaned_up() {
    let work = std::env::temp_dir().join(format!("cinder-work-test-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let s = start(&[("WORK_DIR", work.to_str().unwrap())]).await;
    s.run(HELLO, "").await;
    s.post("/api/compile", json!({"code": HELLO, "emit": "asm"})).await;
    let left: Vec<_> = std::fs::read_dir(&work).unwrap().collect();
    assert!(left.is_empty(), "leftover job directories: {:?}", left);
}
