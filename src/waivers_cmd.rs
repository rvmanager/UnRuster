//! `unruster waivers` — the lifecycle half of the waiver system.
//!
//! A waiver records a human judgment. Judgments decay: the code moves, the
//! check gets sharper, the reasoning stops applying. Left alone, a suppression
//! file becomes a place where findings go to be forgotten — which is worse than
//! not having one, because the output now reads as clean.
//!
//! Two decay signals, deliberately kept apart:
//!
//! * **orphaned** — the waiver suppresses nothing the *audit battery* would
//!   have reported. Mechanical and objective. Two sub-cases, distinguished by
//!   the `below_audit` column: the finding is gone entirely (the comment now
//!   lies), or it only exists below audit's thresholds (harmless, but dead
//!   weight in the only loop that gates).
//! * **stale** — older than `--stale N` days. A proxy, not proof; it asks a
//!   human to re-read, and that is all it can do.
//!
//! Hit counts come from re-running the battery twice against the same
//! [`Suppressions`] the listing is about, output silenced: once configured
//! exactly as `audit` runs it, once wide open. Only the first decides whether a
//! waiver is earning its place. Counting a single permissive pass is what let a
//! real codebase report "0 orphaned" while a third of its ledger had stopped
//! mattering — the tool disagreed with its own audit line in the same run.
//!
//! The `suppresses` column is also the guardrail on item scope: a waiver
//! quietly covering two hundred findings shows up as a 200.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::context::AnalysisCtx;
use crate::emit::{row, site};
use crate::parse::ParsedFile;
use crate::suppress::{Date, HitMode, Scope, Waiver};

/// What `waivers` should do. Listing filters compose; the two mutating actions
/// are mutually exclusive at the CLI boundary.
#[derive(Clone, Copy, PartialEq)]
pub enum Action {
    List,
    /// Strip matching waiver comments from source.
    Remove,
    /// Rewrite legacy waivers with the check that actually hit them.
    Upgrade,
    /// Insert waiver comments from a file of verified judgments.
    Apply,
}

pub struct WaiverOpts<'a> {
    pub action: Action,
    pub check: Option<&'a str>,
    pub stale: Option<i64>,
    pub orphaned: bool,
    pub legacy_only: bool,
    /// Only waivers carrying no date. The summary has always *counted* these;
    /// without a way to list them, a reader reached for the tool's own grammar
    /// as a regex — `grep -rn "unruster: ok(" src/ | grep -vE "…"` — or for
    /// `--stale 9999` as a workaround, since a dated waiver cannot be that old.
    pub undated: bool,
    /// Actually modify files. Without it, mutating actions preview and exit 0.
    pub write: bool,
    /// Let `--remove` take the waivers that suppress only findings below
    /// audit's thresholds. Off by default — see the guard in [`mutate`].
    pub include_below_audit: bool,
    /// `--apply <file>`: a TSV of `file, line, check, key, scope, reason` rows
    /// to insert. `-` reads stdin. See [`parse_applications`].
    pub apply: Option<&'a str>,
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
    // The three probe contexts disagree about `summary` on purpose. Hit
    // counting happens in `retain_unsuppressed`, which runs whether or not rows
    // are printed, so this one can skip the row loops; baseline recording
    // happens *in* those loops, so `battery_at_ref` cannot, and `self-check`'s
    // type-query probe compares the counts the runs return, so it cannot
    // either. `config-drift` ranks the disagreement 0.85 because the contexts
    // are otherwise identical — which is exactly why they sit one field apart.
    //
    // No waiver here any more: a third context made `self_check` the first
    // site, so the row anchors there and the waiver that used to sit on this
    // line stopped suppressing anything. `waivers --orphaned` reported it as
    // `0 0` the first time the ledger was rebuilt from scratch. The reasoning
    // was worth keeping; the dead ledger entry was not.
    let probe = AnalysisCtx {
        files: ctx.files,
        idx: ctx.idx,
        sem: ctx.sem,
        corpus: ctx.corpus,
        summary: true,
        spans: false,
        // Deliberately unscoped even when the run carries `--changed-since`.
        // Whether a waiver suppresses anything is a property of the tree; a
        // probe that inherited the diff scope would report every waiver outside
        // it as orphaned, which is the bug this command exists to detect and
        // not one it should manufacture. `run` scopes the *rows* instead.
        changed: None,
        out: &quiet,
        suppressions: ctx.suppressions,
        suggest_waivers: false,
    };

    // Two passes over the same waivers. Pass 1 is configured exactly as
    // `audit` runs the battery, because `audit` is the loop waivers exist to
    // unblock — those hits decide whether a waiver earns its place. Pass 2 is
    // wide open, and the *difference* is what distinguishes "this comment
    // describes nothing" from "this comment describes a row your audit filters
    // out anyway". Counting only pass 2 is what let a real ledger report
    // "0 orphaned" while a third of it had stopped mattering.
    for (mode, cfg) in [
        (HitMode::Gating, crate::audit::BatteryConfig::gating()),
        (HitMode::BelowAudit, crate::audit::BatteryConfig::permissive()),
    ] {
        ctx.suppressions.set_hit_mode(mode);
        crate::audit::run_silent_battery(&probe, call_source, cfg, &crate::audit::Selection::default());
    }
    ctx.suppressions.set_hit_mode(HitMode::Gating);
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
    if opts.undated && w.date.is_some() {
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
    // Before the empty-ledger guard: `--apply` is how a ledger *starts*, and
    // refusing to run without one would make the first batch impossible. It
    // also reads no hit counts, so the two-pass probe below is pure cost.
    if opts.action == Action::Apply {
        let source = opts.apply.expect("--apply carries its input path");
        return apply(ctx, source, &opts);
    }
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
    // `--changed-since` is a global flag whose help promises it "applies to
    // site-listing commands", and this is one — but it used to fall through
    // here, so a scoped run listed the whole ledger and disagreed with the
    // scoped `audit` line that sent the reader over. The scoping goes on the
    // rows only: `populate_hits` deliberately probes with `changed: None`
    // (below), because whether a waiver is orphaned is a fact about the tree
    // and not about this diff. So a scoped listing is "the waivers in my
    // changed files, judged against everything".
    let ledger = chosen.len();
    ctx.retain_changed(&mut chosen, |w| w.file.as_str());
    let out_of_scope = ledger - chosen.len();
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

    // Said before the rows, and said for the mutating actions too: `--remove`
    // now rewrites only files in the diff, which is the useful reading of the
    // flag and also a silent one if nobody names it.
    if out_of_scope > 0 {
        ctx.out.note(&format!(
            "(note: --changed-since held back {} waiver(s) outside the changed files{}. \
             The counts below are the whole ledger's, and every hit count is measured \
             against the whole tree — orphaned is a fact about the code, not about this \
             diff.)",
            out_of_scope,
            match opts.action {
                Action::List => "",
                _ => ", so this acts on the changed files alone",
            }
        ));
    }

    match opts.action {
        Action::List => list(ctx, &chosen, &opts),
        Action::Remove => return mutate(ctx, &chosen, &opts, Mutation::Remove),
        Action::Upgrade => return mutate(ctx, &chosen, &opts, Mutation::Upgrade),
        Action::Apply => unreachable!("handled before the ledger is read"),
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

/// Point out waivers that differ only by check name and could be one comment.
///
/// `divergence` and `enum-coverage` ask the same question of the same site, so
/// verifying it once used to cost two waivers — on a real ledger six of
/// thirty-three carried the reason `same.`, written purely to satisfy the other
/// check. The group key retires both; this is how anyone finds out.
fn note_groupable(ctx: &AnalysisCtx, all: &[Waiver]) {
    let mut seen: BTreeMap<(&str, String, Option<String>), Vec<&str>> = BTreeMap::new();
    for w in all {
        let (Some(check), Some(key)) = (w.check.as_deref(), w.key.clone()) else {
            continue;
        };
        let Some(group) = crate::suppress::group_of(check) else {
            continue;
        };
        seen.entry((group, key, Some(w.file.clone())))
            .or_default()
            .push(check);
    }
    let mut pairs: Vec<String> = seen
        .into_iter()
        .filter(|(_, checks)| {
            let mut c = checks.clone();
            c.sort_unstable();
            c.dedup();
            c.len() > 1
        })
        .map(|((group, key, _), _)| format!("ok({}/{})", group, key))
        .collect();
    pairs.sort();
    pairs.dedup();
    if pairs.is_empty() {
        return;
    }
    ctx.out.note(&format!(
        "(note: {} site(s) carry one waiver per check where a single group key would do — \
         {}{})",
        pairs.len(),
        pairs
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
        if pairs.len() > 3 { ", …" } else { "" }
    ));
}

/// Every waiver sharing one date is normal — `--suggest-waivers` stamps today,
/// so a session's worth lands together — but it reads as a bug (a real reader's
/// first thought was "date parsing issue"), and it means `--stale` will one day
/// fire on the whole ledger at once rather than a few at a time.
fn note_date_herd(ctx: &AnalysisCtx, all: &[Waiver]) {
    let dates: Vec<Date> = all.iter().filter_map(|w| w.date).collect();
    if dates.len() < 5 {
        return;
    }
    let first = dates[0];
    if dates.iter().all(|d| *d == first) {
        ctx.out.note(&format!(
            "(note: all {} dated waiver(s) carry {} — `--suggest-waivers` stamps today, so a \
             batch written in one session ages together and `--stale` will surface the whole \
             ledger at once)",
            dates.len(),
            first
        ));
    }
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
            "below_audit" => w.below_audit(),
            "at" => site(&w.file, w.comment_line),
            "reason" => if w.reason.is_empty() {
                "(none)".to_string()
            } else {
                w.reason.clone()
            },
        );
    }

    let orphaned = all.iter().filter(|w| w.hits() == 0).count();
    let dead = all
        .iter()
        .filter(|w| w.hits() == 0 && w.below_audit() == 0)
        .count();
    let sub_threshold = orphaned - dead;
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
        "({} of {} waiver(s) shown; {}; {} item-scoped; {} earning nothing in `audit` \
         ({} suppress nothing at all, {} only below audit thresholds); {} legacy; \
         {} undated; widest suppresses {} finding(s))",
        chosen.len(),
        all.len(),
        breakdown.join(", "),
        item_scoped,
        orphaned,
        dead,
        sub_threshold,
        legacy,
        undated,
        widest,
    ));
    // The two halves of `orphaned` want opposite actions, and one note covering
    // both used to recommend `--remove --write` over the whole set. That is
    // right for the dead half and destructive for the other: a waiver whose
    // finding merely scores under the gate is still accurate, and deleting it
    // re-exposes the site the next time a threshold moves. `--remove` now holds
    // those back on its own; the advice has to say the same thing the code does.
    if dead > 0 {
        ctx.out.note(&format!(
            "(note: {} waiver(s) suppress nothing at all — the finding is gone and the \
             comment now lies. `waivers --orphaned --remove` previews the cleanup; add \
             --write to apply)",
            dead
        ));
    }
    if sub_threshold > 0 {
        ctx.out.note(&format!(
            "(note: {} waiver(s) suppress only findings below audit's thresholds. The \
             reason still holds and `--remove` will not touch them — they are listed \
             because they are not holding the gating loop open, not because they are \
             wrong. Check one with `<check> --no-suppress` before deciding)",
            sub_threshold
        ));
    }
    note_groupable(ctx, all);
    note_date_herd(ctx, all);
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
        // `--orphaned` selects on the *audit* hit count, and that is the right
        // question for a listing: it answers "is this holding the gating loop
        // open". It is the wrong question for a deletion. A waiver with no
        // audit hits but a live `below_audit` count is suppressing a real
        // finding that simply scores under the gate — the module header calls
        // that case harmless, and its comment is still true.
        //
        // Deleting it destroys a verified judgment and re-exposes the site the
        // moment a threshold moves, which is the exact re-litigation the whole
        // waiver system exists to prevent. One session hit this: `--orphaned
        // --remove` offered to strip a dated `divergence/NodeContent::Clip`
        // waiver whose finding scores 0.40 against a 0.45 gate, and only a
        // manual `--no-suppress` cross-check caught it. The summary line one
        // row above already distinguished the two cases; this did not.
        //
        // Held back rather than silently kept, because the count in the
        // footer has to keep matching what was written.
        //
        // `--include-below-audit` is the opt-in, and it exists because the
        // guard's own advice was "remove it by hand". A reader clearing a
        // ledger wholesale — retiring a check, re-validating every judgment
        // from scratch — was told to go and do by hand the one thing this
        // command is for, on eleven sites, with no flag that could express it.
        // A safety rail nobody can lower is a rail that gets stepped around,
        // and stepping around it means hand-editing source the tool could have
        // edited correctly.
        if what == Mutation::Remove
            && !opts.include_below_audit
            && w.hits() == 0
            && w.below_audit() > 0
        {
            skipped.push(format!(
                "{}:{} — suppresses {} finding(s) below audit's thresholds, so the reason \
                 still holds; it is only absent from the gating loop. \
                 `--include-below-audit` removes these too",
                w.file,
                w.comment_line,
                w.below_audit()
            ));
            continue;
        }
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

    // Verb-aware: `--remove` holds waivers back too now, and "not upgraded"
    // over a removal preview names an action nobody asked for.
    let held = match what {
        Mutation::Remove => "not removed",
        Mutation::Upgrade => "not upgraded",
    };
    for s in &skipped {
        ctx.out.note(&format!("(note: {} — {})", held, s));
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
            // The two mutations hold rows back for different reasons, and one
            // word cannot carry both: `--upgrade` skips what it cannot name a
            // check for, `--remove` skips what is still suppressing something.
            format!(
                "; {} left alone{}",
                skipped.len(),
                match what {
                    Mutation::Remove => " as still suppressing",
                    Mutation::Upgrade => " as ambiguous",
                }
            )
        },
        if opts.write {
            String::new()
        } else {
            "; dry run — add --write to apply".to_string()
        }
    ));
    Ok(0)
}

// ──────────────────────────────────────────────────────────────────────────
// `--apply` — writing a batch of verified judgments back into the source

/// One row of an `--apply` input file: where the waiver goes, what it waives,
/// and why.
///
/// Every field comes from the caller. Nothing here is inferred, and that is the
/// point: this is the only code path in the tool that *adds* comments to a
/// user's source, and a placement the tool guessed is a judgment nobody made.
struct Application {
    file: String,
    line: usize,
    check: String,
    /// The check-specific key, or `None` for a bare `ok(<check>)`.
    key: Option<String>,
    scope: crate::suppress::Scope,
    reason: String,
}

/// Why bulk application exists.
///
/// The single largest time sink in one 6,000-line session: 95 `panics` sites →
/// JSON dump → grouping script → a hand-built five-class rationale taxonomy →
/// a patch script, and that pipeline was written **four separate times** across
/// the session. `--suggest-waivers` did not help, because its output carried no
/// location — it printed the comment and left the reader to find the line.
///
/// TSV rather than JSON, deliberately. The tool has no JSON reader and adding
/// one to parse six fields would be a dependency for a format `jq` already
/// emits:
///
/// ```text
/// unruster panics --json --suggest-waivers \
///   | jq -r '.sections[].rows[] | select(.waiver_check)
///            | [.file, .line, .waiver_check, .waiver_key, "site", "in-process length"]
///            | @tsv' \
///   | unruster waivers --apply -
/// ```
///
/// Columns: `file`, `line`, `check`, `key`, `scope`, `reason`. `key` may be
/// empty or `-` for a bare `ok(<check>)`. `scope` is `site` (a trailing comment
/// on that line) or `item` (a standalone comment above it, which requires
/// `line` to be an item's declaration line). Blank lines and `#` comments are
/// skipped, as is a header row whose first cell is `file`.
fn parse_applications(text: &str) -> (Vec<Application>, Vec<String>) {
    let mut rows = Vec::new();
    let mut bad = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line_no = n + 1;
        // `\r` only. A general `trim_end` would eat the tab before an empty
        // trailing cell, and an empty *reason* has to be reported as an empty
        // reason rather than as a malformed row.
        let l = raw.strip_suffix('\r').unwrap_or(raw);
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        }
        let cells: Vec<&str> = l.split('\t').collect();
        if cells.first().map(|c| c.trim()) == Some("file") {
            continue; // header
        }
        if cells.len() < 6 {
            bad.push(format!(
                "input line {}: expected 6 tab-separated cells \
                 (file, line, check, key, scope, reason), found {}",
                line_no,
                cells.len()
            ));
            continue;
        }
        let Ok(at) = cells[1].trim().parse::<usize>() else {
            bad.push(format!(
                "input line {}: `{}` is not a line number",
                line_no, cells[1]
            ));
            continue;
        };
        let check = cells[2].trim().to_string();
        if !crate::suppress::known_check_names().contains(&check.as_str()) {
            bad.push(format!(
                "input line {}: `{}` is not a check this tool has — known: {}",
                line_no,
                check,
                crate::suppress::known_check_names().join(", ")
            ));
            continue;
        }
        let scope = match cells[4].trim() {
            "site" => crate::suppress::Scope::Site,
            "item" => crate::suppress::Scope::Item,
            other => {
                bad.push(format!(
                    "input line {}: scope must be `site` or `item`, not `{}`. There is no \
                     default: a placement the tool guessed is a judgment nobody made",
                    line_no, other
                ));
                continue;
            }
        };
        let reason = cells[5].trim().to_string();
        if reason.is_empty() {
            bad.push(format!(
                "input line {}: empty reason. A waiver records a human judgment; without \
                 one it is a silenced finding",
                line_no
            ));
            continue;
        }
        let key = match cells[3].trim() {
            "" | "-" => None,
            k => Some(k.to_string()),
        };
        rows.push(Application {
            file: cells[0].trim().to_string(),
            line: at,
            check,
            key,
            scope,
            reason,
        });
    }
    (rows, bad)
}

/// The comment text a row becomes, without indentation.
fn waiver_line(a: &Application, today: Date) -> String {
    let spec = match &a.key {
        Some(k) => format!("{}/{}", a.check, k),
        None => a.check.clone(),
    };
    format!("// unruster: ok({}) {} — {}", spec, today, a.reason)
}

/// Insert one waiver comment per row, grouped by file and applied bottom-up so
/// earlier line numbers stay valid — [`mutate`]'s discipline, pointed the other
/// way.
///
/// Dry-run by default. Refuses rather than guesses: an `item`-scoped row whose
/// line is not an item's declaration, a line that already carries a waiver, a
/// line past the end of the file, and an unreadable file are all reported and
/// left alone.
fn apply(ctx: &AnalysisCtx, source: &str, opts: &WaiverOpts) -> Result<usize> {
    let text = if source == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        buf
    } else {
        std::fs::read_to_string(source)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {}", source, e))?
    };
    let (rows, mut refused) = parse_applications(&text);

    // Item declaration lines, so an `item` scope can be verified rather than
    // trusted. Without this the command would happily put a standalone comment
    // above a statement and call it item scope, which is the "infers placement"
    // failure the design rules out.
    //
    // Keyed on the canonical path: the index spells files the way `--root`
    // reached them and an input row spells them the way its author did, and a
    // mismatch here would refuse every correct row for the wrong reason.
    let real = |f: &str| std::fs::canonicalize(f).ok();
    let item_lines: std::collections::BTreeSet<(std::path::PathBuf, usize)> = ctx
        .idx
        .iter()
        .filter_map(|d| real(&d.file).map(|p| (p, d.line)))
        .collect();

    let mut by_file: BTreeMap<String, Vec<&Application>> = BTreeMap::new();
    for a in &rows {
        if a.scope == crate::suppress::Scope::Item
            && !real(&a.file).is_some_and(|p| item_lines.contains(&(p, a.line)))
        {
            refused.push(format!(
                "{}:{} — scope `item` but no item is declared on that line. Item scope \
                 covers a whole declaration, so it has to sit above one; use `site` for a \
                 statement",
                a.file, a.line
            ));
            continue;
        }
        by_file.entry(a.file.clone()).or_default().push(a);
    }

    let mut written = 0usize;
    for (file, mut items) in by_file {
        let Ok(src) = std::fs::read_to_string(&file) else {
            refused.push(format!("{} — cannot read; skipped", file));
            continue;
        };
        let mut lines: Vec<String> = src.lines().map(str::to_string).collect();
        // Bottom-up, so an insertion above does not shift the rows below it.
        items.sort_by_key(|a| std::cmp::Reverse(a.line));
        for a in items {
            if a.line == 0 || a.line > lines.len() {
                refused.push(format!(
                    "{}:{} — past the end of the file ({} lines)",
                    file,
                    a.line,
                    lines.len()
                ));
                continue;
            }
            let target = lines[a.line - 1].clone();
            // A second waiver on one line is two judgments where the ledger
            // shows one, and the parser reads only the first.
            let already = target.contains("unruster:")
                || (a.scope == crate::suppress::Scope::Item
                    && a.line >= 2
                    && lines[a.line - 2].contains("unruster:"));
            if already {
                refused.push(format!(
                    "{}:{} — already carries a waiver; left alone",
                    file, a.line
                ));
                continue;
            }
            let comment = waiver_line(a, opts.today);
            match a.scope {
                crate::suppress::Scope::Site => {
                    let rebuilt = format!("{} {}", target.trim_end(), comment);
                    preview(ctx, &file, a.line, &target, Some(&rebuilt));
                    lines[a.line - 1] = rebuilt;
                }
                crate::suppress::Scope::Item => {
                    let indent: String = target
                        .chars()
                        .take_while(|c| c.is_whitespace())
                        .collect();
                    let inserted = format!("{}{}", indent, comment);
                    preview(ctx, &file, a.line, &target, None);
                    ctx.out.line(&format!("+{}:{}: {}", file, a.line, inserted));
                    lines.insert(a.line - 1, inserted);
                }
            }
            written += 1;
        }
        if opts.write {
            let mut body = lines.join("\n");
            if src.ends_with('\n') {
                body.push('\n');
            }
            std::fs::write(&file, body)?;
        }
    }

    for r in &refused {
        ctx.out.note(&format!("(note: not applied — {})", r));
    }
    ctx.out.summary(&format!(
        "({} waiver(s) {}{}{}{})",
        written,
        if opts.write { "applied" } else { "would be applied" },
        if refused.is_empty() {
            String::new()
        } else {
            format!("; {} refused", refused.len())
        },
        if opts.write {
            String::new()
        } else {
            "; dry run — add --write to apply".to_string()
        },
        if opts.write && written > 0 {
            "; run `cargo fmt` if your project reflows comments"
        } else {
            ""
        }
    ));
    // A refused row is a request the tool did not carry out. Reporting exit 0
    // over it would be the same vacuous pass the empty-scan guard exists for.
    if !refused.is_empty() {
        anyhow::bail!(
            "{} of {} row(s) could not be applied; nothing about them was guessed",
            refused.len(),
            refused.len() + written
        );
    }
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
