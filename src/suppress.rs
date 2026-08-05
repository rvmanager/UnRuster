//! In-source finding waivers: `// unruster: ok — <reason>`.
//!
//! A run of this tool over a mature codebase re-reports every site a human
//! already judged intentional. Without a way to record that judgment, round
//! two re-litigates round one — the single most expensive failure mode when an
//! agent works the audit loop across sessions.
//!
//! The waiver lives with the code, mirroring the existing `/// unruster:
//! sealed` contract marker: there is no config file, no path list, nothing to
//! keep in sync with a rename. Two placements are honoured:
//!
//! ```ignore
//! let _ = writeln!(out, "…");  // unruster: ok — writing into a String is infallible
//!
//! // unruster: ok — the poisoned-lock path is handled by the caller
//! let guard = m.lock().ok();
//! ```
//!
//! A trailing comment waives its own line; a standalone comment waives the
//! next non-blank, non-comment line. Both forms require a reason after the
//! marker — a bare `// unruster: ok` is honoured but noted, because a waiver
//! nobody can evaluate is worse than the finding.
//!
//! Scanning is textual on purpose: comments are not part of the AST syn hands
//! back, and a line-oriented scan works uniformly for every check without each
//! scanner having to thread span information it doesn't otherwise need.

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The marker that opens a waiver comment. Matched after `//`, with any amount
/// of surrounding whitespace.
const MARKER: &str = "unruster:";

/// Waived `(file, line)` sites, plus a count of reason-less waivers for the
/// summary line.
#[derive(Debug, Default)]
pub struct Suppressions {
    /// Display path → waived line numbers (1-indexed).
    by_file: HashMap<String, HashSet<usize>>,
    /// Waivers with no reason text after the marker.
    pub unexplained: usize,
}

impl Suppressions {
    pub fn is_empty(&self) -> bool {
        self.by_file.is_empty()
    }

    /// Total waived sites across all files.
    pub fn len(&self) -> usize {
        self.by_file.values().map(HashSet::len).sum()
    }

    pub fn contains(&self, file: &str, line: usize) -> bool {
        self.by_file
            .get(file)
            .map(|s| s.contains(&line))
            .unwrap_or(false)
    }

    fn insert(&mut self, file: &str, line: usize) {
        self.by_file
            .entry(file.to_string())
            .or_default()
            .insert(line);
    }
}

/// Is this waiver word one we honour? `ok` and `ignore` both waive; `sealed`
/// is the enum contract marker handled elsewhere and must NOT waive anything.
fn is_waiver_word(w: &str) -> bool {
    matches!(w, "ok" | "ignore" | "allow")
}

/// Parse one source line's trailing comment, if any. Returns
/// `Some(has_reason)` when the comment is a waiver.
///
/// String-literal awareness matters: `println!("// unruster: ok")` is data, not
/// a waiver, and a codebase that documents this tool would otherwise waive
/// random lines of itself.
fn waiver_in_line(line: &str) -> Option<bool> {
    let comment = find_line_comment(line)?;
    let rest = comment.trim_start();
    let after = rest.strip_prefix(MARKER)?.trim_start();
    let mut words = after.split_whitespace();
    let word = words.next()?;
    if !is_waiver_word(word) {
        return None;
    }
    // A reason is any non-empty text after the waiver word, ignoring the
    // em-dash / hyphen / colon that usually introduces it.
    let reason: String = words.collect::<Vec<_>>().join(" ");
    let reason = reason.trim_start_matches(['—', '-', ':']).trim();
    Some(!reason.is_empty())
}

/// The text after the first `//` that is not inside a string or char literal.
fn find_line_comment(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut in_char = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_str || in_char => {
                i += 2;
                continue;
            }
            b'"' if !in_char => in_str = !in_str,
            b'\'' if !in_str => in_char = !in_char,
            b'/' if !in_str && !in_char && bytes.get(i + 1) == Some(&b'/') => {
                return Some(&line[i + 2..]);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// True for a line that carries no code — blank, or comment-only. A standalone
/// waiver comment applies to the next line that has code.
fn is_codeless(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with("//")
}

/// Collect every waiver in `files`. `display` maps a scanned path to the same
/// string the row output uses, so `contains` lookups match without
/// canonicalizing on the hot path.
pub fn scan(files: &[(String, &Path)]) -> Suppressions {
    let mut out = Suppressions::default();
    for (display, path) in files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let lines: Vec<&str> = src.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(has_reason) = waiver_in_line(line) else {
                continue;
            };
            if !has_reason {
                out.unexplained += 1;
            }
            let target = if is_codeless(line) {
                // Standalone comment: waive the next line that has code.
                lines[i + 1..]
                    .iter()
                    .position(|l| !is_codeless(l))
                    .map(|off| i + 1 + off)
            } else {
                Some(i)
            };
            if let Some(t) = target {
                out.insert(display, t + 1); // 1-indexed line numbers
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_waiver_applies_to_its_own_line() {
        assert_eq!(waiver_in_line("let _ = f(); // unruster: ok — infallible"), Some(true));
    }

    #[test]
    fn waiver_without_a_reason_is_honoured_but_reported() {
        assert_eq!(waiver_in_line("let _ = f(); // unruster: ok"), Some(false));
        assert_eq!(waiver_in_line("let _ = f(); // unruster: ok —"), Some(false));
    }

    #[test]
    fn sealed_marker_is_not_a_waiver() {
        // `/// unruster: sealed` is the enum contract marker. If it waived
        // findings, marking an enum sealed would hide the very rows the marker
        // exists to escalate.
        assert_eq!(waiver_in_line("/// unruster: sealed"), None);
    }

    #[test]
    fn marker_inside_a_string_literal_is_not_a_waiver() {
        assert_eq!(waiver_in_line(r#"println!("// unruster: ok");"#), None);
        assert_eq!(waiver_in_line(r#"let s = "a//b"; // unruster: ok — real"#), Some(true));
    }

    #[test]
    fn unrelated_comments_are_ignored() {
        assert_eq!(waiver_in_line("let x = 1; // just a note"), None);
        assert_eq!(waiver_in_line("let x = 1;"), None);
    }
}
