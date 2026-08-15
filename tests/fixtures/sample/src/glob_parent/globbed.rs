//! `use super::*;` — the commonest glob in a `mod.rs`-shaped crate, and the one
//! stored as the literal string `"super"`, which is indexed under no module and
//! so resolved nothing.

use super::*;

pub fn uses_the_glob() -> usize {
    reaches_the_parent()
}
