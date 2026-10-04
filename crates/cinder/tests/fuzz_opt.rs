//! Differential fuzzing of the optimizer.
//!
//! Random, terminating, undefined-behaviour-free C programs are compiled by
//! `gcc -O0` (the reference) and by `cinder` at -O0, -O1 and -O2; stdout and
//! the exit status must agree. A failure reports the seed (re-run it with
//! `CINDER_FUZZ_SEED=<seed> CINDER_FUZZ_N=1`), keeps the program, and names
//! the optimizer passes whose individual removal makes the program pass.
//!
//! Environment:
//!   `CINDER_FUZZ_N`      number of programs (default 60)
//!   `CINDER_FUZZ_SEED`   first seed (default 1)
//!
//! UB is avoided by construction: arithmetic is on `unsigned` (wrapping),
//! signed operations are limited to division/remainder by positive
//! constants, comparisons and right shifts; shifts are masked; divisors are
//! `| 1`; array indices are masked; loops have constant trip counts; doubles
//! stay small so conversions to `int` are always in range.
#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

struct Gen {
    r: Rng,
    out: String,
    vars: Vec<String>,
    /// loop counters: read-only so every loop terminates
    ro: Vec<String>,
    /// number of already generated helper functions (callable)
    funcs: usize,
    loop_depth: usize,
    loop_ids: usize,
    /// Helper functions only touch their locals: an expression such as
    /// `garr[1] + f0(x)` has unspecified evaluation order, so calls must be pure.
    in_func: bool,
    /// the current function declares the local arrays `loc`, `uc`, `sh`
    has_locals: bool,
}

const SPECIAL: [&str; 12] =
    ["0u", "1u", "2u", "3u", "7u", "8u", "15u", "16u", "255u", "65535u", "2147483647u", "4294967295u"];

impl Gen {
    /// A variable that may be read (includes loop counters).
    fn var(&mut self) -> String {
        let n = (self.vars.len() + self.ro.len()) as u64;
        let i = self.r.below(n) as usize;
        if i < self.vars.len() {
            self.vars[i].clone()
        } else {
            self.ro[i - self.vars.len()].clone()
        }
    }

    /// A variable that may be assigned.
    fn wvar(&mut self) -> String {
        let i = self.r.below(self.vars.len() as u64) as usize;
        self.vars[i].clone()
    }

    fn leaf(&mut self) -> String {
        let kinds = if self.has_locals { 16 } else { 12 };
        match self.r.below(kinds) {
            0..=3 => self.var(),
            4 | 5 => SPECIAL[self.r.below(SPECIAL.len() as u64) as usize].to_string(),
            6 => format!("{}u", self.r.below(100000)),
            7 => format!("arr[{} & 7u]", self.var()),
            8 => format!("garr[{} & 7u]", self.var()),
            9 => "(unsigned)LL0".to_string(),
            10 => "(unsigned)(LL0 >> 32)".to_string(),
            11 => "(unsigned)(int)d0".to_string(),
            12 | 13 => format!("loc[{} & 3u]", self.var()),
            14 => format!("(unsigned)uc[{} & 7u]", self.var()),
            _ => format!("(unsigned)sh[{} & 3u]", self.var()),
        }
    }

    fn expr(&mut self, d: u32) -> String {
        if d == 0 || self.r.chance(22) {
            return self.leaf();
        }
        let a = self.expr(d - 1);
        match self.r.below(19) {
            0 => format!("({} + {})", a, self.expr(d - 1)),
            1 => format!("({} - {})", a, self.expr(d - 1)),
            2 => format!("({} * {})", a, self.expr(d - 1)),
            3 => format!("({} & {})", a, self.expr(d - 1)),
            4 => format!("({} | {})", a, self.expr(d - 1)),
            5 => format!("({} ^ {})", a, self.expr(d - 1)),
            6 => format!("({} << ({} & 31u))", a, self.expr(d - 1)),
            7 => format!("({} >> ({} & 31u))", a, self.expr(d - 1)),
            8 => format!("({} / ({} | 1u))", a, self.expr(d - 1)),
            9 => format!("({} % ({} | 1u))", a, self.expr(d - 1)),
            10 => {
                let c = self.cond(d - 1);
                format!("({} ? {} : {})", c, a, self.expr(d - 1))
            }
            11 => format!("(unsigned)({})", self.cond(d - 1)),
            12 => format!("(unsigned)(short){}", a),
            13 => format!("(unsigned)(unsigned char){}", a),
            14 => {
                let k = [2, 3, 4, 7, 8, 10, 16, 64, 100][self.r.below(9) as usize];
                format!("(unsigned)((int){} / {})", a, k)
            }
            15 => {
                let k = [2, 4, 8, 16, 5][self.r.below(5) as usize];
                format!("(unsigned)((int){} % {})", a, k)
            }
            16 => format!("(unsigned)((int){} >> ({} & 15u))", a, self.expr(d - 1)),
            17 if self.funcs > 0 => {
                let k = self.r.below(self.funcs as u64);
                format!("f{}({}, {})", k, a, self.expr(d - 1))
            }
            _ => format!("(~{})", a),
        }
    }

    fn cond(&mut self, d: u32) -> String {
        let a = self.expr(d.min(2));
        match self.r.below(12) {
            0 => format!("({} < {})", a, self.expr(d.min(2))),
            1 => format!("({} <= {})", a, self.expr(d.min(2))),
            2 => format!("({} > {})", a, self.expr(d.min(2))),
            3 => format!("({} == {})", a, self.expr(d.min(2))),
            4 => format!("({} != {})", a, self.expr(d.min(2))),
            5 => format!("((int){} < (int){})", a, self.expr(d.min(2))),
            6 => format!("((int){} >= (int){})", a, self.expr(d.min(2))),
            7 => format!("({} && {})", a, self.cond(d.saturating_sub(1))),
            8 => format!("({} || {})", a, self.cond(d.saturating_sub(1))),
            9 => format!("(!{})", a),
            10 => format!("(d0 < {}.5)", self.r.below(900)),
            _ => format!("({} & 1u)", a),
        }
    }

    fn line(&mut self, indent: usize, s: &str) {
        for _ in 0..indent {
            self.out.push_str("    ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    /// `lo + rand(span)` statements.
    fn block(&mut self, indent: usize, depth: u32, lo: u64, span: u64) {
        let n = lo + self.r.below(span);
        for _ in 0..n {
            self.stmt(indent, depth);
        }
    }

    fn stmt(&mut self, indent: usize, depth: u32) {
        let compound = depth > 0;
        let mut pick = self.r.below(if compound { 22 } else { 13 });
        if self.r.chance(18) {
            pick = 100 + self.r.below(3);
        }
        if self.in_func && matches!(pick, 4..=9 | 12) {
            pick = 0; // writes to globals, pointers into them, and printing
        }
        match pick {
            0..=3 => {
                let v = self.wvar();
                let op = ["=", "+=", "-=", "^=", "|=", "&=", "*="][self.r.below(7) as usize];
                let e = self.expr(3);
                self.line(indent, &format!("{} {} {};", v, op, e));
            }
            4 => {
                let (i, e) = (self.expr(2), self.expr(3));
                self.line(indent, &format!("arr[{} & 7u] = {};", i, e));
            }
            5 => {
                let (i, e) = (self.expr(2), self.expr(3));
                self.line(indent, &format!("garr[{} & 7u] ^= {};", i, e));
            }
            6 => {
                let e = self.expr(2);
                self.line(indent, &format!("LL0 = LL0 * 6364136223846793005ULL + (unsigned long long){};", e));
            }
            7 => {
                let e = self.expr(2);
                self.line(indent, &format!("d0 = d0 * 0.5 + (double)({} & 1023u);", e));
            }
            8 => {
                let e = self.expr(2);
                self.line(indent, &format!("s0 = (int)({} & 16777215u) - 5000;", e));
            }
            9 => {
                let (i, j, e1, e2) = (self.expr(2), self.expr(2), self.expr(2), self.expr(2));
                let v = self.wvar();
                self.line(indent, "{");
                self.line(indent + 1, &format!("unsigned *p = &arr[{} & 7u], *q = &arr[{} & 7u];", i, j));
                self.line(indent + 1, &format!("*p = {};", e1));
                self.line(indent + 1, &format!("*q = {};", e2));
                self.line(indent + 1, &format!("{} += *p ^ *q;", v));
                self.line(indent, "}");
            }
            10 => {
                let v = self.wvar();
                let e = self.expr(3);
                self.line(indent, &format!("{} = (unsigned)s0 + {};", v, e));
            }
            11 if self.funcs > 0 => {
                let v = self.wvar();
                let k = self.r.below(self.funcs as u64);
                let (a, b) = (self.expr(2), self.expr(2));
                self.line(indent, &format!("{} = f{}({}, {});", v, k, a, b));
            }
            12 => {
                let (c, e) = (self.cond(2), self.var());
                self.line(indent, &format!("if ({}) printf(\"p %u\\n\", {});", c, e));
            }
            100 => {
                let (i, e) = (self.expr(2), self.expr(3));
                self.line(indent, &format!("loc[{} & 3u] = {};", i, e));
            }
            101 => {
                let (i, e) = (self.expr(2), self.expr(3));
                if self.r.chance(50) {
                    self.line(indent, &format!("uc[{} & 7u] = (unsigned char)({});", i, e));
                } else {
                    self.line(indent, &format!("sh[{} & 3u] = (short)({});", i, e));
                }
            }
            102 => {
                let (i, j, e1, e2) = (self.expr(2), self.expr(2), self.expr(2), self.expr(2));
                let v = self.wvar();
                self.line(indent, "{");
                self.line(indent + 1, &format!("unsigned *p = &loc[{} & 3u], *q = &loc[{} & 3u];", i, j));
                self.line(indent + 1, &format!("*p = {};", e1));
                self.line(indent + 1, &format!("*q = {} + *p;", e2));
                self.line(indent + 1, &format!("{} ^= *p + *q;", v));
                self.line(indent, "}");
            }
            13..=15 => {
                let c = self.cond(2);
                self.line(indent, &format!("if ({}) {{", c));
                self.block(indent + 1, depth - 1, 1, 3);
                if self.r.chance(50) {
                    self.line(indent, "} else {");
                    self.block(indent + 1, depth - 1, 1, 3);
                }
                self.line(indent, "}");
            }
            16 | 17 if self.loop_depth < 2 => {
                let id = self.loop_ids;
                self.loop_ids += 1;
                let trips = 1 + self.r.below(9);
                self.line(indent, &format!("for (unsigned i{id} = 0; i{id} < {trips}u; i{id}++) {{"));
                self.ro.push(format!("i{id}"));
                self.loop_depth += 1;
                self.block(indent + 1, depth - 1, 1, 4);
                if self.r.chance(30) {
                    let c = self.cond(2);
                    let kw = if self.r.chance(50) { "break" } else { "continue" };
                    self.line(indent + 1, &format!("if ({}) {};", c, kw));
                }
                self.loop_depth -= 1;
                self.ro.pop();
                self.line(indent, "}");
            }
            18 if self.loop_depth < 2 => {
                let id = self.loop_ids;
                self.loop_ids += 1;
                let trips = 1 + self.r.below(7);
                self.line(indent, &format!("{{ unsigned w{id} = 0;"));
                self.line(indent, &format!("while (w{id} < {trips}u) {{ w{id}++;"));
                self.loop_depth += 1;
                self.block(indent + 1, depth - 1, 1, 3);
                if self.r.chance(30) {
                    let c = self.cond(2);
                    let kw = if self.r.chance(50) { "break" } else { "continue" };
                    self.line(indent + 1, &format!("if ({}) {};", c, kw));
                }
                self.loop_depth -= 1;
                self.line(indent, "} }");
            }
            19 | 20 => {
                let e = self.expr(2);
                self.line(indent, &format!("switch ({} & 3u) {{", e));
                let mut has_default = false;
                for k in 0..self.r.below(3) + 2 {
                    if !has_default && (k == 3 || self.r.chance(15)) {
                        has_default = true;
                        self.line(indent, "default:");
                    } else {
                        self.line(indent, &format!("case {}:", k));
                    }
                    self.block(indent + 1, depth - 1, 1, 2);
                    if self.r.chance(70) {
                        self.line(indent + 1, "break;");
                    }
                }
                self.line(indent, "}");
            }
            _ => {
                let v = self.wvar();
                let e = self.expr(3);
                self.line(indent, &format!("{} = {};", v, e));
            }
        }
    }
}

fn gen_program(seed: u64) -> String {
    let mut g = Gen {
        r: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
        out: String::new(),
        vars: Vec::new(),
        ro: Vec::new(),
        funcs: 0,
        loop_depth: 0,
        loop_ids: 0,
        in_func: false,
        has_locals: false,
    };
    for _ in 0..4 {
        g.r.next();
    }
    g.out.push_str("#include <stdio.h>\n\nunsigned garr[8] = {1, 2, 3, 5, 8, 13, 21, 34};\nunsigned long long LL0 = 88172645463325252ULL;\ndouble d0 = 1.5;\nint s0 = 0;\nunsigned arr[8];\n\n");
    // a tail-recursive helper and a few ordinary ones
    let rec_body = {
        g.vars = vec!["n".into(), "acc".into()];
        g.expr(2)
    };
    let _ = write!(g.out, "static unsigned rec(unsigned n, unsigned acc) {{\n    if (n == 0) return acc;\n    return rec(n - 1, {});\n}}\n\n", rec_body);
    let nfuncs = 1 + g.r.below(4) as usize;
    for j in 0..nfuncs {
        g.vars = vec!["a".into(), "b".into(), "t0".into(), "t1".into()];
        g.in_func = true;
        g.has_locals = true;
        let storage = if g.r.chance(60) { "static " } else { "" };
        let _ = writeln!(g.out, "{}unsigned f{}(unsigned a, unsigned b) {{\n    unsigned t0 = a ^ 0x9e3779b9u, t1 = b + {}u;\n    unsigned loc[4] = {{a, b, t0, t1}};\n    unsigned char uc[8] = {{1, 2, 3, 4, 5, 6, 7, 8}};\n    short sh[4] = {{-1, 2, -3, 4}};", storage, j, g.r.below(1000));
        g.block(1, 2, 2, 4);
        let tail = match g.r.below(4) {
            0 if j > 0 => {
                let c = g.cond(2);
                let k = g.r.below(j as u64);
                let (x, y, z) = (g.expr(2), g.expr(2), g.expr(2));
                format!("return {} ? {} : f{}({}, {});", c, x, k, y, z)
            }
            1 => {
                let c = g.cond(2);
                let (x, y) = (g.expr(2), g.expr(2));
                format!("if ({}) return rec({} & 15u, {}); return {};", c, x, y, g.expr(3))
            }
            _ => format!("return {};", g.expr(3)),
        };
        let _ = writeln!(g.out, "    {}\n}}\n", tail);
        g.funcs += 1;
    }
    g.in_func = false;
    g.has_locals = true;
    g.vars = (0..6).map(|i| format!("v{}", i)).collect();
    g.out.push_str("int main(void) {\n    unsigned v0 = 1, v1 = 2, v2 = 3, v3 = 4, v4 = 5, v5 = 6;\n    unsigned loc[4] = {9, 8, 7, 6};\n    unsigned char uc[8] = {200, 100, 50, 25, 12, 6, 3, 1};\n    short sh[4] = {-300, 300, -1, 1};\n");
    g.block(1, 3, 8, 10);
    g.out.push_str(
        "    printf(\"%u %u %u %u %u %u\\n\", v0, v1, v2, v3, v4, v5);\n    printf(\"%llu %.3f %d\\n\", LL0, d0, s0);\n    for (int i = 0; i < 8; i++) printf(\"%u %u %u\\n\", arr[i], garr[i], (unsigned)uc[i]);\n    for (int i = 0; i < 4; i++) printf(\"%u %d\\n\", loc[i], sh[i]);\n    return (int)((v0 ^ v1 ^ v2 ^ v3 ^ v4 ^ v5) & 127u);\n}\n",
    );
    g.out
}

struct Run {
    stdout: String,
    exit: i32,
    timed_out: bool,
}

fn run(exe: &Path) -> Run {
    let mut child =
        Command::new(exe).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
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
            None if start.elapsed() > Duration::from_secs(5) => {
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            None => std::thread::sleep(Duration::from_millis(1)),
        }
    };
    let exit = match status {
        Some(s) => {
            use std::os::unix::process::ExitStatusExt;
            s.code().unwrap_or(-s.signal().unwrap_or(0))
        }
        None => -1,
    };
    Run { stdout: String::from_utf8_lossy(&reader.join().unwrap()).to_string(), exit, timed_out }
}

fn cinder(src: &Path, exe: &Path, args: &[String]) -> Result<(), String> {
    let out = Command::new(env!("CARGO_BIN_EXE_cinder"))
        .args(args)
        .arg("-o")
        .arg(exe)
        .arg(src)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

/// Compile with cinder and compare with the reference run.
fn matches_reference(src: &Path, dir: &Path, args: &[String], reference: &Run) -> Result<(), String> {
    let exe = dir.join("t.out");
    let _ = fs::remove_file(&exe);
    cinder(src, &exe, args).map_err(|e| format!("cinder {:?} failed to compile:\n{}", args, e))?;
    let got = run(&exe);
    if got.timed_out {
        return Err(format!("cinder {:?}: timed out", args));
    }
    if got.stdout != reference.stdout || got.exit != reference.exit {
        return Err(format!(
            "cinder {:?}: output differs\n--- gcc (exit {}) ---\n{}--- cinder (exit {}) ---\n{}",
            args, reference.exit, reference.stdout, got.exit, got.stdout
        ));
    }
    Ok(())
}

fn check_seed(seed: u64, keep: &Path) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("cinder-fuzz-{}-{}", std::process::id(), seed));
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join("t.c");
    let text = gen_program(seed);
    fs::write(&src, &text).unwrap();

    let gcc_exe = dir.join("ref.out");
    let g = Command::new("gcc").args(["-O0", "-w", "-fno-builtin", "-o"]).arg(&gcc_exe).arg(&src).output().unwrap();
    if !g.status.success() {
        let _ = fs::write(keep.join(format!("seed-{seed}.c")), &text);
        return Err(format!("generator bug: gcc rejected seed {}:\n{}", seed, String::from_utf8_lossy(&g.stderr)));
    }
    let reference = run(&gcc_exe);
    if reference.timed_out {
        return Err(format!("generator bug: seed {} does not terminate", seed));
    }

    let mut failure: Option<String> = None;
    for level in ["-O0", "-O1", "-O2"] {
        if let Err(e) = matches_reference(&src, &dir, &[level.to_string()], &reference) {
            failure = Some(e);
            break;
        }
    }
    let Some(msg) = failure else {
        let _ = fs::remove_dir_all(&dir);
        return Ok(());
    };
    // which single pass, when removed at -O2, makes the program correct?
    let mut culprits: Vec<&str> = Vec::new();
    for pass in cinder::opt::PASS_NAMES {
        let args = vec!["-O2".to_string(), format!("-fno-{}", pass)];
        if matches_reference(&src, &dir, &args, &reference).is_ok() {
            culprits.push(pass);
        }
    }
    let kept = keep.join(format!("seed-{seed}.c"));
    let _ = fs::write(&kept, &text);
    Err(format!(
        "seed {} (kept as {})\npasses whose removal fixes it at -O2: {:?}\n{}",
        seed,
        kept.display(),
        culprits,
        msg
    ))
}

#[test]
fn optimizer_agrees_with_gcc_on_random_programs() {
    let n: u64 = std::env::var("CINDER_FUZZ_N").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let first: u64 = std::env::var("CINDER_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let keep: PathBuf = std::env::temp_dir().join("cinder-fuzz-failures");
    fs::create_dir_all(&keep).unwrap();
    let next = std::sync::atomic::AtomicU64::new(0);
    let failures = std::sync::Mutex::new(Vec::<String>::new());
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(8);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= n {
                    break;
                }
                if let Err(e) = check_seed(first + i, &keep) {
                    failures.lock().unwrap().push(e);
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    println!("fuzz: {} programs, {} failed", n, failures.len());
    if !failures.is_empty() {
        let shown: Vec<&String> = failures.iter().take(5).collect();
        panic!(
            "{} fuzz failures (showing {}):\n\n{}",
            failures.len(),
            shown.len(),
            shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n\n")
        );
    }
}
