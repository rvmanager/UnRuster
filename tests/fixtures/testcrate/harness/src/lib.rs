//! A test-support crate. Nothing in here is marked `#[cfg(test)]` and nothing
//! lives under `tests/` — it is ordinary library code, because a crate pulled
//! in from `[dev-dependencies]` is compiled normally. Only the *package name*
//! says what it is for, which is why the scope rule reads the manifest.
//!
//! The body deliberately trips `error-swallows`: on a real workspace this
//! exact shape (`env::var(…).ok()` in a test harness) was reported as a
//! production defect and twice landed next to an unrelated fix.

pub struct Harness {
    pub root: String,
}

impl Harness {
    pub fn new() -> Self {
        let root = std::env::var("TEST_ROOT").unwrap_or_else(|_| "/tmp".to_string());
        let _ = std::fs::create_dir_all(&root);
        Harness { root }
    }

    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}
