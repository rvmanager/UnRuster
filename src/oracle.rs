//! A deliberately dumb second opinion on "who calls this".
//!
//! # Why a second implementation is the point
//!
//! Every other module here shares one call-site pipeline: `collect_sites` walks
//! the `syn` AST, `matches_target` decides what a query matches, `QueryMatcher`
//! resolves a query to an item. That pipeline is good, and when it is wrong,
//! everything downstream is wrong *identically* — `callers`, `co-call` and
//! `contract-drift` cannot contradict each other about a fact they compute the
//! same way.
//!
//! This is not hypothetical. Two real defects — a fn used only as `.map(f)`,
//! and a fn called only from inside a `row!(… => f(x))` arm — were invisible to
//! the AST path and reported as having *zero* callers. Neither was caught by
//! comparing usage commands to each other. Both were caught because `dead-code`
//! collects **raw identifiers** instead of call sites, and so disagreed: it
//! believed the fn was called while `callers` insisted nobody called it.
//!
//! That accident is worth making deliberate. This module is the same idea,
//! promoted to a first-class oracle: it reads tokens, knows nothing about
//! scopes, use-maps, receiver types or macros, and answers by brute force.
//!
//! # The contract, and why it is one-sided
//!
//! The oracle **over-approximates**. `foo` here means "the token `foo` is
//! followed by `(`, somewhere in this file" — which is true of a method on an
//! unrelated type, a local closure, and a fn from another crate. So:
//!
//! - `callers X` ⊆ `oracle X` is a real invariant. A site the AST path reports
//!   and the token scan cannot see means the AST path invented it.
//! - `oracle X` ⊆ `callers X` is **not** an invariant, and must never be
//!   asserted. The gap is where the interesting question lives: every site the
//!   oracle sees and `callers` drops should be attributable to a rule the tool
//!   states out loud — recursion, visibility, a homonym it filtered — and a gap
//!   with no such reason is a blind spot.
//!
//! # Keeping it honest
//!
//! The value is entirely in the independence. If this ever starts calling
//! `collect_sites`, or reusing `matches_target`, or parsing with `syn`, it
//! stops being evidence and becomes a slower copy of the thing it checks.
//! It is allowed to be crude. It is not allowed to share a code path.

use std::collections::BTreeMap;
use std::path::Path;

/// A file the oracle read: its path relative to the scan root, and its text.
pub type SourceFile = (String, String);

/// A file it could not read: the path, and why.
pub type Unreadable = (String, String);

/// One token-level sighting of `name(` in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub file: String,
    pub line: usize,
    /// The call was written `a::b::name(`, and this is `a::b` — empty for a
    /// bare `name(`. Enough to tell `std::fs::write` from `baseline::write`
    /// without resolving anything.
    pub qualifier: String,
    /// The token before the name was `.`, so this is a method call.
    pub method: bool,
}

/// Every place `name` is used as a callee or handed over as a value, found by
/// reading characters.
///
/// Deliberately not a parse. A parser would agree with the AST path about what
/// a call is, and agreement between two implementations of the same idea proves
/// only that the idea was implemented twice.
pub fn sightings(files: &[SourceFile], name: &str) -> Vec<Sighting> {
    let mut out = Vec::new();
    for (path, src) in files {
        for (i, raw) in src.lines().enumerate() {
            let line = strip_noise(raw);
            let bytes = line.as_bytes();
            let mut from = 0usize;
            while let Some(rel) = line[from..].find(name) {
                let at = from + rel;
                from = at + name.len();
                if !is_whole_word(bytes, at, name.len()) {
                    continue;
                }
                let after = line[at + name.len()..].trim_start();
                // A callee is followed by `(`; a fn handed to a combinator is
                // followed by `)` or `,`. Both are uses; neither is a
                // definition, which `fn name` catches below.
                let used = after.starts_with('(') || after.starts_with(')') || after.starts_with(',');
                if !used || defines_here(&line, at) {
                    continue;
                }
                let (qualifier, method) = prefix_of(&line, at);
                out.push(Sighting {
                    file: path.clone(),
                    line: i + 1,
                    qualifier,
                    method,
                });
            }
        }
    }
    out
}

/// Group sightings by the qualifier they were written with, so a caller can see
/// at a glance that 53 of 62 said `std::fs` and 4 said nothing.
pub fn by_qualifier(s: &[Sighting]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for x in s {
        let k = if x.method {
            ".".to_string()
        } else if x.qualifier.is_empty() {
            "<bare>".to_string()
        } else {
            x.qualifier.clone()
        };
        *m.entry(k).or_insert(0) += 1;
    }
    m
}

/// Read every `.rs` file under `root` as raw text, plus the paths it could not
/// read.
///
/// Its own walk rather than `parse::scan`, on purpose: sharing the file set
/// would make a file the parser skipped invisible to the oracle too, and "which
/// files were read" is exactly the kind of thing the two should be able to
/// disagree about.
pub fn read_tree(root: &Path, exclude: &[String]) -> (Vec<SourceFile>, Vec<Unreadable>) {
    let mut out = Vec::new();
    let mut unreadable: Vec<Unreadable> = Vec::new();
    let walker = ignore::WalkBuilder::new(root).hidden(false).build();
    for entry in walker.flatten() {
        let p = entry.path();
        if p.extension().map(|e| e != "rs").unwrap_or(true) {
            continue;
        }
        let rel = p.strip_prefix(root).unwrap_or(p).display().to_string();
        if exclude.iter().any(|g| glob_hit(g, &rel)) {
            continue;
        }
        // A file this cannot read is a hole in the corpus, and the whole value
        // here is that both sides read the *same* corpus. Swallowing the error
        // would make every comparison over that file quietly one-sided.
        // Keeping *which* error, not just that there was one: "not valid UTF-8"
        // and "permission denied" call for different responses, and a reader
        // told only that a file was skipped cannot tell them apart.
        match std::fs::read_to_string(p) {
            Ok(src) => out.push((rel, src)),
            Err(e) => unreadable.push((rel, e.to_string())),
        }
    }
    (out, unreadable)
}

/// `fixtures/**`-style matching, kept to the one shape the callers use rather
/// than pulling in a glob dependency this module does not otherwise need.
fn glob_hit(pattern: &str, path: &str) -> bool {
    match pattern.strip_suffix("/**") {
        Some(prefix) => path.starts_with(prefix),
        None => path == pattern,
    }
}

/// Drop line comments and string literals, the two places a name can appear
/// looking exactly like a call while being nothing of the kind.
fn strip_noise(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_str = false;
    let mut escaped = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            out.push(' ');
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(' ');
            }
            '/' if chars.peek() == Some(&'/') => break,
            _ => out.push(c),
        }
    }
    out
}

/// The token must not be part of a longer identifier: `run` must not match
/// inside `run_counted` or `pre_run`.
fn is_whole_word(bytes: &[u8], at: usize, len: usize) -> bool {
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    if at > 0 && ident(bytes[at - 1]) {
        return false;
    }
    match bytes.get(at + len) {
        Some(&b) => !ident(b),
        None => true,
    }
}

/// `fn name(` is the definition, not a use of it.
fn defines_here(line: &str, at: usize) -> bool {
    line[..at].trim_end().ends_with("fn")
}

/// What was written immediately before the name: a `::` path, or a `.`.
fn prefix_of(line: &str, at: usize) -> (String, bool) {
    let before = line[..at].trim_end();
    if before.ends_with('.') {
        return (String::new(), true);
    }
    let Some(head) = before.strip_suffix("::") else {
        return (String::new(), false);
    };
    let start = head
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
        .map(|i| i + 1)
        .unwrap_or(0);
    (head[start..].to_string(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(src: &str) -> Vec<(String, String)> {
        vec![("a.rs".to_string(), src.to_string())]
    }

    #[test]
    fn a_bare_call_and_a_qualified_one_are_told_apart() {
        let s = sightings(
            &f("fn go() { write(1); std::fs::write(p, b); baseline::write(x); }"),
            "write",
        );
        let q = by_qualifier(&s);
        assert_eq!(q.get("<bare>"), Some(&1));
        assert_eq!(q.get("std::fs"), Some(&1));
        assert_eq!(q.get("baseline"), Some(&1));
    }

    #[test]
    fn a_method_call_is_marked_as_one() {
        let s = sightings(&f("fn go(v: &Vec<u8>) { v.len(); }"), "len");
        assert_eq!(s.len(), 1);
        assert!(s[0].method);
    }

    /// The two shapes the AST path was blind to, and the reason this exists.
    #[test]
    fn a_fn_reference_and_a_macro_arm_are_both_sightings() {
        assert_eq!(sightings(&f("fn go() { xs.map(widen) }"), "widen").len(), 1);
        assert_eq!(
            sightings(&f(r#"fn go() { row!(out, "at" => at(d, r)) }"#), "at").len(),
            1
        );
    }

    #[test]
    fn the_definition_is_not_a_use_of_itself() {
        assert!(sightings(&f("fn score(a: u32) -> u32 { a }"), "score").is_empty());
    }

    #[test]
    fn a_longer_identifier_does_not_match() {
        assert!(sightings(&f("fn go() { run_counted(1); prerun(2); }"), "run").is_empty());
    }

    #[test]
    fn comments_and_strings_are_not_code() {
        let s = sightings(
            &f("fn go() { // write(1)\n }\nfn h() { let s = \"write(2)\"; }"),
            "write",
        );
        assert!(s.is_empty(), "{:?}", s);
    }
}
