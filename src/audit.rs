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
use crate::context::AnalysisCtx;
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
const DIVERGENCE_MIN_SCORE: f64 = 0.45;

/// Whether a section's findings gate the exit code. Deterministic defect
/// classes gate; candidate classes that need per-site judgment (stringly,
/// error-swallows, god fns, …) are advisory unless `--strict` — otherwise an
/// `until unruster audit` loop could never converge on a healthy codebase
/// whose domain legitimately triggers candidates.
#[derive(Clone, Copy, PartialEq)]
enum Gate {
    Gating,
    Advisory,
}

pub fn run(
    ctx: &AnalysisCtx,
    dead_call_source: &[ParsedFile],
    top: Option<usize>,
    strict: bool,
) -> anyhow::Result<usize> {
    let metrics_top = top.unwrap_or(20);
    let mut gating = 0usize;
    let mut advisory = 0usize;
    let mut checks = 0usize;
    // Each check's own summary line belongs with its rows, on one stream.
    // Splitting them cost a full round-trip of "re-run with 2> redirected"
    // every time someone read this output for the first time.
    let prev_inline = ctx.out.set_summary_inline(true);

    // `count` is a closure so the header prints first. See the module note.
    let mut section = |title: &str,
                       gate: Gate,
                       count: &mut dyn FnMut() -> anyhow::Result<usize>|
     -> anyhow::Result<()> {
        ctx.out.section(title);
        let n = count()?;
        if gate == Gate::Gating || strict {
            gating += n;
        } else {
            advisory += n;
        }
        checks += 1;
        ctx.out.section_end();
        Ok(())
    };

    // Divergence leads the battery: measured across two audit passes on a
    // large codebase, sibling-disagreement rows converted to real defects at a
    // far higher rate than any volume check below.
    section(
        "[high] divergence — sibling paths that disagree (explain: partial-enumeration)",
        Gate::Gating,
        &mut || divergence::run(ctx, None, DIVERGENCE_MIN_SCORE, top.or(Some(40))),
    )?;
    section(
        "[high] divergence --handling — one callee, different care (explain: silent-fallbacks)",
        Gate::Gating,
        &mut || divergence::run_handling(ctx, 2),
    )?;
    section(
        "[high] enum-coverage --all — partial enum dispatch (explain: partial-enumeration)",
        Gate::Gating,
        &mut || {
            parallel_matches::run_enum_coverage(
                ctx,
                None,
                parallel_matches::CoverageOpts {
                    hide_trait_routed: false,
                    // Sites one variant short of exhaustive are the "forgot
                    // one" shape; wider gaps are usually two different jobs.
                    // The full list stays available from the dedicated command.
                    max_missing: Some(1),
                    ..Default::default()
                },
            )
        },
    )?;
    section(
        "[high] dead-code — fns with no observed caller",
        Gate::Gating,
        &mut || dead_code::run(ctx, dead_call_source, false, false),
    )?;
    section(
        "[high] conversion-pairs — one concept in two shapes (explain: replication)",
        Gate::Gating,
        &mut || {
            let prev = if ctx.out.context_lines().is_none() {
                ctx.out.set_context_lines(Some(CONTEXT_LINES))
            } else {
                ctx.out.context_lines()
            };
            let r = conversion_pairs::run(ctx);
            ctx.out.set_context_lines(prev);
            r
        },
    )?;
    section(
        "[medium] error-swallows — silently dropped Results (explain: silent-fallbacks)",
        Gate::Advisory,
        &mut || {
            error_swallows::run(
                ctx,
                error_swallows::SwallowOpts {
                    include_unwrap_or: false,
                    // Infallible `write!` into a String and fallbacks that
                    // already log are the two families that dominated this
                    // check's output while producing no defects.
                    include_infallible: false,
                    include_logged: false,
                },
            )
        },
    )?;
    section(
        "[medium] casts — data-loss classes only (explain: casts)",
        Gate::Advisory,
        &mut || {
            casts::run(
                ctx,
                &[
                    CastClass::NarrowInt,
                    CastClass::SignedFlip,
                    CastClass::FloatInt,
                    CastClass::NarrowFloat,
                    CastClass::Ptr,
                    // `usize-cross` deliberately absent: on a 64-bit target it
                    // is dominated by lossless `u32 as usize` widening, now
                    // classified as `usize-widen` and reachable on demand.
                ],
                None,
                false,
                top,
            )
        },
    )?;
    // Low-volume checks: show the offending line inline. Skipped when the
    // caller set `--context` themselves — their choice wins.
    let auto_context = ctx.out.context_lines().is_none();
    section(
        "[medium] stringly — logic branching on string literals (explain: stringly)",
        Gate::Advisory,
        &mut || {
            let prev = if auto_context {
                ctx.out.set_context_lines(Some(CONTEXT_LINES))
            } else {
                ctx.out.context_lines()
            };
            let r = stringly::run(ctx, false, false, None, top.or(Some(CONTEXTED_TOP)));
            ctx.out.set_context_lines(prev);
            r
        },
    )?;
    section(
        &format!(
            "[medium] metrics — fns with cyclo >= {} (explain: god-function)",
            CYCLO_THRESHOLD
        ),
        Gate::Advisory,
        &mut || metrics::run(ctx, SortKey::Cyclo, metrics_top, Some(CYCLO_THRESHOLD), true),
    )?;
    section(
        "[low] pass-through — single-call wrapper fns (explain: replication)",
        Gate::Advisory,
        &mut || pass_through::run(ctx, 1),
    )?;

    // The battery-wide line goes back to the normal stream: it is the one an
    // `until unruster audit` loop greps for, and callers already redirect for it.
    ctx.out.set_summary_inline(prev_inline);
    // Both numbers, not just the waiver count: two item-scoped waivers hiding
    // seven findings reported as "2 site(s) waived" understates the reach by
    // 3.5x, which is the exact failure this line exists to prevent.
    let waivers = ctx.suppressions.len();
    let hidden = ctx.suppressions.total_hits();
    ctx.out.summary(&format!(
        "(audit: {} gating + {} advisory finding(s) across {} check(s); \
         exit 1 while gating findings remain{}{})",
        gating,
        advisory,
        checks,
        if strict { "; --strict: all gate" } else { "" },
        if waivers > 0 {
            format!(
                "; {} waiver(s) hiding {} finding(s) — `unruster waivers` to review",
                waivers, hidden
            )
        } else {
            String::new()
        }
    ));
    Ok(gating)
}
