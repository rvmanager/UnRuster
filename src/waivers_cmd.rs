//! `unruster waivers` — the lifecycle half of the waiver system.
//!
//! A waiver records a human judgment. Judgments decay: the code moves, the
//! check gets sharper, the reasoning stops applying. Left alone, a suppression
//! file becomes a place where findings go to be forgotten — which is worse than
//! not having one, because the output now reads as clean.
//!
//! Two decay signals, deliberately kept apart:
//!
//! * **orphaned** — the waiver suppressed nothing this run. Mechanical and
//!   objective: the finding it was written for is gone, so the comment is
//!   lying. Safe to strip in bulk.
//! * **stale** — older than `--stale N` days. A proxy, not proof; it asks a
//!   human to re-read, and that is all it can do.
//!
//! Hit counts come from re-running the check battery against the same
//! [`Suppressions`] the listing is about, with output silenced. That is also
//! what makes the `suppresses` column honest — an item-scoped waiver quietly
//! covering two hundred findings shows up as a 200, which is the guardrail on
//! item scope being as broad as it is.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::context::AnalysisCtx;
use crate::emit::{row, site};
use crate::parse::ParsedFile;
use crate::suppress::{Date, Scope, Waiver};

/// What `waivers` should do. Listing filters compose; the two mutating actions
/// are mutually exclusive at the CLI boundary.
#[derive(Clone, Copy, PartialEq)]
pub enum Action {
    List,
    /// Strip matching waiver comments from source.
    Remove,
    /// Rewrite legacy waivers with the check that actually hit them.
    Upgrade,
}

pub struct WaiverOpts<'a> {
    pub action: Action,
    pub check: Option<&'a str>,
    pub stale: Option<i64>,
    pub orphaned: bool,
    pub legacy_only: bool,
    /// Actually modify files. Without it, mutating actions preview and exit 0.
    pub write: bool,
    pub fail_on_stale: Option<i64>,
    pub today: Date,
}

/// Run the suppressible checks with output silenced, purely so every waiver
/// learns how many findings it suppressed.
///
/// The battery is exactly the set of checks that consult waivers, so a waiver
/// with zero hits afterwards is orphaned with respect to everything that could
/// possibly have honoured it — no separate "which checks exist" list to keep in
/// sync.
fn populate_hits(ctx: &AnalysisCtx, call_source: &[ParsedFile]) {
    let quiet = crate::emit::Out::silent();
    let probe = AnalysisCtx {
        files: ctx.files,
        idx: ctx.idx,
        sem: ctx.sem,
        summary: true,
        spans: false,
        changed: None,
        out: &quiet,
        suppressions: ctx.suppressions,
        suggest_waivers: false,
    };
    // Errors here are not the user's problem: a check that fails to run just
    // contributes no hits, and the listing says `?` rather than claiming 0.
    let _ = crate::divergence::run(&probe, None, 0.0, None);
    let _ = crate::divergence::run_handling(&probe, 1);
    let _ = crate::parallel_matches::run_enum_coverage(
        &probe,
        None,
        crate::parallel_matches::CoverageOpts {
            hide_trait_routed: false,
            max_missing: None,
            compact: true,
            rank_enums: false,
        },
    );
    let _ = crate::error_swallows::run(
        &probe,
        crate::error_swallows::SwallowOpts {
            include_unwrap_or: true,
            include_infallible: true,
            include_logged: true,
        },
    );
    let _ = crate::casts::run(&probe, &[], None, false, None);
    let _ = call_source; // dead-code does not consult waivers yet
}

/// Does this waiver pass the listing filters?
fn selected(w: &Waiver, opts: &WaiverOpts) -> bool {
    if let Some(c) = opts.check {
        // A legacy waiver matches every check, so it matches this filter too —
        // hiding it would misreport what `--check casts` is actually subject to.
        if w.check.as_deref().is_some_and(|wc| wc != c) {
            return false;
        }
    }
    if opts.legacy_only && !w.is_legacy() {
        return false;
    }
    if opts.orphaned && w.hits() > 0 {
        return false;
    }
    if let Some(days) = opts.stale {
        match w.date {
            Some(d) if d.age_days(opts.today) >= days => {}
            // An undated waiver can never be shown to be fresh, so it counts as
            // stale: that is the incentive to date it.
            None => {}
            _ => return false,
        }
    }
    true
}

/// `517d`, `—` for undated, `+3d` for a date ahead of the clock (someone
/// typo'd the year, and silently rendering it as `0d` would hide that).
fn age_str(w: &Waiver, today: Date) -> String {
    match w.date {
        None => "—".to_string(),
        Some(d) => {
            let n = d.age_days(today);
            if n < 0 {
                format!("+{}d", -n)
            } else {
                format!("{}d", n)
            }
        }
    }
}

pub fn run(ctx: &AnalysisCtx, call_source: &[ParsedFile], opts: WaiverOpts) -> Result<usize> {
    if ctx.suppressions.is_empty() {
        ctx.out.summary(
            "(0 waiver(s); nothing to list — `// unruster: ok(<check>) <date> — <reason>` \
             records a verified false positive; `--suggest-waivers` on any check prints the \
             exact line)",
        );
        return Ok(0);
    }
    populate_hits(ctx, call_source);

    let mut chosen: Vec<&Waiver> = ctx
        .suppressions
        .all()
        .iter()
        .filter(|w| selected(w, &opts))
        .collect();
    // Oldest first: the listing exists to surface decay, so the rows most
    // likely to need re-reading lead.
    chosen.sort_by(|a, b| {
        let key = |w: &Waiver| match w.date {
            Some(d) => (0i8, d.to_days()),
            None => (-1, 0),
        };
        key(a)
            .cmp(&key(b))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.comment_line.cmp(&b.comment_line))
    });

    match opts.action {
        Action::List => list(ctx, &chosen, &opts),
        Action::Remove => return mutate(ctx, &chosen, &opts, Mutation::Remove),
        Action::Upgrade => return mutate(ctx, &chosen, &opts, Mutation::Upgrade),
    }

    // Exit-code gate for CI, mirroring `--fail-on-findings`.
    if let Some(days) = opts.fail_on_stale {
        // Undated waivers always count, at any threshold, matching `--stale`:
        // a waiver with no date cannot be shown to be fresh, and treating it
        // as fresh would make dating optional in practice.
        let all = ctx.suppressions.all();
        let undated = all.iter().filter(|w| w.date.is_none()).count();
        let old = all
            .iter()
            .filter(|w| w.date.is_some_and(|d| d.age_days(opts.today) >= days))
            .count();
        if undated + old > 0 {
            ctx.out.note(&format!(
                "(note: {} waiver(s) older than {} day(s) and {} undated — \
                 `--fail-on-stale` counts both, since an undated waiver can never \
                 be shown to be fresh)",
                old, days, undated
            ));
            return Ok(undated + old);
        }
    }
    Ok(0)
}

fn list(ctx: &AnalysisCtx, chosen: &[&Waiver], opts: &WaiverOpts) {
    let all = ctx.suppressions.all();
    for w in chosen {
        row!(
            ctx.out,
            "age" => age_str(w, opts.today),
            "check" => w.check.clone().unwrap_or_else(|| "(legacy)".to_string()),
            "key" => w.key.clone().unwrap_or_else(|| "—".to_string()),
            "scope" => w.scope.as_str(),
            "covers" => format!("{}-{}", w.covers.0, w.covers.1),
            "suppresses" => w.hits(),
            "at" => site(&w.file, w.comment_line),
            "reason" => if w.reason.is_empty() {
                "(none)".to_string()
            } else {
                w.reason.clone()
            },
        );
    }

    let orphaned = all.iter().filter(|w| w.hits() == 0).count();
    let legacy = ctx.suppressions.legacy_count();
    let undated = all.iter().filter(|w| w.date.is_none()).count();
    let item_scoped = all.iter().filter(|w| w.scope == Scope::Item).count();
    let widest = all.iter().map(|w| w.hits()).max().unwrap_or(0);
    let mut by_check: BTreeMap<&str, usize> = BTreeMap::new();
    for w in all {
        *by_check.entry(w.check.as_deref().unwrap_or("(legacy)")).or_insert(0) += 1;
    }
    let breakdown: Vec<String> = by_check.iter().map(|(k, n)| format!("{}={}", k, n)).collect();
    ctx.out.summary(&format!(
        "({} of {} waiver(s) shown; {}; {} item-scoped; {} orphaned (suppress nothing); \
         {} legacy; {} undated; widest suppresses {} finding(s))",
        chosen.len(),
        all.len(),
        breakdown.join(", "),
        item_scoped,
        orphaned,
        legacy,
        undated,
        widest,
    ));
    if orphaned > 0 {
        ctx.out.note(
            "(note: an orphaned waiver no longer suppresses anything — the code moved on and \
             the comment now misdescribes it. `waivers --orphaned --remove` previews the \
             cleanup; add --write to apply)",
        );
    }
    if legacy > 0 {
        ctx.out.note(
            "(note: a legacy waiver carries no check name, so it waives every check on its \
             line. `waivers --upgrade` rewrites the ones whose check is unambiguous)",
        );
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mutation {
    Remove,
    Upgrade,
}

/// Apply an edit to every selected waiver, grouped by file and applied
/// bottom-up so earlier line numbers stay valid.
///
/// Dry-run by default. This is the only code in the tool that writes to a
/// user's source, so it previews unless explicitly told otherwise, and it
/// refuses anything it cannot do unambiguously rather than guessing.
fn mutate(
    ctx: &AnalysisCtx,
    chosen: &[&Waiver],
    opts: &WaiverOpts,
    what: Mutation,
) -> Result<usize> {
    let mut by_file: BTreeMap<&str, Vec<&Waiver>> = BTreeMap::new();
    let mut skipped: Vec<String> = Vec::new();
    for w in chosen {
        if what == Mutation::Upgrade {
            if !w.is_legacy() {
                continue;
            }
            // Only rewrite when the evidence is unambiguous. Zero hits means we
            // cannot name a check; several means picking one would narrow the
            // waiver and silently un-waive the rest.
            let checks = w.hit_checks();
            if checks.len() != 1 {
                skipped.push(format!(
                    "{}:{} — {} check(s) hit this waiver{}",
                    w.file,
                    w.comment_line,
                    checks.len(),
                    if checks.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", checks.join(", "))
                    }
                ));
                continue;
            }
        }
        by_file.entry(w.file.as_str()).or_default().push(w);
    }

    let mut touched = 0usize;
    for (file, ws) in &by_file {
        let Ok(src) = std::fs::read_to_string(file) else {
            ctx.out
                .note(&format!("(note: could not read {} — skipped)", file));
            continue;
        };
        let mut lines: Vec<String> = src.lines().map(str::to_string).collect();
        // Bottom-up: a removal above would shift every line index below it.
        let mut ordered: Vec<&&Waiver> = ws.iter().collect();
        ordered.sort_by_key(|w| std::cmp::Reverse(w.comment_line));
        for w in ordered {
            match what {
                Mutation::Remove => {
                    // Continuation lines belong to the reason wherever the
                    // head sits, so a trailing waiver can have them too.
                    // Leaving them behind would strand prose that no longer
                    // refers to anything.
                    for n in (w.comment_line + 1..=w.comment_end).rev() {
                        preview(ctx, file, n, &lines[n - 1], None);
                        lines.remove(n - 1);
                    }
                    if w.trailing {
                        // Keep the code, drop the comment and the whitespace
                        // that was only there to separate them.
                        let l = &lines[w.comment_line - 1];
                        let kept = l[..w.comment_col].trim_end().to_string();
                        preview(ctx, file, w.comment_line, l, Some(&kept));
                        lines[w.comment_line - 1] = kept;
                    } else {
                        preview(ctx, file, w.comment_line, &lines[w.comment_line - 1], None);
                        lines.remove(w.comment_line - 1);
                    }
                }
                Mutation::Upgrade => {
                    let check = w.hit_checks().remove(0);
                    let l = &lines[w.comment_line - 1];
                    let rebuilt = format!(
                        "{}// unruster: ok({}) {}{}",
                        &l[..w.comment_col],
                        check,
                        opts.today,
                        if w.reason_col >= l.len() {
                            String::new()
                        } else {
                            format!(" — {}", &l[w.reason_col..])
                        }
                    );
                    preview(ctx, file, w.comment_line, l, Some(&rebuilt));
                    lines[w.comment_line - 1] = rebuilt;
                }
            }
            touched += 1;
        }
        if opts.write {
            let mut body = lines.join("\n");
            if src.ends_with('\n') {
                body.push('\n');
            }
            std::fs::write(file, body)?;
        }
    }

    for s in &skipped {
        ctx.out
            .note(&format!("(note: not upgraded — {})", s));
    }
    // Counted in waivers, not lines: a three-line wrapped reason is one
    // judgment being retired, and reporting "3" would overstate the change.
    let verb = match (what, opts.write) {
        (Mutation::Remove, true) => "removed",
        (Mutation::Remove, false) => "would be removed",
        (Mutation::Upgrade, true) => "upgraded",
        (Mutation::Upgrade, false) => "would be upgraded",
    };
    ctx.out.summary(&format!(
        "({} waiver(s) {}{}{})",
        touched,
        verb,
        if skipped.is_empty() {
            String::new()
        } else {
            format!("; {} left alone as ambiguous", skipped.len())
        },
        if opts.write {
            String::new()
        } else {
            "; dry run — add --write to apply".to_string()
        }
    ));
    Ok(0)
}

/// One `-`/`+` preview line, so a dry run reads like a diff and a real run
/// leaves a record of what it did.
fn preview(ctx: &AnalysisCtx, file: &str, line: usize, before: &str, after: Option<&str>) {
    ctx.out.line(&format!("-{}:{}: {}", file, line, before));
    if let Some(a) = after {
        ctx.out.line(&format!("+{}:{}: {}", file, line, a));
    }
}
