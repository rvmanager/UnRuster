use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

mod arith_drift;
mod ast;
mod audit;
mod builder_drift;
mod baseline;
mod callers;
mod casts;
mod catch_all;
mod cfg_eval;
mod clones;
mod context;
mod config_drift;
mod contract_drift;
mod conversion_pairs;
mod conversions;
mod dead_code;
mod divergence;
mod emit;
mod error_swallows;
mod explain;
mod field_uses;
mod fingerprint;
mod fields;
mod impls;
mod index;
mod inventory;
mod macro_scan;
mod metrics;
mod outline;
mod parallel_matches;
mod parse;
mod panics;
mod pass_through;
mod semantic;
mod show;
mod stringly;
mod suppress;
mod takes_mut;
mod tests_cmd;
mod type_refs;
mod variants;
mod waivers_cmd;
mod workspace;

use crate::emit::row;

use context::AnalysisCtx;
use emit::Format;
use parse::Scope;

#[derive(Parser)]
#[command(
    name = "unruster",
    about = "Query a Rust codebase: inventory, callers/callees, field uses, variants, impls, metrics, dead-code.",
    // The 294-line design playbook used to live here. clap prints long_about
    // *before* the command list, which pushed `Commands:` to line 297 of a
    // 364-line help — past where any reader (or agent running `help | head`)
    // ever looked, so half the tool was undiscoverable. The playbook now has
    // its own subcommand; this is a ~40-line orientation instead.
    long_about = include_str!("quickstart.txt"),
    version
)]
struct Cli {
    /// Root directory (or file) to scan. Respects .gitignore.
    // `global` like every other flag here. Without it `unruster metrics -r
    // Warden` is a hard clap error ("unexpected argument '-r' found") while
    // the quickstart lists `-r/--root` under GLOBAL FLAGS — so the help was
    // wrong about the one flag people reach for first.
    #[arg(long, short = 'r', global = true, default_value = ".")]
    root: PathBuf,

    /// Cap how many rows each section lists. The summary still counts every
    /// finding, and the cap announces itself — a silent truncation reads as
    /// "that is all there is".
    ///
    /// `--top 0` lifts the cap, the same way `--max-lines 0` does. It used to
    /// mean "cap at zero" here and "all of it" in `contract-drift`, so the same
    /// flag emptied one command's output and filled another's. Nothing wanted
    /// the literal reading — `--summary` is how you ask for no rows.
    ///
    /// Global because it was 23 per-command copies that had drifted into three
    /// behaviours: uncapped, capped-at-20 (`metrics`), and absent from
    /// `error-swallows`, the highest-volume check in the tool.
    #[arg(long, global = true, value_name = "N")]
    top: Option<usize>,

    /// Test-code scope: production (default), tests, or all.
    /// Aliases: `prod` = production, `test` = tests.
    #[arg(long, global = true, value_enum, default_value = "production")]
    scope: Scope,

    /// `--cfg KEY` or `--cfg KEY=VALUE` (repeatable). Items whose cfg
    /// evaluates to definitively False under this env are stripped. Unknown
    /// keys (no `--cfg` provided) leave the item in (best-effort).
    #[arg(long, global = true)]
    cfg: Vec<String>,

    /// Exclude files matching this glob, relative to the root (repeatable),
    /// e.g. `--exclude 'fixtures/**'`. Applied on top of .gitignore.
    #[arg(long, global = true)]
    exclude: Vec<String>,

    /// Skip per-row output; print only the summary line on stderr.
    #[arg(long, global = true)]
    summary: bool,

    /// Render each row's enclosing-fn label as `name@start-end` source lines,
    /// so the relevant body can be read directly (`sed -n 'start,endp'`).
    #[arg(long, global = true)]
    spans: bool,

    /// Keep only rows in files changed vs this git ref (e.g. `HEAD~1`,
    /// `main`); untracked files count as changed. Applies to site-listing
    /// commands (incl. everything `audit` runs); git is the only state read.
    #[arg(long, global = true, value_name = "GIT_REF")]
    changed_since: Option<String>,

    /// Print ±N source lines beneath each finding row (`>` marks the site),
    /// so small findings need no follow-up file reads.
    #[arg(long, global = true, value_name = "N")]
    context: Option<usize>,

    /// Output shape. `tsv` (default) streams tab-separated rows; `json` emits
    /// one document with `file`/`line` and numeric columns as real fields, so
    /// cross-row filtering and ranking need no `awk`.
    #[arg(long, global = true, value_enum, default_value = "tsv")]
    format: Format,

    /// Shorthand for `--format json`.
    #[arg(long, global = true, conflicts_with = "format")]
    json: bool,

    /// Send summary and note lines to stdout instead of stderr, so one
    /// redirect captures the whole run.
    #[arg(long, global = true)]
    all_stdout: bool,

    /// Ignore `// unruster: ok` waiver comments and report every site.
    #[arg(long, global = true)]
    no_suppress: bool,

    /// Print the exact `// unruster: ok(…)` comment that would retire each
    /// row — right check, right key, today's date filled in. Paste it above
    /// the item (item scope) or on the line (site scope).
    #[arg(long, global = true)]
    suggest_waivers: bool,

    /// Add the stable `fp` column to TSV rows. A fingerprint identifies a
    /// finding without its line number, so two runs can be compared across an
    /// edit — `--json` always carries it. Off by default for TSV because a new
    /// column breaks existing `awk`.
    #[arg(long, global = true)]
    fingerprints: bool,

    /// Exit 1 when the command reports one or more findings (0 = clean,
    /// 2 = error/unknown target). For scripted/agent loops:
    /// `until unruster --fail-on-findings <cmd>; do fix; done`.
    #[arg(long, global = true)]
    fail_on_findings: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-shot ranked sweep: run the whole check battery (divergence,
    /// enum-coverage --all, dead-code, conversion-pairs, error-swallows,
    /// clones, config-drift, builder-drift, data-loss casts, stringly, god
    /// fns, pass-through) as severity-ordered sections. Exits 1 while gating
    /// findings remain — the agent-loop entry point:
    /// `until unruster audit; do <fix>; done`. `error-swallows` and `clones`
    /// gate on their top-scoring rows only; `--strict` promotes every advisory
    /// row to gating.
    Audit(AuditArgs),
    /// Sibling builder chains, one missing a step. `config-drift` for method
    /// chains: groups every `Type::ctor(args).a().b()` by constructor *and its
    /// constant arguments*, then reports the calls some chains make and others
    /// omit. Ranks a single missing call between two chains in one function
    /// above a broad difference across the tree.
    BuilderDrift(BuilderDriftArgs),
    /// Same struct, built two ways. Groups every `Foo { … }` literal by type
    /// and reports the fields whose constant values disagree across sites —
    /// the `divergence` thesis applied to configuration rather than enum
    /// dispatch. Ranks a one-field disagreement between two configurations
    /// above a broad one, and demotes literals that vary on purpose.
    ConfigDrift(ConfigDriftArgs),
    /// The same function body written out more than once. Groups every fn by
    /// its body after alpha-renaming locals — so a copy-paste that renamed the
    /// variables still groups — and ranks by how much is duplicated, how many
    /// times, and whether the copies share a name and a directory. Called names
    /// and literals are compared verbatim, so a group is functions that do the
    /// same thing to the same APIs, not merely functions of the same shape.
    Clones(ClonesArgs),
    /// The macro bodies no check could read — where this tool is blind.
    ///
    /// Every run already reports the *count*; this says where. A bare "45 macro
    /// bodies could not be parsed" is a caveat nobody can act on: on a real
    /// codebase it left the reader writing "a `dbg_log!`-heavy region could be
    /// hiding something none of this saw" with no way to check which region.
    /// Rows are `macro, at` — read them as "the checks did not look here".
    BlindSpots,
    /// List all top-level items (struct, enum, trait, fn, impl, ...).
    /// Under `--spans` the `at` column becomes `file:start-end`.
    Inventory(InventoryArgs),
    /// Print one item's exact source, resolved by name through the AST:
    /// `show draft_regions`, `show Window::parse`, `show geom::window::Window`.
    /// Takes several names at once — `show a b c` parses the tree once where
    /// three separate calls parse it three times.
    /// Prints from the doc comment through the closing brace — no `+N` line
    /// budget to guess, no `^fn` anchor to miss an indented method. `--part
    /// sig` for the signature alone, `--part span` for just `file:start-end`.
    /// A name that doesn't resolve answers with the near names, not silence.
    Show(ShowArgs),
    /// AST table of contents for one file: every item with `file:start-end`,
    /// indented by scope. `outline src/trace.rs`. Complete where a
    /// `grep -n '^pub fn'` anchor is not — it sees private items, indented
    /// methods and multi-line signatures — and every row says where the item
    /// ends, so the follow-up read is exact rather than a 150-line window.
    Outline(OutlineArgs),
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
    /// Dispatch sites on an enum that carry a wildcard `_ =>` arm — the sites a
    /// new variant falls through on. One of three views over the same scan:
    /// `catch-all-arms` filters to wildcard arms, `parallel-matches` groups
    /// every site by the variant set it covers, and `enum-coverage` keeps only
    /// the non-exhaustive sites and scores them. Omit the enum to sweep the
    /// tree; every view emits an `enum` column either way.
    CatchAllArms(CatchAllArgs),
    /// Dispatch sites on an enum grouped by which variants they cover — two
    /// groups on one enum is shotgun surgery waiting to happen. The unfiltered
    /// view of the three (see `catch-all-arms`): it keeps exhaustive sites too,
    /// since the point is to compare variant sets rather than to judge any one
    /// site. `--hide-exhaustive` drops the compiler-protected ones,
    /// `--rank-by-gap` sorts by coverage ratio, `--show-missing` lists the
    /// uncovered variants, `--include-matches-macro` also scans `matches!()`.
    ParallelMatches(ParallelMatchesArgs),
    /// Non-exhaustive dispatch sites on an enum, scored by coverage
    /// (gap = covered/total) and sorted descending. Top rows are the predicates
    /// closest to exhaustive — the ones a newly-added variant would silently
    /// mis-bind. The scored view of the three (see `catch-all-arms`): it drops
    /// exhaustive sites, adds `gap`/`covered`/`missing` columns, and is the one
    /// `audit` gates on. `--hide-trait-routed` drops rows whose `_` arm calls a
    /// method on the scrutinee (structurally safe, so a false positive here).
    EnumCoverage(EnumCoverageArgs),
    /// Sibling paths that disagree. Pairs up dispatch sites on one enum whose
    /// enclosing fns look like siblings (same scope and/or a shared name word)
    /// but cover different variant sets, and ranks them by how deliberate the
    /// omission looks — a one-variant gap between twins outranks a wide gap.
    /// `--handling` switches to the error-handling axis: one callee treated
    /// with different care (`.expect` vs `.ok()`) by sibling fns.
    /// Highest-yield check in the tool; start here.
    Divergence(DivergenceArgs),
    /// One function's implementation against the contract its callers assume.
    /// Every other check compares siblings to each other; this one is vertical,
    /// and the reasoning is yours: it prints the callers with the target's
    /// signature but *not* its body or doc comment, so the expectation you
    /// write from them is evidence rather than a description of code you have
    /// already read. `--reveal` then prints the implementation to compare
    /// against. `--candidates` ranks the fns worth the exercise.
    ContractDrift(ContractDriftArgs),
    /// Print the full design-audit playbook (themes, signals, repair recipes).
    /// `explain <topic>` prints one section instead.
    Playbook,
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
    /// Find sites that abort the process instead of reporting a failure:
    /// `.unwrap()`, `.expect(…)`, `panic!`, `unreachable!`, `todo!`,
    /// `unimplemented!`. The mirror of `error-swallows` — that check finds
    /// Results that were discarded, this one finds Results that were asserted.
    /// Ranked so `.unwrap()` on a parse of data from outside the process (a
    /// CLI argument, a response, a file) sorts above an `.expect` on an
    /// in-process call, because that is the crash someone else can trigger.
    Panics(PanicsArgs),
    /// Find raw arithmetic operators among checked siblings: a `+` in a fn
    /// where the neighbouring terms use `saturating_add`. Ranked by how
    /// outnumbered the raw one is — three-to-one is someone who missed a line,
    /// one-to-one is two different jobs in one scope.
    ArithDrift(ArithDriftArgs),
    /// Find pass-through wrappers: fns whose body is a single call/expression.
    PassThrough(PassThroughArgs),
    /// Print one design-audit playbook section (repair recipe) by topic,
    /// e.g. `explain partial-enumeration`, `explain stringly`. Without a
    /// topic, lists all topics. Cheaper than the full --help for agent loops.
    Explain(ExplainArgs),

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
    /// `.args([...])`), then `--subcommand <name>` to list the tests behind
    /// one of those counts — with `--context N` to read their bodies.
    Tests(TestsArgs),
    /// List, audit, and clean up in-source `// unruster: ok(…)` waivers.
    /// Every row reports how many findings it actually suppresses, so a
    /// broad item-scoped waiver can't hide its own reach. `--orphaned`
    /// finds waivers that suppress nothing (the finding is gone; the comment
    /// now lies), `--stale N` those older than N days, `--remove` strips them
    /// (dry-run unless `--write`), `--upgrade` rewrites legacy waivers with
    /// the check that actually hit them.
    Waivers(WaiversArgs),
}

#[derive(Args)]
struct WaiversArgs {
    /// Only waivers for this check (`divergence`, `casts`, …). Legacy waivers
    /// always match — they waive every check, which is the point of listing
    /// them under one.
    #[arg(long)]
    check: Option<String>,

    /// Only waivers at least N days old. Undated waivers always qualify:
    /// they cannot be shown to be fresh.
    #[arg(long, value_name = "DAYS")]
    stale: Option<i64>,

    /// Only waivers that suppressed nothing this run — the code moved on and
    /// the comment no longer describes anything. Mechanical, unlike `--stale`.
    #[arg(long)]
    orphaned: bool,

    /// Only waivers written in the pre-grammar spelling (no check name).
    #[arg(long)]
    legacy: bool,

    /// Strip the matching waiver comments from source. Previews unless
    /// `--write` is also given.
    #[arg(long, conflicts_with = "upgrade")]
    remove: bool,

    /// Rewrite legacy waivers as `ok(<check>) <today>`, keeping the reason.
    /// Only touches waivers hit by exactly one check — anything ambiguous is
    /// reported and left alone. Previews unless `--write` is also given.
    #[arg(long)]
    upgrade: bool,

    /// Actually modify files. Without it `--remove` / `--upgrade` print the
    /// diff and change nothing.
    #[arg(long)]
    write: bool,

    /// Exit 1 if any waiver is undated or at least N days old. For CI, in the
    /// shape of `--fail-on-findings`.
    #[arg(long, value_name = "DAYS")]
    fail_on_stale: Option<i64>,

    /// Treat this date as today (`YYYY-MM-DD`). The clock is the only
    /// non-deterministic input in the tool; this pins it for tests and for
    /// reproducible CI output.
    #[arg(long, value_name = "YYYY-MM-DD")]
    today: Option<String>,
}

/// The subcommand's CLI name, for the `command` field of `--json` output.
/// Exhaustive (no `_`) for the same reason as `implies_fail_on_findings`: a new
/// command must state its own name rather than inherit a wrong one.
/// Checks that consult `// unruster: ok(…)` waivers. Kept next to `cmd_name`
/// because the strings must match its output exactly — `--suggest-waivers`
/// warns when it is invoked on anything not in this list, and `audit` gates on
/// four of these five.
const WAIVER_AWARE_CHECKS: &[&str] = &[
    "audit",
    "divergence",
    "enum-coverage",
    "dead-code",
    "conversion-pairs",
    "config-drift",
    "builder-drift",
    "error-swallows",
    "casts",
    // Both consult waivers and both print suggestions; leaving them off this
    // list made `--suggest-waivers` announce "does not support waivers" and
    // then print one anyway, so a reader had no way to tell which to believe.
    "clones",
    "stringly",
];

fn cmd_name(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Audit(_) => "audit",
        Cmd::BuilderDrift(_) => "builder-drift",
        Cmd::ConfigDrift(_) => "config-drift",
        Cmd::Clones(_) => "clones",
        Cmd::BlindSpots => "blind-spots",
        Cmd::Inventory(_) => "inventory",
        Cmd::Show(_) => "show",
        Cmd::Outline(_) => "outline",
        Cmd::Callers(_) => "callers",
        Cmd::Callees(_) => "callees",
        Cmd::CoCall(_) => "co-call",
        Cmd::FieldUses(_) => "field-uses",
        Cmd::Fields(_) => "fields",
        Cmd::Variants(_) => "variants",
        Cmd::Impls(_) => "impls",
        Cmd::TypeRefs(_) => "type-refs",
        Cmd::TakesMut(_) => "takes-mut",
        Cmd::Metrics(_) => "metrics",
        Cmd::DeadCode(_) => "dead-code",
        Cmd::CatchAllArms(_) => "catch-all-arms",
        Cmd::ParallelMatches(_) => "parallel-matches",
        Cmd::EnumCoverage(_) => "enum-coverage",
        Cmd::Divergence(_) => "divergence",
        Cmd::ContractDrift(_) => "contract-drift",
        Cmd::Playbook => "playbook",
        Cmd::CohortCallees(_) => "cohort-callees",
        Cmd::ErrorSwallows(_) => "error-swallows",
        Cmd::Panics(_) => "panics",
        Cmd::ArithDrift(_) => "arith-drift",
        Cmd::PassThrough(_) => "pass-through",
        Cmd::Explain(_) => "explain",
        Cmd::Casts(_) => "casts",
        Cmd::Conversions(_) => "conversions",
        Cmd::ConversionPairs => "conversion-pairs",
        Cmd::Stringly(_) => "stringly",
        Cmd::Tests(_) => "tests",
        Cmd::Waivers(_) => "waivers",
    }
}

impl Cmd {
    /// Commands that imply `--fail-on-findings`. Exhaustive (no `_`) so a new
    /// command must declare its agent-loop semantics — `unruster enum-coverage
    /// Cmd` flagged the previous `matches!(…, Cmd::Audit(_))` shortcut.
    fn implies_fail_on_findings(&self) -> bool {
        match self {
            Cmd::Audit(_) => true,
            Cmd::BuilderDrift(_)
            | Cmd::ConfigDrift(_)
            | Cmd::Clones(_)
            | Cmd::BlindSpots
            | Cmd::Inventory(_)
            | Cmd::Show(_)
            | Cmd::Outline(_)
            | Cmd::Callers(_)
            | Cmd::Callees(_)
            | Cmd::CoCall(_)
            | Cmd::FieldUses(_)
            | Cmd::Fields(_)
            | Cmd::Variants(_)
            | Cmd::Impls(_)
            | Cmd::TypeRefs(_)
            | Cmd::TakesMut(_)
            | Cmd::Metrics(_)
            | Cmd::DeadCode(_)
            | Cmd::CatchAllArms(_)
            | Cmd::ParallelMatches(_)
            | Cmd::EnumCoverage(_)
            | Cmd::CohortCallees(_)
            | Cmd::Divergence(_)
            // Emits material, not findings: there is no judgment here to fail
            // a build on. The reader supplies the verdict.
            | Cmd::ContractDrift(_)
            | Cmd::Playbook
            | Cmd::ErrorSwallows(_)
            | Cmd::Panics(_)
            | Cmd::ArithDrift(_)
            | Cmd::PassThrough(_)
            | Cmd::Explain(_)
            | Cmd::Casts(_)
            | Cmd::Conversions(_)
            | Cmd::ConversionPairs
            | Cmd::Stringly(_)
            | Cmd::Tests(_) => false,
            // `waivers` returns a count only for `--fail-on-stale`, which is
            // itself the opt-in; a plain listing must never fail a build.
            Cmd::Waivers(a) => a.fail_on_stale.is_some(),
        }
    }
}

#[derive(Args)]
struct AuditArgs {
    /// Compare against the tree as it was at this git ref (`HEAD~1`, `main`),
    /// reporting gone / new / moved. The ref is materialized with `git archive`
    /// and scanned in a temp dir, so nothing is written and a dirty working
    /// tree is fine. Git is the baseline — no state is kept between runs.
    #[arg(long, value_name = "GIT_REF", conflicts_with = "baseline")]
    since: Option<String>,

    /// Compare against a snapshot written by `--write-baseline`. For pinning a
    /// CI gate to a release rather than a commit.
    #[arg(long, value_name = "FILE")]
    baseline: Option<PathBuf>,

    /// Write this run's findings to FILE as a baseline and exit normally.
    #[arg(long, value_name = "FILE")]
    write_baseline: Option<PathBuf>,

    /// With `--since` / `--baseline`: exit 1 only when findings appeared that
    /// were not there before. The gate an agent actually wants — "did I make
    /// it worse" rather than "is it perfect".
    #[arg(long)]
    fail_on_new: bool,

    /// Advisory (candidate-class) findings gate the exit code too. By
    /// default only [high] deterministic defect classes do, so the agent
    /// loop converges on a healthy codebase.
    #[arg(long)]
    strict: bool,

    /// Omit sections that found nothing. Every check still runs and still
    /// counts — the closing line reports how many sections were hidden — this
    /// only stops a mostly-clean battery from spending two thirds of its
    /// output saying so.
    ///
    /// On a healthy tree that is eight of thirteen sections, three lines each,
    /// which is what pushed one session's real findings past its own
    /// `| head -60` and made it run the whole battery a second time with
    /// `| tail -40` to read the rest.
    #[arg(long)]
    findings_only: bool,

    /// Run only these checks (repeatable, or comma-separated). Names are the
    /// ones in each section's `"check"` field: `divergence`,
    /// `divergence-handling`, `enum-coverage`, `dead-code`,
    /// `conversion-pairs`, `error-swallows`, `panics`, `clones`,
    /// `config-drift`, `builder-drift`, `arith-drift`, `casts`, `stringly`,
    /// `metrics`, `pass-through`.
    #[arg(long, value_name = "CHECK", value_delimiter = ',')]
    only: Vec<String>,

    /// Run every check except these (repeatable, or comma-separated). The
    /// low-volume comparison checks are where this tool's signal is; on a large
    /// tree `--skip error-swallows,dead-code` is what makes the rest readable
    /// end to end. The closing line always names what was left out.
    #[arg(long, value_name = "CHECK", value_delimiter = ',')]
    skip: Vec<String>,
}

#[derive(Args)]
struct ClonesArgs {
    /// Ignore bodies smaller than this many canonical tokens. Two copies of a
    /// three-token accessor say something about Rust, not about the codebase.
    #[arg(long, default_value_t = clones::DEFAULT_MIN_TOKENS)]
    min_tokens: usize,

    /// Drop groups below this score. `--min-score 0.75` is the tier `audit`
    /// gates on. The sibling drift checks all took a `--min-score` and this
    /// one, which also ranks its rows, did not.
    #[arg(long, default_value_t = 0.0)]
    min_score: f64,
}

#[derive(Args)]
struct BuilderDriftArgs {
    /// Only chains rooted at this constructor path (e.g. `Command::new`).
    // The field is `ctor` because the arg id must differ from the global
    // `--root` (clap panics at access time, not definition time, when two args
    // share an id with different types). The *display* name was `ROOT` for the
    // same reason, which put `builder-drift [OPTIONS] [ROOT]` one line above
    // `-r, --root <ROOT>  Root directory to scan` — two unrelated things
    // spelled identically in one help screen, and the usage line reads as
    // though the positional is a directory.
    #[arg(value_name = "CTOR")]
    ctor: Option<String>,

    /// Drop rows scoring below this.
    #[arg(long, default_value_t = 0.05)]
    min_score: f64,

}

#[derive(Args)]
struct ConfigDriftArgs {
    /// Only this struct type (last segment).
    ty: Option<String>,

    /// Drop rows scoring below this. Raise to see only the loudest.
    #[arg(long, default_value_t = 0.05)]
    min_score: f64,

}

#[derive(Args)]
struct InventoryArgs {
    /// Filter to one kind (`fn` = free fns; methods are `impl-fn`).
    #[arg(long, short = 'k', value_enum)]
    kind: Option<inventory::ItemKind>,
    /// Filter by visibility.
    #[arg(long, value_enum)]
    vis: Option<inventory::VisFilter>,

    /// Keep only items whose name matches this glob (`*` = any run of chars,
    /// the only metacharacter). A bare pattern matches the last `::` segment;
    /// one containing `::` matches any qualified suffix, so
    /// `--name 'Document::*'` lists one type's members.
    ///
    /// Smartcase: an all-lowercase pattern matches case-insensitively, so
    /// `--name mask` finds `Mask`, `load_mask_for` and `MaskArgs` alike; any
    /// uppercase makes it exact.
    ///
    /// Without it the listing had no way to narrow by name, so the shape people
    /// wrote was `inventory | grep -i mask` — which throws away the item count
    /// and the `--top` cut along with the stderr it redirects, and matches the
    /// file path and the doc column as readily as the name.
    #[arg(long, value_name = "GLOB")]
    name: Option<String>,

    /// Shorthand for `--vis pub`: the tree's external surface.
    // The same pair `outline` and `dead-code` offer. `--vis` was here and this
    // was not, so the three vis-filtering commands each took a different subset
    // of one idea.
    #[arg(long, conflicts_with = "vis")]
    pub_only: bool,
    /// Row order: by kind (a census) or by source position (an outline).
    #[arg(long, value_enum, default_value = "kind")]
    sort: inventory::ItemSort,

    /// Append each item's doc-comment first line as a final column. The same
    /// flag `outline` carries, so the two listings differ in their defaults
    /// rather than in what they can show.
    #[arg(long, alias = "docs")]
    include_docs: bool,

    /// Group the flat listing under a per-module header with per-kind counts.
    /// The rows are the same rows; this adds headers, it does not replace the
    /// listing with a summary.
    #[arg(long)]
    tree: bool,
}

#[derive(Args)]
struct ShowArgs {
    /// The item(s): a bare name (`draft_regions`), a `Type::method`
    /// (`Window::parse`), or any qualified suffix
    /// (`geom::window::Window::parse`). Matching is on whole `::` segments, so
    /// a suffix that isn't one won't silently resolve to something else.
    ///
    /// Repeatable — `show a b c` resolves all three in one pass. The tree is
    /// parsed once per invocation, so a batch is dramatically cheaper than the
    /// same names one call at a time. A name that doesn't resolve is reported
    /// and the rest still run.
    #[arg(required = true, num_args = 1..)]
    name: Vec<String>,

    /// How much of the item to print. `full` = docs + signature + body;
    /// `sig` = docs + signature — a fn's through the return type, and for a
    /// struct/enum/const the whole declaration, since its fields, variants or
    /// value *are* the signature; `doc` = the doc comment and attributes alone;
    /// `span` = no source, just the `file:start-end` row for a reader that will
    /// seek there itself.
    #[arg(long, value_enum, default_value = "full")]
    part: show::Part,

    /// Only items of this kind, for the usual `Foo`-is-a-struct-and-an-impl
    /// case. Same vocabulary as `inventory --kind`.
    #[arg(long, short = 'k', value_enum)]
    kind: Option<inventory::ItemKind>,

    /// Print every match rather than listing them. Off by default: four fn
    /// bodies concatenated under one header is exactly the unreadable output
    /// this command exists to replace.
    #[arg(long)]
    all: bool,

    /// Omit the leading doc comment and attributes.
    #[arg(long, alias = "no-doc")]
    hide_doc: bool,

    /// Prefix each source line with its line number, so a follow-up edit can
    /// be addressed without counting.
    #[arg(long, short = 'n')]
    number: bool,

    /// Stop after N source lines per item and say how many were left.
    /// `--max-lines 0` prints all of it.
    ///
    /// Defaults to 240, so this command bounds its own output and there is no
    /// reason to wrap it in `| head -N`. That matters because the two cuts are
    /// not equivalent: this one names the lines it dropped and the flag that
    /// lifts it, while a pipe ends mid-expression in silence. In one measured
    /// session seventeen of twenty invocations were piped and five were cut
    /// mid-item — twice sending the reader back to a raw `sed -n 'A,Bp'` for
    /// the rest of a body this command had already located exactly.
    #[arg(long, value_name = "N")]
    max_lines: Option<usize>,
}

#[derive(Args)]
struct OutlineArgs {
    /// The file, as a path or any trailing part of one: `src/geom/window.rs`,
    /// `geom/window.rs` and `window.rs` all resolve. Matching is on whole path
    /// components, so `dow.rs` does not.
    file: String,

    /// Only items of this kind.
    #[arg(long, short = 'k', value_enum)]
    kind: Option<inventory::ItemKind>,

    /// Keep only items of this visibility — the same filter and the same
    /// spelling as `inventory --vis`.
    #[arg(long, value_enum)]
    vis: Option<inventory::VisFilter>,

    /// Shorthand for `--vis pub`: the file's external surface.
    #[arg(long, conflicts_with = "vis")]
    pub_only: bool,

    /// Append each item's doc-comment first line as a final column.
    #[arg(long, alias = "docs")]
    include_docs: bool,

    /// Row order: by source position (an outline) or by kind (a census). The
    /// same flag `inventory` carries; only the default differs.
    #[arg(long, value_enum, default_value = "source")]
    sort: inventory::ItemSort,

    /// Drop the nesting indent from the `name` column (friendlier to `awk`,
    /// harder to read).
    #[arg(long)]
    flat: bool,
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
    /// Group call sites by the calling fn, by file, or by top-level module.
    #[arg(long, value_enum)]
    by: Option<context::GroupBy>,
    /// Keep only rows at or above this confidence tier
    /// (heuristic < inferred < resolved < exact).
    #[arg(long, value_enum)]
    min_confidence: Option<context::Confidence>,
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
    #[arg(long = "class", alias = "kind", value_enum, value_delimiter = ',')]
    class: Vec<field_uses::FieldKind>,
    /// (With --candidates) restrict hits to a substring of the receiver
    /// expression — e.g. `--via-receiver common` keeps `x.common.transform` but
    /// drops `node.transform`.
    #[arg(long)]
    via_receiver: Option<String>,
    /// Keep only rows at or above this confidence tier: `via` self/init =
    /// exact, ti = inferred, ? = heuristic.
    #[arg(long, value_enum)]
    min_confidence: Option<context::Confidence>,
}

#[derive(Args)]
struct FieldsArgs {
    /// Struct name (last segment, e.g. `Document`).
    ty: String,
}

#[derive(Args)]
struct VariantsArgs {
    /// Enum name (last segment, e.g. `Token`). Omit to sweep every enum in the
    /// tree — the same contract as `catch-all-arms`, `enum-coverage` and
    /// `divergence`, which this command alone used to differ from.
    #[arg(value_name = "ENUM")]
    name: Option<String>,
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
    /// Keep only rows at or above this confidence tier (alias matches are
    /// `inferred`; name matches are `resolved` when the name has exactly one
    /// definition in the tree, else `heuristic`).
    #[arg(long, value_enum)]
    min_confidence: Option<context::Confidence>,
}

#[derive(Args)]
struct TakesMutArgs {
    /// Type name (last segment). Omit to list the types with the largest
    /// `&mut` surface instead of erroring — the answer to "which type should I
    /// pass here?" is usually the point of running this bare.
    ty: Option<String>,
}

#[derive(Args)]
struct MetricsArgs {
    /// Sort fns by: `loc` (lines), `params`, `cyclo` (cyclomatic complexity),
    /// `nesting` (max control-flow nesting depth).
    #[arg(long, value_enum, default_value = "loc")]
    sort: metrics::SortKey,
    /// Only show fns where the sort metric is >= N. E.g. with
    /// `--sort cyclo --threshold 15`, only fns with cyclo >= 15.
    #[arg(long)]
    threshold: Option<usize>,
}

#[derive(Args)]
struct DeadCodeArgs {
    /// Keep only items of this visibility — the same filter and the same
    /// spelling as `inventory --vis`.
    #[arg(long, value_enum)]
    vis: Option<inventory::VisFilter>,
    /// Shorthand for `--vis pub`.
    #[arg(long, conflicts_with = "vis")]
    pub_only: bool,
    /// Also report trait-impl methods whose name is never called anywhere.
    /// Off by default: dyn-dispatch and generic calls are invisible to a
    /// syntactic scan, so these rows need per-site review.
    #[arg(long)]
    include_trait_impls: bool,
}

#[derive(Args)]
struct CatchAllArgs {
    /// Enum name (last segment). Omit to scan every enum (rows gain a leading
    /// enum column) — the same as `--all`, which is kept as an explicit
    /// spelling. Erroring on a bare invocation just cost a round-trip; naming
    /// an enum *and* passing `--all` is still a contradiction, so it errors.
    #[arg(value_name = "ENUM", conflicts_with = "all")]
    name: Option<String>,
    /// Scan every enum defined in the tree; rows gain a leading enum column.
    #[arg(long)]
    all: bool,
}

#[derive(Args)]
struct ParallelMatchesArgs {
    /// Enum name (last segment). Omit to scan every enum (group rows gain a
    /// leading enum column) — the same as `--all`, which is kept as an
    /// explicit spelling. Naming an enum *and* passing `--all` is a
    /// contradiction, so it errors.
    // Bare invocation used to be a hard error here while `catch-all-arms`,
    // `enum-coverage` and `divergence` all treated it as "every enum". Four
    // commands over one subject, and this was the only one that made you read
    // its help to run it.
    #[arg(value_name = "ENUM", conflicts_with = "all")]
    name: Option<String>,
    /// Scan every enum defined in the tree; group rows gain a leading enum
    /// column. The default when no name is given; accepted for symmetry with
    /// the sibling enum commands.
    #[arg(long)]
    all: bool,
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
    /// Skip enums with fewer than N variants. Defaults to 3 when sweeping
    /// (a 1-of-2 `matches!` is an if/else, not partial dispatch) and 0 when
    /// an enum is named, since naming one means "tell me about this one".
    #[arg(long, value_name = "N")]
    min_variants: Option<usize>,

    /// Enum name (last segment). Omit to scan every enum — bare invocation is
    /// the same as `--all`. Naming an enum *and* passing `--all` contradicts
    /// itself and errors.
    #[arg(value_name = "ENUM", conflicts_with = "all")]
    name: Option<String>,
    /// Scan every enum defined in the tree; rows gain a leading enum column.
    #[arg(long)]
    all: bool,
    /// Keep only sites missing at most N variants. `--max-missing 1` isolates
    /// the "forgot exactly one" shape, which is where this check's real
    /// defects live.
    #[arg(long, value_name = "N")]
    max_missing: Option<usize>,
    /// Drop the covered/missing variant columns; print one header line per
    /// enum instead. On a wide enum the two columns restate the variant set on
    /// every row.
    #[arg(long)]
    compact: bool,
    /// One row per enum (partial-site count + worst gap) instead of one per
    /// site — "which enum should I look at first".
    #[arg(long)]
    rank_enums: bool,
    /// Hide rows whose catch-all / `_` arm routes through a method call on the
    /// matched scrutinee (e.g. `_ => node.paint_slots()`). Those sites are
    /// structurally safe — a newly-added variant must implement the trait
    /// method, so the catch-all picks up its behavior automatically — but the
    /// tool can't see through the call and would otherwise flag them. Cuts the
    /// noise; read the remaining rows' `_` arms to confirm.
    #[arg(long, alias = "hide-trait-routed-catchalls")]
    hide_trait_routed: bool,
}

#[derive(Args)]
struct DivergenceArgs {
    /// Enum name (last segment). Omit to scan every enum.
    #[arg(value_name = "ENUM")]
    name: Option<String>,
    /// Scan every enum — the default when no name is given. Accepted for
    /// symmetry with `enum-coverage --all` / `catch-all-arms --all`, which is
    /// what a reader coming from those commands will type.
    #[arg(long)]
    all: bool,
    /// Compare error-handling care instead of enum coverage: one callee that
    /// sibling fns treat differently (`.expect` in one, `.ok()` in another).
    /// Ignores the enum argument.
    #[arg(long)]
    handling: bool,
    /// Drop pairs scoring below this. Raise to see only the loudest.
    #[arg(long, default_value_t = 0.25)]
    min_score: f64,
    /// With `--handling`: minimum care distance between the two sides
    /// (0 = dropped, 1 = unwrap, 2 = default, 3 = fallback, 4 = expect).
    #[arg(long, default_value_t = 2)]
    min_care_gap: u8,
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
    /// Hide `let _ = write!(buf, …)` into an in-memory buffer — infallible, so
    /// the discard is idiomatic rather than a swallowed error. Shown by
    /// default here; `audit` hides them.
    // Was `--include-infallible <BOOL>`. A value-taking switch sat next to
    // `--include-unwrap-or`, a bare one, on the same command with the opposite
    // default — so `--include-infallible` alone was a *parse error* while
    // `--include-unwrap-or` alone was the whole flag. The convention is that
    // the name states the direction and every switch is bare: `--include-X`
    // when X is off by default, `--hide-X` when it is on.
    #[arg(long, alias = "no-infallible")]
    hide_infallible: bool,
    /// Hide `.unwrap_or_else(|| { log!(…); … })` — the failure is already
    /// observable, so the fallback is a policy, not a silent drop. Shown by
    /// default here; `audit` hides them.
    #[arg(long, alias = "no-logged")]
    hide_logged: bool,
    /// Drop rows below this score. `--min-score 0.55` is the tier `audit`
    /// gates on: an external effect happened and the only report of whether it
    /// worked was discarded.
    ///
    /// This check ranks its rows and gates on the top tier, and was the only
    /// ranked check in the tool with no way to ask for that tier — on a large
    /// workspace it emits several hundred rows and the answer was `awk`.
    #[arg(long, default_value_t = 0.0)]
    min_score: f64,
}

#[derive(Args)]
struct PanicsArgs {
    /// Hide the idiomatic families: `Mutex::lock().unwrap()`, where the panic
    /// is the documented response to a poisoned lock, and assertions over
    /// source literals (`Regex::new("^a$").unwrap()`), whose input cannot vary
    /// at runtime. Shown by default here; `audit` hides them.
    // Bare switch named for its direction, per the convention set by
    // `--hide-infallible` on `error-swallows`: `--include-X` when X is off by
    // default, `--hide-X` when it is on.
    #[arg(long, alias = "no-idiomatic")]
    hide_idiomatic: bool,
    /// Drop rows below this score. `--min-score 0.55` is the tier `audit`
    /// gates on: asserted on data the process did not produce.
    #[arg(long, default_value_t = 0.0)]
    min_score: f64,
}

#[derive(Args)]
struct ArithDriftArgs {
    /// Drop rows below this score, where the score is checked-siblings over
    /// total. Default 0.5 keeps every scope where the checked spelling is at
    /// least as common as the raw one; `audit` uses 0.6, which needs a real
    /// majority.
    #[arg(long, default_value_t = 0.5)]
    min_score: f64,
}

#[derive(Args)]
struct ContractDriftArgs {
    /// The function, method, or macro whose contract to reconstruct. Same
    /// target forms as `callers`: bare name, `Type::method`, `.method`,
    /// `::name`, `name!`. Omit it only with `--candidates`.
    #[arg(required_unless_present = "candidates")]
    name: Option<String>,

    /// Phase 2: print the target's doc comment, body, and callees. Run it
    /// *after* writing the expectation the callers imply — an expectation
    /// written afterwards describes the code instead of testing it.
    #[arg(long, conflicts_with = "candidates")]
    reveal: bool,

    /// Rank the fns worth this exercise (enough callers, spread across enough
    /// modules, enough body, little enough written down) instead of running it
    /// on one. Takes no target. Pairs with `--changed-since`.
    #[arg(long)]
    candidates: bool,

    /// Emit the caller rows and the usage table without the caller bodies.
    /// The bodies are on by default because the expectation lives in what the
    /// caller does *after* the call, and fetching them one `show` at a time is
    /// the round-trip this command exists to remove.
    #[arg(long, conflicts_with = "candidates")]
    no_bodies: bool,

    /// Stop after N source lines per caller body and say how many were left.
    /// `0` lifts the bound. Defaults to 80 — lower than `show`'s, because this
    /// prints one body per caller rather than one per run.
    #[arg(long, value_name = "N")]
    max_lines: Option<usize>,

    /// `--candidates` only: the caller floor. Below three callers there is no
    /// consensus to derive a contract *from*, only one caller's opinion.
    #[arg(long, default_value_t = 3, value_name = "N")]
    min_callers: usize,

    /// Keep only callers at or above this confidence tier. A caller below
    /// `resolved` may not be calling this item at all, and one wrong caller
    /// poisons the expectation derived from the set.
    #[arg(long, value_enum)]
    min_confidence: Option<context::Confidence>,
}

#[derive(Args)]
struct ExplainArgs {
    /// Topic words matched against playbook headings (e.g. `stringly`,
    /// `partial-enumeration`, `god function`). Omit to list topics.
    topic: Option<String>,
}

#[derive(Args)]
struct PassThroughArgs {
    /// Maximum body LOC to consider as pass-through (default 1).
    #[arg(long, default_value_t = 1)]
    max_loc: usize,
}

#[derive(Args)]
struct CastsArgs {
    /// Include `ptr` casts inside `unsafe` blocks / fns. Hidden by default:
    /// an FFI shim's `p as *const Method` has no safer spelling, so the rows
    /// are noise on every run.
    #[arg(long)]
    include_unsafe_ptr: bool,

    /// Filter to one or more comma-separated classes.
    #[arg(long, value_enum, value_delimiter = ',')]
    class: Vec<casts::CastClass>,
    /// Group + count: fn, file, or module.
    #[arg(long, value_enum)]
    by: Option<context::GroupBy>,
    /// Hide safe-widening rows (widen-int / widen-float).
    #[arg(long, alias = "no-widen")]
    hide_widen: bool,
}

#[derive(Args)]
struct TestsArgs {
    /// Include a compact fingerprint of the test body's first `.args([...])`
    /// call (the `--root <path>` / `--scope <val>` prefix is stripped).
    #[arg(long)]
    with_hint: bool,
    /// Group + count tests by which CLI subcommand each invokes (heuristic:
    /// scans `.args([...])` calls in the body for a known-subcommand-shaped
    /// string literal). Drops the per-test list, prints a histogram.
    // Was `--by <TestsBy>` over a single-variant enum — a boolean wearing an
    // enum's clothes, and the only `--by` in the tool that was not `GroupBy`.
    #[arg(long, conflicts_with = "subcommand")]
    by_subcommand: bool,
    /// List the tests that invoke this subcommand — the drill-in for a row of
    /// `--by-subcommand`, which counts them but cannot say which they are.
    /// `--subcommand none` lists the tests whose subcommand went undetected.
    /// Composes with `--with-hint` and with `--context N` to read the bodies.
    #[arg(long, value_name = "NAME")]
    subcommand: Option<String>,
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
}

#[derive(Args)]
struct ConversionsArgs {
    /// Filter to one or more comma-separated kinds (e.g. `.into,::from`).
    #[arg(long = "class", alias = "kind", value_enum, value_delimiter = ',')]
    class: Vec<conversions::ConvKind>,
    /// Group + count: fn, file, or module. Without --by, lists every site.
    #[arg(long, value_enum)]
    by: Option<context::GroupBy>,
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
    excludes: &[String],
) -> Result<Option<Vec<parse::ParsedFile>>> {
    if scope == Scope::All {
        Ok(None)
    } else {
        Ok(Some(parse::parse_dir(root, Scope::All, cfg, excludes)?))
    }
}

/// Report the macro blind-spot count, if any. Emitted on every path including
/// error exits: a reader must know what was *not* analyzed regardless of how
/// the run ended.
///
/// The number comes from `macro_scan::survey`, a single pass over the whole
/// scanned tree, so it is the same for every subcommand at a given commit and
/// scope. It used to accumulate as checks happened to reach macros, which made
/// it a fact about the check battery wearing the phrasing of a fact about the
/// code — one tree reported 21, 18 and 17 in the same session.
/// Does this command draw a conclusion from the code, or only say where the
/// code is?
///
/// Only the first kind can be wrong because a macro body was unreadable, and
/// only the first kind should therefore carry the blind-spot note. `show` and
/// `outline` report spans and print source: an unparseable `json!` changes
/// neither where a fn starts nor where it ends, so the warning is not merely
/// noise there — it answers a question the command never asked. Across one real
/// session it printed 38 times, each an unwrapped three-line paragraph under a
/// four-line function.
fn analyses_code(command_name: &str) -> bool {
    !matches!(command_name, "show" | "outline")
}

/// Commands whose answer is *where a thing is used*, for which a
/// production-only scan is confidently incomplete.
///
/// Kept next to `cmd_name` because the strings must match its output exactly.
/// Deliberately not every command: `inventory` and `outline` catalogue what
/// they were pointed at and a narrower scope narrows the catalogue, which is
/// what the flag is for. These are the ones where the reader's next act is an
/// edit made on the strength of a list being complete.
const USAGE_COMMANDS: &[&str] = &[
    "callers",
    // The command with the most to lose from a scope gap: its premise is
    // "everything that calls this", and a caller set that quietly omits the
    // tests yields a contract derived from half the evidence. It was missing
    // here, and grew a private note of its own that could never fire — the
    // scope filter drops those files before the scan, so nothing downstream
    // can count what was never read.
    "contract-drift",
    "callees",
    "co-call",
    "cohort-callees",
    "field-uses",
    "type-refs",
    "takes-mut",
    "variants",
    "enum-coverage",
    "parallel-matches",
    "catch-all-arms",
];

/// Say that the default scope walked past the tests, on the commands where
/// that changes the answer.
///
/// `--scope production` is the right default for the checks — a lint about test
/// code is mostly noise — and the wrong one for "who uses this", which is the
/// question asked immediately before a signature changes. A real session
/// widening `trace::Options` by one field found its construction sites with
/// `grep -rn "trace::Options {" src/ tests/`; eight of the roughly fourteen
/// were in `tests/render.rs`. The AST answer would have been better in every
/// way except the one that mattered, since by default it would not have looked
/// there — and it would have said so nowhere. A wrong answer arriving from an
/// AST tool is worse than one arriving from grep, because it is believed.
fn report_scope_gap(out: &emit::Out, scope: Scope, command_name: &str) {
    if scope != Scope::Production || !USAGE_COMMANDS.contains(&command_name) {
        return;
    }
    let skipped = parse::scope_skipped();
    if skipped == 0 {
        return;
    }
    // Test-support crates are called out separately, and by name: they are
    // ordinary library code from the inside, so "it was a test file" is not an
    // explanation a reader can check by opening one. Naming them also makes
    // the classification falsifiable — a crate listed here that the reader
    // knows is production is a bug report, where a bare count is not.
    let in_test_crates = parse::scope_skipped_test_crates();
    let named = parse::test_support_crates();
    out.note(&format!(
        "(scope: {} test file(s) were not scanned{} — this answer covers production code \
         only. `--scope all` includes tests, which is usually what you want before \
         changing a signature or a type's shape.)",
        skipped,
        if in_test_crates == 0 {
            String::new()
        } else if named.is_empty() {
            // The graph had no opinion and the crate's *name* is what removed
            // it. Say so, because that rule is a convention and can be wrong.
            format!(
                ", {} of them in crates whose name says test support (no manifest \
                 dev-depends on them in this tree, so the dependency graph could \
                 not confirm it)",
                in_test_crates
            )
        } else {
            format!(
                ", {} of them in test-support crates ({} — production code reaches \
                 them only through a `[dev-dependencies]` edge)",
                in_test_crates,
                named.join(", ")
            )
        }
    ));
}

/// Would a waiver change what this command prints?
///
/// True for the checks that consult `// unruster: ok(…)` — plus `waivers`
/// itself, which exists to audit them. Everything else (navigation, listings,
/// `explain`) is unaffected by a waiver, so telling it about a malformed one is
/// advice it cannot act on and did not ask for.
fn waiver_relevant(command_name: &str) -> bool {
    command_name == "waivers" || WAIVER_AWARE_CHECKS.contains(&command_name)
}

/// Report waivers that cannot do what they look like they do: ones with no
/// reason, ones naming a check that does not exist, and ones predating the
/// `ok(<check>)` grammar. Each is a comment that lies about the codebase, so
/// each is worth one line — to a reader who is running a check it could affect.
fn report_waiver_hygiene(
    out: &emit::Out,
    suppressions: &suppress::Suppressions,
    is_waivers_cmd: bool,
) {
    if suppressions.unexplained > 0 {
        out.note(&format!(
            "note: {} `// unruster: ok` waiver(s) carry no reason — a waiver \
             nobody can evaluate is worse than the finding it hides",
            suppressions.unexplained
        ));
    }
    // A waiver naming a check that does not exist waives nothing, silently.
    // That is the same dead-weight class `--orphaned` reports, but it is a
    // typo rather than drift and can be caught the moment the comment is read.
    let known = suppress::known_check_names();
    let mut unknown: Vec<String> = suppressions
        .all()
        .iter()
        .filter_map(|w| w.check.clone())
        .filter(|c| !known.contains(&c.as_str()))
        .collect();
    unknown.sort();
    unknown.dedup();
    if !unknown.is_empty() {
        out.note(&format!(
            "note: {} waiver(s) name a check this tool does not have ({}) and so waive \
             nothing — known checks and groups: {}",
            unknown.len(),
            unknown.join(", "),
            known.join(", ")
        ));
    }
    // A legacy waiver has no check name, so it silences every check on its
    // line. Say so once per run rather than per finding: the fix is one
    // `waivers --upgrade`, not a decision at each site. `waivers` itself lists
    // them in its own output, so it does not need the summary too.
    let legacy = suppressions.legacy_count();
    if legacy > 0 && !is_waivers_cmd {
        out.note(&format!(
            "note: {} waiver(s) predate the `ok(<check>)` grammar and waive every check \
             on their line — `unruster waivers --upgrade` qualifies the unambiguous ones",
            legacy
        ));
    }
}

fn report_blind_spots(out: &emit::Out) {
    let blind = macro_scan::blind_spots();
    if blind > 0 {
        out.note(&format!(
            "(blind spots: {} macro body(ies) in the scanned tree could not be parsed as \
             expressions — code inside them was not analyzed by any check; \
             `unruster blind-spots` lists them)",
            blind
        ));
    }
}

/// Route one parsed subcommand to its implementation. Pure jump table —
/// extracted so `main` itself stays small (its own `metrics --sort cyclo`
/// flagged the combined fn at 47).
#[allow(clippy::too_many_arguments)]
fn dispatch(
    cmd: Cmd,
    ctx: &AnalysisCtx,
    files: &[parse::ParsedFile],
    root: &std::path::Path,
    scope: Scope,
    cfg: &[String],
    exclude: &[String],
    top: Option<usize>,
) -> Result<usize> {
    match cmd {
        Cmd::Audit(a) => {
            // Like dead-code, the call-set must come from the FULL tree.
            let all_files = full_tree_if_needed(root, scope, cfg, exclude)?;
            let call_source = all_files.as_deref().unwrap_or(files);
            let sel = audit::Selection::new(&a.only, &a.skip)?;
            let comparing = a.since.is_some() || a.baseline.is_some();
            if comparing || a.write_baseline.is_some() {
                ctx.out.start_recording();
            }
            let gating = audit::run(ctx, call_source, top, a.strict, a.findings_only, &sel)?;
            let current = ctx.out.take_recording();

            if let Some(p) = a.write_baseline.as_deref() {
                baseline::write(p, &current)?;
                ctx.out.note(&format!(
                    "note: wrote {} finding(s) to {} — compare a later run with \
                     `audit --baseline {}`",
                    current.len(),
                    p.display(),
                    p.display()
                ));
            }

            let Some((label, base)) = (match (&a.since, &a.baseline) {
                (Some(r), _) => Some((
                    r.clone(),
                    battery_at_ref(r, root, scope, cfg, exclude, &sel)?,
                )),
                (_, Some(p)) => Some((p.display().to_string(), baseline::read(p)?)),
                _ => None,
            }) else {
                return Ok(gating);
            };
            let d = baseline::diff(&base, &current);
            audit::print_diff(ctx, &label, &d);
            // `--fail-on-new` asks "did I make it worse", which is the gate an
            // agent wants mid-change; the default keeps asking "is it clean".
            if a.fail_on_new {
                return Ok(d.new.len());
            }
            Ok(gating)
        }
        Cmd::BuilderDrift(a) => builder_drift::run(ctx, a.ctor.as_deref(), a.min_score),
        Cmd::Clones(a) => Ok(clones::run_counted(ctx, a.min_tokens, a.min_score)?.total),
        Cmd::ConfigDrift(a) => config_drift::run(ctx, a.ty.as_deref(), a.min_score),
        Cmd::BlindSpots => {
            let sites = macro_scan::blind_spot_sites();
            for (file, line, name) in &sites {
                row!(
                    ctx.out,
                    "macro" => name.clone(),
                    "at" => emit::site(file, *line),
                );
            }
            ctx.out.summary(&format!(
                "({} blind spot(s) — macro bodies no check could read; their \
                 contents were not analysed)",
                sites.len()
            ));
            Ok(sites.len())
        }
        Cmd::Inventory(a) => inventory::run(
            ctx,
            a.kind,
            a.vis.or(a.pub_only.then_some(inventory::VisFilter::Pub)),
            a.name.as_deref(),
            a.tree,
            a.sort,
            a.include_docs,
        ),
        Cmd::Show(a) => show::run(
            ctx,
            &a.name,
            &show::ShowOpts {
                part: a.part,
                kind: a.kind.map(inventory::ItemKind::as_str),
                all: a.all,
                no_doc: a.hide_doc,
                number: a.number,
                max_lines: a.max_lines,
            },
        ),
        Cmd::Outline(a) => outline::run(
            ctx,
            &a.file,
            &outline::OutlineOpts {
                root,
                kind: a.kind.map(inventory::ItemKind::as_str),
                // `--pub-only` is `--vis pub`; clap has already rejected both.
                vis: a.vis.or(a.pub_only.then_some(inventory::VisFilter::Pub)),
                sort: a.sort,
                docs: a.include_docs,
                flat: a.flat,
            },
        ),
        Cmd::Callers(a) => {
            if let Some(pattern) = a.among.as_deref() {
                callers::run_callers_among(ctx, &a.name, pattern)
            } else {
                callers::run_callers(ctx, &a.name, a.transitive, a.depth, a.by, a.min_confidence)
            }
        }
        Cmd::Callees(a) => callers::run_callees(ctx, &a.name),
        Cmd::CoCall(a) => callers::run_co_call(ctx, &a.a, &a.b),
        Cmd::FieldUses(a) => field_uses::run(
            ctx,
            &a.ty,
            &a.field,
            field_uses::FieldUsesOpts {
                strict: !a.candidates,
                kinds: &a.class,
                via_receiver: a.via_receiver.as_deref(),
                min_confidence: a.min_confidence,
            },
        ),
        Cmd::Fields(a) => fields::run(ctx, &a.ty),
        Cmd::Variants(a) => variants::run(ctx, a.name.as_deref(), a.bare),
        Cmd::Impls(a) => impls::run(ctx, a.of.as_deref(), a.trait_.as_deref()),
        Cmd::TypeRefs(a) => type_refs::run(ctx, &a.ty, a.min_confidence),
        Cmd::TakesMut(a) => match a.ty.as_deref() {
            Some(ty) => takes_mut::run(ctx, ty),
            None => takes_mut::run_candidates(ctx),
        },
        Cmd::Metrics(a) => metrics::run(ctx, a.sort, a.threshold, false),
        Cmd::DeadCode(a) => {
            // Build the call-set from the FULL tree so production items called
            // only from tests aren't false-flagged as dead.
            let all_files = full_tree_if_needed(root, scope, cfg, exclude)?;
            let call_source = all_files.as_deref().unwrap_or(files);
            let vis = a.vis.or(a.pub_only.then_some(inventory::VisFilter::Pub));
            dead_code::run(ctx, call_source, vis, a.include_trait_impls)
        }
        Cmd::CatchAllArms(a) => catch_all::run(ctx, a.name.as_deref()),
        Cmd::ParallelMatches(a) => parallel_matches::run(
            ctx,
            a.name.as_deref(),
            parallel_matches::ScanOpts {
                partial_only: a.hide_exhaustive,
                rank_by_gap: a.rank_by_gap,
                show_missing: a.show_missing,
                include_matches_macro: a.include_matches_macro,
                include_if_chains: a.include_if_chains,
            },
        ),
        Cmd::EnumCoverage(a) => parallel_matches::run_enum_coverage(
            ctx,
            a.name.as_deref(),
            parallel_matches::CoverageOpts {
                hide_trait_routed: a.hide_trait_routed,
                max_missing: a.max_missing,
                // Ranking enums implies the per-site variant lists are noise.
                compact: a.compact || a.rank_enums,
                rank_enums: a.rank_enums,
                // Naming an enum means "tell me about this one" — no floor.
                // Sweeping means "rank by signal", where 1-of-2 predicates are
                // noise. `--min-variants` overrides either way.
                min_variants: a
                    .min_variants
                    .unwrap_or(if a.name.is_some() { 0 } else { 3 }),
            },
        ),
        Cmd::Divergence(a) => {
            if a.handling {
                divergence::run_handling(ctx, a.min_care_gap)
            } else {
                divergence::run(ctx, a.name.as_deref(), a.min_score)
            }
        }
        Cmd::ContractDrift(a) => contract_drift::run(
            ctx,
            a.name.as_deref().unwrap_or(""),
            &contract_drift::ContractOpts {
                reveal: a.reveal,
                candidates: a.candidates,
                no_bodies: a.no_bodies,
                max_lines: a.max_lines,
                min_callers: a.min_callers,
                min_confidence: a.min_confidence,
                top,
            },
        ),
        Cmd::Playbook => unreachable!("handled before the tree scan"),
        Cmd::CohortCallees(a) => callers::run_cohort_callees(ctx, &a.pattern),
        Cmd::ErrorSwallows(a) => error_swallows::run(
            ctx,
            error_swallows::SwallowOpts {
                include_unwrap_or: a.include_unwrap_or,
                // The flag says "hide"; the option says "include".
                include_infallible: !a.hide_infallible,
                include_logged: !a.hide_logged,
                min_score: a.min_score,
            },
        ),
        Cmd::Panics(a) => panics::run(
            ctx,
            panics::PanicOpts {
                // The flag says "hide"; the option says "include".
                include_idiomatic: !a.hide_idiomatic,
                min_score: a.min_score,
            },
        ),
        Cmd::ArithDrift(a) => arith_drift::run(ctx, a.min_score),
        Cmd::PassThrough(a) => pass_through::run(ctx, a.max_loc),
        Cmd::Explain(_) => unreachable!("handled before the tree scan"),
        Cmd::Casts(a) => casts::run(ctx, &a.class, a.by, a.hide_widen, a.include_unsafe_ptr),
        Cmd::Conversions(a) => conversions::run(ctx, &a.class, a.by),
        Cmd::ConversionPairs => conversion_pairs::run(ctx),
        Cmd::Stringly(a) => {
            stringly::run(ctx, a.include_substring, a.include_map_keys, a.by)
        }
        Cmd::Tests(a) => {
            // Always scan the full tree — under --scope production the tests we
            // want to enumerate would be stripped.
            let all_files = full_tree_if_needed(root, scope, cfg, exclude)?;
            let source = all_files.as_deref().unwrap_or(files);
            tests_cmd::run(
                ctx,
                source,
                &tests_cmd::TestsOpts {
                    with_hint: a.with_hint,
                    by_subcommand: a.by_subcommand,
                    only: a.subcommand.as_deref(),
                },
                &cli_grammar(),
            )
        }
        Cmd::Waivers(a) => {
            let today = match a.today.as_deref() {
                Some(s) => suppress::Date::parse(s).ok_or_else(|| {
                    anyhow::anyhow!("--today must be YYYY-MM-DD, got `{}`", s)
                })?,
                None => suppress::Date::today(),
            };
            let all_files = full_tree_if_needed(root, scope, cfg, exclude)?;
            let call_source = all_files.as_deref().unwrap_or(files);
            let action = if a.remove {
                waivers_cmd::Action::Remove
            } else if a.upgrade {
                waivers_cmd::Action::Upgrade
            } else {
                waivers_cmd::Action::List
            };
            waivers_cmd::run(
                ctx,
                call_source,
                waivers_cmd::WaiverOpts {
                    action,
                    check: a.check.as_deref(),
                    stale: a.stale,
                    orphaned: a.orphaned,
                    legacy_only: a.legacy,
                    write: a.write,
                    fail_on_stale: a.fail_on_stale,
                    today,
                },
            )
        }
    }
}

/// Run the gating battery over `root` as it existed at `git_ref`, and return
/// the findings it produced.
///
/// A full second scan of a materialized snapshot rather than anything
/// persisted: git already holds every prior state of the tree, and the tool
/// already depends on it. The snapshot is deleted when `snap` drops.
fn battery_at_ref(
    git_ref: &str,
    root: &std::path::Path,
    scope: Scope,
    cfg: &[String],
    exclude: &[String],
    sel: &audit::Selection,
) -> Result<Vec<emit::Finding>> {
    let snap = baseline::snapshot(git_ref, root)?;
    let files = parse::parse_dir(&snap.scan_root, scope, cfg, exclude)?;
    let idx = index::NameIndex::build(&files);
    let sem = semantic::Semantic::build(&files);
    let sup = suppress::scan(&files);
    let out = emit::Out::silent();
    out.start_recording();
    let sctx = AnalysisCtx {
        files: &files,
        idx: &idx,
        sem: &sem,
        // NOT `summary: true`: every check guards its row loop with
        // `if !summary`, so setting it would skip the very rows this run exists
        // to record. `Out::silent()` is what suppresses the printing.
        summary: false,
        spans: false,
        changed: None,
        out: &out,
        suppressions: &sup,
        suggest_waivers: false,
    };
    audit::run_silent_battery(&sctx, &files, audit::BatteryConfig::gating(), sel);
    // Rewrite the temp-dir paths back to how the caller spells them, so a
    // `gone` row names a file the reader can actually open.
    let prefix = snap.scan_root.to_string_lossy().into_owned();
    let want = parse::display_path(root);
    Ok(out
        .take_recording()
        .into_iter()
        .map(|mut f| {
            if let Some(rest) = f.file.strip_prefix(&prefix) {
                f.file = format!("{}{}", want, rest);
            }
            f
        })
        .collect())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let format = if cli.json { Format::Json } else { cli.format };
    let mut out = emit::Out::new(format, cli.summary, cli.all_stdout, cli.context);
    out.show_fingerprints = cli.fingerprints;
    // `explain` and `playbook` read only the embedded text — skip the tree scan.
    if let Cmd::Explain(a) = &cli.cmd {
        let result = explain::run(&out, a.topic.as_deref());
        out.finish("explain");
        if let Err(e) = &result {
            if e.downcast_ref::<context::TargetNotFound>().is_some() {
                std::process::exit(2);
            }
        }
        result?;
        return Ok(());
    }
    if matches!(cli.cmd, Cmd::Playbook) {
        explain::run_playbook(&out);
        out.finish("playbook");
        return Ok(());
    }
    let Cli {
        root,
        scope,
        cfg,
        exclude,
        summary,
        spans,
        changed_since,
        fail_on_findings,
        no_suppress,
        suggest_waivers,
        top,
        cmd,
        ..
    } = cli;
    // Exit-code contract: any setup error (bad glob, bad git ref, IO) is 2.
    let files = match parse::parse_dir(&root, scope, &cfg, &exclude) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {:#}", e);
            std::process::exit(2);
        }
    };
    if files.is_empty() {
        // Exit 2, not 0. A scan that saw nothing is a setup error — a typo'd
        // `--root`, a wrong cwd, an over-broad `--exclude` — and reporting it
        // as a clean run is the worst possible answer: `until unruster audit;
        // do fix; done` terminates immediately and a CI gate passes
        // vacuously. This actually happened: an agent ran
        // `unruster -r vectorian/src audit` from the wrong directory and got
        // "0 gating + 0 advisory across 12 checks; clean; exit 0" under a
        // warning it had no reason to read.
        eprintln!(
            "error: no .rs files found under {} (scope={:?}) — nothing was analysed, so \
             this is a setup error rather than a clean result. Check --root, the working \
             directory, --scope, and --exclude.",
            root.display(),
            scope
        );
        std::process::exit(2);
    }
    // Before any check runs. The blind-spot count is a property of the tree,
    // not of the subcommand, and the only way to keep it that way is to take
    // it in one pass over everything that was parsed — see `macro_scan::survey`.
    macro_scan::survey(&files);
    let idx = index::NameIndex::build(&files);
    let sem = semantic::Semantic::build(&files);
    // Waivers are read from the same files that were scanned, so a `//
    // unruster: ok` in an excluded or out-of-scope file has no effect.
    let suppressions = if no_suppress {
        suppress::Suppressions::default()
    } else {
        suppress::scan(&files)
    };
    // Waiver hygiene is advice about waivers, so it goes to the commands that
    // read waivers. It used to print on every invocation of everything: on a
    // `show` whose answer is 49 bytes the preamble was 558 — eleven times the
    // output, on every call, about a subsystem the command does not touch. An
    // agent making fifteen navigation calls paid for it fifteen times.
    if waiver_relevant(cmd_name(&cmd)) {
        report_waiver_hygiene(&out, &suppressions, matches!(cmd, Cmd::Waivers(_)));
    }
    let changed = match changed_since.as_deref() {
        Some(r) => match context::changed_set(r, &root) {
            Ok(set) => Some(set),
            Err(e) => {
                eprintln!("error: {:#}", e);
                std::process::exit(2);
            }
        },
        None => None,
    };
    let ctx = AnalysisCtx {
        files: &files,
        idx: &idx,
        sem: &sem,
        summary,
        spans,
        changed,
        out: &out,
        suppressions: &suppressions,
        suggest_waivers,
    };
    // Silence here is worse than absence: an agent that runs
    // `--suggest-waivers` on an unsupported check gets no line, no error, and
    // no way to tell whether the check has no findings or no waiver support.
    // On a real codebase that dead end sent someone off to invent a parallel
    // `// NOTE (unruster … false positive)` convention this tool cannot read.
    if suggest_waivers && !WAIVER_AWARE_CHECKS.contains(&cmd_name(&cmd)) {
        out.note(&format!(
            "note: `{}` does not support waivers, so --suggest-waivers has nothing to \
             offer here. Checks that do: {}",
            cmd_name(&cmd),
            WAIVER_AWARE_CHECKS.join(", ")
        ));
    }
    let fail_on_findings = fail_on_findings || cmd.implies_fail_on_findings();
    let command_name = cmd_name(&cmd);
    out.set_check(command_name);
    // Single-command runs never open a section, so the budget set here covers
    // the whole run. `audit` re-sets it per section from its own defaults.
    // `Some(0)` is "no cap", not "cap at zero" — see the flag's own help.
    out.set_row_budget(top.filter(|n| *n > 0));
    let result = dispatch(cmd, &ctx, &files, &root, scope, &cfg, &exclude, top);
    if let Some(note) = out.cap_note() {
        out.row_note(&note);
    }
    report_scope_gap(&out, scope, command_name);
    if analyses_code(command_name) {
        report_blind_spots(&out);
    }
    out.finish(command_name);
    let findings = match result {
        Ok(n) => n,
        // Exit-code contract: 0 = clean, 1 = findings (with --fail-on-findings
        // or `audit`), 2 = any error. TargetNotFound already printed its
        // warning; other errors print here.
        Err(e) => {
            if e.downcast_ref::<context::TargetNotFound>().is_none() {
                eprintln!("error: {:#}", e);
            }
            std::process::exit(2);
        }
    };
    // `audit` is the agent-loop entry point: findings always fail it.
    if fail_on_findings && findings > 0 {
        std::process::exit(1);
    }
    Ok(())
}
