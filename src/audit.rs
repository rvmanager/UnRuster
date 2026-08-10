//! `audit` — the one-shot ranked sweep: runs the full check battery as
//! severity-ordered sections and exits 1 on gating findings. Designed as the
//! entry point of an agent loop:
//!
//! ```text
//! until unruster audit --exclude 'fixtures/**'; do <fix top finding>; done
//! ```
//!
//! Sections reuse each command's own scanner and row format, so drilling down
//! with the dedicated command shows identical rows. Severity is a static
//! ranking of the check, not of the individual row — the tool finds
//! candidates; the reader (or agent) judges each one.
//!
//! Ordering note: each section's rows are produced by a *closure*, not by an
//! eagerly-evaluated argument. That is load-bearing. When the count was passed
//! by value, Rust evaluated the check — printing all of its rows — before the
//! function that printed the header ran, so every `## [high] …` header landed
//! *after* the rows it was supposed to introduce, and readers had to slice the
//! output by line number to tell the sections apart.

use crate::casts::CastClass;
use crate::emit::{row, site};
use crate::clones;
use crate::context::{AnalysisCtx, Counts};
use crate::builder_drift;
use crate::config_drift;
use crate::divergence;
use crate::metrics::SortKey;
use crate::parse::ParsedFile;
use crate::{
    casts, conversion_pairs, dead_code, error_swallows, metrics, parallel_matches, pass_through,
    stringly,
};

/// Cyclomatic-complexity threshold above which a fn counts as an audit
/// finding (matches the playbook's god-fn guidance).
const CYCLO_THRESHOLD: usize = 15;

/// Row cap for the two checks that get inline source context. Bounds the
/// output: at most this many rows × (2·[`CONTEXT_LINES`] + 1) snippet lines.
const CONTEXTED_TOP: usize = 20;

/// Rows of the metrics ranking shown when `--top` is not given.
const DEFAULT_METRICS_TOP: usize = 20;

/// Source lines shown around each row of the low-volume checks. On the runs
/// this battery was tuned against, `stringly` returned 4 rows and
/// `conversion-pairs` returned 1 — and every one of them was opened in an
/// editor immediately afterwards. Showing the line inline removes that step.
const CONTEXT_LINES: usize = 2;

/// Divergence score below which pairs are dropped from the audit section.
/// Tuned so the section stays short enough to read in full — the point of
/// putting it first is that its rows are worth reading in full.
///
/// Re-tuned after the overlap term was corrected to a real set intersection:
/// on a 170-enum tree the distribution went 0.35 → 79 pairs, 0.40 → 41,
/// 0.45 → 16. The knee is at 0.45, and the dedicated command (default 0.25)
/// is where someone goes for the long tail.
pub const DIVERGENCE_MIN_SCORE: f64 = 0.45;

/// Whether a section's findings gate the exit code. Deterministic defect
/// classes gate; candidate classes that need per-site judgment (stringly, god
/// fns, …) are advisory unless `--strict` — otherwise an `until unruster audit`
/// loop could never converge on a healthy codebase whose domain legitimately
/// triggers candidates.
///
/// The third state exists because "does this check gate?" was the wrong
/// question for the two highest-volume ones. On a twelve-crate workspace the
/// five gating checks all returned zero while `error-swallows` returned 89 rows
/// containing a permanent loss of Stripe payment confirmations — so `audit`
/// printed `0 gating + 128 advisory … clean, exit 0` and the advertised
/// `until unruster audit; do <fix>; done` loop would have terminated on
/// iteration one, on a codebase with a live money bug in it.
///
/// Making the whole check gate was not the fix either: most of those 89 rows
/// were correct by design, and a gate nobody can clear is a gate nobody runs.
/// `Tiered` gates on the check's own ranking instead, so the gate is the rows
/// where something external happened and nobody checked whether it worked.
#[derive(Clone, Copy, PartialEq)]
enum Gate {
    /// Every row gates.
    Gating,
    /// No row gates (unless `--strict`).
    Advisory,
    /// The check ranks its own rows; the ones above its threshold gate.
    Tiered,
}

/// The battery's per-check configuration, defined once.
///
/// `waivers` must run the identical set to count waiver hits — if the two drift,
/// orphan detection starts answering a different question than the gating loop
/// asks, which is exactly the defect these helpers exist to prevent.
pub fn coverage_opts() -> parallel_matches::CoverageOpts {
    parallel_matches::CoverageOpts {
        // A row the check itself labels "likely false positive" must not gate
        // the agent loop. `_ => scrutinee.method()` is structurally safe: a new
        // variant has to implement the method. The dedicated command still
        // shows them.
        hide_trait_routed: true,
        // A 1-of-2 `matches!` is an if/else, not partial dispatch.
        min_variants: 3,
        // Sites one variant short of exhaustive are the "forgot one" shape;
        // wider gaps are usually two different jobs. The full list stays
        // available from the dedicated command.
        max_missing: Some(1),
        ..Default::default()
    }
}

pub fn swallow_opts() -> error_swallows::SwallowOpts {
    error_swallows::SwallowOpts {
        include_unwrap_or: false,
        // Infallible `write!` into a String and fallbacks that already log are
        // the two families that dominated this check's output while producing
        // no defects.
        include_infallible: false,
        include_logged: false,
    }
}

pub const CAST_CLASSES: &[CastClass] = &[
        CastClass::NarrowInt,
        CastClass::SignedFlip,
        CastClass::FloatInt,
        CastClass::NarrowFloat,
        CastClass::Ptr,
    // `usize-cross` deliberately absent: on a 64-bit target it is dominated by
    // lossless `u32 as usize` widening, now classified as `usize-widen` and
    // reachable on demand.
];

/// How the battery is configured for one pass.
///
/// Two passes exist — `audit`'s own, and the wide-open one `waivers` uses to
/// tell "earns nothing here" from "earns nothing anywhere" — and last release
/// they were two hand-written call sequences that silently drifted apart, which
/// is how orphan detection ended up disagreeing with the audit line. Making the
/// difference a *value* rather than duplicated code means the two configs sit
/// side by side and can be diffed by eye.
#[derive(Clone, Copy)]
pub struct BatteryConfig {
    pub divergence_min_score: f64,
    pub handling_min_care_gap: u8,
    pub coverage: parallel_matches::CoverageOpts,
    pub swallows: error_swallows::SwallowOpts,
    /// Empty = every class (the permissive pass).
    pub cast_classes: &'static [CastClass],
    pub include_unsafe_ptr: bool,
}

impl BatteryConfig {
    /// Exactly what `audit` gates on.
    pub fn gating() -> Self {
        BatteryConfig {
            divergence_min_score: DIVERGENCE_MIN_SCORE,
            handling_min_care_gap: HANDLING_MIN_CARE_GAP,
            coverage: coverage_opts(),
            swallows: swallow_opts(),
            cast_classes: CAST_CLASSES,
            include_unsafe_ptr: false,
        }
    }

    /// Every threshold opened up: reports rows `audit` deliberately filters.
    pub fn permissive() -> Self {
        BatteryConfig {
            divergence_min_score: 0.0,
            handling_min_care_gap: 1,
            coverage: parallel_matches::CoverageOpts {
                compact: true,
                ..Default::default()
            },
            swallows: error_swallows::SwallowOpts {
                include_unwrap_or: true,
                include_infallible: true,
                include_logged: true,
            },
            cast_classes: &[],
            include_unsafe_ptr: true,
        }
    }
}

/// Run the whole battery discarding every result, for its side effect on
/// waiver hit counts. `ctx.out` must already be a silent sink.
///
/// Results are dropped on purpose: a check that fails to run contributes no
/// hits, which is the correct outcome, and there is no caller to report to.
// unruster: ok(error-swallows/let-_) 2026-08-06 — the battery runs purely for
// its effect on waiver hit counts; a check that fails contributes no hits,
// which is the correct outcome, and there is no caller to report to.
pub fn run_silent_battery(ctx: &AnalysisCtx, dead_call_source: &[ParsedFile], cfg: BatteryConfig) {
    // `--top` is enforced in the emitter, after fingerprint recording, so it
    // cannot affect hit counts or baselines however this battery is invoked.
    let checks: [(&str, &dyn Fn() -> anyhow::Result<usize>); 13] = [
        ("divergence", &|| {
            divergence::run(ctx, None, cfg.divergence_min_score)
        }),
        ("divergence-handling", &|| {
            divergence::run_handling(ctx, cfg.handling_min_care_gap)
        }),
        ("enum-coverage", &|| {
            parallel_matches::run_enum_coverage(ctx, None, cfg.coverage)
        }),
        ("dead-code", &|| {
            dead_code::run(ctx, dead_call_source, None, false)
        }),
        ("conversion-pairs", &|| conversion_pairs::run(ctx)),
        ("clones", &|| {
            clones::run(ctx, clones::DEFAULT_MIN_TOKENS)
        }),
        ("error-swallows", &|| error_swallows::run(ctx, cfg.swallows)),
        ("casts", &|| {
            casts::run(ctx, cfg.cast_classes, None, false, cfg.include_unsafe_ptr)
        }),
        ("config-drift", &|| {
            config_drift::run(ctx, None, CONFIG_DRIFT_MIN_SCORE)
        }),
        ("builder-drift", &|| {
            builder_drift::run(ctx, None, BUILDER_DRIFT_MIN_SCORE)
        }),
        // The advisory three too. They consult no waivers, so they add nothing
        // to hit counting — but a *baseline* comparison that omitted them would
        // report every stringly and metrics row as new on the first run.
        ("stringly", &|| {
            stringly::run(ctx, false, false, None)
        }),
        ("metrics", &|| {
            metrics::run(ctx, SortKey::Cyclo, Some(CYCLO_THRESHOLD), true)
        }),
        ("pass-through", &|| pass_through::run(ctx, 1)),
    ];
    for (name, check) in checks {
        let prev = ctx.out.set_check(name);
        let _ = check();
        ctx.out.set_check(&prev);
    }
}

/// Drift score below which rows are dropped from the audit section. The two
/// genuine finds on this codebase scored 0.18 and 0.23; ordinary two-preset
/// structs land under 0.10.
pub const CONFIG_DRIFT_MIN_SCORE: f64 = 0.12;

/// One missing call between two chains scores 0.85 in one function and 0.48
/// across two — both worth reading. Two spellings of the same helper
/// (`context` vs `with_context`) land near 0.28, which is the noise this cut
/// is placed to exclude.
pub const BUILDER_DRIFT_MIN_SCORE: f64 = 0.4;

/// Minimum care distance for the `--handling` axis.
pub const HANDLING_MIN_CARE_GAP: u8 = 2;

pub fn run(
    ctx: &AnalysisCtx,
    dead_call_source: &[ParsedFile],
    top: Option<usize>,
    strict: bool,
    findings_only: bool,
) -> anyhow::Result<usize> {
    let mut gating = 0usize;
    let mut advisory = 0usize;
    let mut checks = 0usize;
    let mut skipped_clean = 0usize;
    // Each check's own summary line belongs with its rows, on one stream.
    // Splitting them cost a full round-trip of "re-run with 2> redirected"
    // every time someone read this output for the first time.
    let prev_inline = ctx.out.set_summary_inline(true);

    // `count` is a closure so the header prints first. See the module note.
    //
    // It returns [`Counts`] rather than a bare total because two checks are
    // ranked and gate only on their top tier: every row still prints, but a
    // `.map_err(|_|)` on a base64 decode must not hold the loop open next to a
    // discarded `DELETE`. For every other check the two numbers are equal.
    // `cap` is this section's default row budget, used when the caller gave no
    // `--top`. Sections that can run long carry one so the battery stays
    // readable in full; the rest are uncapped. `--top` overrides every one of
    // them, and the emitter enforces it after fingerprint recording, so no cap
    // can affect a count, a waiver hit, or a `--since` baseline.
    let mut section = |title: &str,
                       check: &str,
                       gate: Gate,
                       cap: Option<usize>,
                       count: &mut dyn FnMut() -> anyhow::Result<Counts>|
     -> anyhow::Result<()> {
        ctx.out.section(title);
        ctx.out.set_row_budget(top.or(cap));
        // The check name is part of every fingerprint: two checks reporting the
        // same line must not collapse into one identity.
        let prev = ctx.out.set_check(check);
        // A check announces its own `(0 …)` line before anyone can know the
        // section is empty, so `--findings-only` catches the line rather than
        // predicting it. The header is deferred by `section` for the same
        // reason. Nothing about the check's execution changes: the count, the
        // waiver hits and the `--since` baseline are all recorded either way,
        // so what this hides is a rendering and never a finding.
        let held = ctx.out.hold_summary(findings_only);
        let n = count()?;
        ctx.out.hold_summary(held);
        let own_summary = ctx.out.take_held_summary();
        ctx.out.set_check(&prev);
        // `--strict` promotes every advisory row, so it wants the total, not
        // the tier: the flag means "nothing at all", not "nothing important".
        let g = if strict {
            n.total
        } else {
            match gate {
                Gate::Gating => n.total,
                Gate::Advisory => 0,
                Gate::Tiered => n.gating,
            }
        };
        gating += g;
        advisory += n.total - g;
        checks += 1;
        // Clean and nobody asked to see it: drop the header too and move on.
        // `drop_pending_section` reports false when the check printed something
        // that already flushed it — a tree rendering, a matrix — and in that
        // case the section is half on screen and has to be finished properly.
        if findings_only && n.total == 0 && ctx.out.drop_pending_section() {
            ctx.out.set_row_budget(None);
            skipped_clean += 1;
            return Ok(());
        }
        if let Some(s) = own_summary {
            ctx.out.summary(&s);
        }
        if let Some(note) = ctx.out.cap_note() {
            ctx.out.row_note(&note);
        }
        ctx.out.set_row_budget(None);
        ctx.out.section_end();
        Ok(())
    };

    // Divergence leads the battery: measured across two audit passes on a
    // large codebase, sibling-disagreement rows converted to real defects at a
    // far higher rate than any volume check below.
    section(
        "[high] divergence — sibling paths that disagree (explain: partial-enumeration)",
        "divergence",
        Gate::Gating,
        Some(40),
        &mut || Ok(Counts::flat(divergence::run(ctx, None, DIVERGENCE_MIN_SCORE)?)),
    )?;
    section(
        "[high] divergence --handling — one callee, different care (explain: silent-fallbacks)",
        "divergence-handling",
        Gate::Gating,
        None,
        &mut || Ok(Counts::flat(divergence::run_handling(ctx, HANDLING_MIN_CARE_GAP)?)),
    )?;
    section(
        "[high] enum-coverage --all — partial enum dispatch (explain: partial-enumeration)",
        "enum-coverage",
        Gate::Gating,
        None,
        &mut || {
            Ok(Counts::flat(parallel_matches::run_enum_coverage(
                ctx,
                None,
                coverage_opts(),
            )?))
        },
    )?;
    section(
        "[high] dead-code — fns with no observed caller",
        "dead-code",
        Gate::Gating,
        None,
        &mut || Ok(Counts::flat(dead_code::run(ctx, dead_call_source, None, false)?)),
    )?;
    section(
        "[high] conversion-pairs — one concept in two shapes (explain: replication)",
        "conversion-pairs",
        Gate::Gating,
        None,
        &mut || {
            let prev = if ctx.out.context_lines().is_none() {
                ctx.out.set_context_lines(Some(CONTEXT_LINES))
            } else {
                ctx.out.context_lines()
            };
            let r = conversion_pairs::run(ctx);
            ctx.out.set_context_lines(prev);
            Ok(Counts::flat(r?))
        },
    )?;
    section(
        &format!(
            "[high] error-swallows — silently dropped Results; gating at score >= {:.2} \
             (explain: silent-fallbacks)",
            error_swallows::GATING_SCORE
        ),
        "error-swallows",
        // Tiered, not advisory. Every row prints, ranked; the ones that gate are
        // the ones where an external effect happened and the only report of
        // whether it worked was discarded. That tier is where the dropped
        // `DELETE FROM stripe_events` lived while this whole check sat in the
        // advisory pile and `audit` exited 0.
        Gate::Tiered,
        None,
        &mut || error_swallows::run_counted(ctx, swallow_opts()),
    )?;
    section(
        &format!(
            "[high] clones — the same body written out more than once; gating at score \
             >= {:.2} (explain: replication)",
            clones::GATING_SCORE
        ),
        "clones",
        Gate::Tiered,
        Some(20),
        &mut || clones::run_counted(ctx, clones::DEFAULT_MIN_TOKENS),
    )?;
    section(
        "[medium] config-drift — same struct, two configurations (explain: config-drift)",
        "config-drift",
        // Advisory, not gating: a struct built two ways is often deliberate (two
        // presets, a builder). The rows are worth reading — this check exists
        // because a drifted `CoverageOpts` made orphan detection contradict the
        // audit line — but a codebase can hold correct ones indefinitely, and a
        // gating check that can never reach zero is one nobody runs.
        Gate::Advisory,
        Some(10),
        &mut || Ok(Counts::flat(config_drift::run(ctx, None, CONFIG_DRIFT_MIN_SCORE)?)),
    )?;
    section(
        "[medium] builder-drift — sibling chains, one missing a step (explain: builder-drift)",
        "builder-drift",
        // Advisory alongside its config-drift sibling: two chains on one
        // builder often differ on purpose. The rows are worth reading — this
        // check exists because a `Command::new("git")` chain that forgot
        // `.current_dir()` resolved the wrong repository.
        Gate::Advisory,
        Some(10),
        &mut || Ok(Counts::flat(builder_drift::run(ctx, None, BUILDER_DRIFT_MIN_SCORE)?)),
    )?;
    section(
        "[medium] casts — data-loss classes only (explain: casts)",
        "casts",
        Gate::Advisory,
        None,
        &mut || Ok(Counts::flat(casts::run(ctx, CAST_CLASSES, None, false, false)?)),
    )?;
    // Low-volume checks: show the offending line inline. Skipped when the
    // caller set `--context` themselves — their choice wins.
    let auto_context = ctx.out.context_lines().is_none();
    section(
        "[medium] stringly — logic branching on string literals (explain: stringly)",
        "stringly",
        Gate::Advisory,
        Some(CONTEXTED_TOP),
        &mut || {
            let prev = if auto_context {
                ctx.out.set_context_lines(Some(CONTEXT_LINES))
            } else {
                ctx.out.context_lines()
            };
            let r = stringly::run(ctx, false, false, None);
            ctx.out.set_context_lines(prev);
            Ok(Counts::flat(r?))
        },
    )?;
    section(
        &format!(
            "[medium] metrics — fns with cyclo >= {} (explain: god-function)",
            CYCLO_THRESHOLD
        ),
        "metrics",
        Gate::Advisory,
        Some(DEFAULT_METRICS_TOP),
        &mut || Ok(Counts::flat(metrics::run(ctx, SortKey::Cyclo, Some(CYCLO_THRESHOLD), true)?)),
    )?;
    section(
        "[low] pass-through — single-call wrapper fns (explain: replication)",
        "pass-through",
        Gate::Advisory,
        None,
        &mut || Ok(Counts::flat(pass_through::run(ctx, 1)?)),
    )?;

    // The battery-wide line goes back to the normal stream: it is the one an
    // `until unruster audit` loop greps for, and callers already redirect for it.
    ctx.out.set_summary_inline(prev_inline);
    // Both numbers, not just the waiver count: two item-scoped waivers hiding
    // seven findings reported as "2 site(s) waived" understates the reach by
    // 3.5x, which is the exact failure this line exists to prevent.
    //
    // The ledger this run could actually exercise, not the whole file of them.
    // Under `--changed-since` every check calls `retain_changed` *before*
    // `retain_unsuppressed`, so a waiver in an unchanged file never sees a
    // finding and its hit count is zero by construction — not by decay. Tallied
    // whole-ledger, a scoped run on this very tree reported "25 waiver(s) …, 24
    // of them suppressing nothing" where the unscoped answer is 4: a number
    // that reads as a demand to delete two dozen live waivers. `hits` was
    // already scoped (it is counted during the run); this puts the count that
    // divides it on the same footing.
    let ledger: Vec<&crate::suppress::Waiver> = ctx
        .suppressions
        .all()
        .iter()
        .filter(|w| ctx.in_scope(&w.file))
        .collect();
    let waivers = ledger.len();
    let hidden: usize = ledger.iter().map(|w| w.hits()).sum();
    ctx.out.summary(&format!(
        "(audit: {} gating + {} advisory finding(s) across {} check(s){}; {}{}{})",
        gating,
        advisory,
        checks,
        // `--findings-only` hides sections, never findings — but a report with
        // eight of thirteen headers missing has to say which eight are missing
        // and why, or the next reader counts the headers and believes the
        // battery shrank.
        if skipped_clean > 0 {
            format!(
                "; --findings-only hid {} clean section(s), all counted above",
                skipped_clean
            )
        } else {
            String::new()
        },
        // Printing "exit 1 while gating findings remain" next to "0 gating" read
        // as a contradiction on the one line that is supposed to say you are done.
        //
        // Naming the *process's* status matters because this line is what the
        // documented `until unruster audit; do …; done` loop turns on, and the
        // habit that surrounds it is a pipe: one session ran
        // `unruster audit … | tail -40; echo "EXIT=$?"` and read back `EXIT=0`,
        // which was `tail`'s. It happened to be clean that time.
        if gating > 0 {
            "exit 1 while gating findings remain (the process's status — after a \
             pipe `$?` is the pipe's)"
        } else {
            "clean: no gating findings, exit 0"
        },
        if strict { "; --strict: all gate" } else { "" },
        if waivers > 0 {
            // Every check that honours waivers has now run, so a waiver with
            // zero hits is orphaned against all of them — a mistyped key or a
            // scope that missed. Saying only "N waivers hiding M findings"
            // left a real codebase with 33 waivers hiding 30 findings and
            // nobody noticing that at least three of them did nothing.
            let dead = ledger.iter().filter(|w| w.hits() == 0).count();
            format!(
                "; {} waiver(s){} hiding {} finding(s){} — `unruster waivers` to review",
                waivers,
                // Say which ledger, so a count far below the file's own is read
                // as a scope and not as waivers having gone missing.
                if ctx.changed.is_some() {
                    " in the changed files"
                } else {
                    ""
                },
                hidden,
                if dead > 0 {
                    format!(", {} of them suppressing nothing", dead)
                } else {
                    String::new()
                }
            )
        } else if !ctx.suppressions.is_empty() {
            // Scoped past every waiver there is. Silence here reads as "this
            // tree has no waivers", which is the opposite of true.
            format!(
                "; none of the {} waiver(s) in this tree are in the changed files",
                ctx.suppressions.len()
            )
        } else {
            String::new()
        }
    ));
    Ok(gating)
}

/// Render a cross-run comparison as its own section.
///
/// Kept out of the row stream: existing rows keep their column shape, and a
/// reader (or an `awk` pipeline) that does not care about the baseline sees
/// exactly what it saw before.
pub fn print_diff(ctx: &AnalysisCtx, against: &str, d: &crate::baseline::Diff) {
    ctx.out.section(&format!(
        "[baseline] vs {} — {}",
        against,
        d.summary()
    ));
    for f in &d.gone {
        row!(
            ctx.out,
            "status" => "gone",
            "check" => f.check.clone(),
            "was" => site(&f.file, f.line),
            "what" => f.label.clone(),
        );
    }
    for (was, now) in &d.moved {
        row!(
            ctx.out,
            "status" => "moved",
            "check" => now.check.clone(),
            "was" => site(&was.file, was.line),
            "what" => now.label.clone(),
            "now" => site(&now.file, now.line),
        );
    }
    for f in &d.new {
        row!(
            ctx.out,
            "status" => "new",
            "check" => f.check.clone(),
            "was" => site(&f.file, f.line),
            "what" => f.label.clone(),
        );
    }
    ctx.out.summary(&format!("(baseline: {} vs {})", d.summary(), against));
    ctx.out.section_end();
}
