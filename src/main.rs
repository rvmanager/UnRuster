use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

mod ast;
mod callers;
mod casts;
mod catch_all;
mod cfg_eval;
mod context;
mod conversion_pairs;
mod conversions;
mod dead_code;
mod error_swallows;
mod field_uses;
mod fields;
mod impls;
mod index;
mod inventory;
mod macro_scan;
mod metrics;
mod parallel_matches;
mod parse;
mod pass_through;
mod semantic;
mod stringly;
mod takes_mut;
mod tests_cmd;
mod type_refs;
mod variants;

use context::AnalysisCtx;
use parse::Scope;

#[derive(Parser)]
#[command(
    name = "unruster",
    about = "Query a Rust codebase: inventory, callers/callees, field uses, variants, impls, metrics, dead-code.",
    long_about = include_str!("playbook.txt"),
    version
)]
struct Cli {
    /// Root directory (or file) to scan. Respects .gitignore.
    #[arg(long, short = 'r', default_value = ".")]
    root: PathBuf,

    /// Test-code scope: production (default), tests, or all.
    /// Aliases: `prod` = production, `test` = tests.
    #[arg(long, global = true, value_enum, default_value = "production")]
    scope: Scope,

    /// `--cfg KEY` or `--cfg KEY=VALUE` (repeatable). Items whose cfg
    /// evaluates to definitively False under this env are stripped. Unknown
    /// keys (no `--cfg` provided) leave the item in (best-effort).
    #[arg(long, global = true)]
    cfg: Vec<String>,

    /// Skip per-row output; print only the summary line on stderr.
    #[arg(long, global = true)]
    summary: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List all top-level items (struct, enum, trait, fn, impl, ...).
    Inventory(InventoryArgs),
    /// Find call sites of a function, method, or macro.
    Callers(CallersArgs),
    /// List callees made from inside a function or method.
    Callees(CalleesArgs),
    /// Paired-action invariant check: for a coupled pair (A, B) where calling
    /// one without the other leaks an invariant, list the asymmetric callers —
    /// fns that call A but not B (`A-only`) or B but not A (`B-only`). Both-
    /// callers are the canonical pattern (counted, not listed). Each row is a
    /// candidate; some asymmetries are correct, so a human filters.
    CoCall(CoCallArgs),
    /// Find read/write sites of a field on a given type.
    FieldUses(FieldUsesArgs),

    /// List fields of a struct with read/write/init counts per field.
    Fields(FieldsArgs),
    /// List enum variants and their construction + match sites.
    Variants(VariantsArgs),
    /// List `impl` blocks; filter by self-type or by trait.
    Impls(ImplsArgs),
    /// Find every site that names a given type (coupling footprint).
    TypeRefs(TypeRefsArgs),
    /// Find fns whose signature takes `&mut <Type>`.
    TakesMut(TakesMutArgs),
    /// Rank fns by LOC, params, cyclomatic complexity, or nesting depth;
    /// structs by field count; enums by variant count. Use `--threshold N` to
    /// filter by the sort metric.
    Metrics(MetricsArgs),

    /// List fns with no caller in the scanned tree (heuristic; pub items may have external callers).
    DeadCode(DeadCodeArgs),
    /// Find match sites on a given enum that contain a wildcard `_ =>` arm.
    CatchAllArms(CatchAllArgs),
    /// Group match sites on an enum by which variants they cover (shotgun-surgery candidates).
    /// `--hide-exhaustive` hides compiler-protected exhaustive groups; `--rank-by-gap`
    /// sorts by coverage ratio; `--show-missing` lists uncovered variants;
    /// `--include-matches-macro` also scans `matches!()`.
    ParallelMatches(ParallelMatchesArgs),
    /// Score every PARTIAL match / `matches!` site on an enum by coverage
    /// (gap_score = covered/total), sorted descending. Top rows are the
    /// predicates closest to exhaustive — the ones a newly-added variant would
    /// silently mis-bind. Synthesis of `parallel-matches --hide-exhaustive
    /// --rank-by-gap --show-missing --include-matches-macro`.
    /// `--hide-trait-routed-catchalls` drops rows whose `_` arm calls a method
    /// on the scrutinee (structurally-safe false positives).
    EnumCoverage(EnumCoverageArgs),
    /// Cohort divergence matrix: for a name-pattern cohort of fns (e.g.
    /// `wrap_in_*`), show a (callee × function) grid. A callee called by most
    /// of the cohort but missing from one column is a divergence candidate —
    /// the sibling that forgot to call a shared helper.
    CohortCallees(CohortCalleesArgs),
    /// Find Result/Option error-swallowing patterns. Detects method calls
    /// (`.ok()`, `.err()`, `.unwrap_or_default()`, `.unwrap_or_else(...)`,
    /// `.map_err(|_|...)`) and syntactic forms (`match { Err(_) => ... }`,
    /// `if let Ok(...)` with no else, `while let Ok(...)`, `let _ = expr;`).
    /// Each row carries a `kind` label so you can grep by category. Some hits
    /// are intentional (e.g. `let _ =` of a Drop guard) — review per site.
    ErrorSwallows(ErrorSwallowsArgs),
    /// Find pass-through wrappers: fns whose body is a single call/expression.
    PassThrough(PassThroughArgs),

    /// Find `as` casts; classifies narrowing / signed-flip / pointer / float-int /
    /// usize-cross. Many casts in one fn = shape-juggling design smell:
    /// pick one type at the boundary, cast once, pass the typed value through.
    Casts(CastsArgs),
    /// Find conversion method/fn calls (.into / .to_string / Type::from / ...).
    /// Use `--by fn --top 10` to find conversion-heavy fns — a fn with many
    /// conversion calls is reshaping the same value repeatedly, usually a sign
    /// the wrong type was chosen at the boundary.
    Conversions(ConversionsArgs),
    /// Find bidirectional `From<A> for B` + `From<B> for A` pairs — same
    /// concept in two shapes, prime merge candidates: collapse to one type,
    /// or make one a view (`AsRef`) of the other.
    ConversionPairs,
    /// Find stringly-typed code: branching/matching on string literals.
    /// Catches `x == "lit"`, `x.eq("lit")`, `match x { "lit" => ... }`,
    /// `assert_eq!(x, "lit")`. Each row = candidate for an enum or newtype
    /// (e.g. `pub struct ActionId(&'static str)`) so the compiler catches
    /// typos and missing cases.
    Stringly(StringlyArgs),
    /// List `#[test]`/`#[bench]`/`#[tokio::test]` fns with their
    /// `file:start-end` and name. Always scans the full tree (ignores --scope)
    /// since test code is the whole point. Use `--with-hint` to include the
    /// `args(...)` body fingerprint; use `--by subcommand` to group tests by
    /// which CLI subcommand they invoke (assert_cmd-style: looks at
    /// `.args([...])`).
    Tests(TestsArgs),
}

#[derive(Args)]
struct InventoryArgs {
    /// Filter to one kind (`fn` = free fns; methods are `impl-fn`).
    #[arg(long, short = 'k', value_enum)]
    kind: Option<inventory::ItemKind>,
    /// Filter by visibility.
    #[arg(long, value_enum)]
    vis: Option<inventory::VisFilter>,
    /// Render as a module tree instead of a flat list.
    #[arg(long)]
    tree: bool,
}

#[derive(Args)]
struct CallersArgs {
    /// Function, method, or macro to look for. Forms:
    ///   bare name (e.g. `translate`)        — matches free fns, methods, and macros by last segment
    ///   `Type::method` (e.g. `Doc::write`)  — matches paths ending in `Type::method`
    ///   `.method` (e.g. `.write`)           — matches method calls only
    ///   `::name` (e.g. `::open`)            — matches free-fn paths only (skip methods/macros)
    ///   `name!` (e.g. `eprintln!`)          — matches macro invocations only
    name: String,
    /// Include indirect callers via the static call graph (last-segment name matching).
    #[arg(long)]
    transitive: bool,
    /// Maximum transitive depth (default: unlimited).
    #[arg(long)]
    depth: Option<usize>,
    /// Group results by file (path) or module (top-level module).
    #[arg(long, value_enum)]
    by: Option<callers::CallersBy>,
    /// Cohort mode: invert the query. Instead of listing call sites, show which
    /// functions in this name-pattern cohort (last-segment glob, `*` = any run,
    /// e.g. `wrap_in_*`) call the named helper (✓) and which don't (✗). The ✗
    /// rows — siblings that skip a helper their cohort-mates use — are your
    /// divergence candidates.
    #[arg(long)]
    among: Option<String>,
}

#[derive(Args)]
struct CalleesArgs {
    /// Containing function (last-segment match: `translate` or `Doc::translate`).
    name: String,
}

#[derive(Args)]
struct CoCallArgs {
    /// First half of the coupled pair (the "A" action). Same target forms as
    /// `callers`: bare name, `Type::method`, `.method`, `::name`, `name!`.
    a: String,
    /// Second half of the coupled pair (the "B" action). Same forms as `a`.
    b: String,
}

#[derive(Args)]
struct FieldUsesArgs {
    /// Type name (last segment only, e.g. `Document`).
    ty: String,
    /// Field name.
    field: String,
    /// Also report non-self field accesses (noisier; no type inference).
    #[arg(long)]
    candidates: bool,
    /// Filter to one or more comma-separated access kinds (e.g. `read,write`).
    #[arg(long, value_enum, value_delimiter = ',')]
    kind: Vec<field_uses::FieldKind>,
    /// (With --candidates) restrict hits to a substring of the receiver
    /// expression — e.g. `--via-receiver common` keeps `x.common.transform` but
    /// drops `node.transform`.
    #[arg(long)]
    via_receiver: Option<String>,
}

#[derive(Args)]
struct FieldsArgs {
    /// Struct name (last segment, e.g. `Document`).
    ty: String,
}

#[derive(Args)]
struct VariantsArgs {
    /// Enum name (last segment, e.g. `Token`).
    name: String,
    /// Match bare variant names too (e.g. `V1` in addition to `Enum::V1`).
    /// Useful when callers `use Enum::*;` — noisier.
    #[arg(long)]
    bare: bool,
}

#[derive(Args)]
struct ImplsArgs {
    /// Filter to impls of this self-type (last segment).
    #[arg(long)]
    of: Option<String>,
    /// Filter to impls of this trait (last segment).
    #[arg(long = "trait")]
    trait_: Option<String>,
}

#[derive(Args)]
struct TypeRefsArgs {
    /// Type name (last segment).
    ty: String,
}

#[derive(Args)]
struct TakesMutArgs {
    /// Type name (last segment).
    ty: String,
}

#[derive(Args)]
struct MetricsArgs {
    /// Sort fns by: `loc` (lines), `params`, `cyclo` (cyclomatic complexity),
    /// `nesting` (max control-flow nesting depth).
    #[arg(long, value_enum, default_value = "loc")]
    sort: metrics::SortKey,
    /// Top N per category to print.
    #[arg(long, default_value_t = 20)]
    top: usize,
    /// Only show fns where the sort metric is >= N. E.g. with
    /// `--sort cyclo --threshold 15`, only fns with cyclo >= 15.
    #[arg(long)]
    threshold: Option<usize>,
}

#[derive(Args)]
struct DeadCodeArgs {
    /// Only list pub items.
    #[arg(long)]
    pub_only: bool,
}

#[derive(Args)]
struct CatchAllArgs {
    /// Enum name (last segment).
    name: String,
}

#[derive(Args)]
struct ParallelMatchesArgs {
    /// Enum name (last segment).
    name: String,
    /// Hide exhaustive groups (variant set == the full enum). Exhaustive
    /// matches are compiler-protected; only partials can silently mis-bind a
    /// newly-added variant.
    #[arg(long, alias = "partial")]
    hide_exhaustive: bool,
    /// Sort groups by coverage ratio (covered/total) descending instead of by
    /// site count. A 7/8 predicate is a louder defect signal than a 1/8 one.
    /// Prefixes each group with `[covered/total]`.
    #[arg(long)]
    rank_by_gap: bool,
    /// For each group, also list the variants NOT covered.
    #[arg(long)]
    show_missing: bool,
    /// Also scan `matches!(x, Enum::V ...)` invocations. `matches!` carries an
    /// implicit no-match arm, so it's treated as a wildcard group — exactly the
    /// silent-misclassify risk. Off by default for back-compat; guaranteed-
    /// supported (not best-effort) when set. `enum-coverage` always includes it.
    #[arg(long)]
    include_matches_macro: bool,
    /// Also scan `if x == Enum::A { … } else if x == Enum::B { … }` dispatch
    /// chains (length ≥ 2). The implicit/explicit `else` silently re-bins a
    /// newly-added variant, exactly like a `match` with `_` or a partial
    /// `matches!`. Off by default for back-compat; guaranteed-supported when
    /// set. `enum-coverage` always includes it.
    #[arg(long)]
    include_if_chains: bool,
}

#[derive(Args)]
struct EnumCoverageArgs {
    /// Enum name (last segment).
    name: String,
    /// Hide rows whose catch-all / `_` arm routes through a method call on the
    /// matched scrutinee (e.g. `_ => node.paint_slots()`). Those sites are
    /// structurally safe — a newly-added variant must implement the trait
    /// method, so the catch-all picks up its behavior automatically — but the
    /// tool can't see through the call and would otherwise flag them. Cuts the
    /// noise; read the remaining rows' `_` arms to confirm.
    #[arg(long)]
    hide_trait_routed_catchalls: bool,
}

#[derive(Args)]
struct CohortCalleesArgs {
    /// Name pattern for the cohort (last-segment glob, `*` = any run). E.g.
    /// `wrap_in_*`, `*_handler`, `parse_*_token`.
    pattern: String,
}

#[derive(Args)]
struct ErrorSwallowsArgs {
    /// Include `.unwrap_or(...)` (any arg). Noisy; off by default.
    #[arg(long)]
    include_unwrap_or: bool,
}

#[derive(Args)]
struct PassThroughArgs {
    /// Maximum body LOC to consider as pass-through (default 1).
    #[arg(long, default_value_t = 1)]
    max_loc: usize,
}

#[derive(Args)]
struct CastsArgs {
    /// Filter to one or more comma-separated classes.
    #[arg(long, value_enum, value_delimiter = ',')]
    class: Vec<casts::CastClass>,
    /// Group + count: fn, file, or module.
    #[arg(long, value_enum)]
    by: Option<context::GroupBy>,
    /// Hide safe-widening rows (widen-int / widen-float).
    #[arg(long, alias = "no-widen")]
    hide_widen: bool,
    /// Show only the top N rows (applies after --by grouping if set).
    #[arg(long)]
    top: Option<usize>,
}

#[derive(Args)]
struct TestsArgs {
    /// Include a compact fingerprint of the test body's first `.args([...])`
    /// call (the `--root <path>` / `--scope <val>` prefix is stripped).
    #[arg(long)]
    with_hint: bool,
    /// Group + count tests by a dimension. `subcommand`: which CLI subcommand
    /// each test invokes (heuristic: scans `.args([...])` calls in the body
    /// for a known-subcommand-shaped string literal). Drops the per-test
    /// list, prints a histogram.
    #[arg(long, value_enum)]
    by: Option<TestsBy>,
}

#[derive(Clone, Copy, ValueEnum)]
enum TestsBy {
    Subcommand,
}

#[derive(Args)]
struct StringlyArgs {
    /// Also flag `.starts_with("lit")` / `.ends_with("lit")` / `.contains("lit")`.
    /// Off by default — many legitimate text-processing uses.
    #[arg(long)]
    include_substring: bool,
    /// Also flag `map.get("lit")` / `.contains_key("lit")` / `.remove("lit")`.
    /// Off by default — many legitimate canonical-key map uses.
    #[arg(long)]
    include_map_keys: bool,
    /// Group + count: fn, file, or module.
    #[arg(long, value_enum)]
    by: Option<context::GroupBy>,
    /// Show only the top N rows (applies after --by grouping if set).
    #[arg(long)]
    top: Option<usize>,
}

#[derive(Args)]
struct ConversionsArgs {
    /// Filter to one or more comma-separated kinds (e.g. `.into,::from`).
    #[arg(long, value_enum, value_delimiter = ',')]
    kind: Vec<conversions::ConvKind>,
    /// Group + count: fn, file, or module. Without --by, lists every site.
    #[arg(long, value_enum)]
    by: Option<context::GroupBy>,
    /// Show only the top N rows (applies after --by grouping if set).
    #[arg(long)]
    top: Option<usize>,
}

/// Derive the CLI's own grammar (subcommand names + which flags consume a
/// value) from clap introspection. `tests` uses this to classify test bodies
/// by the subcommand they invoke; deriving it here means the lists can never
/// drift when a subcommand or flag is added.
fn cli_grammar() -> tests_cmd::CliGrammar {
    use clap::CommandFactory;
    let cmd = Cli::command();
    let subcommands: Vec<String> = cmd
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();
    let mut value_flags = std::collections::BTreeSet::new();
    let mut collect = |c: &clap::Command| {
        for a in c.get_arguments() {
            if !a.get_action().takes_values() {
                continue;
            }
            if let Some(l) = a.get_long() {
                value_flags.insert(format!("--{}", l));
            }
            if let Some(s) = a.get_short() {
                value_flags.insert(format!("-{}", s));
            }
        }
    };
    collect(&cmd);
    for sc in cmd.get_subcommands() {
        collect(sc);
    }
    tests_cmd::CliGrammar {
        subcommands,
        value_flags,
    }
}

/// Some subcommands (`dead-code`, `tests`) must reason over the FULL tree —
/// tests and `cfg(test)` items — regardless of the user's `--scope`. Re-parse
/// the tree under `Scope::All`, but skip the work when the production scan was
/// already `Scope::All` (the caller falls back to its own `files`). Returns
/// `None` in that case so the caller can reuse what it has.
fn full_tree_if_needed(
    root: &std::path::Path,
    scope: Scope,
    cfg: &[String],
) -> Result<Option<Vec<parse::ParsedFile>>> {
    if scope == Scope::All {
        Ok(None)
    } else {
        Ok(Some(parse::parse_dir(root, Scope::All, cfg)?))
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let scope = cli.scope;
    let files = parse::parse_dir(&cli.root, scope, &cli.cfg)?;
    if files.is_empty() {
        eprintln!(
            "warning: no .rs files found under {} (scope={:?})",
            cli.root.display(),
            scope
        );
    }
    let idx = index::NameIndex::build(&files);
    let sem = semantic::Semantic::build(&files);
    let ctx = AnalysisCtx {
        files: &files,
        idx: &idx,
        sem: &sem,
        summary: cli.summary,
    };
    let result = match cli.cmd {
        Cmd::Inventory(a) => inventory::run(&ctx, a.kind, a.vis, a.tree),
        Cmd::Callers(a) => {
            if let Some(pattern) = a.among.as_deref() {
                callers::run_callers_among(&ctx, &a.name, pattern)
            } else {
                callers::run_callers(&ctx, &a.name, a.transitive, a.depth, a.by)
            }
        }
        Cmd::Callees(a) => callers::run_callees(&ctx, &a.name),
        Cmd::CoCall(a) => callers::run_co_call(&ctx, &a.a, &a.b),
        Cmd::FieldUses(a) => field_uses::run(
            &ctx,
            &a.ty,
            &a.field,
            !a.candidates,
            &a.kind,
            a.via_receiver.as_deref(),
        ),
        Cmd::Fields(a) => fields::run(&ctx, &a.ty),
        Cmd::Variants(a) => variants::run(&ctx, &a.name, a.bare),
        Cmd::Impls(a) => impls::run(&ctx, a.of.as_deref(), a.trait_.as_deref()),
        Cmd::TypeRefs(a) => type_refs::run(&ctx, &a.ty),
        Cmd::TakesMut(a) => takes_mut::run(&ctx, &a.ty),
        Cmd::Metrics(a) => metrics::run(&ctx, a.sort, a.top, a.threshold),
        Cmd::DeadCode(a) => {
            // Build the call-set from the FULL tree so production items called
            // only from tests aren't false-flagged as dead.
            let all_files = full_tree_if_needed(&cli.root, scope, &cli.cfg)?;
            let call_source = all_files.as_deref().unwrap_or(&files);
            dead_code::run(&ctx, call_source, a.pub_only)
        }
        Cmd::CatchAllArms(a) => catch_all::run(&ctx, &a.name),
        Cmd::ParallelMatches(a) => parallel_matches::run(
            &ctx,
            &a.name,
            parallel_matches::ScanOpts {
                partial_only: a.hide_exhaustive,
                rank_by_gap: a.rank_by_gap,
                show_missing: a.show_missing,
                include_matches_macro: a.include_matches_macro,
                include_if_chains: a.include_if_chains,
            },
        ),
        Cmd::EnumCoverage(a) => {
            parallel_matches::run_enum_coverage(&ctx, &a.name, a.hide_trait_routed_catchalls)
        }
        Cmd::CohortCallees(a) => callers::run_cohort_callees(&ctx, &a.pattern),
        Cmd::ErrorSwallows(a) => error_swallows::run(&ctx, a.include_unwrap_or),
        Cmd::PassThrough(a) => pass_through::run(&ctx, a.max_loc),
        Cmd::Casts(a) => casts::run(&ctx, &a.class, a.by, a.hide_widen, a.top),
        Cmd::Conversions(a) => conversions::run(&ctx, &a.kind, a.by, a.top),
        Cmd::ConversionPairs => conversion_pairs::run(&ctx),
        Cmd::Stringly(a) => {
            stringly::run(&ctx, a.include_substring, a.include_map_keys, a.by, a.top)
        }
        Cmd::Tests(a) => {
            // Always scan the full tree — under --scope production the tests we
            // want to enumerate would be stripped.
            let all_files = full_tree_if_needed(&cli.root, scope, &cli.cfg)?;
            let source = all_files.as_deref().unwrap_or(&files);
            tests_cmd::run(
                &ctx,
                source,
                a.with_hint,
                matches!(a.by, Some(TestsBy::Subcommand)),
                &cli_grammar(),
            )
        }
    };
    if let Err(e) = &result {
        if e.downcast_ref::<context::TargetNotFound>().is_some() {
            // The command already printed the warning (and its summary line);
            // exit 2 distinguishes "unknown target" from "no findings" (exit 0).
            std::process::exit(2);
        }
    }
    result
}
