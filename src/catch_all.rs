//! `catch-all-arms <Enum>` — match sites on the enum that contain a wildcard
//! `_ =>` arm. A filtered view over the `parallel-matches` scanner (wildcard
//! sites only, `match` exprs only), so the pattern semantics of the two
//! commands can't drift apart.

use crate::context::{warn_unknown_target, AnalysisCtx, TargetNotFound};
use crate::parallel_matches::{collect_sites, variant_names_of};

pub fn run(ctx: &AnalysisCtx, enum_name: &str) -> anyhow::Result<()> {
    let files = ctx.files;
    let index = ctx.idx;
    let summary = ctx.summary;
    let variant_names = variant_names_of(files, enum_name);
    if variant_names.is_empty() {
        if index.knows_name(enum_name) {
            eprintln!(
                "note: `{}` is named in the tree but no enum definition with variants \
                 was found under --scope; nothing to scan",
                enum_name
            );
            eprintln!("(0 match site(s) on `{}` with a wildcard arm)", enum_name);
            return Ok(());
        }
        warn_unknown_target("enum", enum_name);
        eprintln!("(0 match site(s) on `{}` with a wildcard arm)", enum_name);
        return Err(TargetNotFound::err("enum", enum_name));
    }

    // `match` exprs only (no `matches!`, no if-chains — this command is about
    // literal wildcard arms), keeping only sites that actually have one.
    let sites = collect_sites(files, enum_name, &variant_names, false, false);
    let mut all: Vec<_> = sites
        .iter()
        .filter(|s| s.wildcard && !s.variants.is_empty())
        .collect();
    all.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));

    if !summary {
        for h in &all {
            println!(
                "{}\t{}\t{}:{}",
                h.context,
                h.variants.join(","),
                h.file,
                h.line
            );
        }
    }
    eprintln!(
        "({} match site(s) on `{}` with a wildcard arm)",
        all.len(),
        enum_name
    );
    Ok(())
}
