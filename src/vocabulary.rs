//! `vocabulary` — the concepts this codebase has *declared*, and where they leak.
//!
//! ```text
//! /// The canonical identifier for a user.
//! ///
//! /// unruster: concept(user.id)
//! pub struct UserId(u64);
//! ```
//!
//! # Why a declaration, when everything else here is derived
//!
//! Every other check in this tool computes its findings from the source and
//! asks the reader for nothing. That is the right default, and it has a
//! ceiling: [`crate::concepts`] can tell you that `UserId`, `AccountId` and
//! `OwnerId` look like one concept, and it can never tell you **which of the
//! three is supposed to be the one**. That is a decision, not a measurement,
//! and only a person can record it.
//!
//! # Why this declaration is safe to add, when most are not
//!
//! An annotation that nothing can contradict is worse than no annotation: it
//! drifts exactly as the code drifts, and it drifts *silently*, because a
//! reader who finds a marker believes it. So the rule for adding one here is
//! that it must have a falsifier — something that can make it wrong and say so.
//! `concept(…)` has three:
//!
//! * **`duplicate`** — two items claim one concept. This is the whole point:
//!   the second claimant is a compile-clean, test-clean, review-clean way to
//!   split a concept in half, and it is now a finding.
//! * **`undeclared`** — an item that [`crate::concepts`] clusters with a
//!   declared one, claiming nothing. The canonical home exists and something
//!   grew up beside it.
//! * **`malformed`** — `concept()` with no name. Recorded rather than ignored,
//!   because a marker the tool quietly skips is one the author believes is
//!   working.
//!
//! This is the same discipline `waivers` already applies to the negative
//! marker: a `// unruster: ok(…)` that suppresses nothing, or names a check
//! that does not exist, is reported rather than trusted. `/// unruster: sealed`
//! established that a *positive* marker can live in the docs; this gives the
//! form an argument and a check.
//!
//! # What it deliberately does not do
//!
//! It does not require declarations. A codebase with no `concept(…)` markers
//! reports nothing, and `--coverage` is how you ask the opposite question
//! ("which clusters have no canonical home?"). A vocabulary nobody adopted must
//! not turn into a wall of findings on first run.

use std::collections::BTreeMap;

use crate::context::{AnalysisCtx, Counts};
use crate::corpus::Corpus;
use crate::emit::{row, site};
use crate::facts::ItemFact;

/// What is wrong with a declaration, or with the code around one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Status {
    /// Listed only under `--all`: a healthy, unique claim.
    Ok,
    /// A cluster of look-alike declarations with no canonical claim at all.
    /// Advisory, and only under `--coverage`.
    Unclaimed,
    /// Something clusters with a declared concept and claims nothing.
    Undeclared,
    /// `concept()` with no name in it.
    Malformed,
    /// Two items claim one concept.
    Duplicate,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Unclaimed => "unclaimed",
            Status::Undeclared => "undeclared",
            Status::Malformed => "malformed",
            Status::Duplicate => "duplicate",
        }
    }

    /// Does this hold an `audit` loop open? `unclaimed` does not: a codebase
    /// that has not adopted the vocabulary would otherwise fail its own gate
    /// on first run, which is how a gate stops being run.
    fn gating(self) -> bool {
        matches!(
            self,
            Status::Duplicate | Status::Malformed | Status::Undeclared
        )
    }
}

struct Finding<'a> {
    status: Status,
    concept: String,
    item: &'a ItemFact,
    note: String,
}

pub struct Opts {
    /// Also list healthy declarations, so the command doubles as the ledger.
    pub all: bool,
    /// Report look-alike clusters that no declaration covers.
    pub coverage: bool,
}

/// Every item carrying a `concept(…)` marker, keyed by the name it claims.
///
/// Local items are excluded like everywhere else — a marker inside a fn body
/// claims a concept nothing outside can see.
fn claims(c: &Corpus) -> BTreeMap<&str, Vec<&ItemFact>> {
    let mut m: BTreeMap<&str, Vec<&ItemFact>> = BTreeMap::new();
    for i in c.declarations() {
        if let Some(name) = &i.concept {
            m.entry(name.as_str()).or_default().push(i);
        }
    }
    for v in m.values_mut() {
        v.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));
    }
    m
}

pub fn run(ctx: &AnalysisCtx, corpus: &Corpus, opts: &Opts) -> anyhow::Result<usize> {
    Ok(run_counted(ctx, corpus, opts)?.total)
}

pub fn run_counted(ctx: &AnalysisCtx, corpus: &Corpus, opts: &Opts) -> anyhow::Result<Counts> {
    let claimed = claims(corpus);
    let declared_total: usize = claimed.values().map(Vec::len).sum();
    let mut findings: Vec<Finding> = Vec::new();

    for (name, items) in &claimed {
        if name.is_empty() {
            for i in items {
                findings.push(Finding {
                    status: Status::Malformed,
                    concept: "—".to_string(),
                    item: i,
                    note: "`concept()` names nothing, so nothing is claimed and no \
                           uniqueness check can fire"
                        .to_string(),
                });
            }
            continue;
        }
        if items.len() > 1 {
            // Every claimant is reported, not just the second: which one *should*
            // be canonical is the reader's decision, and leading with an
            // arbitrary "first" would be the tool making it for them.
            for i in items {
                let others: Vec<String> = items
                    .iter()
                    .filter(|o| !std::ptr::eq(**o, *i))
                    .map(|o| format!("{} {}:{}", o.qpath, o.file, o.line))
                    .collect();
                findings.push(Finding {
                    status: Status::Duplicate,
                    concept: (*name).to_string(),
                    item: i,
                    note: format!("also claimed by {}", others.join("  ")),
                });
            }
            continue;
        }
        if opts.all {
            findings.push(Finding {
                status: Status::Ok,
                concept: (*name).to_string(),
                item: items[0],
                note: "sole claimant".to_string(),
            });
        }
    }

    // The other half: look-alikes of something already declared. This is where
    // the declaration earns its keep — `concepts` found the cluster, and the
    // marker is what turns "these three resemble each other" into "this one is
    // the home and that one drifted away from it".
    //
    // At the reporting floor, deliberately. The two statuses below ask
    // different questions and need different evidence:
    //
    // * `undeclared` has a *declared home in the cluster*. Somebody wrote the
    //   marker, which is the strongest signal available — the cluster's score
    //   is beside the point, because the reader has already said this concept
    //   is real. Raising the floor here hides the drift the marker exists to
    //   catch: on a real codebase the `PlacementTool` / `ParametricDef` pair
    //   scores 0.63, and a gating floor would have made the one row the
    //   vocabulary produced there vanish — and orphaned the waiver written
    //   against it.
    // * `unclaimed` has no marker at all, so all it has is the score. That one
    //   *does* take the gating tier: see its arm below.
    for (score, cluster) in crate::concepts::clusters(corpus, None) {
        let declared: Vec<&ItemFact> = cluster
            .iter()
            .filter(|m| m.concept.as_deref().is_some_and(|c| !c.is_empty()))
            .copied()
            .collect();
        match declared.as_slice() {
            // Nobody has claimed anything here, so the cluster's own score is
            // the only evidence, and `--coverage` is advice rather than a
            // finding. At the reporting floor it was advice nobody could act
            // on: 270 unclaimed clusters on a real codebase, "mostly `label()`
            // methods and action newtypes where one concept, many declarations
            // is just Rust". A suggestion list that long teaches a reader to
            // stop reading suggestion lists.
            [] if opts.coverage && score >= crate::concepts::GATING_SCORE => {
                let first = cluster[0];
                findings.push(Finding {
                    status: Status::Unclaimed,
                    concept: "—".to_string(),
                    item: first,
                    note: format!(
                        "{} look-alike declaration(s) and no `concept(…)` among them: {}",
                        cluster.len(),
                        cluster
                            .iter()
                            .skip(1)
                            .map(|m| m.qpath.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
            [home] => {
                for m in &cluster {
                    if std::ptr::eq(*m, *home) || m.concept.is_some() {
                        continue;
                    }
                    findings.push(Finding {
                        status: Status::Undeclared,
                        concept: home.concept.clone().unwrap_or_default(),
                        item: m,
                        note: format!(
                            "resembles `{}`, the declared home of this concept ({}:{})",
                            home.qpath, home.file, home.line
                        ),
                    });
                }
            }
            // Two declared homes in one cluster is already reported as a
            // `duplicate` when they claim the same name, and is a legitimate
            // pair of distinct concepts when they do not.
            _ => {}
        }
    }

    if ctx.changed.is_some() {
        findings.retain(|f| ctx.in_scope(&f.item.file));
    }
    let waived = ctx.retain_unsuppressed("vocabulary", &mut findings, |f| {
        crate::suppress::Site::keyed(f.item.file.as_str(), f.item.line, &f.concept)
    });

    findings.sort_by(|a, b| {
        b.status
            .cmp(&a.status)
            .then_with(|| a.concept.cmp(&b.concept))
            .then_with(|| a.item.file.cmp(&b.item.file))
            .then_with(|| a.item.line.cmp(&b.item.line))
    });
    // A duplicate reports every claimant, so the same (file, line) can only
    // appear once per status; anything else would be one item counted twice.
    findings.dedup_by(|a, b| {
        a.status == b.status && a.item.file == b.item.file && a.item.line == b.item.line
    });

    if !ctx.summary {
        let today = crate::suppress::Date::today();
        for f in &findings {
            row!(
                ctx.out,
                "status" => f.status.as_str(),
                "concept" => f.concept.clone(),
                "at" => site(&f.item.file, f.item.line),
                "item" => f.item.qpath.clone(),
                "note" => f.note.clone(),
            );
            if f.status == Status::Undeclared || f.status == Status::Unclaimed {
                ctx.out.hint(&format!(
                    "  /// unruster: concept({})    ← on the one that should be canonical",
                    if f.concept == "—" {
                        "your.concept.name"
                    } else {
                        f.concept.as_str()
                    }
                ));
            }
            ctx.suggest("vocabulary", Some(&f.concept), today);
        }
    }

    let gating = findings.iter().filter(|f| f.status.gating()).count();
    ctx.out.summary(&format!(
        "({} finding(s){}; {} concept(s) declared across {} item(s){}{}; \
         explain: vocabulary)",
        findings.len(),
        if gating > 0 {
            format!(", {} gating (duplicate/malformed/undeclared)", gating)
        } else {
            String::new()
        },
        claimed.keys().filter(|k| !k.is_empty()).count(),
        declared_total,
        if opts.coverage {
            String::new()
        } else {
            "; --coverage also reports look-alike clusters nobody has claimed".to_string()
        },
        ctx.waived_note(waived)
    ));
    Ok(Counts {
        total: findings.len(),
        gating,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus_of(srcs: &[(&str, &str)]) -> Corpus {
        let mut c = Corpus::default();
        for (path, src) in srcs {
            let pf = crate::parse::ParsedFile {
                path: std::path::PathBuf::from(path),
                ast: syn::parse_file(src).expect("parse"),
                module: crate::parse::module_of(
                    std::path::Path::new("."),
                    std::path::Path::new(path),
                ),
            };
            let f = crate::facts::derive(&pf);
            c.items.extend(f.items);
            c.bodies.extend(f.bodies);
        }
        c
    }

    #[test]
    fn a_marker_is_read_off_the_doc_comment() {
        let c = corpus_of(&[(
            "src/a.rs",
            "/// The id of a user.\n/// unruster: concept(user.id)\npub struct UserId(u64);",
        )]);
        assert_eq!(c.items[0].concept.as_deref(), Some("user.id"));
    }

    /// The falsifier the whole design turns on: a second claimant is a
    /// compile-clean, review-clean way to split a concept in half.
    #[test]
    fn two_claimants_of_one_concept_are_both_reported() {
        let c = corpus_of(&[
            ("src/a.rs", "/// unruster: concept(user.id)\npub struct UserId(u64);"),
            ("src/b.rs", "/// unruster: concept(user.id)\npub struct Principal(u64);"),
        ]);
        let m = claims(&c);
        assert_eq!(m["user.id"].len(), 2);
    }

    #[test]
    fn a_nameless_marker_is_recorded_rather_than_ignored() {
        let c = corpus_of(&[("src/a.rs", "/// unruster: concept()\npub struct X(u64);")]);
        assert_eq!(c.items[0].concept.as_deref(), Some(""));
    }

    #[test]
    fn a_bare_word_is_not_a_marker() {
        let c = corpus_of(&[(
            "src/a.rs",
            "/// This documents the `concept(user.id)` grammar itself.\npub struct X(u64);",
        )]);
        assert_eq!(c.items[0].concept, None);
    }

    /// The marker is a declaration, not a waiver, so it lives in `///` — and a
    /// `//` line comment must not be mistaken for one.
    #[test]
    fn a_line_comment_is_not_a_declaration() {
        let c = corpus_of(&[("src/a.rs", "// unruster: concept(user.id)\npub struct X(u64);")]);
        assert_eq!(c.items[0].concept, None);
    }

    #[test]
    fn a_marker_on_one_item_does_not_leak_to_the_next() {
        let c = corpus_of(&[(
            "src/a.rs",
            "/// unruster: concept(user.id)\npub struct UserId(u64);\npub struct Other(u64);",
        )]);
        assert_eq!(c.items[1].concept, None);
    }

    /// `sealed` shares the parser now, so it has to keep working — including
    /// rejecting the near-spelling the old inline `contains` accepted.
    #[test]
    fn the_shared_parser_still_reads_sealed_and_rejects_sealedish() {
        let f: syn::ItemEnum =
            syn::parse_str("/// unruster: sealed\npub enum E { A }").expect("parse");
        assert_eq!(crate::ast::doc_marker(&f.attrs, "sealed"), Some(None));
        let g: syn::ItemEnum =
            syn::parse_str("/// unruster: sealedish\npub enum E { A }").expect("parse");
        assert_eq!(crate::ast::doc_marker(&g.attrs, "sealed"), None);
    }
}
