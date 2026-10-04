//! The libc headers bundled into the compiler binary.
//!
//! glibc's own headers rely on dozens of GNU extensions, so Cinder ships small
//! headers that declare glibc's ABI (types, prototypes, constants) using only
//! standard C11. They are embedded with `include_str!`, which keeps behaviour
//! identical on every host and means the compiler is a single file.

macro_rules! bundle {
    ($($name:literal),* $(,)?) => {
        /// Contents of a bundled header by its `#include <...>` name.
        pub fn bundled(name: &str) -> Option<&'static str> {
            match name {
                $($name => Some(include_str!(concat!("../include/", $name))),)*
                _ => None,
            }
        }

        /// All bundled header names (for docs and tests).
        pub const BUNDLED_NAMES: &[&str] = &[$($name),*];
    };
}

bundle! {
    "assert.h", "ctype.h", "errno.h", "float.h", "inttypes.h", "iso646.h", "limits.h",
    "math.h", "stdalign.h", "stdarg.h", "stdbool.h", "stddef.h", "stdint.h", "stdio.h",
    "stdlib.h", "stdnoreturn.h", "string.h", "time.h", "unistd.h", "sys/types.h",
}

/// A note for "header not found": says *why* for well-known headers Cinder cannot provide (parallel
/// programming runtimes), and otherwise, for `<...>` includes, lists what it does bundle.
pub fn missing_header_hint(name: &str, angle: bool) -> Option<String> {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base {
        "omp.h" => Some(
            "OpenMP is not supported: Cinder has no omp.h and does not implement '#pragma omp' (build the program without OpenMP)"
                .into(),
        ),
        "mpi.h" | "mpio.h" => Some("MPI is not supported: Cinder has no MPI headers or runtime".into()),
        "pthread.h" | "semaphore.h" | "threads.h" | "stdatomic.h" => {
            Some("threads and atomics are not supported yet (no pthread.h, threads.h or stdatomic.h)".into())
        }
        _ if angle => Some(format!("Cinder bundles only these standard headers: {}", BUNDLED_NAMES.join(", "))),
        _ => None,
    }
}

/// Names answered by `__has_builtin`.
pub fn is_known_builtin(name: &str) -> bool {
    matches!(
        name,
        "__builtin_va_start"
            | "__builtin_va_end"
            | "__builtin_va_arg"
            | "__builtin_va_copy"
            | "__builtin_offsetof"
            | "__builtin_expect"
            | "__builtin_unreachable"
            | "__builtin_trap"
            | "__builtin_huge_val"
            | "__builtin_inf"
            | "__builtin_inff"
            | "__builtin_nan"
            | "__builtin_nanf"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_header_resolves() {
        for n in BUNDLED_NAMES {
            assert!(bundled(n).is_some(), "{n}");
        }
        assert!(bundled("nonexistent.h").is_none());
    }

    #[test]
    fn missing_header_hints() {
        assert!(missing_header_hint("omp.h", true).unwrap().contains("OpenMP is not supported"));
        assert!(missing_header_hint("mpi.h", false).unwrap().contains("MPI is not supported"));
        assert!(missing_header_hint("sys/mpi.h", true).unwrap().contains("MPI is not supported"));
        assert!(missing_header_hint("pthread.h", true).unwrap().contains("threads"));
        let generic = missing_header_hint("wchar.h", true).unwrap();
        assert!(generic.contains("stdio.h") && generic.contains("sys/types.h"));
        assert_eq!(missing_header_hint("mine.h", false), None);
    }
}
