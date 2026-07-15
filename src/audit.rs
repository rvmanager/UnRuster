//! `audit` — the one-shot ranked sweep: runs the full check battery as
//! severity-ordered sections and exits 1 on any finding. Designed as the
//! entry point of an agent loop:
//!
//! ```text
//! until unruster audit --exclude 'fixtures/**'; do <fix top finding>; done
//! ```
//!
//! Sections reuse each command's own scanner and row format (rows to stdout,
//! per-check summary to stderr), so drilling down with the dedicated command
//! shows identical rows. Severity is a static ranking of the check, not of
//! the individual row — the tool finds candidates; the reader (or agent)
//! judges each one.

use crate::casts::CastClass;
use crate::context::AnalysisCtx;
use crate::metrics::SortKey;
use crate::parse::ParsedFile;
use crate::{
    casts, conversion_pairs, dead_code, error_swallows, metrics, parallel_matches, pass_through,
    stringly,
};

/// Cyclomatic-complexity threshold above which a fn counts as an audit
/// finding (matches the playbook's god-fn guidance).
const CYCLO_THRESHOLD: usize = 15;

pub fn run(
    ctx: &AnalysisCtx,
    dead_call_source: &[ParsedFile],
    top: Option<usize>,
) -> anyhow::Result<usize> {
    let metrics_top = top.unwrap_or(20);
    let mut total = 0usize;
    let mut checks = 0usize;

    let mut section = |title: &str, count: anyhow::Result<usize>| -> anyhow::Result<()> {
        if !ctx.summary {
            println!("## {}", title);
        }
        let n = count?;
        total += n;
        checks += 1;
        if !ctx.summary {
            println!();
        }
        Ok(())
    };

    section(
        "[high] enum-coverage --all — partial enum dispatch (explain: partial-enumeration)",
        parallel_matches::run_enum_coverage(ctx, None, false),
    )?;
    section(
        "[high] dead-code — fns with no observed caller",
        dead_code::run(ctx, dead_call_source, false, false),
    )?;
    section(
        "[high] conversion-pairs — one concept in two shapes (explain: replication)",
        conversion_pairs::run(ctx),
    )?;
    section(
        "[medium] error-swallows — silently dropped Results (explain: silent-fallbacks)",
        error_swallows::run(ctx, false),
    )?;
    section(
        "[medium] casts — data-loss classes only (explain: casts)",
        casts::run(
            ctx,
            &[
                CastClass::NarrowInt,
                CastClass::SignedFlip,
                CastClass::FloatInt,
                CastClass::NarrowFloat,
                CastClass::Ptr,
                CastClass::UsizeCross,
            ],
            None,
            false,
            top,
        ),
    )?;
    section(
        "[medium] stringly — logic branching on string literals (explain: stringly)",
        stringly::run(ctx, false, false, None, top),
    )?;
    section(
        &format!(
            "[medium] metrics — fns with cyclo >= {} (explain: god-function)",
            CYCLO_THRESHOLD
        ),
        metrics::run(ctx, SortKey::Cyclo, metrics_top, Some(CYCLO_THRESHOLD), true),
    )?;
    section(
        "[low] pass-through — single-call wrapper fns (explain: replication)",
        pass_through::run(ctx, 1),
    )?;

    eprintln!(
        "(audit: {} finding(s) across {} check(s); exit 1 while any remain)",
        total, checks
    );
    Ok(total)
}
