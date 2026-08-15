//! The production half of the workspace. Scanned under every scope, so the
//! run has something to report and the scope note has somewhere to appear.

pub fn load(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}
