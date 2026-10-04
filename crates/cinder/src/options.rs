//! Command-line parsing. The flag set follows cc/Clang conventions so Cinder
//! can be dropped into existing build commands.

use crate::diag::WarnConfig;
use crate::pp::{MacroCmd, PpOptions};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Compile, assemble and link (default).
    Link,
    /// `-c`: stop after assembling to an object file.
    Object,
    /// `-S`: stop after generating assembly.
    Asm,
    /// `-E`: stop after preprocessing.
    Preprocess,
    /// `--emit-ast`: print the syntax tree.
    Ast,
    /// `-fsyntax-only`: run the front end (and lowering, for its diagnostics) and stop.
    Check,
    /// `--emit-hir`: print the typed tree with all implicit conversions explicit.
    Hir,
    /// `--emit-ir`: print the SSA IR (after the selected optimization level).
    Ir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub inputs: Vec<String>,
    pub output: Option<String>,
    pub mode: Mode,
    pub opt_level: u8,
    pub pp: PpOptions,
    pub warn: WarnConfig,
    pub color: ColorChoice,
    pub diag_json: bool,
    pub error_limit: usize,
    /// `-l`, `-L`, `-static`, ... forwarded to the linker driver.
    pub link_args: Vec<String>,
    /// `-fpass` (true) / `-fno-pass` (false) optimizer toggles, in order.
    pub pass_flags: Vec<(String, bool)>,
    pub verbose: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            inputs: Vec::new(),
            output: None,
            mode: Mode::Link,
            opt_level: 0,
            pp: PpOptions::default(),
            warn: WarnConfig::default(),
            color: ColorChoice::Auto,
            diag_json: false,
            error_limit: 20,
            link_args: Vec::new(),
            pass_flags: Vec::new(),
            verbose: false,
        }
    }
}

#[derive(Debug)]
pub enum Cli {
    Run(Box<Options>),
    Help,
    Version,
}

pub const HELP: &str = "\
OVERVIEW: cinder - a from-scratch C11 compiler for x86-64 Linux

USAGE: cinder [options] file...

OPTIONS:
  -o <file>             Write output to <file>
  -c                    Compile and assemble, but do not link
  -S                    Compile only; output assembly (AT&T syntax)
  -E                    Preprocess only; output to stdout
  -O0 -O1 -O2           Optimization level (-O3 and -Os are treated as -O2)
  -I <dir>              Add a directory to the include search path
  -isystem <dir>        Add a real system include directory (searched after the bundled headers)
  -nostdinc             Do not use the bundled libc headers
  -D <name>[=<value>]   Define a macro
  -U <name>             Undefine a macro
  -Wall -Wextra         Enable groups of warnings (-Wall includes -Wconversion)
  -W<name> -Wno-<name>  Enable / disable one warning (e.g. -Wunused-variable)
  -Werror[=<name>]      Treat warnings (or one warning) as errors
  -w                    Suppress all warnings
  -f<pass> -fno-<pass>  Toggle an optimization pass (mem2reg, sccp, dce, cse, copyprop,
                        licm, strength, inline, tailcall, simplifycfg, peephole)
  -l<lib> -L<dir>       Passed to the linker
  -static               Link statically
  --emit-ast            Print the abstract syntax tree
  -fsyntax-only         Check the program and report diagnostics; produce no output
  --emit-hir            Print the typed tree (types and implicit conversions made explicit)
  --emit-ir             Print the SSA intermediate representation
  --color=<auto|always|never>
  --diagnostics-format=<text|json>
  -ferror-limit=<n>     Stop after n errors (0 = no limit)
  -v                    Print the commands that are run
  --version             Print the version
  --help                Print this help
";

fn take_value(args: &[String], i: &mut usize, flag: &str, attached: &str) -> Result<String, String> {
    if !attached.is_empty() {
        return Ok(attached.to_string());
    }
    *i += 1;
    args.get(*i).cloned().ok_or_else(|| format!("argument to '{}' is missing (expected 1 value)", flag))
}

pub fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut o = Options::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--help" | "-help" | "-h" => return Ok(Cli::Help),
            "--version" | "-version" | "-V" => return Ok(Cli::Version),
            "-c" => o.mode = Mode::Object,
            "-S" => o.mode = Mode::Asm,
            "-E" => o.mode = Mode::Preprocess,
            "--emit-ast" => o.mode = Mode::Ast,
            "-fsyntax-only" => o.mode = Mode::Check,
            "--emit-hir" => o.mode = Mode::Hir,
            "--emit-ir" => o.mode = Mode::Ir,
            "-O0" => o.opt_level = 0,
            "-O" | "-O1" => o.opt_level = 1,
            "-O2" | "-O3" | "-Os" | "-Ofast" => o.opt_level = 2,
            "-w" => o.warn.suppress_all = true,
            "-nostdinc" => o.pp.no_bundled_headers = true,
            "-static" | "-pie" | "-no-pie" | "-shared" => o.link_args.push(a.to_string()),
            "-v" => o.verbose = true,
            "-g"
            | "-g0"
            | "-g1"
            | "-g2"
            | "-g3"
            | "-pedantic"
            | "-pedantic-errors"
            | "-ansi"
            | "-pipe"
            | "-fPIC"
            | "-fpic"
            | "-fPIE"
            | "-fpie"
            | "-fno-strict-aliasing"
            | "-fstrict-aliasing"
            | "-fomit-frame-pointer"
            | "-fno-omit-frame-pointer"
            | "-fcolor-diagnostics"
            | "-fno-color-diagnostics"
            | "-fno-common"
            | "-fcommon"
            | "-fwrapv"
            | "-fno-builtin" => {
                if a == "-fcolor-diagnostics" {
                    o.color = ColorChoice::Always;
                } else if a == "-fno-color-diagnostics" {
                    o.color = ColorChoice::Never;
                }
            }
            "-o" => o.output = Some(take_value(args, &mut i, "-o", "")?),
            "-I" => o.pp.include_dirs.push(PathBuf::from(take_value(args, &mut i, "-I", "")?)),
            "-isystem" | "-idirafter" | "-iquote" => {
                let v = take_value(args, &mut i, a, "")?;
                if a == "-iquote" {
                    o.pp.include_dirs.push(PathBuf::from(v));
                } else {
                    o.pp.system_dirs.push(PathBuf::from(v));
                }
            }
            "-D" => o.pp.defines.push(MacroCmd::Define(take_value(args, &mut i, "-D", "")?)),
            "-U" => o.pp.defines.push(MacroCmd::Undef(take_value(args, &mut i, "-U", "")?)),
            "-l" | "-L" => {
                let v = take_value(args, &mut i, a, "")?;
                o.link_args.push(format!("{}{}", a, v));
            }
            "-x" | "-MF" | "-MT" | "-MQ" => {
                let _ = take_value(args, &mut i, a, "")?;
            }
            "-MD" | "-MMD" | "-MP" | "-M" | "-MM" => {}
            "--color" => {
                let v = take_value(args, &mut i, "--color", "")?;
                o.color = parse_color(&v)?;
            }
            _ if a.starts_with("--color=") => o.color = parse_color(&a["--color=".len()..])?,
            _ if a.starts_with("--diagnostics-format=") => match &a["--diagnostics-format=".len()..] {
                "json" => o.diag_json = true,
                "text" => o.diag_json = false,
                other => return Err(format!("invalid value '{}' in '--diagnostics-format'", other)),
            },
            _ if a.starts_with("-ferror-limit=") => {
                o.error_limit = a["-ferror-limit=".len()..].parse().map_err(|_| format!("invalid value in '{}'", a))?;
            }
            _ if a.starts_with("-std=")
                || a.starts_with("-march=")
                || a.starts_with("-mtune=")
                || a.starts_with("-fdiagnostics-") => {}
            _ if a.starts_with("-Werror") || a.starts_with("-Wno-error") => {
                let flag = &a[2..];
                if !o.warn.apply_flag(flag) {
                    return Err(format!("unknown warning option '{}'", a));
                }
            }
            _ if a.starts_with("-W") => {
                // `-Wl,<args>` / `-Wa,` / `-Wp,` go to the toolchain, not the warning system.
                if let Some(list) = a.strip_prefix("-Wl,") {
                    o.link_args.extend(list.split(',').map(|s| format!("-Wl,{}", s)));
                } else if a.starts_with("-Wa,") || a.starts_with("-Wp,") {
                    // ignored
                } else if !o.warn.apply_flag(&a[2..]) {
                    return Err(format!("unknown warning option '{}'", a));
                }
            }
            _ if a.starts_with("-I") => o.pp.include_dirs.push(PathBuf::from(&a[2..])),
            _ if a.starts_with("-D") => o.pp.defines.push(MacroCmd::Define(a[2..].to_string())),
            _ if a.starts_with("-U") => o.pp.defines.push(MacroCmd::Undef(a[2..].to_string())),
            _ if a.starts_with("-l") || a.starts_with("-L") => o.link_args.push(a.to_string()),
            _ if a.starts_with("-o") && a.len() > 2 => o.output = Some(a[2..].to_string()),
            _ if a.starts_with("-O") => {
                // -O<n> for n > 2, or -Oz etc.
                o.opt_level = if a[2..].chars().all(|c| c == '0') {
                    0
                } else if a == "-O1" {
                    1
                } else {
                    2
                };
            }
            _ if a.starts_with("-f") => {
                let rest = &a[2..];
                match rest.strip_prefix("no-") {
                    Some(name) => o.pass_flags.push((name.to_string(), false)),
                    None => o.pass_flags.push((rest.to_string(), true)),
                }
            }
            _ if a.starts_with('-') && a.len() > 1 => return Err(format!("unknown argument: '{}'", a)),
            _ => o.inputs.push(a.to_string()),
        }
        i += 1;
    }
    o.pp.opt_level = o.opt_level;
    Ok(Cli::Run(Box::new(o)))
}

fn parse_color(v: &str) -> Result<ColorChoice, String> {
    match v {
        "auto" => Ok(ColorChoice::Auto),
        "always" => Ok(ColorChoice::Always),
        "never" => Ok(ColorChoice::Never),
        other => Err(format!("invalid value '{}' in '--color'", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Options {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse_args(&v).unwrap() {
            Cli::Run(o) => *o,
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn basic_flags() {
        let o = parse(&["-O2", "-o", "out", "a.c", "b.c", "-c"]);
        assert_eq!(o.opt_level, 2);
        assert_eq!(o.output.as_deref(), Some("out"));
        assert_eq!(o.inputs, ["a.c", "b.c"]);
        assert_eq!(o.mode, Mode::Object);
        assert_eq!(o.pp.opt_level, 2);
    }

    #[test]
    fn attached_and_separate_values() {
        let o = parse(&["-Iinc", "-I", "inc2", "-DX=1", "-D", "Y", "-UZ", "-lm", "-L/opt", "-oout"]);
        assert_eq!(o.pp.include_dirs.len(), 2);
        assert_eq!(o.pp.defines.len(), 3);
        assert_eq!(o.link_args, ["-lm", "-L/opt"]);
        assert_eq!(o.output.as_deref(), Some("out"));
    }

    #[test]
    fn optimization_levels() {
        assert_eq!(parse(&["-O0"]).opt_level, 0);
        assert_eq!(parse(&["-O1"]).opt_level, 1);
        assert_eq!(parse(&["-O"]).opt_level, 1);
        assert_eq!(parse(&["-O3"]).opt_level, 2);
        assert_eq!(parse(&["-Os"]).opt_level, 2);
        assert_eq!(parse(&["-O2", "-O0"]).opt_level, 0); // last wins
    }

    #[test]
    fn warning_flags() {
        let o = parse(&["-Wall", "-Wno-unused-variable", "-Werror=return-type"]);
        use crate::diag::Warn;
        assert!(o.warn.is_enabled(Warn::Uninitialized));
        assert!(!o.warn.is_enabled(Warn::UnusedVariable));
        assert!(o.warn.is_error(Warn::ReturnType));
        assert!(parse_args(&["-Wbogus".to_string()]).is_err());
        let o = parse(&["-w"]);
        assert!(!o.warn.is_enabled(Warn::ReturnType));
    }

    #[test]
    fn pass_toggles_and_modes() {
        let o = parse(&["-fno-inline", "-fmem2reg", "--emit-ir"]);
        assert_eq!(o.pass_flags, [("inline".to_string(), false), ("mem2reg".to_string(), true)]);
        assert_eq!(o.mode, Mode::Ir);
        assert_eq!(parse(&["-E"]).mode, Mode::Preprocess);
        assert_eq!(parse(&["-S"]).mode, Mode::Asm);
        assert_eq!(parse(&["--emit-ast"]).mode, Mode::Ast);
    }

    #[test]
    fn misc() {
        assert!(matches!(parse_args(&["--version".to_string()]).unwrap(), Cli::Version));
        assert!(matches!(parse_args(&["--help".to_string()]).unwrap(), Cli::Help));
        assert!(parse_args(&["-bogus".to_string()]).is_err());
        assert!(parse_args(&["-o".to_string()]).is_err());
        let o = parse(&["--color=always", "--diagnostics-format=json", "-ferror-limit=5", "-std=c11", "-g"]);
        assert_eq!(o.color, ColorChoice::Always);
        assert!(o.diag_json);
        assert_eq!(o.error_limit, 5);
    }
}
