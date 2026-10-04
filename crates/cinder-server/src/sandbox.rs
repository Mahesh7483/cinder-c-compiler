//! Running untrusted code with limits.
//!
//! The playground compiles and then *executes* whatever C a stranger sends, so
//! every child process (the compiler on that source, and the compiled
//! program) is confined in layers that need no special privileges beyond what
//! a default, unprivileged Docker container has (user namespaces — and with
//! them `nsjail`/`bubblewrap` — are not available on Render):
//!
//! 1. **separate uid** — when the server runs as root, each concurrent slot runs
//!    as its own unprivileged numeric uid (`RUN_UID_BASE + slot`), so
//!    `RLIMIT_NPROC` is meaningful and runs cannot touch each other;
//! 2. **chroot into an empty, root-owned directory** that contains only the
//!    statically linked program: no libc to load, no `/etc`, no `/tmp`, nothing to
//!    read, nothing to write, nothing else to `exec`;
//! 3. **resource limits** — CPU time, address space, processes, open files, file
//!    size, core dumps;
//! 4. **`no_new_privs` + a seccomp-BPF filter** that fails network, mount, ptrace,
//!    module, privilege-changing and similar system calls (`EPERM`) and kills
//!    the process on a foreign syscall ABI;
//! 5. a **wall-clock limit** (the whole process group is killed), **output caps**
//!    on stdout/stderr, and process-group cleanup after the run.
//!
//! Layers 1–2 need root (the Docker image runs the server as root and drops to
//! the slot uid in the child). A server running as root never runs user code as
//! root: if the chroot is unavailable it still drops to the slot uid, and if that
//! fails too, running programs is disabled. Only an *unprivileged* server (local
//! development) falls back to layers 3–5, and says so; `SANDBOX=require` refuses
//! to run code in that situation.

use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub struct Limits {
    pub cpu_secs: u64,
    pub mem_bytes: u64,
    /// `None` leaves the process limit alone (an unprivileged server shares its uid with the program).
    pub processes: Option<u64>,
    /// Ceiling for the resident memory of *all* processes of the job together. `RLIMIT_AS` is
    /// per process, so on its own a fork bomb could still hold `processes x mem_bytes`.
    pub group_rss: Option<u64>,
    pub files: u64,
    pub fsize: u64,
    pub wall: Duration,
    pub output_cap: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Isolation {
    /// `chroot` into this directory before exec (needs root).
    pub chroot: Option<PathBuf>,
    /// Switch to this numeric uid/gid before exec (needs root).
    pub uid: Option<u32>,
    /// Install the seccomp filter.
    pub seccomp: bool,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    /// Killed because the job's processes together exceeded `Limits::group_rss`.
    pub memory_exceeded: bool,
    pub truncated: bool,
    pub time_ms: u64,
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

// ───────────────────────────── seccomp ─────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JEQ_K: u16 = 0x15;
const BPF_JSET_K: u16 = 0x45;
const BPF_RET_K: u16 = 0x06;
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
const PR_SET_SECCOMP: libc::c_int = 22;
const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// x86-64 system calls that fail with `EPERM` in sandboxed processes.
pub const DENIED_SYSCALLS: &[(u32, &str)] = &[
    // networking
    (41, "socket"),
    (42, "connect"),
    (43, "accept"),
    (44, "sendto"),
    (45, "recvfrom"),
    (46, "sendmsg"),
    (47, "recvmsg"),
    (48, "shutdown"),
    (49, "bind"),
    (50, "listen"),
    (51, "getsockname"),
    (52, "getpeername"),
    (53, "socketpair"),
    (54, "setsockopt"),
    (55, "getsockopt"),
    (288, "accept4"),
    (299, "recvmmsg"),
    (307, "sendmmsg"),
    // debugging / cross-process access
    (101, "ptrace"),
    (310, "process_vm_readv"),
    (311, "process_vm_writev"),
    (298, "perf_event_open"),
    (321, "bpf"),
    (323, "userfaultfd"),
    // filesystem and namespace manipulation
    (165, "mount"),
    (166, "umount2"),
    (155, "pivot_root"),
    (161, "chroot"),
    (272, "unshare"),
    (308, "setns"),
    (304, "open_by_handle_at"),
    (303, "name_to_handle_at"),
    (133, "mknod"),
    (259, "mknodat"),
    (167, "swapon"),
    (168, "swapoff"),
    (179, "quotactl"),
    // kernel / machine state
    (169, "reboot"),
    (170, "sethostname"),
    (171, "setdomainname"),
    (172, "iopl"),
    (173, "ioperm"),
    (174, "create_module"),
    (175, "init_module"),
    (176, "delete_module"),
    (313, "finit_module"),
    (246, "kexec_load"),
    (320, "kexec_file_load"),
    (163, "acct"),
    (164, "settimeofday"),
    (159, "adjtimex"),
    (227, "clock_settime"),
    (305, "clock_adjtime"),
    (103, "syslog"),
    (154, "modify_ldt"),
    // keys
    (248, "add_key"),
    (249, "request_key"),
    (250, "keyctl"),
    // changing identity or capabilities
    (105, "setuid"),
    (106, "setgid"),
    (113, "setreuid"),
    (114, "setregid"),
    (116, "setgroups"),
    (117, "setresuid"),
    (119, "setresgid"),
    (122, "setfsuid"),
    (123, "setfsgid"),
    (126, "capset"),
];

fn stmt(code: u16, k: u32) -> SockFilter {
    SockFilter { code, jt: 0, jf: 0, k }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// The classic-BPF program: kill on a foreign architecture or the x32 ABI,
/// `EPERM` for every denied syscall, allow everything else.
pub fn build_filter() -> Vec<SockFilter> {
    let n = DENIED_SYSCALLS.len();
    let mut f = Vec::with_capacity(n + 8);
    f.push(stmt(BPF_LD_W_ABS, 4)); // seccomp_data.arch
    f.push(jump(BPF_JEQ_K, AUDIT_ARCH_X86_64, 1, 0));
    f.push(stmt(BPF_RET_K, SECCOMP_RET_KILL_PROCESS));
    f.push(stmt(BPF_LD_W_ABS, 0)); // seccomp_data.nr
    f.push(jump(BPF_JSET_K, X32_SYSCALL_BIT, 0, 1));
    f.push(stmt(BPF_RET_K, SECCOMP_RET_KILL_PROCESS));
    for (i, (nr, _)) in DENIED_SYSCALLS.iter().enumerate() {
        // jump over the remaining comparisons and the ALLOW to the EPERM return
        f.push(jump(BPF_JEQ_K, *nr, (n - i) as u8, 0));
    }
    f.push(stmt(BPF_RET_K, SECCOMP_RET_ALLOW));
    f.push(stmt(BPF_RET_K, SECCOMP_RET_ERRNO | libc::EPERM as u32));
    f
}

// ───────────────────────────── the child side ─────────────────────────────

struct ChildPlan {
    chroot: Option<CString>,
    uid: Option<u32>,
    limits: Limits,
    filter: Option<Vec<SockFilter>>,
}

fn setrlimit(resource: libc::__rlimit_resource_t, soft: u64, hard: u64) -> io::Result<()> {
    let lim = libc::rlimit { rlim_cur: soft as libc::rlim_t, rlim_max: hard as libc::rlim_t };
    // SAFETY: `lim` is a valid rlimit for the duration of the call.
    if unsafe { libc::setrlimit(resource, &lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Runs in the forked child just before `exec`; only async-signal-safe libc calls.
unsafe fn apply_in_child(plan: &ChildPlan) -> io::Result<()> {
    if let Some(dir) = &plan.chroot {
        if libc::chroot(dir.as_ptr()) != 0 || libc::chdir(c"/".as_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if let Some(uid) = plan.uid {
        if libc::setgroups(0, std::ptr::null()) != 0 || libc::setgid(uid) != 0 || libc::setuid(uid) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let l = &plan.limits;
    setrlimit(libc::RLIMIT_CPU, l.cpu_secs, l.cpu_secs + 1)?;
    setrlimit(libc::RLIMIT_AS, l.mem_bytes, l.mem_bytes)?;
    if let Some(n) = l.processes {
        setrlimit(libc::RLIMIT_NPROC, n, n)?;
    }
    setrlimit(libc::RLIMIT_NOFILE, l.files, l.files)?;
    setrlimit(libc::RLIMIT_FSIZE, l.fsize, l.fsize)?;
    setrlimit(libc::RLIMIT_CORE, 0, 0)?;
    if let Some(filter) = &plan.filter {
        if libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        let prog = SockFprog { len: filter.len() as u16, filter: filter.as_ptr() };
        if libc::prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog as *const SockFprog, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

// ───────────────────────────── scratch directories ─────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A uniquely named directory, removed when dropped.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(base: &Path, prefix: &str) -> io::Result<TempDir> {
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let name =
            format!("{}-{}-{}-{:08x}", prefix, std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed), nanos);
        let path = base.join(name);
        std::fs::create_dir_all(base)?;
        std::fs::create_dir(&path)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        Ok(TempDir { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Give the directory to `uid` (so a process running as that uid can write there). No-op unless root.
    pub fn chown(&self, uid: u32) -> io::Result<()> {
        if !is_root() {
            return Ok(());
        }
        let c = CString::new(self.path.as_os_str().as_encoded_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: valid NUL-terminated path.
        if unsafe { libc::chown(c.as_ptr(), uid, uid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ───────────────────────────── running a process ─────────────────────────────

async fn drain<R: AsyncRead + Unpin>(mut r: R, cap: usize, over: Arc<Notify>, flag: Arc<AtomicBool>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(out.len());
                out.extend_from_slice(&buf[..n.min(room)]);
                if n > room {
                    flag.store(true, Ordering::SeqCst);
                    over.notify_one();
                    break;
                }
            }
        }
    }
    out
}

fn kill_group(pid: u32) {
    // SAFETY: signalling a process group we created; failure (already gone) is fine.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

/// Kill every process owned by numeric `uid`, including ones that left the job's process group with
/// `setsid()` / `setpgid()` and would survive `kill_group`. Only meaningful for a per-slot sandbox uid,
/// which belongs to exactly one job at a time. Implemented by running `/bin/true` as that uid and
/// calling `kill(-1, SIGKILL)` in the child (the caller itself is never a target of `kill(-1)`).
async fn kill_uid(uid: u32) {
    let mut cmd = Command::new("/bin/true");
    cmd.uid(uid).gid(uid).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: the closure only calls the async-signal-safe kill(2).
    unsafe {
        cmd.pre_exec(|| {
            libc::kill(-1, libc::SIGKILL);
            Ok(())
        });
    }
    if let Ok(mut child) = cmd.spawn() {
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    }
}

/// Resident memory in bytes, summed over every process in process group `pgid` (from `/proc`).
pub fn group_rss(pgid: u32) -> u64 {
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let Ok(dir) = std::fs::read_dir("/proc") else { return 0 };
    let mut total = 0;
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{name}/stat")) else { continue };
        total += rss_in_group(&stat, pgid, page);
    }
    total
}

/// `/proc/<pid>/stat` is `pid (comm) state ppid pgrp ... rss ...`; `comm` may contain spaces and
/// parentheses, so fields are counted from the last `)`: pgrp is the 3rd and rss the 22nd after it.
fn rss_in_group(stat: &str, pgid: u32, page: u64) -> u64 {
    let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else { return 0 };
    let f: Vec<&str> = rest.split_whitespace().collect();
    if f.len() > 21 && f[2].parse() == Ok(pgid) {
        f[21].parse::<u64>().unwrap_or(0) * page
    } else {
        0
    }
}

/// Completes when the group's resident memory exceeds `cap` (never, without a cap).
async fn memory_watch(pgid: u32, cap: Option<u64>) {
    let Some(cap) = cap.filter(|_| pgid != 0) else { return std::future::pending().await };
    let mut tick = tokio::time::interval(Duration::from_millis(40));
    loop {
        tick.tick().await;
        if group_rss(pgid) > cap {
            return;
        }
    }
}

/// Run `program` with `args`, feeding `stdin`, under `limits` and `iso`.
pub async fn run(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &[(&str, &str)],
    stdin: &[u8],
    limits: &Limits,
    iso: &Isolation,
) -> io::Result<Outcome> {
    let mut cmd = Command::new(program);
    cmd.args(args).env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).process_group(0);

    let chroot = match &iso.chroot {
        Some(p) => Some(
            CString::new(p.as_os_str().as_encoded_bytes())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
        ),
        None => None,
    };
    let plan = ChildPlan {
        chroot,
        uid: iso.uid,
        limits: limits.clone(),
        filter: if iso.seccomp && cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(build_filter())
        } else {
            None
        },
    };
    // SAFETY: the closure only calls async-signal-safe libc functions on data prepared before the fork.
    unsafe {
        cmd.pre_exec(move || apply_in_child(&plan));
    }

    let start = Instant::now();
    // Another thread forking while a freshly written executable is still open for
    // writing can make exec fail with ETXTBSY; it clears within milliseconds.
    let mut tries = 0;
    let mut child = loop {
        match cmd.spawn() {
            Ok(c) => break c,
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 10 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(e) => return Err(e),
        }
    };
    let pid = child.id().unwrap_or(0);
    let stdin_pipe = child.stdin.take();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let input = stdin.to_vec();
    tokio::spawn(async move {
        if let Some(mut s) = stdin_pipe {
            let _ = s.write_all(&input).await;
            let _ = s.shutdown().await;
        }
    });
    let over = Arc::new(Notify::new());
    let flag = Arc::new(AtomicBool::new(false));
    let out_task = tokio::spawn(drain(stdout, limits.output_cap, over.clone(), flag.clone()));
    let err_task = tokio::spawn(drain(stderr, limits.output_cap, over.clone(), flag.clone()));

    let mut timed_out = false;
    let mut memory_exceeded = false;
    let status = tokio::select! {
        r = child.wait() => Some(r?),
        _ = tokio::time::sleep(limits.wall) => { timed_out = true; None }
        _ = over.notified() => None,
        _ = memory_watch(pid, limits.group_rss) => { memory_exceeded = true; None }
    };
    let status = match status {
        Some(s) => s,
        None => {
            kill_group(pid);
            child.wait().await?
        }
    };
    // nothing the program started may outlive it (or keep the pipes open) -- not even a descendant
    // that moved to a session of its own
    kill_group(pid);
    if let Some(uid) = iso.uid {
        kill_uid(uid).await;
    }
    let time_ms = start.elapsed().as_millis() as u64;
    let join = |t: tokio::task::JoinHandle<Vec<u8>>| async move {
        tokio::time::timeout(Duration::from_secs(1), t).await.ok().and_then(|r| r.ok()).unwrap_or_default()
    };
    let (stdout, stderr) = (join(out_task).await, join(err_task).await);

    use std::os::unix::process::ExitStatusExt;
    Ok(Outcome {
        stdout,
        stderr,
        exit_code: status.code(),
        signal: status.signal(),
        timed_out,
        memory_exceeded,
        truncated: flag.load(Ordering::SeqCst),
        time_ms,
    })
}

pub fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGABRT => "SIGABRT",
        libc::SIGFPE => "SIGFPE",
        libc::SIGILL => "SIGILL",
        libc::SIGBUS => "SIGBUS",
        libc::SIGKILL => "SIGKILL",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGSYS => "SIGSYS",
        libc::SIGINT => "SIGINT",
        libc::SIGALRM => "SIGALRM",
        other => return format!("signal {}", other),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parsing_survives_odd_process_names() {
        // `comm` is "a) b (c" -- spaces and parentheses must not shift the fields
        let stat = "123 (a) b (c) S 1 777 123 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 5000 1000000 250 18446744073709551615 0 0";
        assert_eq!(rss_in_group(stat, 777, 4096), 250 * 4096);
        assert_eq!(rss_in_group(stat, 778, 4096), 0);
        assert_eq!(rss_in_group("garbage", 1, 4096), 0);
        assert_eq!(rss_in_group("1 (x) S 1", 1, 4096), 0);
    }

    #[test]
    fn the_current_process_group_has_resident_memory() {
        // SAFETY: getpgrp has no preconditions.
        let pgrp = unsafe { libc::getpgrp() } as u32;
        if std::path::Path::new("/proc/self/stat").exists() {
            assert!(group_rss(pgrp) > 0);
        }
    }

    #[test]
    fn filter_structure() {
        let f = build_filter();
        let n = DENIED_SYSCALLS.len();
        assert_eq!(f.len(), 6 + n + 2);
        // first jeq on the denied list must land on the final EPERM return
        let first = 6;
        for i in 0..n {
            let target = first + i + 1 + f[first + i].jt as usize;
            assert_eq!(target, f.len() - 1, "jump {i} must reach the EPERM return");
            assert_eq!(f[first + i].code, BPF_JEQ_K);
        }
        assert_eq!(f[f.len() - 2], stmt(BPF_RET_K, SECCOMP_RET_ALLOW));
        assert_eq!(f[f.len() - 1].k, SECCOMP_RET_ERRNO | libc::EPERM as u32);
        // the arch check skips exactly the KILL return
        assert_eq!(f[1].jt, 1);
        assert_eq!(f[2].k, SECCOMP_RET_KILL_PROCESS);
    }

    #[test]
    fn denied_syscall_numbers_are_unique_and_include_the_important_ones() {
        let mut nrs: Vec<u32> = DENIED_SYSCALLS.iter().map(|d| d.0).collect();
        nrs.sort();
        let len = nrs.len();
        nrs.dedup();
        assert_eq!(len, nrs.len(), "duplicate syscall numbers");
        for name in ["socket", "connect", "ptrace", "mount", "setuid", "unshare", "bpf"] {
            assert!(DENIED_SYSCALLS.iter().any(|d| d.1 == name), "{name} must be denied");
        }
        assert!(DENIED_SYSCALLS.len() < 250, "branch offsets are 8-bit");
        // execve/fork/clone must stay allowed (the program is started with exec; programs may fork)
        for name in ["execve", "fork", "clone", "write", "read", "mmap"] {
            assert!(!DENIED_SYSCALLS.iter().any(|d| d.1 == name));
        }
    }

    #[test]
    fn temp_dirs_are_unique_and_cleaned_up() {
        let base = std::env::temp_dir().join("cinder-sandbox-test");
        let (a, b) = (TempDir::new(&base, "t").unwrap(), TempDir::new(&base, "t").unwrap());
        assert_ne!(a.path(), b.path());
        let p = a.path().to_path_buf();
        assert!(p.is_dir());
        drop(a);
        assert!(!p.exists());
    }

    #[test]
    fn signal_names() {
        assert_eq!(signal_name(libc::SIGSEGV), "SIGSEGV");
        assert_eq!(signal_name(libc::SIGXCPU), "SIGXCPU");
        assert_eq!(signal_name(200), "signal 200");
    }
}
