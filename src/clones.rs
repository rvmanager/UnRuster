//! Duplicated function bodies — the same code written out more than once.
//!
//! This is the `replication` thesis at its most literal, and it was the tool's
//! largest blind spot. On a twelve-crate workspace an agent working the audit
//! loop found, by hand-grepping, seven byte-identical copies of `caller<T>`,
//! five of `parse_uuid`, four of `ts`, and six inline re-spellings of the same
//! UUID parse — sixteen copies of three concepts, all inside one directory.
//! Consolidating them deleted eleven other findings outright rather than
//! waiving them, which made it the highest-leverage change of that session.
//! `conversion-pairs` reported zero, `pass-through` reported zero, and nothing
//! else looked.
//!
//! # What counts as a clone
//!
//! Bodies are compared after **alpha-renaming**: every binding a function
//! introduces — parameters, `let` patterns, closure arguments, match arm
//! bindings, loop patterns — is rewritten to a positional placeholder in
//! order of first appearance. Everything else is compared verbatim, including
//! called method and path names, literals, and control flow.
//!
//! That canonicalization lives in [`crate::facts`], and this check consumes it
//! rather than repeating it. It used to have its own copy, and `clones`
//! reported the two — `Renamer::bind`, `Renamer::bind_pat`,
//! `BindingCollector::visit_expr_closure`, three groups in its own output. The
//! duplication was also a correctness risk with a name: an "exact clone" and a
//! "near clone with zero differing leaves" are supposed to be the same
//! statement, and they can only be guaranteed to be while one canonicalizer
//! produces both.
//!
//! That cut is deliberate. Renaming *called* names would group any two
//! functions with the same shape (`fn a() { x.foo() }` and `fn b() { y.bar() }`),
//! which is a similarity metric, not a defect report. Keeping them means a
//! group is a set of functions that do the same thing to the same APIs, and
//! differ only in what the locals are called — which is what a copy-paste
//! actually looks like.
//!
//! Two copies of a three-token accessor are not a finding, so bodies below
//! `--min-tokens` are ignored.
//!
//! # Precision
//!
//! EXACT. Two bodies in one group are token-identical after renaming; there is
//! no similarity threshold and no fuzzy match. What the check cannot tell you
//! is whether consolidating is *right* — a trait impl that must repeat a
//! default, a macro-generated shape, two modules deliberately kept
//! independent. Those are judgment calls, which is what the waiver is for.

use std::collections::HashMap;

use crate::context::{AnalysisCtx, Counts};
use crate::corpus::Corpus;
use crate::emit::{row, site};
use crate::facts::BodyFact;

/// A set of functions sharing one canonical body.
#[derive(Debug)]
struct Group<'a> {
    members: Vec<&'a BodyFact>,
    tokens: usize,
    /// Every member declares the same identifier. The strongest form: the same
    /// helper, written out N times, under N different roofs.
    same_name: bool,
    /// Every member lives under the same directory — copies that a single
    /// `mod` could hold, which is where consolidation is cheapest and safest.
    same_dir: bool,
    /// The shortest description of what is duplicated — also the waiver key,
    /// which is why it is stored rather than recomputed: a `Site` borrows its
    /// key, so a temporary `String` cannot be one.
    label: String,
}

impl Group<'_> {
    // unruster: ok(concepts/signature:score) 2026-08-12 — five ranked checks,
    // five different formulas over five unrelated structs. What they share is
    // an output *contract* — a 0..1 score, gated at the check's own
    // `GATING_SCORE` — which each module already states in that constant's own
    // doc. A trait would add a vtable and share no code.
    /// Rank: how much is duplicated, how many times, and how easy the fix is.
    ///
    /// Size and copy count multiply because they compound — four copies of a
    /// 40-token body is not twice the problem of two copies, it is the same
    /// helper having drifted four ways. The two booleans are what turn a
    /// merely-identical pair into an obviously-extractable one, and they are
    /// what put `parse_uuid`-shaped findings on top.
    fn score(&self) -> f64 {
        let copies = self.members.len() as f64;
        // Saturating at 40 tokens and at 5 copies: past those the finding is
        // already "yes, extract this", and letting either run away would sort
        // one enormous group above three obvious ones.
        let bulk = (self.tokens as f64 / 40.0).min(1.0);
        // Zero at exactly two copies. A pair is the smallest group that can
        // exist, so it earns no credit for being one — otherwise every pair
        // clears the gate and the gate is the list.
        let spread = ((copies - 2.0) / 3.0).clamp(0.0, 1.0);
        let named = if self.same_name { 0.20 } else { 0.0 };
        // Weakest of the signals, and weighted like it: in a crate whose
        // modules all sit directly in `src/`, "same directory" is free.
        let local = if self.same_dir { 0.05 } else { 0.0 };
        (0.25 + 0.20 * bulk + 0.175 * spread + named + local).min(1.0)
    }

    /// The shortest description of what is duplicated.
    fn make_label(members: &[&BodyFact], same_name: bool) -> String {
        if same_name {
            members[0].name.clone()
        } else {
            // Distinct names: name the group by its members so the row is
            // still greppable.
            let mut names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            names.join("/")
        }
    }
}

/// Directory of a display path, for the `same_dir` signal.
fn dir_of(file: &str) -> &str {
    file.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Minimum canonical tokens before a body can be part of a group. A `{ self.0 }`
/// accessor repeated thirty times is a data point about the language, not about
/// the codebase.
pub const DEFAULT_MIN_TOKENS: usize = 24;

/// The score at or above which a clone group is a gating audit finding.
///
/// Set so that the class it admits is "the same named helper, written out more
/// than twice, in one directory" — the `parse_uuid` shape, where the fix is a
/// `mod` and a `use` and it deletes findings rather than moving them.
///
/// Two identical copies of one helper in two modules lands at 0.65 and stays
/// advisory on purpose. It is a real lead — the pair can drift, and on this
/// codebase `pat_is_ok` and `peel` are exactly that — but a pair is also how a
/// deliberate boundary between two crates looks, and gating on it would put the
/// whole list in the gate. At 0.75 the gate is the groups where copy count or
/// bulk has already answered the question.
pub const GATING_SCORE: f64 = 0.75;

pub fn run(ctx: &AnalysisCtx, min_tokens: usize) -> anyhow::Result<usize> {
    Ok(run_counted(ctx, min_tokens, 0.0)?.total)
}

/// As [`run`], but also reporting how many groups clear [`GATING_SCORE`].
///
/// `min_score` drops groups below a floor. This check ranks its rows and gates
/// on the top tier, and — like `error-swallows`, and unlike the three drift
/// checks it sits next to — used to offer no way to ask for that tier.
pub fn run_counted(
    ctx: &AnalysisCtx,
    min_tokens: usize,
    min_score: f64,
) -> anyhow::Result<Counts> {
    let corpus: &Corpus = ctx.corpus;
    let bodies: Vec<&BodyFact> = corpus
        .bodies
        .iter()
        .filter(|b| b.tokens >= min_tokens)
        .collect();
    let scanned = bodies.len();

    let mut by_canon: HashMap<String, Vec<&BodyFact>> = HashMap::new();
    for b in bodies {
        by_canon.entry(b.canon()).or_default().push(b);
    }

    let mut groups: Vec<Group> = by_canon
        .into_values()
        .filter(|m| m.len() > 1)
        .map(|mut members| {
            members.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));
            let tokens = members[0].tokens;
            let same_name = members.iter().all(|m| m.name == members[0].name);
            let d = dir_of(&members[0].file);
            let same_dir = members.iter().all(|m| dir_of(&m.file) == d);
            let label = Group::make_label(&members, same_name);
            Group {
                members,
                tokens,
                same_name,
                same_dir,
                label,
            }
        })
        .collect();

    // `--changed-since` keeps a group when *any* copy is in the changed set:
    // the finding is the duplication, and you can act on it from either end.
    if ctx.changed.is_some() {
        groups.retain(|g| g.members.iter().any(|m| ctx.in_scope(&m.file)));
    }

    // Keyed on the group label — the same key `--suggest-waivers` prints.
    // These two disagreed: the suggestion said `ok(clones/<label>)` while the
    // filter matched on an empty key, so the comment the tool told you to write
    // was inert and only a bare `ok(clones)` did anything. A suggestion that
    // does nothing is worse than no suggestion, because it looks like it worked.
    let waived = ctx.retain_unsuppressed("clones", &mut groups, |g| {
        crate::suppress::Site::keyed(g.members[0].file.as_str(), g.members[0].line, &g.label)
    });

    // Before the counts below, so a filtered row is not a finding rather than
    // a hidden one.
    let below_floor = if min_score > 0.0 {
        let n = groups.len();
        groups.retain(|g| g.score() >= min_score);
        n - groups.len()
    } else {
        0
    };

    groups.sort_by(|a, b| {
        b.score()
            .partial_cmp(&a.score())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.members.len().cmp(&a.members.len()))
            .then_with(|| a.members[0].file.cmp(&b.members[0].file))
            .then_with(|| a.members[0].line.cmp(&b.members[0].line))
    });

    if !ctx.summary {
        let today = crate::suppress::Date::today();
        for g in &groups {
            let label = &g.label;
            let first = g.members[0];
            // `at` stays a real site so `--json` keeps file and line as fields
            // and `--context` can quote the source. The remaining copies ride in
            // one text column: a group has a variable number of members and the
            // row shape has to be fixed.
            row!(
                ctx.out,
                "what" => label.clone(),
                "score" => format!("{:.2}", g.score()),
                "copies" => g.members.len().to_string(),
                "tokens" => g.tokens.to_string(),
                "at" => site(&first.file, first.line),
                "fn" => first.qpath.clone(),
                "others" => g.members[1..]
                    .iter()
                    .map(|m| format!("{} {}:{}", m.qpath, m.file, m.line))
                    .collect::<Vec<_>>()
                    .join("  "),
            );
            ctx.suggest("clones", Some(label), today);
        }
    }

    let copies: usize = groups.iter().map(|g| g.members.len()).sum();
    let gating = groups.iter().filter(|g| g.score() >= GATING_SCORE).count();
    ctx.out.summary(&format!(
        "({} duplicated bod(ies) across {} group(s){}{}; {} fn(s) scanned; \
         min_tokens={}{}; explain: replication)",
        copies,
        groups.len(),
        if gating > 0 {
            format!(
                ", {} at score >= {:.2} (the tier `audit` gates on)",
                gating, GATING_SCORE
            )
        } else {
            String::new()
        },
        if below_floor > 0 {
            format!("; {} below --min-score {:.2}", below_floor, min_score)
        } else {
            String::new()
        },
        scanned,
        min_tokens,
        ctx.waived_note(waived)
    ));
    Ok(Counts {
        total: groups.len(),
        gating,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The canonicalization tests live beside the canonicalizer, in `facts`.
    // This module now tests only what it still owns: how a group is ranked.

    fn body(name: &str, file: &str, tokens: usize) -> BodyFact {
        BodyFact {
            name: name.into(),
            qpath: format!("m::{name}"),
            file: file.into(),
            line: 1,
            end: 9,
            tokens,
            skeleton: "·".into(),
            leaves: vec!["c".into()],
        }
    }

    fn group(members: &[BodyFact]) -> Group<'_> {
        let members: Vec<&BodyFact> = members.iter().collect();
        let tokens = members[0].tokens;
        let same_name = members.iter().all(|m| m.name == members[0].name);
        let d = dir_of(&members[0].file);
        let same_dir = members.iter().all(|m| dir_of(&m.file) == d);
        let label = Group::make_label(&members, same_name);
        Group {
            members,
            tokens,
            same_name,
            same_dir,
            label,
        }
    }

    /// The `parse_uuid` shape — one named helper, five copies, one directory —
    /// must gate. That is the finding the check was built to produce.
    #[test]
    fn the_same_helper_copied_across_one_directory_gates() {
        let bodies: Vec<BodyFact> = (0..5)
            .map(|i| body("parse_uuid", &format!("src/services/s{i}.rs"), 30))
            .collect();
        let g = group(&bodies);
        assert!(
            g.score() >= GATING_SCORE,
            "score {:.2} should gate",
            g.score()
        );
    }

    /// Two small, differently-named bodies in unrelated directories are a lead,
    /// not a gate.
    #[test]
    fn an_incidental_pair_does_not_gate() {
        let bodies = [body("encode", "src/a/x.rs", 24), body("write_tag", "src/b/y.rs", 24)];
        let g = group(&bodies);
        assert!(
            g.score() < GATING_SCORE,
            "score {:.2} should not gate",
            g.score()
        );
    }

    /// One helper duplicated in exactly two modules is a real lead that can
    /// drift, and it stays advisory: a pair is also how a deliberate boundary
    /// between two crates looks, and gating on pairs puts the whole list in
    /// the gate.
    #[test]
    fn a_named_pair_reports_but_does_not_gate() {
        let bodies = [
            body("pat_is_ok", "src/divergence.rs", 81),
            body("pat_is_ok", "src/error_swallows.rs", 81),
        ];
        let g = group(&bodies);
        assert!(g.score() > 0.5, "a named pair is still worth reading");
        assert!(
            g.score() < GATING_SCORE,
            "score {:.2} should not gate",
            g.score()
        );
    }

    /// More copies of more code always ranks higher, so the top of the list is
    /// the biggest lever.
    #[test]
    fn score_rises_with_bulk_and_copies() {
        let small = [body("f", "src/a.rs", 24), body("f", "src/b.rs", 24)];
        let bigger = [body("f", "src/a.rs", 80), body("f", "src/b.rs", 80)];
        let more: Vec<BodyFact> = (0..4)
            .map(|i| body("f", &format!("src/{i}.rs"), 80))
            .collect();
        assert!(group(&bigger).score() > group(&small).score());
        assert!(group(&more).score() > group(&bigger).score());
    }

    #[test]
    fn label_names_the_helper_when_every_copy_agrees() {
        let same = [body("parse_uuid", "src/a.rs", 30), body("parse_uuid", "src/b.rs", 30)];
        assert_eq!(group(&same).label, "parse_uuid");
        let differing = [body("to_pb", "src/a.rs", 30), body("into_proto", "src/b.rs", 30)];
        assert_eq!(group(&differing).label, "into_proto/to_pb");
    }
}
