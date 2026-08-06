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
    // unruster: ok(casts/narrow-int) 2026-08-06 — the algorithm's outputs are
    // bounded by construction: `m` is 1..=12 and `d` is 1..=31, and `y`
    // overflows i32 only past year 2.1e9, which needs a day count no clock can
    // produce (`today()` falls back to the epoch when the clock is unreadable).
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
    /// Findings this waiver hides that the audit battery would *not* have
    /// reported anyway — rows below `--max-missing`, under the variant floor,
    /// trait-routed catch-alls, sub-threshold divergence pairs. Counted
    /// separately because a waiver whose whole contribution lands here is dead
    /// weight in the only loop that gates, while still being harmless enough
    /// that calling it a lie would overstate the case.
    below_audit: Cell<usize>,
    /// Which checks those hits came from — the evidence `--upgrade` uses to
    /// qualify a legacy waiver.
    hit_checks: RefCell<BTreeSet<String>>,
}

impl Waiver {
    /// Findings suppressed that the audit battery would have gated on. This is
    /// the number that decides whether a waiver is earning its place.
    pub fn hits(&self) -> usize {
        self.hits.get()
    }

    /// Findings suppressed that only a permissive, non-gating configuration
    /// surfaces. See [`Waiver::below_audit`].
    pub fn below_audit(&self) -> usize {
        self.below_audit.get()
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

/// Which counter [`Suppressions::matches`] increments.
///
/// `waivers` runs the battery twice — once configured exactly as `audit` runs
/// it, once wide open — so it can tell "this waiver is load-bearing" from "this
/// waiver only hides rows the audit already filters out". Without the split,
/// orphan detection answered a question nobody asks: *does this suppress
/// anything under maximally permissive settings?*
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HitMode {
    /// Hits count toward [`Waiver::hits`] — the gating number.
    Gating,
    /// Hits count toward [`Waiver::below_audit`].
    BelowAudit,
}

/// Every waiver in the scanned tree, indexed by file.
#[derive(Debug, Default)]
pub struct Suppressions {
    waivers: Vec<Waiver>,
    by_file: HashMap<String, Vec<usize>>,
    /// Waivers with no reason text after the head.
    pub unexplained: usize,
    /// Which counter `matches` bumps. Defaults to `Gating`, so an ordinary
    /// command run attributes its hits to the number that matters.
    mode: Cell<HitModeRepr>,
}

/// `Cell` needs `Copy + Default`; `HitMode` has no sensible `Default`.
type HitModeRepr = bool;
const MODE_GATING: HitModeRepr = false;
const MODE_BELOW: HitModeRepr = true;

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

    /// Findings suppressed so far this run, across every waiver. Meaningful
    /// only after the checks have run; `audit` reads it at the end so its
    /// summary can state reach rather than just how many comments exist.
    pub fn total_hits(&self) -> usize {
        self.waivers.iter().map(Waiver::hits).sum()
    }

    /// Route subsequent `matches` hits to the given counter; returns the
    /// previous mode so a caller can restore it.
    pub fn set_hit_mode(&self, m: HitMode) -> HitMode {
        let prev = self.mode.replace(match m {
            HitMode::Gating => MODE_GATING,
            HitMode::BelowAudit => MODE_BELOW,
        });
        if prev == MODE_GATING {
            HitMode::Gating
        } else {
            HitMode::BelowAudit
        }
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
            if self.mode.get() == MODE_GATING {
                w.hits.set(w.hits.get() + 1);
            } else {
                w.below_audit.set(w.below_audit.get() + 1);
            }
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

/// Checks that ask the same question of the same site, and the name they are
/// already grouped under in the audit output (`explain: partial-enumeration`).
///
/// `divergence` and `enum-coverage` both answer "does this dispatch site
/// deliberately omit variant X?". Without a group, verifying that once cost two
/// waivers — and on a real codebase six of thirty-three had the reason `same.`,
/// written only because the check name differed. The group name is not invented
/// here: both checks already print it as their `explain:` topic.
const CHECK_GROUPS: &[(&str, &[&str])] = &[
    ("partial-enumeration", &["divergence", "enum-coverage"]),
    (
        "silent-fallbacks",
        &["error-swallows", "divergence-handling"],
    ),
    ("replication", &["conversion-pairs", "pass-through"]),
];

/// The group a check belongs to, if any. Lets `waivers` spot two waivers that
/// differ only by check name and suggest the one-comment spelling.
pub fn group_of(check: &str) -> Option<&'static str> {
    CHECK_GROUPS
        .iter()
        .find(|(_, members)| members.contains(&check))
        .map(|(g, _)| *g)
}

/// Every check a waiver key may name, group aliases included — used to warn
/// about a misspelled check rather than silently waiving nothing.
pub fn known_check_names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = CHECK_GROUPS.iter().map(|(g, _)| *g).collect();
    v.extend(CHECK_GROUPS.iter().flat_map(|(_, m)| m.iter().copied()));
    v.extend(["dead-code", "casts", "stringly"]);
    v.sort_unstable();
    v.dedup();
    v
}

/// An unqualified waiver matches every check — that is the legacy contract and
/// breaking it would silently un-waive judgments already recorded. A waiver
/// naming a group matches every check in it.
fn check_matches(waiver: Option<&str>, check: &str) -> bool {
    let Some(w) = waiver else { return true };
    if w == check {
        return true;
    }
    CHECK_GROUPS
        .iter()
        .any(|(g, members)| *g == w && members.contains(&check))
}

/// An unkeyed waiver matches any key. A keyed waiver requires a keyed finding
/// and matches three ways, narrowest spelling first:
///
/// * exact — `ok(divergence/NodeContent::Group)`
/// * bare variant — `ok(divergence/Group)`, so the enum path is optional
/// * whole enum — `ok(enum-coverage/ActiveModal)` covers `ActiveModal::None`,
///   `ActiveModal::TextEntry`, … in one comment.
///
/// The last form is why this isn't just equality: an enum-coverage row lists
/// every uncovered variant, and without a prefix match retiring one row meant
/// pasting one comment per missing variant — four, on a real codebase.
fn key_matches(waiver: Option<&str>, finding: Option<&str>) -> bool {
    let Some(w) = waiver else { return true };
    let Some(f) = finding else { return false };
    f == w
        || f.rsplit("::").next() == Some(w)
        || f.strip_prefix(w).is_some_and(|r| r.starts_with("::"))
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

/// Where one item lives, 1-indexed and inclusive.
///
/// Three lines rather than two, because a waiver can legitimately sit on
/// either side of an item's documentation:
///
/// ```ignore
/// // unruster: ok(…)   ← above the docs      → header_start
/// /// Docs.
/// #[inline]
/// // unruster: ok(…)   ← below them          → keyword
/// fn f() {}                                  → .. last
/// ```
///
/// `syn` reports `item.span().start()` as the first *attribute* line (doc
/// comments are attributes), so matching on that alone silently demotes the
/// second placement to site scope — which is how six findings survived a
/// waiver that claimed to cover them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ItemSpan {
    /// First line of the item including its attributes and doc comments.
    pub header_start: usize,
    /// Line of the item's own keyword (`fn`, `impl`, `enum`, …).
    pub keyword: usize,
    /// Last line of the item.
    pub last: usize,
}

/// Item spans plus the line ranges of multi-line string literals.
///
/// [`find_line_comment`] tracks quotes within one line, which is enough for
/// `println!("// unruster: ok")` but not for a literal that spans lines: its
/// continuation lines carry no opening quote, so a `//` inside one reads as a
/// real comment. That is precisely the "a codebase documenting this tool
/// waives random lines of itself" failure the parser doc warns about, and
/// unruster's own test fixtures tripped it. `syn` has already lexed these
/// literals — raw strings and escapes included — so the spans are exact.
struct SourceSpans {
    spans: Vec<ItemSpan>,
    /// `(first_line, last_line)` of every statement. A method chain broken
    /// across lines reports its finding at the offending method's line, not at
    /// the `let`, so a waiver written above the statement has to cover the
    /// whole statement or it silently misses by three lines.
    stmt_spans: Vec<(usize, usize)>,
    /// Inclusive 1-indexed line ranges that lie *inside* a multi-line literal
    /// (the opening line is excluded — its quote is visible to the scanner).
    literal_lines: Vec<(usize, usize)>,
}

/// Everything a waiver scan needs to know about one parsed file.
pub struct FileSpans {
    items: Vec<ItemSpan>,
    stmts: Vec<(usize, usize)>,
    literal_lines: Vec<(usize, usize)>,
}

impl FileSpans {
    /// Used when the file won't re-parse. Every waiver in it then falls back
    /// to site scope and no line is masked — the same behaviour as before
    /// spans existed, rather than a silent loss of waivers.
    fn empty() -> Self {
        FileSpans {
            items: Vec::new(),
            stmts: Vec::new(),
            literal_lines: Vec::new(),
        }
    }

    /// The statement starting at `line`, if any — widest wins, so a `let`
    /// wrapping a block gets the block.
    fn stmt_at(&self, line: usize) -> Option<(usize, usize)> {
        self.stmts
            .iter()
            .copied()
            .filter(|&(a, b)| a == line && b > a)
            .max_by_key(|&(a, b)| b - a)
    }

    fn contains_literal(&self, line: usize) -> bool {
        self.literal_lines
            .iter()
            .any(|&(a, b)| line >= a && line <= b)
    }
}

impl SourceSpans {
    fn collect(file: &syn::File) -> FileSpans {
        let mut v = SourceSpans {
            spans: Vec::new(),
            stmt_spans: Vec::new(),
            literal_lines: Vec::new(),
        };
        v.visit_file(file);
        v.spans
            .sort_unstable_by_key(|s| (s.header_start, s.keyword, s.last));
        v.stmt_spans.sort_unstable();
        FileSpans {
            items: v.spans,
            stmts: v.stmt_spans,
            literal_lines: v.literal_lines,
        }
    }

    /// `kw` must be the item's own keyword token, not the item node — the
    /// node's span starts at its first attribute.
    fn push<T: Spanned, K: Spanned>(&mut self, node: &T, kw: &K, attrs: &[syn::Attribute]) {
        let s = node.span();
        let keyword = kw.span().start().line;
        let header_start = attrs
            .iter()
            .map(|a| a.span().start().line)
            .chain(std::iter::once(s.start().line))
            .min()
            .unwrap_or(keyword)
            .min(keyword);
        self.spans.push(ItemSpan {
            header_start,
            keyword,
            last: s.end().line.max(keyword),
        });
    }
}

/// Every item kind a waiver can sensibly scope to. Nested items are reached
/// through the default walk, so a `fn` inside a `fn` body gets its own span.
impl<'ast> Visit<'ast> for SourceSpans {
    /// Every literal, not just strings: byte strings and raw strings can span
    /// lines too, and the cost of over-collecting is nil (a numeric literal is
    /// always one line, so it never contributes a range).
    fn visit_stmt(&mut self, st: &'ast syn::Stmt) {
        let sp = st.span();
        self.stmt_spans.push((sp.start().line, sp.end().line));
        visit::visit_stmt(self, st);
    }

    fn visit_lit(&mut self, l: &'ast syn::Lit) {
        let s = l.span();
        let (start, end) = (s.start().line, s.end().line);
        if end > start {
            self.literal_lines.push((start + 1, end));
        }
        visit::visit_lit(self, l);
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.push(i, &i.sig.fn_token, &i.attrs);
        visit::visit_item_fn(self, i);
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.push(i, &i.impl_token, &i.attrs);
        visit::visit_item_impl(self, i);
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.push(i, &i.mod_token, &i.attrs);
        visit::visit_item_mod(self, i);
    }
    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.push(i, &i.enum_token, &i.attrs);
        visit::visit_item_enum(self, i);
    }
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.push(i, &i.struct_token, &i.attrs);
        visit::visit_item_struct(self, i);
    }
    fn visit_item_union(&mut self, i: &'ast syn::ItemUnion) {
        self.push(i, &i.union_token, &i.attrs);
        visit::visit_item_union(self, i);
    }
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.push(i, &i.trait_token, &i.attrs);
        visit::visit_item_trait(self, i);
    }
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.push(i, &i.const_token, &i.attrs);
        visit::visit_item_const(self, i);
    }
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.push(i, &i.static_token, &i.attrs);
        visit::visit_item_static(self, i);
    }
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        self.push(i, &i.type_token, &i.attrs);
        visit::visit_item_type(self, i);
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.push(i, &i.sig.fn_token, &i.attrs);
        visit::visit_impl_item_fn(self, i);
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.push(i, &i.sig.fn_token, &i.attrs);
        visit::visit_trait_item_fn(self, i);
    }
}

/// The item a standalone waiver attaches to: one whose *header* — anywhere
/// from its first attribute through its keyword — contains the next line with
/// code. That range is what lets the waiver sit above the docs or below them
/// and mean the same thing.
///
/// A waiver inside a body can't match: the enclosing item's keyword is above
/// it, so `next_code <= keyword` fails and the caller falls back to site scope.
/// Ties prefer the widest span, which is how a waiver above `impl Foo` takes
/// the impl rather than its first method.
fn item_at(spans: &[ItemSpan], next_code: usize) -> Option<ItemSpan> {
    spans
        .iter()
        .copied()
        .filter(|s| s.header_start <= next_code && next_code <= s.keyword)
        .max_by_key(|s| (s.last.saturating_sub(s.keyword), std::cmp::Reverse(s.keyword)))
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
        // Re-parse the raw text rather than reusing `f.ast`: that one has had
        // its cfg-false items stripped, so its spans have holes wherever, say,
        // a `#[cfg(test)] mod tests` used to be — while the *text* this scan
        // walks still has those lines. The mismatch let string literals inside
        // stripped code register as waivers. A comment's meaning should not
        // depend on which `--cfg` flags were passed, either.
        // unruster: ok(error-swallows/.unwrap_or_else) 2026-08-06 — a file that
        // `parse_dir` already parsed cannot fail here; the fallback exists so a
        // race on disk degrades to site-scoped waivers rather than losing them.
        let spans = syn::parse_file(&src)
            .map(|ast| SourceSpans::collect(&ast))
            .unwrap_or_else(|_| FileSpans::empty());
        scan_source(&mut out, &display, &src, &spans);
    }
    out
}

/// The textual half of [`scan`], split out so tests can drive it without
/// touching the filesystem.
fn scan_source(out: &mut Suppressions, display: &str, src: &str, spans: &FileSpans) {
    let lines: Vec<&str> = src.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        // Inside a multi-line literal there is no comment, only text that
        // looks like one.
        if spans.contains_literal(i + 1) {
            i += 1;
            continue;
        }
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
                    match item_at(&spans.items, next_code) {
                        // Cover from the keyword, not the header: the doc
                        // comments above an item hold no findings, and
                        // starting there would let one item's waiver reach
                        // back over the line the previous item ends on.
                        Some(s) => (Scope::Item, (s.keyword, s.last)),
                        // Not an item: cover the whole statement when it spans
                        // lines, so a waiver above a broken-up method chain
                        // reaches the method that was actually flagged.
                        None => match spans.stmt_at(next_code) {
                            Some((a, b)) => (Scope::Site, (a, b)),
                            None => (Scope::Site, (next_code, next_code)),
                        },
                    }
                }
                // No code after it at all — a standalone waiver dangling at
                // end of file. It guards nothing, but stays listed so
                // `waivers --orphaned` can report it as dead rather than
                // dropping it silently.
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
            below_audit: Cell::new(0),
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

    /// Parse `src` for real and scan it. Hand-supplied spans were how a
    /// placement bug survived: the tests agreed with each other about where
    /// items start, and both were wrong. Everything below goes through the
    /// actual `syn` span collection.
    fn scan_str(src: &str) -> Suppressions {
        let file = syn::parse_file(src).expect("fixture must parse");
        let spans = SourceSpans::collect(&file);
        let mut s = Suppressions::default();
        scan_source(&mut s, "f.rs", src, &spans);
        s
    }

    #[test]
    fn item_spans_separate_the_header_from_the_keyword() {
        // `syn` reports an item's span as starting at its first attribute, and
        // doc comments are attributes. Conflating that with the keyword line
        // is what silently demoted item scope to site scope.
        let src = "/// Docs.\n/// More docs.\n#[inline]\npub fn f() {\n    let x = 1;\n}\n";
        let file = syn::parse_file(src).unwrap();
        let spans = SourceSpans::collect(&file);
        assert_eq!(
            spans.items,
            vec![ItemSpan {
                header_start: 1,
                keyword: 4,
                last: 6
            }]
        );
    }

    #[test]
    fn a_waiver_inside_a_multi_line_literal_is_text_not_a_waiver() {
        // Found by running unruster on itself: this file's own test fixtures
        // were being read as live waivers. Per-line quote tracking cannot see
        // that a continuation line sits inside a string.
        let src = "fn f() {\n    let doc = \"usage:\n\
                   // unruster: ok(casts/ptr) 2026-08-06 — this is documentation\n\
                   end\";\n    let _ = doc;\n}\n";
        let s = scan_str(src);
        assert!(
            s.is_empty(),
            "text inside a literal must not waive: {:?}",
            s.all()
        );
    }

    #[test]
    fn a_raw_string_spanning_lines_is_also_inert() {
        let src = "fn f() {\n    let d = r#\"\n// unruster: ok — nope\n\"#;\n    let _ = d;\n}\n";
        assert!(scan_str(src).is_empty());
    }

    #[test]
    fn a_real_waiver_after_a_multi_line_literal_still_registers() {
        // The mask must not bleed past the literal's closing line.
        let src = "fn f() {\n    let d = \"a\nb\";\n    let _ = d;\n\
                       // unruster: ok(error-swallows) 2026-08-06 — real one\n\
                   let _ = g();\n}\n";
        let s = scan_str(src);
        assert_eq!(s.len(), 1, "{:?}", s.all());
        assert_eq!(s.all()[0].reason, "real one");
    }

    #[test]
    fn wrapped_reason_is_rejoined() {
        let src = "fn g() {\n\
                   // unruster: ok(casts/ptr) 2026-08-06 — objc runtime guarantees\n\
                   // alignment for these selectors\nlet x = 1;\n}\n";
        let s = scan_str(src);
        assert_eq!(s.len(), 1);
        let w = &s.all()[0];
        assert_eq!(
            w.reason,
            "objc runtime guarantees alignment for these selectors"
        );
        assert_eq!(w.comment_line, 2);
        assert_eq!(w.comment_end, 3);
        // The waiver still lands on the code line, not on its own comment.
        assert_eq!(w.covers, (4, 4));
    }

    #[test]
    fn standalone_waiver_above_an_item_takes_item_scope() {
        let src = "// unruster: ok(dead-code) 2026-08-06 — called from a json! macro\n\
                   fn f() {\n    let _ = g();\n}\n";
        let s = scan_str(src);
        let w = &s.all()[0];
        assert_eq!(w.scope, Scope::Item);
        assert_eq!(w.covers, (2, 4));
    }

    #[test]
    fn item_scope_holds_above_the_docs_and_below_them() {
        // Both placements are natural and must mean the same thing. The
        // second one is the regression: `syn` puts the item's span start at
        // the doc comment, so a naive match found no item after the waiver
        // and silently fell back to site scope.
        let above = "// unruster: ok(dead-code) 2026-08-06 — serde names it\n\
                     /// Docs.\n#[inline]\nfn f() {\n    let _ = g();\n}\n";
        let below = "/// Docs.\n#[inline]\n\
                     // unruster: ok(dead-code) 2026-08-06 — serde names it\n\
                     fn f() {\n    let _ = g();\n}\n";
        let a = scan_str(above);
        assert_eq!(a.all()[0].scope, Scope::Item);
        assert_eq!(a.all()[0].covers, (4, 6));
        let b = scan_str(below);
        assert_eq!(b.all()[0].scope, Scope::Item, "waiver below the docs");
        assert_eq!(b.all()[0].covers, (4, 6));
    }

    #[test]
    fn item_scope_reaches_every_method_in_an_impl() {
        // The claim the docs make, on real spans: one comment above `impl`
        // covers all of its methods.
        let src = "struct S;\n// unruster: ok(error-swallows) 2026-08-06 — all deliberate\n\
                   impl S {\n    fn a(&self) { let _ = 1; }\n\
                   \n    fn b(&self) { let _ = 2; }\n}\n";
        let s = scan_str(src);
        let w = &s.all()[0];
        assert_eq!(w.scope, Scope::Item);
        assert_eq!(w.covers, (3, 7), "the whole impl, not just its first fn");
        assert!(s.matches("error-swallows", Site::new("f.rs", 4)));
        assert!(s.matches("error-swallows", Site::new("f.rs", 6)));
    }

    #[test]
    fn a_waiver_above_a_method_takes_the_method_not_the_impl() {
        let src = "struct S;\nimpl S {\n    fn a(&self) { let _ = 1; }\n\
                   \n    // unruster: ok(error-swallows) 2026-08-06 — just this one\n\
                   fn b(&self) { let _ = 2; }\n}\n";
        let s = scan_str(src);
        assert_eq!(s.all()[0].covers, (6, 6));
        assert!(!s.matches("error-swallows", Site::new("f.rs", 3)));
        assert!(s.matches("error-swallows", Site::new("f.rs", 6)));
    }

    #[test]
    fn standalone_waiver_above_a_statement_stays_site_scoped() {
        let src = "fn f() {\n    // unruster: ok(error-swallows) 2026-08-06 — guard\n\
                       let _ = g();\n    let _ = h();\n}\n";
        let s = scan_str(src);
        let w = &s.all()[0];
        assert_eq!(w.scope, Scope::Site);
        assert_eq!(w.covers, (3, 3));
        // Emphatically not the rest of the body.
        assert!(!s.matches("error-swallows", Site::new("f.rs", 4)));
    }

    #[test]
    fn matching_respects_check_and_key() {
        let src = "// unruster: ok(divergence/NodeContent::Group) 2026-08-06 — structural\nfn f() {}\n";
        let s = scan_str(src);
        assert!(s.matches("divergence", Site::keyed("f.rs", 2, "NodeContent::Group")));
        // Wrong variant, wrong check, and unkeyed findings must all survive.
        assert!(!s.matches("divergence", Site::keyed("f.rs", 2, "NodeContent::Image")));
        assert!(!s.matches("casts", Site::keyed("f.rs", 2, "NodeContent::Group")));
        assert!(!s.matches("divergence", Site::new("f.rs", 2)));
    }

    #[test]
    fn bare_variant_key_matches_a_qualified_finding() {
        let src = "// unruster: ok(divergence/Group) 2026-08-06 — structural\nfn f() {}\n";
        let s = scan_str(src);
        assert!(s.matches("divergence", Site::keyed("f.rs", 2, "NodeContent::Group")));
    }

    #[test]
    fn legacy_waiver_matches_every_check() {
        let src = "fn f() {\n    let _ = g(); // unruster: ok — legacy\n}\n";
        let s = scan_str(src);
        assert!(s.matches("error-swallows", Site::keyed("f.rs", 2, "let-_")));
        assert!(s.matches("casts", Site::new("f.rs", 2)));
        assert_eq!(s.legacy_count(), 1);
    }

    #[test]
    fn hits_are_counted_per_waiver() {
        let src = "// unruster: ok(error-swallows) 2026-08-06 — all of them\nfn f() {}\n";
        let s = scan_str(src);
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
