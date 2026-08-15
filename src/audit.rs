//! `audit` — the one-shot ranked sweep: runs the full check battery as
//! severity-ordered sections and exits 1 on gating findings. Designed as the
//! entry point of an agent loop:
//!
//! ```text
//! until unruster audit; do <fix top finding>; done
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
    arith_drift, casts, concepts, conversion_pairs, dead_code, doc_drift, error_swallows, metrics,
    near_clones, panics, parallel_matches, pass_through, stringly, validation, vocabulary,
};

/// Cyclomatic-complexity threshold above which a fn counts as an audit
/// finding (matches the playbook's god-fn guidance).
const CYCLO_THRESHOLD: usize = 15;

/// Parameter count above which a fn counts as an audit finding.
///
/// Clippy's own `too_many_arguments` default, and it is here because the
/// standard `near-clones` fix trades duplication for parameter count.
/// Parameterising two near-clones into one `draw_two_point_symbol` pushed it
/// from 7 arguments to 8 and tripped that very lint — a consequence `audit`
/// could not see, since its `metrics` section gates on `cyclo` alone. Advisory:
/// this is the cost side of a trade the reader is making on purpose, not a
/// defect.
const PARAMS_THRESHOLD: usize = 7;

/// Row cap for the two checks that get inline source context. Bounds the
/// output: at most this many rows × (2·[`CONTEXT_LINES`] + 1) snippet lines.
const CONTEXTED_TOP: usize = 20;

/// Rows of the metrics ranking shown when `--top` is not given.
const DEFAULT_METRICS_TOP: usize = 20;

/// Rows shown for the two ranked, high-volume checks when `--top` is not given.
///
/// Matches `divergence`'s cap rather than the tighter 20 the low-volume
/// sections use: these two rank their own rows, so the cut is at a score
/// boundary and not an arbitrary one, and 40 still fits in a screenful of
/// scrollback. `--top` overrides it; the count in the summary line, the waiver
/// hits and the `--since` baseline are all taken before the cap applies.
const ERROR_SWALLOWS_TOP: usize = 40;

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

/// Every check in the battery, in the order it runs. The names `--only` and
/// `--skip` accept, and the names that appear as `"check"` in `--json`.
pub const CHECKS: &[&str] = &[
    "divergence",
    "divergence-handling",
    "enum-coverage",
    "dead-code",
    "conversion-pairs",
    "error-swallows",
    "panics",
    "clones",
    "near-clones",
    "concepts",
    "vocabulary",
    "doc-drift",
    "validation-drift",
    "config-drift",
    "builder-drift",
    "arith-drift",
    "casts",
    "stringly",
    "metrics",
    // The same check, ranked the other way. Named separately so `--only` /
    // `--skip` can address the two sections independently — they answer
    // different questions and a reader who wants the complexity ranking rarely
    // wants the argument-count one in the same breath.
    "metrics-params",
    "pass-through",
];

/// The score at which a `Tiered` check's rows start gating.
///
/// The one place the mapping from a section's check name to its own
/// `GATING_SCORE` is written down. Each check owns its constant; this says
/// which section is asking. An unlisted name is treated as gating nothing,
/// which is the safe direction for a *cap* — it can only show more rows.
fn tier_floor(check: &str) -> f64 {
    match check {
        "error-swallows" => crate::error_swallows::GATING_SCORE,
        "panics" => crate::panics::GATING_SCORE,
        "clones" => crate::clones::GATING_SCORE,
        "near-clones" => crate::near_clones::GATING_SCORE,
        "concepts" => crate::concepts::GATING_SCORE,
        "doc-drift" => crate::doc_drift::GATING_SCORE,
        "validation-drift" => crate::validation::GATING_SCORE,
        // `vocabulary` gates on a status rather than a score, and its rows
        // carry no `score` cell for the floor to read.
        _ => f64::INFINITY,
    }
}

/// Which checks this run should execute.
///
/// The battery had no selector at all, so the one recommendation to come out of
/// a 200-defect evaluation — "read `audit` with `error-swallows` left out, and
/// the checks that actually named defects fit on a screen" — could not be
/// typed. `--top` already bounded rows per section; nothing bounded sections.
#[derive(Clone, Default)]
pub struct Selection {
    /// `None` = every check. `Some` = exactly these.
    only: Option<Vec<String>>,
    skip: Vec<String>,
}

impl Selection {
    /// Validate against [`CHECKS`] and build. An unknown name is an error
    /// rather than a silent no-op: `--skip error_swallows` that quietly skips
    /// nothing reads as a check that found nothing.
    pub fn new(only: &[String], skip: &[String]) -> anyhow::Result<Self> {
        let check = |names: &[String], flag: &str| -> anyhow::Result<()> {
            for n in names {
                if !CHECKS.contains(&n.as_str()) {
                    anyhow::bail!(
                        "unknown check `{}` for --{}. Known checks: {}",
                        n,
                        flag,
                        CHECKS.join(", ")
                    );
                }
            }
            Ok(())
        };
        check(only, "only")?;
        check(skip, "skip")?;
        let sel = Selection {
            only: (!only.is_empty()).then(|| only.to_vec()),
            skip: skip.to_vec(),
        };
        if CHECKS.iter().all(|c| !sel.wants(c)) {
            anyhow::bail!("--only / --skip selected no checks at all; nothing would run");
        }
        Ok(sel)
    }

    pub fn wants(&self, check: &str) -> bool {
        if self.skip.iter().any(|s| s == check) {
            return false;
        }
        match &self.only {
            Some(only) => only.iter().any(|o| o == check),
            None => true,
        }
    }

    fn is_full(&self) -> bool {
        self.only.is_none() && self.skip.is_empty()
    }

    /// The checks this selection turns off, for the summary line. A report
    /// missing five of fifteen sections has to say which five, or it reads as a
    /// battery that shrank.
    fn omitted(&self) -> Vec<&'static str> {
        CHECKS
            .iter()
            .filter(|c| !self.wants(c))
            .copied()
            .collect()
    }
}

/// The battery's per-check configuration, defined once.
///
/// `waivers` must run the identical set to count waiver hits — if the two drift,
/// orphan detection starts answering a different question than the gating loop
/// asks, which is exactly the defect these helpers exist to prevent.
// unruster: ok(config-drift/CoverageOpts) 2026-08-12 — these are the two
// presets by construction: the gating configuration and the `--strict`
// permissive one. Differing is what they are for, and `config-drift`'s own doc
// names "two presets" as the deliberate case it cannot tell from a defect.
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

// unruster: ok(config-drift/SwallowOpts) 2026-08-11 — the two configurations
// are the point. `BatteryConfig` exists precisely so the gating pass and the
// permissive one sit side by side and can be diffed by eye; they differed
// silently as two hand-written call sequences before that.
pub fn swallow_opts() -> error_swallows::SwallowOpts {
    error_swallows::SwallowOpts {
        include_unwrap_or: false,
        // Infallible `write!` into a String and fallbacks that already log are
        // the two families that dominated this check's output while producing
        // no defects.
        include_infallible: false,
        include_logged: false,
        min_score: 0.0,
    }
}

// unruster: ok(config-drift/PanicOpts) 2026-08-11 — same as its `SwallowOpts`
// sibling above: `audit` reads for defects and hides the idiomatic families,
// the permissive pass reports everything so `waivers` can tell "earns nothing
// here" from "earns nothing anywhere".
pub fn panic_opts() -> panics::PanicOpts {
    panics::PanicOpts {
        // `Mutex::lock().unwrap()` and friends: the panic is the documented
        // response to a poisoned lock, and on the tree this was calibrated
        // against they were the single largest family.
        include_idiomatic: false,
        min_score: 0.0,
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
    pub panics: panics::PanicOpts,
    pub arith_min_score: f64,
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
            panics: panic_opts(),
            arith_min_score: ARITH_DRIFT_MIN_SCORE,
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
                min_score: 0.0,
            },
            panics: panics::PanicOpts {
                include_idiomatic: true,
                min_score: 0.0,
            },
            arith_min_score: 0.0,
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
pub fn run_silent_battery(
    ctx: &AnalysisCtx,
    dead_call_source: &[ParsedFile],
    cfg: BatteryConfig,
    sel: &Selection,
) {
    // `--top` is enforced in the emitter, after fingerprint recording, so it
    // cannot affect hit counts or baselines however this battery is invoked.
    //
    // `sel` is honoured here as well as in `run`. It has to be: this is the
    // baseline half of `--since`, and a baseline that ran a check the current
    // run skipped reports every one of that check's findings as `gone`.
    let checks: [(&str, &dyn Fn() -> anyhow::Result<usize>); 20] = [
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
            dead_code::run(ctx, dead_call_source, None, false, false)
        }),
        ("conversion-pairs", &|| conversion_pairs::run(ctx)),
        ("clones", &|| {
            clones::run(ctx, clones::DEFAULT_MIN_TOKENS)
        }),
        ("near-clones", &|| {
            near_clones::run(
                ctx,
                ctx.corpus,
                &near_clones::Opts {
                    min_tokens: near_clones::DEFAULT_MIN_TOKENS,
                    max_diff: near_clones::DEFAULT_MAX_DIFF,
                    min_score: 0.0,
                },
            )
        }),
        ("concepts", &|| {
            concepts::run(
                ctx,
                ctx.corpus,
                &concepts::Opts {
                    kind: None,
                    min_score: concepts::DEFAULT_MIN_SCORE,
                },
            )
        }),
        ("vocabulary", &|| {
            vocabulary::run(
                ctx,
                ctx.corpus,
                &vocabulary::Opts {
                    all: false,
                    coverage: false,
                },
            )
        }),
        ("doc-drift", &|| {
            doc_drift::run(
                ctx,
                &doc_drift::Opts {
                    names: false,
                    min_score: 0.0,
                },
            )
        }),
        ("validation-drift", &|| {
            validation::run_drift(ctx, VALIDATION_DRIFT_MIN_SCORE)
        }),
        ("error-swallows", &|| error_swallows::run(ctx, cfg.swallows)),
        ("panics", &|| panics::run(ctx, cfg.panics)),
        ("casts", &|| {
            casts::run(ctx, cfg.cast_classes, None, false, cfg.include_unsafe_ptr)
        }),
        ("config-drift", &|| {
            config_drift::run(ctx, None, CONFIG_DRIFT_MIN_SCORE)
        }),
        ("builder-drift", &|| {
            builder_drift::run(ctx, None, BUILDER_DRIFT_MIN_SCORE)
        }),
        ("arith-drift", &|| {
            arith_drift::run(ctx, cfg.arith_min_score)
        }),
        // The advisory three too. They consult no waivers, so they add nothing
        // to hit counting — but a *baseline* comparison that omitted them would
        // report every stringly and metrics row as new on the first run.
        ("stringly", &|| {
            stringly::run(ctx, false, false, None)
        }),
        ("metrics", &|| {
            metrics::run(ctx, SortKey::Cyclo, Some(CYCLO_THRESHOLD), true, crate::context::GroupBy::Fn)
        }),
        ("pass-through", &|| pass_through::run(ctx, 1)),
    ];
    for (name, check) in checks {
        if !sel.wants(name) {
            continue;
        }
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

/// Validation-drift score below which rows are dropped from the audit section.
/// One unchecked sibling among two checked ones scores 0.62; a one-to-one split
/// lands at 0.47, which is the noise this cut excludes.
pub const VALIDATION_DRIFT_MIN_SCORE: f64 = 0.55;

/// Minimum care distance for the `--handling` axis.
pub const HANDLING_MIN_CARE_GAP: u8 = 2;

/// Arithmetic-drift score below which rows are dropped from the audit section.
/// One raw operator among three saturating siblings scores 0.75; an even split
/// (one of two) scores 0.5 and is usually two different jobs in one scope.
pub const ARITH_DRIFT_MIN_SCORE: f64 = 0.6;

pub fn run(
    ctx: &AnalysisCtx,
    dead_call_source: &[ParsedFile],
    top: Option<usize>,
    strict: bool,
    findings_only: bool,
    sel: &Selection,
) -> anyhow::Result<usize> {
    let mut gating = 0usize;
    let mut advisory = 0usize;
    let mut checks = 0usize;
    let mut skipped_clean = 0usize;
    // Line one, before any section, so it survives the `head` it is warning
    // about. Every session's first `audit` was piped and cut; one of them cost
    // three recovery commands (`| head -200`, then `cat` the tool-results file,
    // then `sed -n '199,500p'`). The same mechanism that fixed `show` — say how
    // much is coming, *before* it comes.
    if !ctx.summary {
        ctx.out.note(&format!(
            "(note: {} check(s){}; each section caps its advisory rows{} and names the \
             command that lists the rest. Every gating row is shown whatever the cap, so \
             this digest is complete about what holds the exit code open — read it whole \
             rather than piping it to `head`.)",
            CHECKS.len(),
            if findings_only {
                ", clean ones dropped"
            } else {
                ""
            },
            match top {
                Some(0) => " (uncapped by --top 0)".to_string(),
                Some(n) => format!(" at --top {}", n),
                None => String::new(),
            }
        ));
    }
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
    // The row cap must never hide a gating row. `--findings-only` is sold as a
    // complete digest of what gates, and every session's first `audit` was
    // piped to `head` and cut — three recovery commands in one of them — so a
    // cap that can silently drop the rows the exit code is about is the same
    // defect one layer in. Rows arrive score-sorted, so this only fires when a
    // section's gating tier is longer than its default cap.
    let floor_of = |gate: Gate, check: &str| -> Option<f64> {
        if strict {
            return Some(f64::NEG_INFINITY);
        }
        match gate {
            // Every row gates: none of them may be dropped.
            Gate::Gating => Some(f64::NEG_INFINITY),
            // No row gates: the cap is the whole story.
            Gate::Advisory => None,
            Gate::Tiered => Some(tier_floor(check)),
        }
    };
    let mut section = |title: &str,
                       check: &str,
                       gate: Gate,
                       cap: Option<usize>,
                       count: &mut dyn FnMut() -> anyhow::Result<Counts>|
     -> anyhow::Result<()> {
        // `--only` / `--skip`: the check does not run at all, so it costs
        // nothing and contributes nothing. The closing line names every check
        // left out, so a shortened report cannot be misread as a clean one.
        if !sel.wants(check) {
            return Ok(());
        }
        // The check name is part of every fingerprint: two checks reporting the
        // same line must not collapse into one identity. Set *before* the
        // section opens so an empty section still carries its check in `--json`
        // — otherwise a clean `metrics` and a clean `dead-code` are the same
        // anonymous object.
        let prev = ctx.out.set_check(check);
        ctx.out.section(title);
        ctx.out
            .set_row_budget_keeping(top.or(cap), floor_of(gate, check));
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
        // Taken while this check is still the current one: the note names the
        // command that gives the rest, and `set_check` below restores `audit`.
        let cap_note = ctx.out.cap_note();
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
        if let Some(note) = cap_note {
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
        &mut || Ok(Counts::flat(dead_code::run(ctx, dead_call_source, None, false, false)?)),
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
        // Capped like every other section that can run long. It was the one
        // that was not: on a twelve-crate workspace it emitted 665 of the
        // battery's ~800 rows — 82% of the output — and the reader who gave up
        // on it gave up on the battery. Rows are score-sorted, so the cap keeps
        // the tier that gates and drops the tail, and `cap_note` says so.
        Some(ERROR_SWALLOWS_TOP),
        &mut || error_swallows::run_counted(ctx, swallow_opts()),
    )?;
    section(
        &format!(
            "[high] panics — `.unwrap()` / `.expect()` / `panic!` on fallible work; \
             gating at score >= {:.2} (explain: silent-fallbacks)",
            panics::GATING_SCORE
        ),
        "panics",
        // Tiered like its sibling: the rows that gate are the ones that panic
        // on data the process did not produce — a parse of an argument, a
        // response, a file — where a crash is the whole defect.
        Gate::Tiered,
        Some(ERROR_SWALLOWS_TOP),
        &mut || panics::run_counted(ctx, panic_opts()),
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
        &mut || clones::run_counted(ctx, clones::DEFAULT_MIN_TOKENS, 0.0),
    )?;
    section(
        &format!(
            "[high] near-clones — one body, edited on one side only; gating at score \
             >= {:.2} (explain: replication)",
            near_clones::GATING_SCORE
        ),
        "near-clones",
        // Tiered beside `clones`, and ranked above it in severity for the same
        // reason the check exists: two identical copies are a maintenance
        // smell, whereas two copies that differ in one leaf are a fix that
        // landed once. The row names the leaf, so the top of this section is
        // the shortest path from "run the battery" to "open one file".
        Gate::Tiered,
        Some(20),
        &mut || {
            near_clones::run_counted(
                ctx,
                ctx.corpus,
                &near_clones::Opts {
                    min_tokens: near_clones::DEFAULT_MIN_TOKENS,
                    max_diff: near_clones::DEFAULT_MAX_DIFF,
                    min_score: 0.0,
                },
            )
        },
    )?;
    section(
        &format!(
            "[high] concepts — one concept declared more than once; gating at score \
             >= {:.2} (explain: concept-drift)",
            concepts::GATING_SCORE
        ),
        "concepts",
        // Tiered: a cognate pair inside one module is a lead, three cognate
        // declarations spread across modules is a concept somebody duplicated.
        // The score already draws that line, so the gate follows it.
        Gate::Tiered,
        Some(20),
        &mut || {
            concepts::run_counted(
                ctx,
                ctx.corpus,
                &concepts::Opts {
                    kind: None,
                    min_score: concepts::DEFAULT_MIN_SCORE,
                },
            )
        },
    )?;
    section(
        "[high] vocabulary — a declared concept claimed twice, or drifted away from; \
         gating on duplicate/malformed/undeclared (explain: vocabulary)",
        "vocabulary",
        // Tiered: `unclaimed` is advisory and off by default here, so a
        // codebase that has not adopted `concept(…)` reports nothing rather
        // than failing its own gate on the first run.
        Gate::Tiered,
        Some(20),
        &mut || {
            vocabulary::run_counted(
                ctx,
                ctx.corpus,
                &vocabulary::Opts {
                    all: false,
                    coverage: false,
                },
            )
        },
    )?;
    section(
        &format!(
            "[medium] doc-drift — the docs and the code disagreeing; gating at score \
             >= {:.2} (explain: doc-drift)",
            doc_drift::GATING_SCORE
        ),
        "doc-drift",
        // Tiered: an unbacked `# Panics`/`# Errors` heading is a contradiction
        // and gates; a missing heading or a suspicious name in prose is a lead.
        Gate::Tiered,
        Some(20),
        &mut || {
            doc_drift::run_counted(
                ctx,
                // `stale-name` is off here as well as by default: it could not
                // survive a run over this codebase (205 rows, essentially all
                // wrong), and a class that cannot do that has no business
                // holding an agent loop open.
                &doc_drift::Opts {
                    names: false,
                    min_score: 0.0,
                },
            )
        },
    )?;
    section(
        &format!(
            "[medium] validation-drift — a sibling that checks nothing among siblings that \
             do; gating at score >= {:.2} (explain: validation-drift)",
            validation::GATING_SCORE
        ),
        "validation-drift",
        Gate::Tiered,
        Some(15),
        &mut || validation::run_drift_counted(ctx, VALIDATION_DRIFT_MIN_SCORE),
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
        "[medium] arith-drift — one raw operator among saturating siblings (explain: divergence)",
        "arith-drift",
        // Advisory with its drift siblings: a scope that mixes `+` and
        // `saturating_add` is often right, because the raw op is on a value the
        // author knows cannot overflow. The rows are worth reading — this check
        // exists because three of four adjacent RFC 9111 age terms saturated and
        // the fourth did not.
        Gate::Advisory,
        Some(20),
        &mut || Ok(Counts::flat(arith_drift::run(ctx, ARITH_DRIFT_MIN_SCORE)?)),
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
        &mut || Ok(Counts::flat(metrics::run(ctx, SortKey::Cyclo, Some(CYCLO_THRESHOLD), true, crate::context::GroupBy::Fn)?)),
    )?;
    section(
        &format!(
            "[low] metrics — fns with params >= {} (explain: god-function)",
            PARAMS_THRESHOLD
        ),
        "metrics-params",
        Gate::Advisory,
        Some(DEFAULT_METRICS_TOP),
        &mut || {
            Ok(Counts::flat(metrics::run(
                ctx,
                SortKey::Params,
                Some(PARAMS_THRESHOLD),
                true,
                crate::context::GroupBy::Fn,
            )?))
        },
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
        "(audit: {} gating + {} advisory finding(s) across {} check(s){}{}; {}{}{})",
        gating,
        advisory,
        checks,
        // A selected run reports what it did not look at. Without this the
        // battery's own line is the same shape whether five checks were
        // silenced or found nothing, and "0 gating findings" would be read as
        // "clean" either way.
        if sel.is_full() {
            String::new()
        } else {
            format!(
                " of {}; --only/--skip left out: {}",
                CHECKS.len(),
                sel.omitted().join(", ")
            )
        },
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
