use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

mod ast;
mod callers;
mod catch_all;
mod cfg_eval;
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
mod takes_mut;
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
        ──────────────────────────────────────────────────────────────────────\n\
        DESIGN AUDIT PLAYBOOK — which queries surface which refactor candidate.\n\
        Use this as input to design decisions; the tool finds candidates, you\n\
        decide whether to act.\n\
        \n\
        EXTRACT A TRAIT (replace a concrete type with an interface):\n\
          unruster takes-mut <Type>          # mutation surface\n\
          unruster type-refs <Type>          # coupling: how many modules name it\n\
          unruster callers --by module <Type>::<method>   # per-method caller spread\n\
          unruster inventory --kind impl-fn | grep '<Type>::'   # method count\n\
        Signal: many `&mut Type` fns + many modules naming Type, where most\n\
        callers only touch a subset of methods → trait extraction lets callers\n\
        depend on the interface, not the concrete type.\n\
        \n\
        PRIVATIZE A FIELD (stop external write bleeding):\n\
          unruster fields <Type>             # pub/priv breakdown\n\
          unruster field-uses <Type> <field> --candidates\n\
        Signal: writes appear with context outside `Type::*` methods → make the\n\
        field private and add a method that enforces the invariant.\n\
        \n\
        REPLACE ENUM-MATCH WITH POLYMORPHISM:\n\
          unruster parallel-matches <Enum>   # match sites grouped by variant set\n\
          unruster variants <Enum>           # ctor + match counts per variant\n\
          unruster catch-all-arms <Enum>     # `_ =>` arms (knowledge leak)\n\
        Signal: ≥2 match sites cover the same variant set → push behavior into\n\
        a trait with one impl per variant; callers stop knowing the variants.\n\
        \n\
        NEWTYPE (when a primitive plays many roles — String/u32/usize as ids,\n\
        sizes, indices all at once):\n\
          unruster type-refs String          # often huge; tells you primitive is overloaded\n\
          unruster takes-mut String\n\
          unruster callers <fn-that-takes-primitive> --by module\n\
        Signal: same primitive returned/accepted across unrelated APIs → wrap\n\
        each role in a newtype (`UserId(u32)`, `Pixels(u32)`) so the compiler\n\
        catches mix-ups.\n\
        \n\
        SPLIT A STRUCT (low-cohesion / god-struct):\n\
          unruster fields <Type>             # field count + per-field counts\n\
          unruster metrics --sort loc        # top structs by field count\n\
          # Then for each field group, run:\n\
          unruster field-uses <Type> <field> --candidates  # to map field-to-callers\n\
        Signal: disjoint sets of fns touch disjoint sets of fields → split into\n\
        focused structs that match the access pattern.\n\
        \n\
        DEAD ENUM VARIANTS (kept around with no producers):\n\
          unruster variants <Enum>\n\
        Signal: variant has 0 ctor sites and is only seen in `_ =>` arms → drop\n\
        the variant (or document why it's a placeholder).\n\
        \n\
        GOD FUNCTION TO SPLIT:\n\
          unruster metrics --sort loc --top 20\n\
          unruster callees <Type>::<fn>      # check helper-call clustering\n\
        Signal: long fn calling several disjoint helper clusters → split the\n\
        body into named helpers.\n\
        \n\
        SHRINK A PUB SURFACE (internal API hygiene):\n\
          unruster inventory --vis pub --kind impl-fn\n\
          unruster dead-code --pub-only\n\
          unruster callers --by module <Type>::<method>\n\
        Signal: pub fn with 0 callers in the tree → privatize. pub fn called\n\
        from 1 module → consider making it pub(crate) or pub(super).\n\
        \n\
        SILENT FALLBACKS (forbidden by callee's contract):\n\
          unruster error-swallows [--include-unwrap-or]\n\
        Signal: `match-err-wild` / `if-let-ok` / `let-_` / `.map_err(|_|)` /\n\
        `.unwrap_or_default()` hits. Each row is a candidate — some are\n\
        intentional (parse cascades, drop guards); review per site.\n\
        \n\
        BREAKING-CHANGE BLAST RADIUS (before renaming/changing a signature):\n\
          unruster callers <Type>::<method>          # direct callers\n\
          unruster callers --transitive <Type>::<method>   # indirect callers\n\
          unruster type-refs <Type>                  # full coupling footprint\n\
        Signal: caller count + transitive depth tells you the change cost.\n\
        \n\
        REDUCE DATA REPLICATION / REPETITION / CONVERSION:\n\
          unruster impls --trait From                # From<X> for Y impl count\n\
          unruster impls --trait Into                # Into impls\n\
          unruster impls --trait TryFrom\n\
          unruster impls --trait AsRef               # cheap-conversion proliferation\n\
          unruster fields <A> ; unruster fields <B>  # field-shape overlap between candidates\n\
          unruster pass-through                      # thin wrappers, often in conversion layers\n\
          unruster parallel-matches <Enum>           # same per-variant logic in multiple sites\n\
          unruster callers .clone                    # clone() hotspots — fragmented ownership\n\
          unruster callers .to_string                # conversion hotspot\n\
          unruster callers .to_owned\n\
          unruster callers .into\n\
          unruster inventory --kind fn               # then grep '::(from|to|as|into)_' for clusters\n\
        Signals:\n\
        - Many `From<A> for B` (plus `From<B> for A`) impls between the same\n\
          two types → A and B are the same logical concept in two shapes;\n\
          merge them, or make one a view (`AsRef<B> for A`).\n\
        - Two structs whose `fields <T>` outputs share most field names+types\n\
          → parallel data representations; collapse to one and convert at the\n\
          boundary, or make one a wrapper of the other.\n\
        - `.clone()` called heavily on one type → ownership is fragmented;\n\
          consider `Rc`/`Arc`, a single-owner pattern, or borrowing.\n\
        - `parallel-matches` finding the same variant set in many sites →\n\
          per-variant logic is being re-derived; push the work onto the type\n\
          (trait method) so the logic lives in one place.\n\
        - A cluster of `to_*` / `from_*` / `as_*` / `into_*` fns on the same\n\
          type, especially with overlapping names (`to_str`, `as_str`,\n\
          `to_string`, `into_string`) → conversion namespace is sprawling;\n\
          consolidate to the standard trait impls (`AsRef<str>`, `Display`)\n\
          and delete the bespoke helpers.\n\
        - pass-through fn whose body is `to_other_repr(x)` → the two reprs\n\
          coexist with no behavior difference; one should win.\n\
        \n\
        General principle: the tool finds candidates. The decision \"extract a\n\
        trait here\" / \"newtype that primitive\" / \"split that fn\" / \"merge\n\
        these two types\" stays with the person reading the output.",
    version
)]
struct Cli {
    /// Root directory (or file) to scan. Respects .gitignore.
    #[arg(long, short = 'r', default_value = ".")]
    root: PathBuf,

    /// Test-code scope: production (default), tests, or all.
    #[arg(long, global = true, default_value = "production")]
    scope: String,

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
    /// Rank fns by LOC / parameter count, structs by field count, enums by variant count.
    Metrics(MetricsArgs),

    /// List fns with no caller in the scanned tree (heuristic; pub items may have external callers).
    DeadCode(DeadCodeArgs),
    /// Find match sites on a given enum that contain a wildcard `_ =>` arm.
    CatchAllArms(CatchAllArgs),
    /// Group match sites on an enum by which variants they cover (shotgun-surgery candidates).
    ParallelMatches(ParallelMatchesArgs),
    /// Find Result/Option error-swallowing patterns. Detects method calls
    /// (`.ok()`, `.err()`, `.unwrap_or_default()`, `.unwrap_or_else(...)`,
    /// `.map_err(|_|...)`) and syntactic forms (`match { Err(_) => ... }`,
    /// `if let Ok(...)` with no else, `while let Ok(...)`, `let _ = expr;`).
    /// Each row carries a `kind` label so you can grep by category. Some hits
    /// are intentional (e.g. `let _ =` of a Drop guard) — review per site.
    ErrorSwallows(ErrorSwallowsArgs),
    /// Find pass-through wrappers: fns whose body is a single call/expression.
    PassThrough(PassThroughArgs),
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
    /// Sort fns by: `loc` (lines), `params` (parameter count).
    #[arg(long, default_value = "loc")]
    sort: String,
    /// Top N per category to print.
    #[arg(long, default_value_t = 20)]
    top: usize,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let scope = Scope::parse(&cli.scope)?;
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
        Cmd::Callers(a) => callers::run_callers(
            &files,
            &idx,
            &sem,
            &a.name,
            a.transitive,
            a.depth,
            a.by.as_deref(),
            summary,
        ),
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
        Cmd::Metrics(a) => metrics::run(&files, &a.sort, a.top, summary),
        Cmd::DeadCode(a) => dead_code::run(&files, &idx, a.pub_only, summary),
        Cmd::CatchAllArms(a) => catch_all::run(&files, &idx, &a.name, summary),
        Cmd::ParallelMatches(a) => parallel_matches::run(&files, &idx, &a.name, summary),
        Cmd::ErrorSwallows(a) => error_swallows::run(&files, a.include_unwrap_or, summary),
        Cmd::PassThrough(a) => pass_through::run(&files, a.max_loc, summary),
    }
}
