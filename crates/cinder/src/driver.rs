//! Compiler driver: command line -> pipeline -> files/exit code.

use crate::diag::{self, Diagnostic, Renderer};
use crate::options::{self, Cli, ColorChoice, Mode, Options};
use crate::pp;
use crate::session::Session;
use std::io::{IsTerminal, Write};

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
    if opts.inputs.len() > 1
        && opts.output.is_some()
        && matches!(opts.mode, Mode::Object | Mode::Asm | Mode::Preprocess)
    {
        let _ = writeln!(err, "cinder: error: cannot specify -o when generating multiple output files");
        return 1;
    }
    let mut status = 0;
    for input in &opts.inputs {
        if compile_one(&opts, input, out, err) != 0 {
            status = 1;
        }
    }
    status
}

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

fn compile_one(opts: &Options, input: &str, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
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
            sess.diags.emit(Diagnostic::error(crate::source::Span::DUMMY, format!("{}: '{}'", why, input)));
            flush_diagnostics(&mut sess, opts, err);
            return 1;
        }
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let file = sess.sources.add_file(input, Some(input.into()), text);

    let toks = pp::preprocess(&mut sess, file, &opts.pp);
    if opts.mode == Mode::Preprocess {
        if !sess.diags.has_errors() {
            let text = pp::output::print(&sess, &toks);
            if let Err(code) = write_output(opts, text.as_bytes(), out, err) {
                return code;
            }
        }
        flush_diagnostics(&mut sess, opts, err);
        return i32::from(sess.diags.has_errors());
    }

    if !sess.diags.has_errors() {
        sess.diags.error(
            crate::source::Span::DUMMY,
            "parsing and code generation are not yet supported (only -E works so far)",
        );
    }
    flush_diagnostics(&mut sess, opts, err);
    1
}

/// Write to `-o <file>` or stdout.
fn write_output(opts: &Options, data: &[u8], out: &mut dyn Write, err: &mut dyn Write) -> Result<(), i32> {
    match &opts.output {
        Some(path) if path != "-" => std::fs::write(path, data).map_err(|e| {
            let _ = writeln!(err, "cinder: error: cannot write '{}': {}", path, e);
            1
        }),
        _ => {
            let _ = out.write_all(data);
            Ok(())
        }
    }
}
