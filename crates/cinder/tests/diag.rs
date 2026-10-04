//! Diagnostic golden tests.
//!
//! Every `tests/diag/*.c` is compiled with `cinder -fsyntax-only --color=never`
//! (from inside that directory, so paths in messages are stable) and the full
//! rendered stderr (source lines, carets, notes, summary) must equal the
//! neighbouring `<name>.expected`. A first line `// flags: ...` adds compiler
//! flags (default `-Wall`). Rewrite the goldens with `CINDER_BLESS=1`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn diag_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/diag")
}

fn flags_of(src: &str) -> Vec<String> {
    match src.lines().next().and_then(|l| l.strip_prefix("// flags:")) {
        Some(f) => f.split_whitespace().map(|s| s.to_string()).collect(),
        None => vec!["-Wall".to_string()],
    }
}

#[test]
fn diagnostics_match_goldens() {
    let dir = diag_dir();
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("tests/diag")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "c"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no diagnostic samples found");
    let bless = std::env::var("CINDER_BLESS").is_ok();
    let mut failures: Vec<String> = Vec::new();
    for f in &files {
        let src = fs::read_to_string(f).unwrap();
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let out = Command::new(env!("CARGO_BIN_EXE_cinder"))
            .current_dir(&dir)
            .args(["--color=never", "-fsyntax-only"])
            .args(flags_of(&src))
            .arg(&name)
            .output()
            .expect("run cinder");
        let got = String::from_utf8_lossy(&out.stderr).to_string();
        let golden = f.with_extension("expected");
        if bless {
            fs::write(&golden, &got).unwrap();
            continue;
        }
        let want = fs::read_to_string(&golden)
            .unwrap_or_else(|_| panic!("missing {} (run with CINDER_BLESS=1)", golden.display()));
        if got != want {
            failures.push(format!("--- {name}: expected ---\n{want}--- actual ---\n{got}"));
        }
    }
    assert!(failures.is_empty(), "{} diagnostic goldens differ:\n\n{}", failures.len(), failures.join("\n"));
}

/// The rendering must be stable: same input, same bytes (no hash-order or timing dependence).
#[test]
fn diagnostics_are_deterministic() {
    let dir = diag_dir();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_cinder"))
            .current_dir(&dir)
            .args(["--color=never", "-fsyntax-only", "-Wall", "-Wextra", "warnings.c"])
            .output()
            .unwrap()
            .stderr
    };
    assert_eq!(run(), run());
}

#[test]
fn json_diagnostics_are_well_formed() {
    let dir = diag_dir();
    let out = Command::new(env!("CARGO_BIN_EXE_cinder"))
        .current_dir(&dir)
        .args(["--diagnostics-format=json", "-fsyntax-only", "-Wall", "uninitialized.c"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.trim_start().starts_with("{\"diagnostics\":["), "{text}");
    assert!(text.contains("\"maybe-uninitialized\"") || text.contains("maybe-uninitialized"), "{text}");
    assert!(text.trim_end().ends_with('}'), "{text}");
}
