//! In-source finding waivers: `// unruster: ok(<check>[/<key>]) <date> — <reason>`.
//!
//! A run of this tool over a mature codebase re-reports every site a human
//! already judged intentional. Without a way to record that judgment, round
//! two re-litigates round one — the single most expensive failure mode when an
//! agent works the audit loop across sessions.
//!
//! The waiver lives with the code, mirroring the existing `/// unruster:
//! sealed` contract marker: there is no config file, no path list, nothing to
//! keep in sync with a rename.
//!
//! ```ignore
//! // unruster: ok(divergence/NodeContent::Group) 2026-08-06 — Group is a
//! // structural child edge, not a consumer reference.
//! impl NodeArena { … }                       // ← item scope: the whole impl
//!
//! let _ = f();  // unruster: ok(error-swallows) 2026-08-06 — Drop guard
//! ```
//!
//! # Grammar
//!
//! ```text
//! waiver = "unruster:" verb [ "(" check [ "/" key ] ")" ] [ date ] [ dash ] reason
//! verb   = "ok" | "ignore" | "allow"        ; "sealed" is a contract, not a waiver
//! date   = YYYY "-" MM "-" DD               ; when the judgment was last confirmed
//! dash   = "—" | "--" | "-" | ":"           ; cosmetic
//! ```
//!
//! The head (verb, check, key, date) always lives on the opening line, so
//! `grep -rn "unruster: ok("` returns every machine-readable field of every
//! waiver — including the multi-line ones.
//!
//! # Scope
//!
//! * A **trailing** comment waives its own line.
//! * A **standalone** comment whose next code line begins an item (fn, impl,
//!   mod, enum, …) waives that item's entire span, nested items included —
//!   the `#[allow(…)]` inheritance model, and the reason one comment can
//!   retire a whole family of sibling findings.
//! * Any other standalone comment waives the next line that has code.
//!
//! # Matching
//!
//! A waiver suppresses a finding when the line is in range **and** the check
//! matches (an unqualified `ok` matches every check — the legacy spelling)
//! **and** the key matches (an unkeyed waiver matches every key; a keyed
//! waiver never matches an unkeyed finding, so it can't over-suppress).
//!
//! # Reflow tolerance
//!
//! Reasons continue onto immediately-following comment-only lines. This is
//! parse tolerance, not syntax: a human — or `rustfmt` with `wrap_comments` —
//! who breaks a long waiver across lines gets the same result as before the
//! break. Absorption stops at a blank line, a code line, a doc comment, or a
//! line opening with a screaming-case convention (`TODO`, `SAFETY`, …) so an
//! unrelated neighbouring comment is never swallowed.
//!
//! Scanning is textual for the comments (they are not part of the AST syn
//! hands back) and AST-driven for item spans.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};

use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::parse::{display_path, ParsedFile};

/// The marker that opens a waiver comment. Matched after `//`, with any amount
/// of surrounding whitespace.
const MARKER: &str = "unruster:";

/// Comment openers that end a greedy reason continuation. Matched
/// case-sensitively in upper case on purpose: `// NOTE: …` is a convention
/// marker and must not be absorbed, while `// note that the cast is …` is
/// ordinary prose and should be.
const STOP_WORDS: &[&str] = &[
    "TODO",
    "FIXME",
    "SAFETY",
    "NOTE",
    "HACK",
    "XXX",
    "PANIC",
    "INVARIANT",
    "PERF",
    "WARN",
];

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// A calendar date, to the day. Hand-rolled rather than pulling in `chrono`:
/// the only operations needed are parse, render, and "how many days ago",
/// which is one well-known algorithm each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub y: i32,
    pub m: u32,
    pub d: u32,
}

impl std::fmt::Display for Date {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.y, self.m, self.d)
    }
}

impl Date {
    /// Parse `YYYY-MM-DD`. Returns `None` for anything else, which is how the
    /// grammar distinguishes a date token from the first word of a reason.
    pub fn parse(s: &str) -> Option<Date> {
        let b = s.as_bytes();
        if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
            return None;
        }
        if !b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                true
            } else {
                c.is_ascii_digit()
            }
        }) {
            return None;
        }
        let y: i32 = s[0..4].parse().ok()?;
        let m: u32 = s[5..7].parse().ok()?;
        let d: u32 = s[8..10].parse().ok()?;
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1970..=9999).contains(&y) {
            return None;
        }
        Some(Date { y, m, d })
    }

    /// Today, from the system clock. The only non-deterministic input in the
    /// tool, which is why every command that consumes it also accepts
    /// `--today` (the test suite would otherwise drift).
    pub fn today() -> Date {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Date::from_days(secs.div_euclid(86_400))
    }

    /// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
    pub fn to_days(self) -> i64 {
        let y = i64::from(self.y) - i64::from(self.m <= 2);
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = i64::from(if self.m > 2 { self.m - 3 } else { self.m + 9 });
        let doy = (153 * mp + 2) / 5 + i64::from(self.d) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// Inverse of [`Self::to_days`] (`civil_from_days`).
    pub fn from_days(z: i64) -> Date {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        Date {
            y: (y + i64::from(m <= 2)) as i32,
            m: m as u32,
            d: d as u32,
        }
    }

    /// Whole days between `self` and `today`. Negative for a future date —
    /// reported rather than clamped, since a date ahead of the clock means
    /// someone typo'd the year and should hear about it.
    pub fn age_days(self, today: Date) -> i64 {
        today.to_days() - self.to_days()
    }
}

// ---------------------------------------------------------------------------
// Waivers
// ---------------------------------------------------------------------------

/// What a waiver covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// One line — a trailing comment, or a standalone comment above a
    /// statement.
    Site,
    /// A whole item and everything lexically inside it.
    Item,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Site => "site",
            Scope::Item => "item",
        }
    }
}

/// One parsed `// unruster: ok(…)` comment.
#[derive(Debug)]
pub struct Waiver {
    pub file: String,
    /// 1-indexed line of the opening comment — where the head lives, and the
    /// line `waivers --remove` rewrites or deletes.
    pub comment_line: usize,
    /// 1-indexed last line of the comment block (equal to `comment_line`
    /// unless a reason continued).
    pub comment_end: usize,
    /// Inclusive 1-indexed line range this waiver suppresses.
    pub covers: (usize, usize),
    pub scope: Scope,
    /// `None` for the legacy unqualified `// unruster: ok` — matches every
    /// check, which is exactly why the qualified form exists.
    pub check: Option<String>,
    pub key: Option<String>,
    pub date: Option<Date>,
    pub reason: String,
    /// The comment sits after code on its line.
    pub trailing: bool,
    /// Byte offset of `//` within `comment_line`.
    pub comment_col: usize,
    /// Byte offset within `comment_line` where the reason text begins, so
    /// `--upgrade` can rewrite the head without touching the prose.
    pub reason_col: usize,
    /// Findings this waiver suppressed during the run.
    hits: Cell<usize>,
    /// Which checks those hits came from — the evidence `--upgrade` uses to
    /// qualify a legacy waiver.
    hit_checks: RefCell<BTreeSet<String>>,
}

impl Waiver {
    pub fn hits(&self) -> usize {
        self.hits.get()
    }

    pub fn hit_checks(&self) -> Vec<String> {
        self.hit_checks.borrow().iter().cloned().collect()
    }

    /// Legacy = written before the grammar carried a check name. Honoured
    /// forever (it waives everything on its line), reported so it can be
    /// upgraded.
    pub fn is_legacy(&self) -> bool {
        self.check.is_none()
    }

}

/// A finding offered up for suppression matching.
#[derive(Clone, Copy, Debug)]
pub struct Site<'a> {
    pub file: &'a str,
    pub line: usize,
    /// Check-specific refinement — `NodeContent::Group` for divergence, the
    /// cast class for casts, the swallow kind for error-swallows. `None` when
    /// the check has no sub-key, in which case only an unkeyed waiver matches.
    pub key: Option<&'a str>,
}

impl<'a> Site<'a> {
    pub fn new(file: &'a str, line: usize) -> Self {
        Site {
            file,
            line,
            key: None,
        }
    }

    pub fn keyed(file: &'a str, line: usize, key: &'a str) -> Self {
        Site {
            file,
            line,
            key: Some(key),
        }
    }
}

/// Every waiver in the scanned tree, indexed by file.
#[derive(Debug, Default)]
pub struct Suppressions {
    waivers: Vec<Waiver>,
    by_file: HashMap<String, Vec<usize>>,
    /// Waivers with no reason text after the head.
    pub unexplained: usize,
}

impl Suppressions {
    pub fn is_empty(&self) -> bool {
        self.waivers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.waivers.len()
    }

    pub fn all(&self) -> &[Waiver] {
        &self.waivers
    }

    /// Waivers written in the pre-grammar spelling (no check name).
    pub fn legacy_count(&self) -> usize {
        self.waivers.iter().filter(|w| w.is_legacy()).count()
    }

    /// Does a waiver cover this finding? Records the hit, so
    /// `unruster waivers` can report what each waiver is actually buying and
    /// flag the ones that no longer suppress anything.
    pub fn matches(&self, check: &str, site: Site<'_>) -> bool {
        let Some(idxs) = self.by_file.get(site.file) else {
            return false;
        };
        let mut hit = false;
        for &i in idxs {
            let w = &self.waivers[i];
            if site.line < w.covers.0 || site.line > w.covers.1 {
                continue;
            }
            if !check_matches(w.check.as_deref(), check) {
                continue;
            }
            if !key_matches(w.key.as_deref(), site.key) {
                continue;
            }
            w.hits.set(w.hits.get() + 1);
            w.hit_checks.borrow_mut().insert(check.to_string());
            hit = true;
        }
        hit
    }

    fn push(&mut self, w: Waiver) {
        self.by_file
            .entry(w.file.clone())
            .or_default()
            .push(self.waivers.len());
        self.waivers.push(w);
    }
}

/// An unqualified waiver matches every check — that is the legacy contract and
/// breaking it would silently un-waive judgments already recorded.
fn check_matches(waiver: Option<&str>, check: &str) -> bool {
    waiver.is_none_or(|w| w == check)
}

/// An unkeyed waiver matches any key. A keyed waiver requires a keyed finding
/// and matches either the full key or its last `::` segment, so
/// `ok(divergence/Group)` and `ok(divergence/NodeContent::Group)` both work.
fn key_matches(waiver: Option<&str>, finding: Option<&str>) -> bool {
    let Some(w) = waiver else { return true };
    let Some(f) = finding else { return false };
    f == w || f.rsplit("::").next() == Some(w)
}

// ---------------------------------------------------------------------------
// Parsing one comment
// ---------------------------------------------------------------------------

/// Is this waiver word one we honour? `sealed` is the enum contract marker
/// handled elsewhere and must NOT waive anything — marking an enum sealed
/// would otherwise hide the very rows the marker exists to escalate.
fn is_waiver_word(w: &str) -> bool {
    matches!(w, "ok" | "ignore" | "allow")
}

/// The head of a waiver comment, before greedy continuation fills in the rest
/// of the reason.
#[derive(Debug, PartialEq)]
struct Head {
    check: Option<String>,
    key: Option<String>,
    date: Option<Date>,
    reason: String,
    /// Byte offset of `//` within the source line.
    comment_col: usize,
    /// Byte offset within the source line where the reason starts.
    reason_col: usize,
    /// The `//` is preceded by code on the same line.
    trailing: bool,
}

/// Parse a source line into a waiver head, if it carries one.
///
/// String-literal awareness matters: `println!("// unruster: ok")` is data, not
/// a waiver, and a codebase that documents this tool would otherwise waive
/// random lines of itself.
fn parse_head(line: &str) -> Option<Head> {
    let (comment_col, body) = find_line_comment(line)?;
    // `///` and `//!` are documentation. `/// unruster: sealed` is a contract
    // declaration about an enum, never a waiver.
    if body.starts_with('/') || body.starts_with('!') {
        return None;
    }
    let body_col = comment_col + 2;
    let trimmed = body.trim_start();
    let after_ws = body_col + (body.len() - trimmed.len());
    let rest = trimmed.strip_prefix(MARKER)?;
    let after_marker = after_ws + MARKER.len();

    let spaced = rest.trim_start();
    let mut cursor = after_marker + (rest.len() - spaced.len());

    // verb, up to whitespace or the opening paren of the spec
    let verb_len = spaced
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(spaced.len());
    let verb = &spaced[..verb_len];
    if !is_waiver_word(verb) {
        return None;
    }
    let mut tail = &spaced[verb_len..];
    cursor += verb_len;

    // optional (check[/key])
    let (mut check, mut key) = (None, None);
    if let Some(inner) = tail.strip_prefix('(') {
        let close = inner.find(')')?;
        let spec = inner[..close].trim();
        if !spec.is_empty() {
            match spec.split_once('/') {
                Some((c, k)) => {
                    check = Some(c.trim().to_string());
                    let k = k.trim();
                    if !k.is_empty() {
                        key = Some(k.to_string());
                    }
                }
                None => check = Some(spec.to_string()),
            }
        }
        tail = &inner[close + 1..];
        cursor += close + 2; // '(' + spec + ')'
    }

    // optional ISO date
    let spaced = tail.trim_start();
    cursor += tail.len() - spaced.len();
    let mut date = None;
    let tok_len = spaced
        .find(char::is_whitespace)
        .unwrap_or(spaced.len());
    if let Some(d) = Date::parse(&spaced[..tok_len]) {
        date = Some(d);
        cursor += tok_len;
    }
    let tail = &spaced[if date.is_some() { tok_len } else { 0 }..];

    // the rest is the reason, minus the cosmetic dash
    let spaced = tail.trim_start();
    cursor += tail.len() - spaced.len();
    let undashed = spaced
        .strip_prefix('—')
        .or_else(|| spaced.strip_prefix("--"))
        .or_else(|| spaced.strip_prefix('-'))
        .or_else(|| spaced.strip_prefix(':'));
    let reason_body = match undashed {
        Some(u) => {
            cursor += spaced.len() - u.len();
            u
        }
        None => spaced,
    };
    let final_reason = reason_body.trim_start();
    cursor += reason_body.len() - final_reason.len();

    Some(Head {
        check,
        key,
        date,
        reason: final_reason.trim_end().to_string(),
        comment_col,
        reason_col: cursor,
        trailing: !line[..comment_col].trim().is_empty(),
    })
}

/// Byte offset of the first `//` that is not inside a string or char literal,
/// plus the text following it.
fn find_line_comment(line: &str) -> Option<(usize, &str)> {
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
                return Some((i, &line[i + 2..]));
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

/// Can this line continue the reason of the waiver above it? Comment-only,
/// not a doc comment, not another waiver, and not opening with a screaming-case
/// convention marker that plainly belongs to something else.
fn is_continuation(line: &str) -> bool {
    let t = line.trim_start();
    let Some(body) = t.strip_prefix("//") else {
        return false;
    };
    if body.starts_with('/') || body.starts_with('!') {
        return false;
    }
    let text = body.trim_start();
    if text.is_empty() {
        return false;
    }
    if text.starts_with(MARKER) {
        return false;
    }
    !STOP_WORDS.iter().any(|w| {
        text.strip_prefix(w)
            .is_some_and(|r| r.is_empty() || r.starts_with([':', ' ', '(', '!', '-']))
    })
}

// ---------------------------------------------------------------------------
// Item spans
// ---------------------------------------------------------------------------

/// `(first_line, last_line)` of every item in a file, 1-indexed and inclusive.
/// `first_line` accounts for attributes and doc comments, which precede the
/// item keyword and are where a waiver above a documented fn would otherwise
/// fall into the gap.
struct ItemSpans {
    spans: Vec<(usize, usize)>,
}

impl ItemSpans {
    fn collect(file: &syn::File) -> Vec<(usize, usize)> {
        let mut v = ItemSpans { spans: Vec::new() };
        v.visit_file(file);
        v.spans.sort_unstable();
        v.spans
    }

    fn push<T: Spanned>(&mut self, node: &T, attrs: &[syn::Attribute]) {
        let s = node.span();
        let first = attrs
            .iter()
            .map(|a| a.span().start().line)
            .chain(std::iter::once(s.start().line))
            .min()
            .unwrap_or_else(|| s.start().line);
        let last = s.end().line.max(first);
        self.spans.push((first, last));
    }
}

/// Every item kind a waiver can sensibly scope to. Nested items are reached
/// through the default walk, so a `fn` inside a `fn` body gets its own span.
impl<'ast> Visit<'ast> for ItemSpans {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.push(i, &i.attrs);
        visit::visit_item_fn(self, i);
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.push(i, &i.attrs);
        visit::visit_item_impl(self, i);
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.push(i, &i.attrs);
        visit::visit_item_mod(self, i);
    }
    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.push(i, &i.attrs);
        visit::visit_item_enum(self, i);
    }
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.push(i, &i.attrs);
        visit::visit_item_struct(self, i);
    }
    fn visit_item_union(&mut self, i: &'ast syn::ItemUnion) {
        self.push(i, &i.attrs);
        visit::visit_item_union(self, i);
    }
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.push(i, &i.attrs);
        visit::visit_item_trait(self, i);
    }
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.push(i, &i.attrs);
        visit::visit_item_const(self, i);
    }
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.push(i, &i.attrs);
        visit::visit_item_static(self, i);
    }
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        self.push(i, &i.attrs);
        visit::visit_item_type(self, i);
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.push(i, &i.attrs);
        visit::visit_impl_item_fn(self, i);
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.push(i, &i.attrs);
        visit::visit_trait_item_fn(self, i);
    }
}

/// The item a standalone waiver ending at `comment_end` attaches to: the
/// widest one whose first line falls between the comment and the next line
/// with code (inclusive), so doc comments and attributes in the gap don't
/// break the association.
fn item_at(spans: &[(usize, usize)], comment_end: usize, next_code: usize) -> Option<(usize, usize)> {
    spans
        .iter()
        .copied()
        .filter(|&(first, _)| first > comment_end && first <= next_code)
        .max_by_key(|&(first, last)| (last.saturating_sub(first), std::cmp::Reverse(first)))
}

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

/// Collect every waiver in `files`. Waivers are read from the same files that
/// were scanned, so one in an excluded or out-of-scope file has no effect.
pub fn scan(files: &[ParsedFile]) -> Suppressions {
    let mut out = Suppressions::default();
    for f in files {
        let Ok(src) = std::fs::read_to_string(&f.path) else {
            continue;
        };
        let display = display_path(&f.path);
        let spans = ItemSpans::collect(&f.ast);
        scan_source(&mut out, &display, &src, &spans);
    }
    out
}

/// The textual half of [`scan`], split out so tests can drive it without
/// touching the filesystem.
fn scan_source(out: &mut Suppressions, display: &str, src: &str, spans: &[(usize, usize)]) {
    let lines: Vec<&str> = src.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let Some(head) = parse_head(lines[i]) else {
            i += 1;
            continue;
        };
        // Greedy continuation: absorb following comment-only lines into the
        // reason so a reflowed waiver reads the same as an unwrapped one.
        let mut end = i;
        let mut reason = head.reason.clone();
        while end + 1 < lines.len() && is_continuation(lines[end + 1]) {
            let extra = lines[end + 1]
                .trim_start()
                .trim_start_matches('/')
                .trim();
            if !extra.is_empty() {
                if !reason.is_empty() {
                    reason.push(' ');
                }
                reason.push_str(extra);
            }
            end += 1;
        }
        if reason.is_empty() {
            out.unexplained += 1;
        }

        let (scope, covers) = if head.trailing {
            (Scope::Site, (i + 1, i + 1))
        } else {
            match lines[end + 1..].iter().position(|l| !is_codeless(l)) {
                Some(off) => {
                    let next_code = end + 1 + off + 1; // 1-indexed
                    match item_at(spans, end + 1, next_code) {
                        Some((first, last)) => (Scope::Item, (first, last)),
                        None => (Scope::Site, (next_code, next_code)),
                    }
                }
                // A trailing waiver at end-of-file guards nothing; keep it
                // listed (so `waivers` can report it as dead) but covering
                // only its own line.
                None => (Scope::Site, (i + 1, i + 1)),
            }
        };

        out.push(Waiver {
            file: display.to_string(),
            comment_line: i + 1,
            comment_end: end + 1,
            covers,
            scope,
            check: head.check,
            key: head.key,
            date: head.date,
            reason,
            trailing: head.trailing,
            comment_col: head.comment_col,
            reason_col: head.reason_col,
            hits: Cell::new(0),
            hit_checks: RefCell::new(BTreeSet::new()),
        });
        i = end + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(line: &str) -> Option<Head> {
        parse_head(line)
    }

    #[test]
    fn trailing_waiver_applies_to_its_own_line() {
        let h = head("let _ = f(); // unruster: ok — infallible").unwrap();
        assert!(h.trailing);
        assert_eq!(h.reason, "infallible");
        assert_eq!(h.check, None);
    }

    #[test]
    fn qualified_waiver_parses_check_key_and_date() {
        let h = head("// unruster: ok(divergence/NodeContent::Group) 2026-08-06 — structural edge")
            .unwrap();
        assert_eq!(h.check.as_deref(), Some("divergence"));
        assert_eq!(h.key.as_deref(), Some("NodeContent::Group"));
        assert_eq!(h.date, Some(Date { y: 2026, m: 8, d: 6 }));
        assert_eq!(h.reason, "structural edge");
        assert!(!h.trailing);
    }

    #[test]
    fn check_without_a_key_parses() {
        let h = head("// unruster: ok(error-swallows) 2026-01-02 — Drop guard").unwrap();
        assert_eq!(h.check.as_deref(), Some("error-swallows"));
        assert_eq!(h.key, None);
        assert_eq!(h.reason, "Drop guard");
    }

    #[test]
    fn legacy_waiver_still_parses_with_no_check_or_date() {
        let h = head("// unruster: ok — the poisoned-lock path is handled").unwrap();
        assert_eq!(h.check, None);
        assert_eq!(h.date, None);
        assert_eq!(h.reason, "the poisoned-lock path is handled");
    }

    #[test]
    fn reason_col_points_past_the_head() {
        let line = "// unruster: ok(casts/ptr) 2026-08-06 — objc FFI";
        let h = head(line).unwrap();
        assert_eq!(&line[h.reason_col..], "objc FFI");
        assert_eq!(h.comment_col, 0);
    }

    #[test]
    fn reason_col_is_right_for_a_trailing_waiver() {
        let line = "    let _ = f();  // unruster: ok(error-swallows) 2026-08-06 — guard";
        let h = head(line).unwrap();
        assert_eq!(&line[h.reason_col..], "guard");
        assert_eq!(&line[..h.comment_col], "    let _ = f();  ");
    }

    #[test]
    fn waiver_without_a_reason_is_honoured_but_reported() {
        assert_eq!(head("let _ = f(); // unruster: ok").unwrap().reason, "");
        assert_eq!(head("let _ = f(); // unruster: ok —").unwrap().reason, "");
        assert_eq!(
            head("// unruster: ok(casts) 2026-08-06").unwrap().reason,
            ""
        );
    }

    #[test]
    fn sealed_marker_is_not_a_waiver() {
        // `/// unruster: sealed` is the enum contract marker. If it waived
        // findings, marking an enum sealed would hide the very rows the marker
        // exists to escalate.
        assert!(head("/// unruster: sealed").is_none());
        assert!(head("// unruster: sealed").is_none());
    }

    #[test]
    fn doc_comments_are_never_waivers() {
        assert!(head("/// unruster: ok(casts) 2026-08-06 — no").is_none());
        assert!(head("//! unruster: ok(casts) 2026-08-06 — no").is_none());
    }

    #[test]
    fn marker_inside_a_string_literal_is_not_a_waiver() {
        assert!(head(r#"println!("// unruster: ok");"#).is_none());
        assert!(head(r#"let s = "a//b"; // unruster: ok — real"#).is_some());
    }

    #[test]
    fn unrelated_comments_are_ignored() {
        assert!(head("let x = 1; // just a note").is_none());
        assert!(head("let x = 1;").is_none());
    }

    #[test]
    fn continuation_absorbs_wrapped_prose_but_not_convention_markers() {
        assert!(is_continuation("// and the rest of the reason"));
        assert!(is_continuation("   // note that this is prose"));
        assert!(!is_continuation("// TODO: revisit"));
        assert!(!is_continuation("// SAFETY: pointer is aligned"));
        assert!(!is_continuation("// NOTE: unrelated"));
        assert!(!is_continuation("/// doc"));
        assert!(!is_continuation("let x = 1;"));
        assert!(!is_continuation("// unruster: ok — a second waiver"));
    }

    fn scan_str(src: &str, spans: &[(usize, usize)]) -> Suppressions {
        let mut s = Suppressions::default();
        scan_source(&mut s, "f.rs", src, spans);
        s
    }

    #[test]
    fn wrapped_reason_is_rejoined() {
        let src = "// unruster: ok(casts/ptr) 2026-08-06 — objc runtime guarantees\n\
                   // alignment for these selectors\nlet x = p as *const u8;\n";
        let s = scan_str(src, &[]);
        assert_eq!(s.len(), 1);
        let w = &s.all()[0];
        assert_eq!(
            w.reason,
            "objc runtime guarantees alignment for these selectors"
        );
        assert_eq!(w.comment_line, 1);
        assert_eq!(w.comment_end, 2);
        // The waiver still lands on the code line, not on its own comment.
        assert_eq!(w.covers, (3, 3));
    }

    #[test]
    fn standalone_waiver_above_an_item_takes_item_scope() {
        let src = "// unruster: ok(dead-code) 2026-08-06 — called from a json! macro\n\
                   fn f() {\n    let _ = g();\n}\n";
        let s = scan_str(src, &[(2, 4)]);
        let w = &s.all()[0];
        assert_eq!(w.scope, Scope::Item);
        assert_eq!(w.covers, (2, 4));
    }

    #[test]
    fn item_scope_survives_doc_comments_and_attributes_in_the_gap() {
        let src = "// unruster: ok(dead-code) 2026-08-06 — serde attribute names it\n\
                   /// Docs.\n#[inline]\nfn f() {}\n";
        // syn reports the item span starting at the first attribute (line 2).
        let s = scan_str(src, &[(2, 4)]);
        let w = &s.all()[0];
        assert_eq!(w.scope, Scope::Item);
        assert_eq!(w.covers, (2, 4));
    }

    #[test]
    fn standalone_waiver_above_a_statement_stays_site_scoped() {
        let src = "fn f() {\n    // unruster: ok(error-swallows) 2026-08-06 — guard\n\
                       let _ = g();\n}\n";
        let s = scan_str(src, &[(1, 4)]);
        let w = &s.all()[0];
        assert_eq!(w.scope, Scope::Site);
        assert_eq!(w.covers, (3, 3));
    }

    #[test]
    fn matching_respects_check_and_key() {
        let src = "// unruster: ok(divergence/NodeContent::Group) 2026-08-06 — structural\nfn f() {}\n";
        let s = scan_str(src, &[(2, 2)]);
        assert!(s.matches("divergence", Site::keyed("f.rs", 2, "NodeContent::Group")));
        // Bare last segment also matches, so nobody has to remember the path.
        assert!(s.matches("divergence", Site::keyed("f.rs", 2, "NodeContent::Group")));
        // Wrong variant, wrong check, and unkeyed findings must all survive.
        assert!(!s.matches("divergence", Site::keyed("f.rs", 2, "NodeContent::Image")));
        assert!(!s.matches("casts", Site::keyed("f.rs", 2, "NodeContent::Group")));
        assert!(!s.matches("divergence", Site::new("f.rs", 2)));
    }

    #[test]
    fn bare_variant_key_matches_a_qualified_finding() {
        let src = "// unruster: ok(divergence/Group) 2026-08-06 — structural\nfn f() {}\n";
        let s = scan_str(src, &[(2, 2)]);
        assert!(s.matches("divergence", Site::keyed("f.rs", 2, "NodeContent::Group")));
    }

    #[test]
    fn legacy_waiver_matches_every_check() {
        let src = "let _ = f(); // unruster: ok — legacy\n";
        let s = scan_str(src, &[]);
        assert!(s.matches("error-swallows", Site::keyed("f.rs", 1, "let-_")));
        assert!(s.matches("casts", Site::new("f.rs", 1)));
        assert_eq!(s.legacy_count(), 1);
    }

    #[test]
    fn hits_are_counted_per_waiver() {
        let src = "// unruster: ok(error-swallows) 2026-08-06 — all of them\nfn f() {}\n";
        let s = scan_str(src, &[(2, 2)]);
        assert!(s.matches("error-swallows", Site::new("f.rs", 2)));
        assert!(s.matches("error-swallows", Site::new("f.rs", 2)));
        assert_eq!(s.all()[0].hits(), 2);
        assert_eq!(s.all()[0].hit_checks(), vec!["error-swallows".to_string()]);
    }

    #[test]
    fn dates_round_trip_and_age_correctly() {
        let d = Date::parse("2026-08-06").unwrap();
        assert_eq!(d.to_string(), "2026-08-06");
        assert_eq!(Date::from_days(d.to_days()), d);
        let later = Date::parse("2026-08-16").unwrap();
        assert_eq!(d.age_days(later), 10);
        // Across a leap day, and across a year boundary.
        let a = Date::parse("2024-02-28").unwrap();
        let b = Date::parse("2024-03-01").unwrap();
        assert_eq!(a.age_days(b), 2);
        assert_eq!(
            Date::parse("2023-12-31")
                .unwrap()
                .age_days(Date::parse("2024-01-01").unwrap()),
            1
        );
    }

    #[test]
    fn bad_dates_are_rejected_rather_than_guessed() {
        assert!(Date::parse("2026-8-6").is_none());
        assert!(Date::parse("26-08-06").is_none());
        assert!(Date::parse("2026-13-01").is_none());
        assert!(Date::parse("2026-08-32").is_none());
        assert!(Date::parse("not-a-date").is_none());
        // An undated waiver keeps its whole reason rather than eating a word.
        let h = head("// unruster: ok(casts) yesterday it was fine").unwrap();
        assert_eq!(h.date, None);
        assert_eq!(h.reason, "yesterday it was fine");
    }
}
