//! Compiler driver: command line -> pipeline -> files/exit code.
//!
//! The compiler proper produces x86-64 assembly. Turning that into an
//! executable uses the system `as` (assembler) and `cc` (only as a linker
//! driver, to find the C runtime and libc).

use crate::backend::{self, BackendOptions};
use crate::diag::{self, Diagnostic, Renderer};
use crate::options::{self, Cli, ColorChoice, Mode, Options};
use crate::pp;
use crate::session::Session;
use crate::source::Span;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// Entry point used by `main` and by tests. Returns the process exit code.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let opts = match options::parse_args(args) {
        Ok(Cli::Help) => {
            let _ = out.write_all(options::HELP.as_bytes());
            return 0;
        }
        Ok(Cli::Version) => {
            let _ = writeln!(out, "cinder {} (x86-64 Linux, System V ABI)", crate::VERSION);
            return 0;
        }
        Ok(Cli::Run(o)) => *o,
        Err(msg) => {
            let _ = writeln!(err, "cinder: error: {}", msg);
            return 1;
        }
    };
    if opts.inputs.is_empty() {
        let _ = writeln!(err, "cinder: error: no input files");
        return 1;
    }
    let multi_out = matches!(opts.mode, Mode::Object | Mode::Asm | Mode::Preprocess);
    if opts.inputs.len() > 1 && opts.output.is_some() && multi_out {
        let _ = writeln!(err, "cinder: error: cannot specify -o when generating multiple output files");
        return 1;
    }

    let mut temps = TempFiles::default();
    let mut status = 0;
    let mut link_inputs: Vec<String> = Vec::new();
    for input in &opts.inputs {
        let ext = Path::new(input).extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "o" | "a" | "so" => link_inputs.push(input.clone()),
            "s" | "S" => match assemble(&opts, Path::new(input), &mut temps, err) {
                Ok(obj) => link_inputs.push(obj),
                Err(()) => status = 1,
            },
            _ => match compile_c(&opts, input, out, err) {
                Compiled::Done => {}
                Compiled::Failed => status = 1,
                Compiled::Asm(text) => match finish_asm(&opts, input, &text, &mut temps, out, err) {
                    Ok(Some(obj)) => link_inputs.push(obj),
                    Ok(None) => {}
                    Err(()) => status = 1,
                },
            },
        }
    }
    if status == 0 && opts.mode == Mode::Link {
        if let Err(()) = link(&opts, &link_inputs, err) {
            status = 1;
        }
    }
    temps.cleanup();
    status
}

// ───────────────────────────── diagnostics output ─────────────────────────────

fn use_color(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            if std::env::var_os("NO_COLOR").is_some() {
                return false;
            }
            if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
                return true;
            }
            std::io::stderr().is_terminal()
        }
    }
}

fn flush_diagnostics(sess: &mut Session, opts: &Options, err: &mut dyn Write) {
    let summary = sess.diags.summary();
    let diags = sess.diags.take();
    if opts.diag_json {
        let items: Vec<String> = diags.iter().map(|d| diag::to_json(d, &sess.sources)).collect();
        let _ = writeln!(
            err,
            "{{\"diagnostics\":[{}],\"errors\":{},\"warnings\":{}}}",
            items.join(","),
            sess.diags.error_count(),
            sess.diags.warning_count()
        );
    } else if !diags.is_empty() {
        let r = Renderer::new(&sess.sources, use_color(opts.color));
        let _ = err.write_all(r.render_all(&diags, summary).as_bytes());
    }
}

// ───────────────────────────── temp files ─────────────────────────────

#[derive(Default)]
struct TempFiles {
    paths: Vec<PathBuf>,
}

impl TempFiles {
    fn new_path(&mut self, ext: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "cinder-{}-{}.{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst),
            ext
        ));
        self.paths.push(p.clone());
        p
    }

    fn cleanup(&mut self) {
        for p in self.paths.drain(..) {
            let _ = std::fs::remove_file(p);
        }
    }
}

// ───────────────────────────── compilation of one C file ─────────────────────────────

enum Compiled {
    /// Output for a front-end-only mode was written.
    Done,
    Failed,
    Asm(String),
}

fn compile_c(opts: &Options, input: &str, out: &mut dyn Write, err: &mut dyn Write) -> Compiled {
    let mut sess = Session::new();
    sess.diags.config = opts.warn.clone();
    sess.diags.error_limit = opts.error_limit;

    let bytes = match std::fs::read(input) {
        Ok(b) => b,
        Err(e) => {
            let why = if e.kind() == std::io::ErrorKind::NotFound {
                "no such file or directory".to_string()
            } else {
                e.to_string()
            };
            sess.diags.emit(Diagnostic::error(Span::DUMMY, format!("{}: '{}'", why, input)));
            flush_diagnostics(&mut sess, opts, err);
            return Compiled::Failed;
        }
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let file = sess.sources.add_file(input, Some(input.into()), text);

    macro_rules! stop_on_errors {
        () => {
            if sess.diags.has_errors() {
                flush_diagnostics(&mut sess, opts, err);
                return Compiled::Failed;
            }
        };
    }
    macro_rules! emit_text {
        ($text:expr) => {{
            if write_output(opts, $text.as_bytes(), out, err).is_err() {
                return Compiled::Failed;
            }
            flush_diagnostics(&mut sess, opts, err);
            return Compiled::Done;
        }};
    }

    let toks = pp::preprocess(&mut sess, file, &opts.pp);
    if opts.mode == Mode::Preprocess {
        stop_on_errors!();
        let text = pp::output::print(&sess, &toks);
        emit_text!(text);
    }
    stop_on_errors!();

    let tu = crate::parse::parse(&mut sess, toks);
    if opts.mode == Mode::Ast {
        stop_on_errors!();
        let text = crate::ast_dump::dump(&sess.sources, &tu);
        emit_text!(text);
    }
    stop_on_errors!();

    let Some(hir) = crate::sema::analyze(&mut sess, &tu) else {
        flush_diagnostics(&mut sess, opts, err);
        return Compiled::Failed;
    };
    stop_on_errors!();
    if opts.mode == Mode::Hir {
        let text = crate::hir_dump::dump(&sess.sources, &hir);
        emit_text!(text);
    }

    let module = crate::lower::lower_module(&mut sess, &hir, input);
    stop_on_errors!();
    if let Err(errs) = crate::ir::verify::verify_module(&module) {
        for e in errs {
            sess.diags.error(Span::DUMMY, format!("internal compiler error: invalid IR: {}", e));
        }
        flush_diagnostics(&mut sess, opts, err);
        return Compiled::Failed;
    }
    if opts.mode == Mode::Ir {
        let text = crate::ir::print::print_module(&module);
        emit_text!(text);
    }

    let asm = backend::compile_module(&module, &BackendOptions::default());
    flush_diagnostics(&mut sess, opts, err);
    Compiled::Asm(asm)
}

/// Where the assembler/object output goes by default (current directory, like cc).
fn default_output(input: &str, ext: &str) -> PathBuf {
    let stem = Path::new(input).file_stem().and_then(|s| s.to_str()).unwrap_or("a");
    PathBuf::from(format!("{}.{}", stem, ext))
}

/// Handle the assembly text of a compiled C file according to the mode.
/// Returns the object file to link, if any.
fn finish_asm(
    opts: &Options,
    input: &str,
    asm: &str,
    temps: &mut TempFiles,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Option<String>, ()> {
    if opts.mode == Mode::Asm {
        match &opts.output {
            Some(p) if p == "-" => {
                let _ = out.write_all(asm.as_bytes());
            }
            Some(p) => std::fs::write(p, asm).map_err(|e| {
                let _ = writeln!(err, "cinder: error: cannot write '{}': {}", p, e);
            })?,
            None => {
                let p = default_output(input, "s");
                std::fs::write(&p, asm).map_err(|e| {
                    let _ = writeln!(err, "cinder: error: cannot write '{}': {}", p.display(), e);
                })?;
            }
        }
        return Ok(None);
    }
    let asm_path = temps.new_path("s");
    std::fs::write(&asm_path, asm).map_err(|e| {
        let _ = writeln!(err, "cinder: error: cannot write temporary file: {}", e);
    })?;
    // For `-c` the object is named after the *source* file.
    let obj = if opts.mode == Mode::Object {
        Some(match &opts.output {
            Some(p) => PathBuf::from(p),
            None => default_output(input, "o"),
        })
    } else {
        None
    };
    let obj = assemble_to(opts, &asm_path, obj, temps, err)?;
    Ok(if opts.mode == Mode::Object { None } else { Some(obj) })
}

fn run_tool(opts: &Options, program: &str, args: &[String], err: &mut dyn Write) -> Result<(), ()> {
    if opts.verbose {
        let _ = writeln!(err, "{} {}", program, args.join(" "));
    }
    match Command::new(program).args(args).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let _ = err.write_all(&o.stderr);
            let _ =
                writeln!(err, "cinder: error: '{}' failed with exit code {}", program, o.status.code().unwrap_or(-1));
            Err(())
        }
        Err(e) => {
            let _ = writeln!(err, "cinder: error: cannot run '{}': {} (is it installed?)", program, e);
            Err(())
        }
    }
}

/// Assemble a user-provided `.s` file.
fn assemble(opts: &Options, asm: &Path, temps: &mut TempFiles, err: &mut dyn Write) -> Result<String, ()> {
    let obj = if opts.mode == Mode::Object {
        Some(match &opts.output {
            Some(p) => PathBuf::from(p),
            None => default_output(&asm.to_string_lossy(), "o"),
        })
    } else {
        None
    };
    assemble_to(opts, asm, obj, temps, err)
}

fn assemble_to(
    opts: &Options,
    asm: &Path,
    obj: Option<PathBuf>,
    temps: &mut TempFiles,
    err: &mut dyn Write,
) -> Result<String, ()> {
    let obj = obj.unwrap_or_else(|| temps.new_path("o"));
    // `as` honours `.file`/`.loc`, so debug line tables come for free.
    run_tool(
        opts,
        "as",
        &["-o".to_string(), obj.to_string_lossy().to_string(), asm.to_string_lossy().to_string()],
        err,
    )?;
    Ok(obj.to_string_lossy().to_string())
}

/// Link objects into an executable with `cc` as the linker driver.
fn link(opts: &Options, objs: &[String], err: &mut dyn Write) -> Result<(), ()> {
    let output = opts.output.clone().unwrap_or_else(|| "a.out".to_string());
    let mut args: Vec<String> = Vec::new();
    // Generated code uses absolute addressing for globals: link as a non-PIE executable.
    if !opts.link_args.iter().any(|a| a == "-pie") {
        args.push("-no-pie".to_string());
    }
    args.push("-o".to_string());
    args.push(output);
    args.extend(objs.iter().cloned());
    args.extend(opts.link_args.iter().cloned());
    args.push("-lm".to_string());
    run_tool(opts, "cc", &args, err)
}

/// Write to `-o <file>` or stdout.
fn write_output(opts: &Options, data: &[u8], out: &mut dyn Write, err: &mut dyn Write) -> Result<(), ()> {
    match &opts.output {
        Some(path) if path != "-" => std::fs::write(path, data).map_err(|e| {
            let _ = writeln!(err, "cinder: error: cannot write '{}': {}", path, e);
        }),
        _ => {
            let _ = out.write_all(data);
            Ok(())
        }
    }
}
