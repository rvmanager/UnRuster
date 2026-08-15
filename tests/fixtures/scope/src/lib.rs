//! Production code. The two sibling files are test-named and must be excluded
//! under `--scope production` even though nothing marks them `#[cfg(test)]`
//! from inside — the gate lives in the parent's `mod` declaration.
pub mod foo_tests;
pub mod tests;

pub fn production_path() {
    let _ = std::fs::read("real");
}
