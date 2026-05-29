use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

mod ast;
mod callers;
mod casts;
mod catch_all;
mod cfg_eval;
mod conversion_pairs;
mod conversions;
mod dead_code;
mod error_swallows;
mod field;
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

use parse::Scope;

#[derive(Parser)]
#[command(
    name = "unruster",
    about = "Query a Rust codebase: inventory, callers/callees, field uses, variants, impls, metrics, dead-code.",
    long_about = "Query a Rust codebase via syntactic (syn) analysis.\n\
        \n\
        Precision tiers (look for the `via` column on results to see which fired):\n\
        \n\
        PRECISE (raw AST shape — trustworthy):\n\
        - Item inventory, impl blocks, struct fields, enum variants.\n\
        - self.field accesses inside `impl Type`.\n\
        - Match-site / pattern shapes (catch-all-arms, parallel-matches).\n\
        - Free-fn / method / macro call sites by last-segment name.\n\
        \n\
        APPROXIMATE (semantic-lite, may have false positives/negatives):\n\
        - field-uses `via=ti`: receiver type inferred from local lets, params,\n\
          and obvious constructors. Misses method-chain results, generics,\n\
          trait dispatch, closure captures.\n\
        - type-refs `via=alias`: walks `type X = Y;` chains. Misses associated-\n\
          type re-exports (`impl Foo { type Out = Bar; }`).\n\
        - callers `Type::method`: also matches paths where the head segment is\n\
          renamed by `use foo::Type as Other`. Misses dyn-dispatch and generics.\n\
        - dead-code: last-segment name matching; pub items may have external\n\
          callers we don't see; trait methods are skipped to cut false positives.\n\
        \n\
        BEST-EFFORT (heuristic — flag and verify):\n\
        - field-uses `via=?` (--candidates only): receiver type unknown.\n\
        - Macro body scanning: token streams parsed speculatively as expressions.\n\
        - Custom macros and DSLs whose args aren't expressions are missed.\n\
        \n\
        Top-level flags:\n\
        - --scope production (default) / tests / all: controls test-code inclusion.\n\
        - --cfg KEY[=VALUE]: repeatable. Items whose cfg evaluates to definitively\n\
          False under this env are stripped. Unknown keys leave items in.\n\
        - --summary: skip per-row output, keep summary line.\n\
        \n\
        ══════════════════════════════════════════════════════════════════════\n\
        DESIGN AUDIT PLAYBOOK — pick the theme matching your concern.\n\
        The tool finds candidates; you decide whether to act.\n\
        \n\
        INDEX (five themes, jump to the one you need):\n\
          1. TYPE & DATA DESIGN     — when types should change shape\n\
          2. ENCAPSULATION & API    — what's leaking, hidden, or expensive to change\n\
          3. DISPATCH & CONTROL FLOW — branching smells: scattered logic, god fns\n\
          4. CORRECTNESS & SAFETY   — silent errors, silent data loss\n\
          5. AUDIT META             — auditing the test suite itself\n\
        \n\
        ──── 1. TYPE & DATA DESIGN ──────────────────────────────────────────\n\
        When concrete types should become traits, primitives become newtypes,\n\
        structs should split, or duplicate concepts should merge.\n\
        \n\
        ◇ EXTRACT A TRAIT (concrete type → interface)\n\
          unruster takes-mut <Type>                       # mutation surface\n\
          unruster type-refs <Type>                       # modules naming it\n\
          unruster callers --by module <Type>::<method>   # per-method caller spread\n\
          unruster inventory --kind impl-fn | grep '<Type>::'   # method count\n\
          Signal: many `&mut Type` fns + many naming modules + most callers\n\
          touch only a subset → extract trait(s); callers depend on interface.\n\
        \n\
        ◇ NEWTYPE (primitive overloaded across roles — `String`/`u32` as both id\n\
          and value)\n\
          unruster type-refs String                       # often huge\n\
          unruster takes-mut String\n\
          unruster callers <fn-taking-primitive> --by module\n\
          Signal: same primitive returned/accepted across unrelated APIs →\n\
          wrap each role (`UserId(u32)`, `Pixels(u32)`) so the compiler catches\n\
          mix-ups.\n\
        \n\
        ◇ SPLIT A STRUCT (low-cohesion / god-struct)\n\
          unruster fields <Type>                          # field count\n\
          unruster metrics --top 10                       # top structs by field count\n\
          unruster field-uses <Type> <field> --candidates # field-to-callers map\n\
          Signal: disjoint sets of fns touch disjoint sets of fields → split.\n\
        \n\
        ◇ DEAD ENUM VARIANTS\n\
          unruster variants <Enum>\n\
          Signal: 0 ctor sites + only seen in `_ =>` arms → drop the variant.\n\
        \n\
        ◇ REDUCE DATA REPLICATION / REPETITION / CONVERSION\n\
          unruster impls --trait From / Into / TryFrom / AsRef\n\
          unruster fields <A> ; unruster fields <B>       # shape overlap\n\
          unruster conversion-pairs                       # mutual From pairs\n\
          unruster pass-through                           # thin wrapper layers\n\
          unruster callers .clone                         # clone hotspots\n\
          unruster callers .to_string / .to_owned / .into # conversion hotspots\n\
          unruster inventory --kind fn | grep '::(from|to|as|into)_'\n\
          Signals: bidirectional `A ↔ B` From + overlapping fields = same\n\
          concept in two shapes (merge); heavy `.clone()` on one type =\n\
          fragmented ownership (Rc/Arc/borrow); sprawling `to_*`/`from_*`\n\
          namespaces on one type = collapse to standard trait impls.\n\
        \n\
        ──── 2. ENCAPSULATION & API SURFACE ─────────────────────────────────\n\
        What's leaking out, what should be hidden, what's the cost of changing\n\
        a published signature.\n\
        \n\
        ◇ PRIVATIZE A FIELD (stop external write bleeding)\n\
          unruster fields <Type>                          # pub/priv breakdown\n\
          unruster field-uses <Type> <field> --candidates\n\
          Signal: writes from contexts outside `Type::*` → make field private,\n\
          expose a method that enforces the invariant.\n\
        \n\
        ◇ SHRINK A PUB SURFACE (internal API hygiene)\n\
          unruster inventory --vis pub --kind impl-fn\n\
          unruster dead-code --pub-only\n\
          unruster callers --by module <Type>::<method>\n\
          Signal: pub fn with 0 callers → privatize. pub fn called from 1\n\
          module → consider `pub(crate)` or `pub(super)`.\n\
        \n\
        ◇ BREAKING-CHANGE BLAST RADIUS (before renaming/changing a signature)\n\
          unruster callers <Type>::<method>\n\
          unruster callers --transitive <Type>::<method>\n\
          unruster type-refs <Type>\n\
          Signal: direct + transitive caller count + coupling footprint tells\n\
          you the change cost.\n\
        \n\
        ──── 3. DISPATCH & CONTROL FLOW ─────────────────────────────────────\n\
        Branching design smells: scattered per-variant logic, brittle string\n\
        dispatch, oversized functions that should split.\n\
        \n\
        ◇ REPLACE ENUM-MATCH WITH POLYMORPHISM\n\
          unruster parallel-matches <Enum>                # match sites by variant set\n\
          unruster variants <Enum>                        # ctor + match per variant\n\
          unruster catch-all-arms <Enum>                  # `_ =>` arms (knowledge leak)\n\
          Signal: ≥2 match sites cover the same variant set → push behavior\n\
          into a trait method per variant; callers stop knowing the variants.\n\
        \n\
        ◇ PARTIAL-ENUMERATION DEFECT (a predicate that silently mis-bins a NEW\n\
          variant — run BEFORE adding a variant to find every site that won't\n\
          force-update)\n\
          unruster enum-coverage <Enum>                   # the one-stop synthesis\n\
          unruster parallel-matches <Enum> --partial --rank-by-gap --show-missing\n\
          unruster parallel-matches <Enum> --include-matches-macro\n\
          Signal: `enum-coverage` lists every PARTIAL match / `matches!` site —\n\
          one row per (gap_score, covered, missing, file:line), sorted by\n\
          gap_score = covered/total descending. The top rows are predicates\n\
          covering almost every variant (e.g. 7/8): a new variant falls into\n\
          their `_`/`matches!`-false arm with zero compiler warning. Exhaustive\n\
          sites are compiler-protected and hidden. `matches!()` is\n\
          guaranteed-supported (always on in enum-coverage; opt-in elsewhere via\n\
          --include-matches-macro) — its implicit no-match arm IS the risk.\n\
        \n\
        ◇ SIBLING-COHORT DIVERGENCE (one sibling in a `do_x_*` family forgot a\n\
          call its cohort-mates all make — e.g. `wrap_in_transform` skipped\n\
          `mark_pending` that `wrap_in_group`/`_composite` both call)\n\
          unruster callers <helper> --among '<name-glob>'  # who calls it / who doesn't\n\
          unruster cohort-callees '<name-glob>'             # (callee × fn) matrix\n\
          Signal: `--among` lists each cohort fn ✓ (calls helper) or ✗ (doesn't);\n\
          the ✗ rows are candidates. `cohort-callees` shows the whole grid —\n\
          a callee present in the majority of columns but missing from one is\n\
          flagged `<- divergence`. The tool finds the asymmetry; you decide\n\
          whether that sibling SHOULD have made the call (some omissions are\n\
          correct — not every sibling wraps an expandable kind).\n\
        \n\
        ◇ STRINGLY-TYPED CODE (logic branches on string literals)\n\
          unruster stringly                               # ==, .eq, match, assert_eq!\n\
          unruster stringly --include-substring           # also .starts_with/.contains\n\
          unruster stringly --include-map-keys            # also map.get(\"lit\")\n\
          unruster stringly --by fn                       # rank worst offenders\n\
          Signals: multiple `match-lit` arms in one fn → replace with enum\n\
          (compiler catches typos); `cmp-eq`/`cmp-method` hits → newtype the id\n\
          (e.g. `pub struct ActionId(&'static str)`); `map-lit-key` →\n\
          `HashMap<MyEnumKey, V>` instead of `HashMap<String, V>`.\n\
        \n\
        ◇ GOD FUNCTION TO SPLIT (long / complex / deeply-nested)\n\
          unruster metrics --sort loc --top 20            # by line count\n\
          unruster metrics --sort cyclo --top 20          # by cyclomatic complexity\n\
          unruster metrics --sort nesting --top 20        # by max nesting depth\n\
          unruster metrics --sort cyclo --threshold 15    # above-threshold\n\
          unruster metrics --sort nesting --threshold 4   # deeply-indented\n\
          unruster callees <Type>::<fn>                   # helper-call clustering\n\
          Signals: high LOC + low cyclo → extract named sections, not split;\n\
          high cyclo (≥15) → split decision logic into focused helpers, or\n\
          trait-dispatch (theme 3); high nesting (≥4) → flatten with early\n\
          returns / guard clauses; all three high → genuine god fn, split first.\n\
        \n\
        ──── 4. CORRECTNESS & SAFETY ────────────────────────────────────────\n\
        Runtime risks hidden in the code: silent error-swallowing, silent data\n\
        loss from casts and conversions.\n\
        \n\
        ◇ SILENT FALLBACKS (Result/Option error swallowing)\n\
          unruster error-swallows [--include-unwrap-or]\n\
          Signal: `match-err-wild` / `if-let-ok` / `let-_` / `.map_err(|_|)` /\n\
          `.unwrap_or_default()` hits. Some are intentional (parse cascades,\n\
          drop guards) — review per site.\n\
        \n\
        ◇ EXCESSIVE CASTS / SHAPE-JUGGLING (the same value renamed/reshaped\n\
          repeatedly because the wrong type was chosen at the boundary)\n\
          unruster casts                                  # all `as` casts, classified\n\
          unruster casts --class narrow-int,signed-flip,float-int,ptr\n\
          unruster casts --by fn                          # rank cast-heavy fns\n\
          unruster casts --no-widen                       # hide safe widenings\n\
          unruster conversions --by fn --top 10           # conversion-heavy fns\n\
          unruster conversions --kind .into,.to_string,::from\n\
          Signals: one fn doing many casts → wrong-typed input, cast once at\n\
          boundary; `narrow-int`/`signed-flip`/`float-int` → silent data-loss\n\
          candidates (prove safe or use checked helper); fn with 5+ conversion\n\
          calls = surrounding API wants the wrong type.\n\
        \n\
        ──── 5. AUDIT META ─────────────────────────────────────────────────\n\
        Auditing the test suite itself — coverage imbalance, near-duplicates,\n\
        missing flag combos.\n\
        \n\
        ◇ TEST-SUITE AUDIT\n\
          unruster tests                                  # list #[test] fns + file:start-end\n\
          unruster tests --with-hint                      # add args() fingerprint per test\n\
          unruster tests --by-subcommand                  # histogram by CLI subcommand\n\
          Signals: `--by-subcommand` exposes coverage imbalance (5 tests vs 1);\n\
          `--with-hint` exposes near-duplicates (same args fingerprint = same\n\
          test in disguise); `file:start-end` lets an agent read just the\n\
          relevant body via `sed -n start,endp file`.\n\
        \n\
        ─────────────────────────────────────────────────────────────────────\n\
        Closing principle: the tool finds candidates. The decision \"extract a\n\
        trait\" / \"newtype that primitive\" / \"split that fn\" / \"merge these\n\
        two types\" stays with the person reading the output.",
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
    /// Find read/write sites of a field on a given type.
    FieldUses(FieldArgs),

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
    /// `--partial` hides compiler-protected exhaustive groups; `--rank-by-gap`
    /// sorts by coverage ratio; `--show-missing` lists uncovered variants;
    /// `--include-matches-macro` also scans `matches!()`.
    ParallelMatches(ParallelMatchesArgs),
    /// Score every PARTIAL match / `matches!` site on an enum by coverage
    /// (gap_score = covered/total), sorted descending. Top rows are the
    /// predicates closest to exhaustive — the ones a newly-added variant would
    /// silently mis-bind. Synthesis of `parallel-matches --partial
    /// --rank-by-gap --show-missing --include-matches-macro`.
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
    /// usize-cross. Many casts in one fn = shape-juggling design smell.
    Casts(CastsArgs),
    /// Find conversion method/fn calls (.into / .to_string / Type::from / ...).
    /// Use `--by fn --top 10` to find conversion-heavy fns.
    Conversions(ConversionsArgs),
    /// Find bidirectional `From<A> for B` + `From<B> for A` pairs — same
    /// concept in two shapes, prime merge candidates.
    ConversionPairs,
    /// Find stringly-typed code: branching/matching on string literals.
    /// Catches `x == "lit"`, `x.eq("lit")`, `match x { "lit" => ... }`,
    /// `assert_eq!(x, "lit")`. Each row = candidate for an enum or newtype.
    Stringly(StringlyArgs),
    /// List `#[test]`/`#[bench]`/`#[tokio::test]` fns with `file:start-end`
    /// + name. Always scans the full tree (ignores --scope) since test code
    /// is the whole point. Use `--with-hint` to include the `args(...)` body
    /// fingerprint; use `--by subcommand` to group tests by which CLI
    /// subcommand they invoke (assert_cmd-style: looks at `.args([...])`).
    Tests(TestsArgs),
}

#[derive(Args)]
struct InventoryArgs {
    /// Filter to one kind: struct, enum, trait, fn, impl, mod, const, static, type, trait-fn, impl-fn.
    #[arg(long, short = 'k')]
    kind: Option<String>,
    /// Filter by visibility: pub, crate, priv.
    #[arg(long)]
    vis: Option<String>,
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
    /// Group results: `file` (path) or `module` (top-level module).
    #[arg(long)]
    by: Option<String>,
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
struct FieldArgs {
    /// Type name (last segment only, e.g. `Document`).
    ty: String,
    /// Field name.
    field: String,
    /// Also report non-self field accesses (noisier; no type inference).
    #[arg(long)]
    candidates: bool,
    /// Filter to reads only.
    #[arg(long)]
    reads_only: bool,
    /// Filter to writes only.
    #[arg(long)]
    writes_only: bool,
    /// Filter to inits only.
    #[arg(long)]
    inits_only: bool,
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
    #[arg(long, default_value = "loc")]
    sort: String,
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
    #[arg(long)]
    partial: bool,
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
}

#[derive(Args)]
struct EnumCoverageArgs {
    /// Enum name (last segment).
    name: String,
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
    /// Filter to one or more comma-separated classes:
    /// narrow-int, widen-int, signed-flip, float-int, int-float,
    /// narrow-float, widen-float, ptr, usize-cross, unknown, other.
    #[arg(long)]
    class: Option<String>,
    /// Group + count: fn, file, or module.
    #[arg(long)]
    by: Option<String>,
    /// Suppress safe-widening rows (widen-int / widen-float).
    #[arg(long)]
    no_widen: bool,
}

#[derive(Args)]
struct TestsArgs {
    /// Include a compact fingerprint of the test body's first `.args([...])`
    /// call (the `--root <path>` / `--scope <val>` prefix is stripped).
    #[arg(long)]
    with_hint: bool,
    /// Group + count tests by which CLI subcommand they invoke (heuristic:
    /// scans `.args([...])` calls in the body for a known-subcommand-shaped
    /// string literal). Drops the per-test list, prints a histogram.
    #[arg(long)]
    by_subcommand: bool,
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
    #[arg(long)]
    by: Option<String>,
}

#[derive(Args)]
struct ConversionsArgs {
    /// Filter to one or more comma-separated kinds:
    /// `.into`, `.try_into`, `.to_string`, `.to_owned`, `.to_vec`,
    /// `.as_str`, `.as_bytes`, `.as_ref`, `.as_mut`, `.parse`,
    /// `.cloned`, `.copied`, `.collect`, `::from`, `::try_from`.
    #[arg(long)]
    kind: Option<String>,
    /// Group + count: fn, file, or module. Without --by, lists every site.
    #[arg(long)]
    by: Option<String>,
    /// Show only the top N rows (applies after --by grouping if set).
    #[arg(long)]
    top: Option<usize>,
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
    let summary = cli.summary;
    match cli.cmd {
        Cmd::Inventory(a) => inventory::run(&files, a.kind.as_deref(), a.vis.as_deref(), a.tree, summary),
        Cmd::Callers(a) => {
            if let Some(pattern) = a.among.as_deref() {
                callers::run_callers_among(&files, &idx, &sem, &a.name, pattern, summary)
            } else {
                callers::run_callers(
                    &files,
                    &idx,
                    &sem,
                    &a.name,
                    a.transitive,
                    a.depth,
                    a.by.as_deref(),
                    summary,
                )
            }
        }
        Cmd::Callees(a) => callers::run_callees(&files, &idx, &sem, &a.name, summary),
        Cmd::FieldUses(a) => {
            let mut kinds: Vec<&str> = Vec::new();
            if a.reads_only {
                kinds.push("read");
            }
            if a.writes_only {
                kinds.push("write");
            }
            if a.inits_only {
                kinds.push("init");
            }
            field::run(
                &files,
                &sem.fn_sigs,
                &a.ty,
                &a.field,
                !a.candidates,
                &kinds,
                a.via_receiver.as_deref(),
                summary,
            )
        }
        Cmd::Fields(a) => fields::run(&files, &idx, &a.ty, summary),
        Cmd::Variants(a) => variants::run(&files, &idx, &a.name, a.bare, summary),
        Cmd::Impls(a) => impls::run(&idx, a.of.as_deref(), a.trait_.as_deref(), summary),
        Cmd::TypeRefs(a) => type_refs::run(&files, &idx, &sem.aliases, &a.ty, summary),
        Cmd::TakesMut(a) => takes_mut::run(&files, &idx, &a.ty, summary),
        Cmd::Metrics(a) => metrics::run(&files, &a.sort, a.top, a.threshold, summary),
        Cmd::DeadCode(a) => {
            // Build the call-set from the FULL tree (including tests + cfg(test))
            // regardless of the user's --scope, so production items called only
            // from tests aren't false-flagged. Skip the re-parse if the user
            // already asked for --scope all.
            let all_files_owned: Option<Vec<parse::ParsedFile>> = if scope == Scope::All {
                None
            } else {
                Some(parse::parse_dir(&cli.root, Scope::All, &cli.cfg)?)
            };
            let call_source: &[parse::ParsedFile] = match &all_files_owned {
                Some(v) => v,
                None => &files,
            };
            dead_code::run(&files, &idx, call_source, a.pub_only, summary)
        }
        Cmd::CatchAllArms(a) => catch_all::run(&files, &idx, &a.name, summary),
        Cmd::ParallelMatches(a) => parallel_matches::run(
            &files,
            &idx,
            &a.name,
            a.partial,
            a.rank_by_gap,
            a.show_missing,
            a.include_matches_macro,
            summary,
        ),
        Cmd::EnumCoverage(a) => parallel_matches::run_enum_coverage(&files, &idx, &a.name, summary),
        Cmd::CohortCallees(a) => callers::run_cohort_callees(&files, &idx, &sem, &a.pattern, summary),
        Cmd::ErrorSwallows(a) => error_swallows::run(&files, a.include_unwrap_or, summary),
        Cmd::PassThrough(a) => pass_through::run(&files, a.max_loc, summary),
        Cmd::Casts(a) => casts::run(
            &files,
            &sem.fn_sigs,
            a.class.as_deref(),
            a.by.as_deref(),
            a.no_widen,
            summary,
        ),
        Cmd::Conversions(a) => conversions::run(
            &files,
            a.kind.as_deref(),
            a.by.as_deref(),
            a.top,
            summary,
        ),
        Cmd::ConversionPairs => conversion_pairs::run(&files, summary),
        Cmd::Stringly(a) => stringly::run(
            &files,
            a.include_substring,
            a.include_map_keys,
            a.by.as_deref(),
            summary,
        ),
        Cmd::Tests(a) => {
            // Always scan the full tree — under --scope production the
            // tests we want to enumerate would be stripped.
            let all_files = if scope == Scope::All {
                None
            } else {
                Some(parse::parse_dir(&cli.root, Scope::All, &cli.cfg)?)
            };
            let source: &[parse::ParsedFile] = match &all_files {
                Some(v) => v,
                None => &files,
            };
            tests_cmd::run(source, a.with_hint, a.by_subcommand, summary)
        }
    }
}
