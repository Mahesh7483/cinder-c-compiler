//! End-to-end tests: compile C programs with the `cinder` binary, run them,
//! and compare stdout and the exit code with the recorded expectation.
//!
//! Test programs live in `tests/e2e/*.cases` as "split-file" bundles:
//!
//! ```text
//! //// case: add_two_numbers
//! //// exit: 3
//! #include <stdio.h>
//! int main(void) { printf("hi\n"); return 3; }
//! //// stdout
//! hi
//! ```
//!
//! Directives (a line starting with `//// `):
//!   `case: <name>`        starts a case; everything up to the next directive is C source
//!   `exit: <n>`           expected exit status (default 0)
//!   `stdin`               following lines are fed to the program on stdin
//!   `stdout`              following lines are the expected stdout
//!   `flags: <args>`       extra compiler flags
//!   `expect-error: <txt>` the compile must fail and its diagnostics contain <txt>
//!   `skip-gcc`            do not cross-check this case against GCC
//!   `min-opt: <n>`        only run at -O<n> and above (GCC's reference build uses -O<n> too);
//!                         for programs that rely on tail calls or inlining to fit the stack
//!
//! Environment:
//!   `CINDER_E2E_OPTS`     comma-separated -O levels to test (default "0,1,2")
//!   `CINDER_E2E_FILTER`   only run cases whose name contains this substring
//!   `CINDER_DIFF_GCC=1`   also run every case through `gcc -O0` and require the same result
//!   `CINDER_BLESS=1`      rewrite `exit`/`stdout` of every case from GCC's behaviour
#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
struct Case {
    name: String,
    source: String,
    exit: i32,
    stdin: String,
    stdout: String,
    flags: Vec<String>,
    expect_error: Option<String>,
    skip_gcc: bool,
    min_opt: u8,
    has_stdout: bool,
    file: String,
}

fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2e")
}

fn parse_bundle(path: &Path) -> Vec<Case> {
    let text = fs::read_to_string(path).unwrap();
    let mut cases: Vec<Case> = Vec::new();
    let mut cur: Option<Case> = None;
    #[derive(PartialEq)]
    enum Sec {
        Source,
        Stdin,
        Stdout,
    }
    let mut sec = Sec::Source;
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("//// ") {
            let rest = rest.trim_end();
            if let Some(name) = rest.strip_prefix("case: ") {
                if let Some(c) = cur.take() {
                    cases.push(c);
                }
                cur = Some(Case {
                    name: name.trim().to_string(),
                    file: path.file_name().unwrap().to_string_lossy().to_string(),
                    ..Default::default()
                });
                sec = Sec::Source;
                continue;
            }
            let c = cur.as_mut().unwrap_or_else(|| panic!("{}: directive before any case", path.display()));
            if let Some(n) = rest.strip_prefix("exit: ") {
                c.exit = n.trim().parse().unwrap();
            } else if rest == "stdin" {
                sec = Sec::Stdin;
            } else if rest == "stdout" {
                sec = Sec::Stdout;
                c.has_stdout = true;
            } else if let Some(f) = rest.strip_prefix("flags: ") {
                c.flags = f.split_whitespace().map(|s| s.to_string()).collect();
            } else if let Some(e) = rest.strip_prefix("expect-error: ") {
                c.expect_error = Some(e.to_string());
            } else if rest == "skip-gcc" {
                c.skip_gcc = true;
            } else if let Some(n) = rest.strip_prefix("min-opt: ") {
                c.min_opt = n.trim().parse().unwrap();
            } else {
                panic!("{}: unknown directive {:?}", path.display(), rest);
            }
            continue;
        }
        if let Some(c) = cur.as_mut() {
            match sec {
                Sec::Source => c.source.push_str(line),
                Sec::Stdin => c.stdin.push_str(line),
                Sec::Stdout => c.stdout.push_str(line),
            }
        }
    }
    if let Some(c) = cur.take() {
        cases.push(c);
    }
    cases
}

fn all_cases() -> Vec<Case> {
    let mut files: Vec<PathBuf> = fs::read_dir(cases_dir())
        .expect("tests/e2e directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "cases"))
        .collect();
    files.sort();
    let filter = std::env::var("CINDER_E2E_FILTER").ok();
    let mut out = Vec::new();
    for f in files {
        for c in parse_bundle(&f) {
            if filter.as_ref().is_none_or(|flt| c.name.contains(flt.as_str())) {
                out.push(c);
            }
        }
    }
    out
}

struct RunResult {
    stdout: String,
    exit: i32,
    timed_out: bool,
}

fn run_with_timeout(mut cmd: Command, stdin: &str, limit: Duration) -> RunResult {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn test program");
    {
        let mut sin = child.stdin.take().unwrap();
        let data = stdin.to_string();
        std::thread::spawn(move || {
            let _ = sin.write_all(data.as_bytes());
        });
    }
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut buf);
        buf
    });
    let start = Instant::now();
    let (status, timed_out) = loop {
        match child.try_wait().unwrap() {
            Some(s) => break (Some(s), false),
            None if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    };
    let stdout = String::from_utf8_lossy(&reader.join().unwrap()).to_string();
    let exit = match status {
        Some(s) => s.code().unwrap_or(-(s_signal(&s))),
        None => -1,
    };
    RunResult { stdout, exit, timed_out }
}

fn s_signal(s: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.signal().unwrap_or(0)
}

fn workdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cinder-e2e-{}", std::process::id())).join(tag);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Compile with cinder. Ok(exe path) or Err(diagnostics text).
fn compile_cinder(c: &Case, opt: &str, dir: &Path) -> Result<PathBuf, String> {
    let src = dir.join("prog.c");
    fs::write(&src, &c.source).unwrap();
    let exe = dir.join("prog");
    let out = Command::new(env!("CARGO_BIN_EXE_cinder"))
        .arg(format!("-O{}", opt))
        .args(&c.flags)
        .arg("--color=never")
        .arg("-o")
        .arg(&exe)
        .arg(&src)
        .output()
        .expect("run cinder");
    if out.status.success() {
        Ok(exe)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

fn compile_gcc(c: &Case, dir: &Path) -> Result<PathBuf, String> {
    let src = dir.join("ref.c");
    fs::write(&src, &c.source).unwrap();
    let exe = dir.join("ref");
    let out = Command::new("gcc")
        .arg(format!("-O{}", c.min_opt))
        .args(["-w", "-fno-builtin", "-o"])
        .arg(&exe)
        .arg(&src)
        .arg("-lm")
        .output()
        .expect("run gcc");
    if out.status.success() {
        Ok(exe)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

fn check_case(c: &Case, opt: &str, diff_gcc: bool) -> Result<(), String> {
    if opt.parse::<u8>().is_ok_and(|o| o < c.min_opt) {
        return Ok(());
    }
    let dir = workdir(&format!("{}-O{}", c.name, opt));
    if let Some(expected) = &c.expect_error {
        return match compile_cinder(c, opt, &dir) {
            Ok(_) => Err("expected a compile error but compilation succeeded".to_string()),
            Err(diag) => {
                if diag.contains(expected.as_str()) {
                    Ok(())
                } else {
                    Err(format!("diagnostics do not contain {:?}:\n{}", expected, diag))
                }
            }
        };
    }
    let exe = compile_cinder(c, opt, &dir).map_err(|d| format!("compilation failed:\n{}", d))?;
    let got = run_with_timeout(Command::new(&exe), &c.stdin, Duration::from_secs(10));
    if got.timed_out {
        return Err("timed out".to_string());
    }
    if got.stdout != c.stdout || got.exit != c.exit {
        return Err(format!(
            "output mismatch\n--- expected (exit {}) ---\n{}--- actual (exit {}) ---\n{}",
            c.exit, c.stdout, got.exit, got.stdout
        ));
    }
    if diff_gcc && !c.skip_gcc {
        let g = compile_gcc(c, &dir).map_err(|d| format!("gcc rejected the program:\n{}", d))?;
        let r = run_with_timeout(Command::new(&g), &c.stdin, Duration::from_secs(10));
        if r.stdout != got.stdout || r.exit != got.exit {
            return Err(format!(
                "differs from gcc\n--- gcc (exit {}) ---\n{}--- cinder (exit {}) ---\n{}",
                r.exit, r.stdout, got.exit, got.stdout
            ));
        }
    }
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

fn bless(path: &Path) {
    let cases = parse_bundle(path);
    let mut text = String::new();
    for c in cases {
        let dir = workdir(&format!("bless-{}", c.name));
        let mut c2 = c.clone();
        if c.expect_error.is_none() {
            let exe = compile_gcc(&c, &dir).unwrap_or_else(|e| panic!("gcc rejected {}:\n{}", c.name, e));
            let r = run_with_timeout(Command::new(&exe), &c.stdin, Duration::from_secs(10));
            assert!(!r.timed_out, "{} timed out under gcc", c.name);
            c2.exit = r.exit;
            c2.stdout = r.stdout;
            c2.has_stdout = true;
        }
        text.push_str(&format!("//// case: {}\n", c2.name));
        text.push_str(&c2.source);
        if !c2.source.ends_with('\n') {
            text.push('\n');
        }
        if c2.exit != 0 {
            text.push_str(&format!("//// exit: {}\n", c2.exit));
        }
        if !c2.flags.is_empty() {
            text.push_str(&format!("//// flags: {}\n", c2.flags.join(" ")));
        }
        if c2.skip_gcc {
            text.push_str("//// skip-gcc\n");
        }
        if c2.min_opt > 0 {
            text.push_str(&format!("//// min-opt: {}\n", c2.min_opt));
        }
        if let Some(e) = &c2.expect_error {
            text.push_str(&format!("//// expect-error: {}\n", e));
        }
        if !c2.stdin.is_empty() {
            text.push_str("//// stdin\n");
            text.push_str(&c2.stdin);
        }
        if c2.has_stdout && c2.expect_error.is_none() {
            text.push_str("//// stdout\n");
            text.push_str(&c2.stdout);
        }
    }
    fs::write(path, text).unwrap();
}

#[test]
fn e2e_cases() {
    if std::env::var("CINDER_BLESS").is_ok() {
        let mut files: Vec<PathBuf> = fs::read_dir(cases_dir())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "cases"))
            .collect();
        files.sort();
        for f in files {
            bless(&f);
        }
        return;
    }
    let diff_gcc = std::env::var("CINDER_DIFF_GCC").is_ok_and(|v| v != "0");
    let opts: Vec<String> = std::env::var("CINDER_E2E_OPTS")
        .unwrap_or_else(|_| "0,1,2".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    let cases = all_cases();
    assert!(!cases.is_empty(), "no e2e cases found");
    let jobs: Vec<(Case, String)> =
        cases.iter().flat_map(|c| opts.iter().map(move |o| (c.clone(), o.clone()))).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failures = std::sync::Mutex::new(Vec::<String>::new());
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(8);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some((c, o)) = jobs.get(i) else { break };
                if let Err(e) = check_case(c, o, diff_gcc) {
                    failures.lock().unwrap().push(format!("[{} -O{}] ({})\n{}", c.name, o, c.file, e));
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    println!("e2e: {} cases x {} opt levels = {} runs, {} failed", cases.len(), opts.len(), jobs.len(), failures.len());
    if !failures.is_empty() {
        let shown: Vec<&String> = failures.iter().take(15).collect();
        panic!(
            "{} e2e failures (showing {}):\n\n{}",
            failures.len(),
            shown.len(),
            shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n\n")
        );
    }
}
