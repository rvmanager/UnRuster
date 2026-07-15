//! `catch-all-arms <Enum>` — match sites on the enum that contain a wildcard
//! `_ =>` arm. A filtered view over the `parallel-matches` scanner (wildcard
//! sites only, `match` exprs only), so the pattern semantics of the two
//! commands can't drift apart.

use crate::context::{warn_unknown_target, AnalysisCtx, TargetNotFound};
use crate::parallel_matches::{collect_sites, enum_sealed, variant_names_of};

pub fn run(ctx: &AnalysisCtx, target: Option<&str>) -> anyhow::Result<usize> {
    match target {
        Some(enum_name) => {
            let variant_names = variant_names_of(ctx.files, enum_name);
            if variant_names.is_empty() {
                if ctx.idx.knows_name(enum_name) {
                    eprintln!(
                        "note: `{}` is named in the tree but no enum definition with variants \
                         was found under --scope; nothing to scan",
                        enum_name
                    );
                    eprintln!("(0 match site(s) on `{}` with a wildcard arm)", enum_name);
                    return Ok(0);
                }
                warn_unknown_target("enum", enum_name);
                eprintln!("(0 match site(s) on `{}` with a wildcard arm)", enum_name);
                return Err(TargetNotFound::err("enum", enum_name));
            }
            let (count, sealed_rows) = scan_one(ctx, enum_name, &variant_names, false);
            eprintln!(
                "({} match site(s) on `{}` with a wildcard arm{}; explain: partial-enumeration)",
                count,
                enum_name,
                if sealed_rows > 0 {
                    format!("; {} on a SEALED enum", sealed_rows)
                } else {
                    String::new()
                }
            );
            Ok(count)
        }
        // `--all`: every enum in the index; rows gain a leading enum column.
        None => {
            let mut count = 0usize;
            let mut sealed_rows = 0usize;
            let mut scanned = 0usize;
            for name in ctx.idx.enum_names() {
                let variant_names = variant_names_of(ctx.files, &name);
                if variant_names.is_empty() {
                    continue;
                }
                scanned += 1;
                let (c, s) = scan_one(ctx, &name, &variant_names, true);
                count += c;
                sealed_rows += s;
            }
            eprintln!(
                "({} match site(s) with a wildcard arm across {} enum(s); --all{}; explain: partial-enumeration)",
                count,
                scanned,
                if sealed_rows > 0 {
                    format!("; {} on SEALED enums", sealed_rows)
                } else {
                    String::new()
                }
            );
            Ok(count)
        }
    }
}

/// Scan one enum's `match` exprs (no `matches!`, no if-chains — this command
/// is about literal wildcard arms) and print sites that have one. With
/// `prefixed` (--all mode) rows carry a leading enum-name column. Returns
/// (rows shown, rows on a sealed enum).
fn scan_one(
    ctx: &AnalysisCtx,
    enum_name: &str,
    variant_names: &[String],
    prefixed: bool,
) -> (usize, usize) {
    let sealed = enum_sealed(ctx.files, enum_name);
    let sites = collect_sites(ctx.files, enum_name, variant_names, false, false, ctx.spans);
    let mut all: Vec<_> = sites
        .iter()
        .filter(|s| s.wildcard && !s.variants.is_empty())
        .collect();
    ctx.retain_changed(&mut all, |s| &s.file);
    all.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));

    if !ctx.summary {
        for h in &all {
            let tag = if sealed { "\tSEALED" } else { "" };
            let body = format!(
                "{}\t{}\t{}:{}{}",
                h.context,
                h.variants.join(","),
                h.file,
                h.line,
                tag
            );
            if prefixed {
                println!("{}\t{}", enum_name, body);
            } else {
                println!("{}", body);
            }
            ctx.print_context(&h.file, h.line);
        }
    }
    let sealed_rows = if sealed { all.len() } else { 0 };
    (all.len(), sealed_rows)
}
