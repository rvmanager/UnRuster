//! The parent half of a glob import, laid out the way `svggen`'s `geom` is:
//! a `mod.rs` holding the shared helpers and a submodule that reaches them
//! through `use super::*;`.

pub mod globbed;

/// Spelled bare inside `globbed`, so a reader writes
/// `glob_parent::globbed::reaches_the_parent` — the module they are looking at,
/// plus the name they can see.
pub fn reaches_the_parent() -> usize {
    7
}

/// One of two same-named fns, to pin which copy a qualified miss suggests.
pub fn twinned() -> usize {
    1
}

pub mod nested {
    pub fn calls_it() -> usize {
        super::twinned()
    }
}
