//! Server configuration, read from environment variables.
//!
//! | variable | default | meaning |
//! |----------|---------|---------|
//! | `PORT` | 8080 | listen port (Render sets it) |
//! | `BIND` | 0.0.0.0 | listen address |
//! | `CINDER_BIN` | `cinder` next to this executable, else `cinder` on `PATH` | the compiler |
//! | `WEB_DIR` | `web` | static files of the playground |
//! | `WORK_DIR` | system temp dir | scratch space for compiles and runs |
//! | `MAX_CONCURRENT` | 4 | simultaneous compiles + runs |
//! | `QUEUE_WAIT_SECS` | 8 | how long a request waits for a free slot before a 503 |
//! | `COMPILE_TIMEOUT_SECS` | 10 | wall-clock limit for one compile (CPU limit is the same) |
//! | `COMPILE_MEMORY_MB` | 256 | resident-memory ceiling of one compile (compiler, assembler and linker together) |
//! | `RUN_CPU_SECS` | 2 | CPU-time limit of a program |
//! | `RUN_WALL_SECS` | 5 | wall-clock limit of a program |
//! | `RUN_MEMORY_MB` | 128 | address-space limit of each process of a program, and ceiling for the resident memory of all of them together |
//! | `RUN_PROCESSES` | 16 | process/thread limit of a program |
//! | `OUTPUT_LIMIT_BYTES` | 65536 | stdout and stderr cap (each) |
//! | `MAX_CODE_BYTES` | 65536 | largest accepted source |
//! | `MAX_STDIN_BYTES` | 65536 | largest accepted stdin |
//! | `RATE_RUN_PER_MIN` | 20 | `/api/run` requests per client per minute (burst 5) |
//! | `RATE_COMPILE_PER_MIN` | 60 | `/api/compile` requests per client per minute (burst 10) |
//! | `SANDBOX` | auto | `auto`, `require` (refuse to run code without the full sandbox) or `off` (dangerous; local development only) |
//! | `RUN_UID_BASE` | 20000 | numeric uid of slot 0; slot *n* uses `base + n` |
//! | `CLIENT_IP_HEADER` | unset | a header the edge proxy overwrites with the real client address (`CF-Connecting-IP` behind Cloudflare); wins over `X-Forwarded-For` when present |
//! | `TRUST_PROXY_HOPS` | 1 | reverse proxies in front of the server: the client address is the *n*-th entry from the right of `X-Forwarded-For` (0 ignores the header) |

use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SandboxMode {
    /// Use chroot + uid drop when running as root, otherwise rlimits + seccomp only.
    Auto,
    /// Refuse to run programs unless the full sandbox (chroot + uid drop) is available.
    Require,
    /// No isolation at all. Only for local development.
    Off,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub bind: String,
    pub cinder_bin: PathBuf,
    pub web_dir: PathBuf,
    pub work_dir: PathBuf,
    pub max_concurrent: usize,
    pub queue_wait_secs: u64,
    pub compile_secs: u64,
    pub compile_mem_mb: u64,
    pub run_cpu_secs: u64,
    pub run_wall_secs: u64,
    pub run_mem_mb: u64,
    pub run_processes: u64,
    pub output_limit: usize,
    pub max_code_bytes: usize,
    pub max_stdin_bytes: usize,
    pub rate_run_per_min: u32,
    pub rate_compile_per_min: u32,
    pub sandbox: SandboxMode,
    pub uid_base: u32,
    pub trust_proxy_hops: usize,
    pub client_ip_header: Option<String>,
}

fn get<T: std::str::FromStr>(vars: &dyn Fn(&str) -> Option<String>, key: &str, default: T) -> T {
    vars(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Config {
        Config::from_vars(&|k| std::env::var(k).ok())
    }

    pub fn from_vars(vars: &dyn Fn(&str) -> Option<String>) -> Config {
        let sandbox = match vars("SANDBOX").as_deref().map(str::trim) {
            Some("off") | Some("none") => SandboxMode::Off,
            Some("require") | Some("required") | Some("on") => SandboxMode::Require,
            _ => SandboxMode::Auto,
        };
        let cinder_bin = match vars("CINDER_BIN") {
            Some(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => {
                let sibling = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("cinder")));
                match sibling {
                    Some(p) if p.exists() => p,
                    _ => PathBuf::from("cinder"),
                }
            }
        };
        Config {
            port: get(vars, "PORT", 8080),
            bind: vars("BIND").unwrap_or_else(|| "0.0.0.0".to_string()),
            cinder_bin,
            web_dir: vars("WEB_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("web")),
            work_dir: vars("WORK_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir),
            max_concurrent: get::<usize>(vars, "MAX_CONCURRENT", 4).clamp(1, 64),
            queue_wait_secs: get(vars, "QUEUE_WAIT_SECS", 8),
            compile_secs: get(vars, "COMPILE_TIMEOUT_SECS", 10),
            compile_mem_mb: get(vars, "COMPILE_MEMORY_MB", 256),
            run_cpu_secs: get(vars, "RUN_CPU_SECS", 2),
            run_wall_secs: get(vars, "RUN_WALL_SECS", 5),
            run_mem_mb: get(vars, "RUN_MEMORY_MB", 128),
            run_processes: get(vars, "RUN_PROCESSES", 16),
            output_limit: get(vars, "OUTPUT_LIMIT_BYTES", 65536),
            max_code_bytes: get(vars, "MAX_CODE_BYTES", 65536),
            max_stdin_bytes: get(vars, "MAX_STDIN_BYTES", 65536),
            rate_run_per_min: get(vars, "RATE_RUN_PER_MIN", 20),
            rate_compile_per_min: get(vars, "RATE_COMPILE_PER_MIN", 60),
            sandbox,
            uid_base: get(vars, "RUN_UID_BASE", 20000),
            trust_proxy_hops: get(vars, "TRUST_PROXY_HOPS", 1),
            client_ip_header: vars("CLIENT_IP_HEADER").map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_vars(&move |k| m.get(k).cloned())
    }

    #[test]
    fn defaults() {
        let c = cfg(&[]);
        assert_eq!((c.port, c.max_concurrent, c.run_cpu_secs, c.run_mem_mb), (8080, 4, 2, 128));
        assert_eq!(c.sandbox, SandboxMode::Auto);
        assert_eq!(c.output_limit, 65536);
        assert_eq!(c.client_ip_header, None);
        assert_eq!(
            cfg(&[("CLIENT_IP_HEADER", " CF-Connecting-IP ")]).client_ip_header.as_deref(),
            Some("cf-connecting-ip")
        );
        assert_eq!(cfg(&[("CLIENT_IP_HEADER", "  ")]).client_ip_header, None);
    }

    #[test]
    fn overrides_and_garbage() {
        let c = cfg(&[
            ("PORT", "3000"),
            ("RUN_CPU_SECS", "1"),
            ("SANDBOX", "require"),
            ("RUN_MEMORY_MB", "lots"),
            ("MAX_CONCURRENT", "0"),
        ]);
        assert_eq!(c.port, 3000);
        assert_eq!(c.run_cpu_secs, 1);
        assert_eq!(c.sandbox, SandboxMode::Require);
        assert_eq!(c.run_mem_mb, 128, "unparsable values fall back to the default");
        assert_eq!(c.max_concurrent, 1, "at least one slot");
        assert_eq!(cfg(&[("SANDBOX", "off")]).sandbox, SandboxMode::Off);
    }
}
