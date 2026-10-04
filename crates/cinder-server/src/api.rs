//! The HTTP API: `GET /api/health`, `POST /api/compile`, `POST /api/run`.

use crate::config::{Config, SandboxMode};
use crate::output;
use crate::ratelimit::{Kind, RateLimiter};
use crate::sandbox::{self, Isolation, Limits, Outcome, TempDir};
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// ───────────────────────────── shared state ─────────────────────────────

/// How programs are run, decided by the start-up self-test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunMode {
    /// chroot + separate uid + seccomp + rlimits.
    Full,
    /// separate uid + seccomp + rlimits (a root server that cannot chroot).
    UidOnly,
    /// seccomp + rlimits only (the server is not root).
    Degraded,
    /// no isolation (`SANDBOX=off`, local development).
    Unsafe,
    /// running programs is switched off, with the reason.
    Disabled(String),
}

impl RunMode {
    pub fn label(&self) -> &'static str {
        match self {
            RunMode::Full => "chroot+uid+seccomp+rlimits",
            RunMode::UidOnly => "uid+seccomp+rlimits",
            RunMode::Degraded => "seccomp+rlimits",
            RunMode::Unsafe => "none",
            RunMode::Disabled(_) => "disabled",
        }
    }
}

pub struct AppState {
    pub cfg: Config,
    pub limiter: RateLimiter,
    sem: Arc<Semaphore>,
    free_uids: Mutex<Vec<u32>>,
    pub run_mode: Mutex<RunMode>,
}

/// One of the `MAX_CONCURRENT` execution slots; its uid returns to the pool on drop.
pub struct Slot {
    pub uid: u32,
    state: Arc<AppState>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.state.free_uids.lock().unwrap().push(self.uid);
    }
}

impl AppState {
    pub fn new(cfg: Config) -> Arc<AppState> {
        let free: Vec<u32> = (0..cfg.max_concurrent as u32).rev().map(|i| cfg.uid_base + i).collect();
        Arc::new(AppState {
            limiter: RateLimiter::new(cfg.rate_run_per_min, cfg.rate_compile_per_min),
            sem: Arc::new(Semaphore::new(cfg.max_concurrent)),
            free_uids: Mutex::new(free),
            run_mode: Mutex::new(RunMode::Disabled("not probed yet".into())),
            cfg,
        })
    }

    async fn slot(self: &Arc<Self>) -> Result<Slot, ApiError> {
        let wait = Duration::from_secs(self.cfg.queue_wait_secs);
        let permit = match tokio::time::timeout(wait, self.sem.clone().acquire_owned()).await {
            Ok(Ok(p)) => p,
            _ => {
                return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "the server is busy, try again in a moment"))
            }
        };
        let uid = self.free_uids.lock().unwrap().pop().unwrap_or(self.cfg.uid_base);
        Ok(Slot { uid, state: self.clone(), _permit: permit })
    }

    pub fn mode(&self) -> RunMode {
        self.run_mode.lock().unwrap().clone()
    }
}

// ───────────────────────────── errors ─────────────────────────────

pub struct ApiError {
    status: StatusCode,
    message: String,
    retry_after: Option<u64>,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> ApiError {
        ApiError { status, message: message.into(), retry_after: None }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut resp = (self.status, Json(serde_json::json!({ "error": self.message }))).into_response();
        if let Some(s) = self.retry_after {
            if let Ok(v) = s.to_string().parse() {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

fn client_ip(headers: &HeaderMap, peer: SocketAddr, trusted_hops: usize) -> IpAddr {
    if trusted_hops > 0 {
        if let Some(v) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            // the proxy appends the address it saw: count from the right, never trust the left
            let parts: Vec<&str> = v.split(',').map(str::trim).collect();
            if let Some(p) = parts.len().checked_sub(trusted_hops).and_then(|i| parts.get(i)) {
                if let Ok(ip) = p.parse() {
                    return ip;
                }
            }
        }
    }
    peer.ip()
}

fn rate_limit(st: &AppState, ip: IpAddr, kind: Kind) -> Result<(), ApiError> {
    st.limiter.check(ip, kind).map_err(|retry| ApiError {
        status: StatusCode::TOO_MANY_REQUESTS,
        message: format!("too many requests, retry in {} s", retry),
        retry_after: Some(retry),
    })
}

fn body<T>(r: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    match r {
        Ok(Json(v)) => Ok(v),
        Err(e) => Err(ApiError::new(e.status(), format!("invalid request: {}", e.body_text()))),
    }
}

// ───────────────────────────── health ─────────────────────────────

pub async fn health(State(st): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mode = st.mode();
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "sandbox": mode.label(),
        "runEnabled": !matches!(mode, RunMode::Disabled(_)),
    }))
}

// ───────────────────────────── compiling ─────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileReq {
    code: String,
    #[serde(default)]
    opt_level: u8,
    #[serde(default = "default_emit")]
    emit: String,
}

fn default_emit() -> String {
    "asm".into()
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostics {
    diagnostics: serde_json::Value,
    errors: u64,
    warnings: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileResp {
    ok: bool,
    emit: String,
    output: String,
    line_map: Vec<u32>,
    #[serde(flatten)]
    diags: Diagnostics,
    /// stderr text that was not a diagnostic report (driver errors, crashes)
    stderr: String,
    time_ms: u64,
    timed_out: bool,
}

const TOOL_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

fn compile_limits(cfg: &Config) -> Limits {
    Limits {
        cpu_secs: cfg.compile_secs,
        mem_bytes: 3 << 30,
        processes: if sandbox::is_root() { Some(64) } else { None },
        files: 256,
        fsize: 64 << 20,
        wall: Duration::from_secs(cfg.compile_secs),
        output_cap: 4 << 20,
    }
}

fn tool_isolation(st: &AppState, slot: &Slot) -> Isolation {
    match st.cfg.sandbox {
        SandboxMode::Off => Isolation::default(),
        _ => Isolation { chroot: None, uid: if sandbox::is_root() { Some(slot.uid) } else { None }, seccomp: true },
    }
}

/// Run the compiler on `main.c` in `dir` with extra `args`.
async fn run_cinder(st: &AppState, slot: &Slot, dir: &TempDir, args: &[&str]) -> std::io::Result<Outcome> {
    let mut a: Vec<String> =
        ["-Wall", "--color=never", "--diagnostics-format=json", "--restrict-includes", "-ferror-limit=50"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    a.extend(args.iter().map(|s| s.to_string()));
    a.push("main.c".into());
    let tmp = dir.path().to_string_lossy().to_string();
    let env = [("PATH", TOOL_PATH), ("HOME", tmp.as_str()), ("TMPDIR", tmp.as_str()), ("LC_ALL", "C")];
    sandbox::run(
        &st.cfg.cinder_bin,
        &a,
        Some(dir.path()),
        &env,
        &[],
        &compile_limits(&st.cfg),
        &tool_isolation(st, slot),
    )
    .await
}

/// Split the compiler's stderr into the JSON diagnostics report and any other text.
fn parse_stderr(stderr: &str) -> (Diagnostics, String) {
    let mut diags = Diagnostics { diagnostics: serde_json::Value::Array(vec![]), errors: 0, warnings: 0 };
    let mut rest = String::new();
    for line in stderr.lines() {
        if line.starts_with("{\"diagnostics\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                diags.diagnostics = v["diagnostics"].clone();
                diags.errors = v["errors"].as_u64().unwrap_or(0);
                diags.warnings = v["warnings"].as_u64().unwrap_or(0);
                continue;
            }
        }
        rest.push_str(line);
        rest.push('\n');
    }
    (diags, rest)
}

fn new_dir(st: &AppState, slot: &Slot, code: &str) -> Result<TempDir, ApiError> {
    let internal =
        |e: std::io::Error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("scratch directory: {}", e));
    let dir = TempDir::new(&st.cfg.work_dir, "cinder-job").map_err(internal)?;
    if st.cfg.sandbox != SandboxMode::Off {
        dir.chown(slot.uid).map_err(internal)?;
    }
    std::fs::write(dir.path().join("main.c"), code).map_err(internal)?;
    Ok(dir)
}

fn check_code(cfg: &Config, code: &str) -> Result<(), ApiError> {
    if code.len() > cfg.max_code_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("source is larger than {} bytes", cfg.max_code_bytes),
        ));
    }
    if code.contains('\0') {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "source contains a NUL byte"));
    }
    Ok(())
}

pub async fn compile(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    req: Result<Json<CompileReq>, JsonRejection>,
) -> Result<Json<CompileResp>, ApiError> {
    let req = body(req)?;
    check_code(&st.cfg, &req.code)?;
    rate_limit(&st, client_ip(&headers, peer, st.cfg.trust_proxy_hops), Kind::Compile)?;
    let emit_flags: &[&str] = match req.emit.as_str() {
        "asm" => &["-S", "-o", "-"],
        "ir" => &["--emit-ir-lines"],
        "ast" => &["--emit-ast"],
        "hir" => &["--emit-hir"],
        "pp" => &["-E"],
        other => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("unknown emit kind '{}' (asm, ir, ast, hir, pp)", other),
            ))
        }
    };
    let opt = format!("-O{}", req.opt_level.min(2));
    let slot = st.slot().await?;
    let dir = new_dir(&st, &slot, &req.code)?;
    let mut args = vec![opt.as_str()];
    args.extend_from_slice(emit_flags);
    let out = run_cinder(&st, &slot, &dir, &args)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot start the compiler: {}", e)))?;
    drop(dir);
    drop(slot);

    let stderr_text = String::from_utf8_lossy(&out.stderr).into_owned();
    let (diags, mut rest) = parse_stderr(&stderr_text);
    if let Some(sig) = out.signal {
        rest.push_str(&format!(
            "the compiler was terminated by {} (it may have hit a resource limit)\n",
            sandbox::signal_name(sig)
        ));
    }
    if out.timed_out {
        rest.push_str("the compiler took too long and was stopped\n");
    }
    let raw = String::from_utf8_lossy(&out.stdout).into_owned();
    let (text, line_map) = match req.emit.as_str() {
        "asm" => output::clean_asm(&raw),
        "ir" => output::clean_ir(&raw),
        _ => output::plain(&raw),
    };
    Ok(Json(CompileResp {
        ok: out.exit_code == Some(0),
        emit: req.emit,
        output: text,
        line_map,
        diags,
        stderr: rest,
        time_ms: out.time_ms,
        timed_out: out.timed_out,
    }))
}

// ───────────────────────────── running ─────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReq {
    code: String,
    #[serde(default)]
    opt_level: u8,
    #[serde(default)]
    stdin: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResp {
    ok: bool,
    /// false when compilation failed (nothing was run)
    compiled: bool,
    #[serde(flatten)]
    diags: Diagnostics,
    compile_stderr: String,
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    signal: Option<String>,
    timed_out: bool,
    truncated: bool,
    time_ms: u64,
    compile_ms: u64,
    sandbox: &'static str,
}

fn run_limits(cfg: &Config, mode: &RunMode) -> Limits {
    Limits {
        cpu_secs: cfg.run_cpu_secs,
        mem_bytes: cfg.run_mem_mb << 20,
        // the limit is per uid: only meaningful when programs run as their own uid
        processes: if matches!(mode, RunMode::Full | RunMode::UidOnly) { Some(cfg.run_processes) } else { None },
        files: 32,
        fsize: 1 << 20,
        wall: Duration::from_secs(cfg.run_wall_secs),
        output_cap: cfg.output_limit,
    }
}

pub async fn run(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    req: Result<Json<RunReq>, JsonRejection>,
) -> Result<Json<RunResp>, ApiError> {
    let req = body(req)?;
    check_code(&st.cfg, &req.code)?;
    if req.stdin.len() > st.cfg.max_stdin_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("stdin is larger than {} bytes", st.cfg.max_stdin_bytes),
        ));
    }
    let mode = st.mode();
    if let RunMode::Disabled(why) = &mode {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("running programs is disabled on this server: {}", why),
        ));
    }
    rate_limit(&st, client_ip(&headers, peer, st.cfg.trust_proxy_hops), Kind::Run)?;
    let slot = st.slot().await?;
    let dir = new_dir(&st, &slot, &req.code)?;

    // compile and link statically: the program then runs alone in an empty chroot
    let opt = format!("-O{}", req.opt_level.min(2));
    let out = run_cinder(&st, &slot, &dir, &[opt.as_str(), "-static", "-o", "prog", "-lm"])
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot start the compiler: {}", e)))?;
    let stderr_text = String::from_utf8_lossy(&out.stderr).into_owned();
    let (diags, mut compile_rest) = parse_stderr(&stderr_text);
    if let Some(sig) = out.signal {
        compile_rest.push_str(&format!("the compiler was terminated by {}\n", sandbox::signal_name(sig)));
    }
    if out.timed_out {
        compile_rest.push_str("the compiler took too long and was stopped\n");
    }
    let prog = dir.path().join("prog");
    if out.exit_code != Some(0) || !prog.is_file() {
        return Ok(Json(RunResp {
            ok: false,
            compiled: false,
            diags,
            compile_stderr: compile_rest,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            signal: None,
            timed_out: false,
            truncated: false,
            time_ms: 0,
            compile_ms: out.time_ms,
            sandbox: mode.label(),
        }));
    }

    let limits = run_limits(&st.cfg, &mode);
    let ran = run_in_mode(&st, &mode, &slot, dir.path(), &prog, req.stdin.as_bytes(), &limits)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot start the program: {}", e)))?;
    drop(dir);
    drop(slot);

    Ok(Json(RunResp {
        ok: ran.exit_code == Some(0) && !ran.timed_out,
        compiled: true,
        diags,
        compile_stderr: compile_rest,
        stdout: String::from_utf8_lossy(&ran.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&ran.stderr).into_owned(),
        exit_code: ran.exit_code,
        signal: ran.signal.map(sandbox::signal_name),
        timed_out: ran.timed_out,
        truncated: ran.truncated,
        time_ms: ran.time_ms,
        compile_ms: out.time_ms,
        sandbox: mode.label(),
    }))
}

/// Run the compiled `prog` (in the job directory `dir`) as `mode` prescribes.
async fn run_in_mode(
    st: &AppState,
    mode: &RunMode,
    slot: &Slot,
    dir: &Path,
    prog: &Path,
    stdin: &[u8],
    limits: &Limits,
) -> std::io::Result<Outcome> {
    let env = [("LC_ALL", "C")];
    match mode {
        RunMode::Full => run_chrooted(st, slot, prog, stdin, limits).await,
        RunMode::UidOnly => {
            let iso = Isolation { chroot: None, uid: Some(slot.uid), seccomp: true };
            sandbox::run(prog, &[], Some(dir), &env, stdin, limits, &iso).await
        }
        RunMode::Degraded | RunMode::Unsafe => {
            let iso = Isolation { chroot: None, uid: None, seccomp: *mode == RunMode::Degraded };
            sandbox::run(prog, &[], Some(dir), &env, stdin, limits, &iso).await
        }
        RunMode::Disabled(why) => Err(std::io::Error::other(why.clone())),
    }
}

/// Link the program into an empty root-owned directory and run it there as the slot's uid.
/// (A hard link, not a copy: writing the file in this multi-threaded process would race with
/// forks of other requests and make `exec` fail with ETXTBSY.)
async fn run_chrooted(
    st: &AppState,
    slot: &Slot,
    prog: &Path,
    stdin: &[u8],
    limits: &Limits,
) -> std::io::Result<Outcome> {
    let root = TempDir::new(&st.cfg.work_dir, "cinder-root")?;
    let target = root.path().join("prog");
    if std::fs::hard_link(prog, &target).is_err() {
        std::fs::copy(prog, &target)?;
    }
    let iso = Isolation { chroot: Some(root.path().to_path_buf()), uid: Some(slot.uid), seccomp: true };
    sandbox::run(Path::new("/prog"), &[], None, &[("LC_ALL", "C")], stdin, limits, &iso).await
}

// ───────────────────────────── start-up self-test ─────────────────────────────

/// Compile and run a tiny program through the real code path to learn how much
/// isolation this environment supports, and switch to the strongest working mode.
pub async fn probe(st: &Arc<AppState>) -> RunMode {
    let wanted: Vec<RunMode> = match (st.cfg.sandbox, sandbox::is_root()) {
        (SandboxMode::Off, _) => vec![RunMode::Unsafe],
        (SandboxMode::Require, true) => vec![RunMode::Full],
        (SandboxMode::Require, false) => {
            let mode = RunMode::Disabled(
                "SANDBOX=require but the server is not running as root (needed for chroot and uid separation)".into(),
            );
            *st.run_mode.lock().unwrap() = mode.clone();
            return mode;
        }
        // a root server never falls back to running programs as root
        (SandboxMode::Auto, true) => vec![RunMode::Full, RunMode::UidOnly],
        (SandboxMode::Auto, false) => vec![RunMode::Degraded],
    };
    let mut last_error = String::new();
    for mode in wanted {
        *st.run_mode.lock().unwrap() = mode.clone();
        match self_test(st).await {
            Ok(()) => return mode,
            Err(e) => last_error = format!("{}: {}", mode.label(), e),
        }
    }
    let mode = RunMode::Disabled(format!("the sandbox self-test failed ({})", last_error));
    *st.run_mode.lock().unwrap() = mode.clone();
    mode
}

async fn self_test(st: &Arc<AppState>) -> Result<(), String> {
    let slot = st.slot().await.map_err(|_| "no free slot".to_string())?;
    let code = "#include <stdio.h>\nint main(void) { char c[16]; if (!fgets(c, sizeof c, stdin)) return 3; printf(\"ok:%s\", c); return 0; }\n";
    let dir = new_dir(st, &slot, code).map_err(|e| e.message)?;
    let out = run_cinder(st, &slot, &dir, &["-O1", "-static", "-o", "prog", "-lm"]).await.map_err(|e| e.to_string())?;
    let prog = dir.path().join("prog");
    if out.exit_code != Some(0) || !prog.is_file() {
        return Err(format!(
            "could not compile the test program: {}",
            String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("")
        ));
    }
    let mode = st.mode();
    let limits = run_limits(&st.cfg, &mode);
    let ran = run_in_mode(st, &mode, &slot, dir.path(), &prog, b"hi\n", &limits).await.map_err(|e| e.to_string())?;
    if ran.stdout != b"ok:hi\n" || ran.exit_code != Some(0) {
        return Err(format!(
            "unexpected result (exit {:?}, signal {:?}, stdout {:?})",
            ran.exit_code,
            ran.signal,
            String::from_utf8_lossy(&ran.stdout)
        ));
    }
    Ok(())
}
