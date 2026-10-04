//! Global string interner. Identifiers and keywords are interned once so that
//! comparisons, macro-table lookups and hide-set operations are `u32` compares.
//!
//! Interned strings are leaked: the compiler is a short-lived process and the
//! total identifier text is bounded by the size of the input.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Symbol(u32);

#[derive(Default)]
struct Interner {
    map: HashMap<&'static str, u32>,
    strs: Vec<&'static str>,
}

thread_local! {
    static INTERNER: RefCell<Interner> = RefCell::new(Interner::default());
}

impl Symbol {
    pub fn new(s: &str) -> Symbol {
        INTERNER.with(|i| {
            let mut i = i.borrow_mut();
            if let Some(&id) = i.map.get(s) {
                return Symbol(id);
            }
            let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
            let id = i.strs.len() as u32;
            i.strs.push(leaked);
            i.map.insert(leaked, id);
            Symbol(id)
        })
    }

    pub fn as_str(self) -> &'static str {
        INTERNER.with(|i| i.borrow().strs[self.0 as usize])
    }

    pub fn index(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`", self.as_str())
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for Symbol {
    fn from(s: &str) -> Symbol {
        Symbol::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_stable() {
        let a = Symbol::new("foo");
        let b = Symbol::new("foo");
        let c = Symbol::new("bar");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.as_str(), "foo");
        assert_eq!(c.as_str(), "bar");
    }
}
