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
}
