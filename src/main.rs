use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

mod ast;
mod callers;
mod field;
mod inventory;
mod parse;

#[derive(Parser)]
#[command(
    name = "unruster",
    about = "Query a Rust codebase: inventory, callers/callees, field uses.",
    long_about = "Query a Rust codebase via syntactic (syn) analysis.\n\
        \n\
        Known limitations:\n\
        - Macro bodies (println!, format!, write!, ...) are not scanned — field/call\n\
          accesses inside macro args are missed.\n\
        - Non-`self` field accesses are matched by field name only; the receiver's\n\
          type isn't inferred. Use --candidates on field-uses to see them.\n\
        - `cfg`-gated code is included regardless of features.",
    version
)]
struct Cli {
    /// Root directory (or file) to scan. Respects .gitignore.
    #[arg(long, short = 'r', default_value = ".")]
    root: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List all top-level items (struct, enum, trait, fn, impl, ...).
    Inventory(InventoryArgs),
    /// Find call sites of a function or method by name.
    Callers(CallersArgs),
    /// List callees made from inside a function or method.
    Callees(CalleesArgs),
    /// Find read/write sites of a field on a given type.
    FieldUses(FieldArgs),
}

#[derive(Args)]
struct InventoryArgs {
    /// Filter to one kind: struct, enum, trait, fn, impl, mod, const, static, type, trait-fn.
    #[arg(long, short = 'k')]
    kind: Option<String>,
}

#[derive(Args)]
struct CallersArgs {
    /// Function or method to look for. Forms:
    ///   bare name (e.g. `translate`)        — matches free fns and methods by last segment
    ///   `Type::method` (e.g. `Doc::write`)  — matches paths ending in `Type::method`
    ///   `.method` (e.g. `.write`)           — matches method calls only
    ///   `::name` (e.g. `::open`)            — matches free-fn paths only
    name: String,
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let files = parse::parse_dir(&cli.root)?;
    if files.is_empty() {
        eprintln!("warning: no .rs files found under {}", cli.root.display());
    }
    match cli.cmd {
        Cmd::Inventory(a) => inventory::run(&files, a.kind.as_deref()),
        Cmd::Callers(a) => callers::run_callers(&files, &a.name),
        Cmd::Callees(a) => callers::run_callees(&files, &a.name),
        Cmd::FieldUses(a) => field::run(&files, &a.ty, &a.field, !a.candidates),
    }
}
