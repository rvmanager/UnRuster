//! A waiver naming a check that does not exist. It waives nothing, and without
//! the unknown-check note it does so silently.
// unruster: ok(divergance/Foo::Bar) 2026-08-06 — misspelled `divergence`
pub fn f(n: u64) -> u32 {
    n as u32
}
