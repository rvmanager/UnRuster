use std::collections::BTreeMap;

use syn::visit::{self, Visit};

use crate::ast::{line_of, type_short};
use crate::index::NameIndex;
use crate::macro_scan::{macro_body, Body};
use crate::parse::{display_path, ParsedFile};

#[derive(Debug)]
struct Site {
    file: String,
    line: usize,
    context: String,
    /// Names of the target enum's variants that appear in this match site.
    variants: Vec<String>,
    /// Did this site have a wildcard arm? `matches!()` always counts as having
    /// one — the implicit "no-match" branch is exactly the silent-misclassify
    /// risk this tool hunts for.
    wildcard: bool,
    /// True if this site is a `matches!()` invocation rather than a `match` expr.
    is_macro: bool,
}

struct ParaVisitor<'a> {
    target_enum: &'a str,
    variant_names: &'a [String],
    file: &'a str,
    module: &'a str,
    /// Scan `matches!(scrutinee, PAT)` invocations in addition to `match` exprs.
    include_matches_macro: bool,
    fn_stack: Vec<String>,
    impl_stack: Vec<String>,
    mod_stack: Vec<String>,
    sites: Vec<Site>,
}

impl<'a> ParaVisitor<'a> {
    fn enclosing(&self) -> String {
        let mut path: Vec<String> = Vec::new();
        if !self.module.is_empty() {
            path.push(self.module.to_string());
        }
        path.extend(self.mod_stack.iter().cloned());
        if let Some(t) = self.impl_stack.last() {
            path.push(t.clone());
        }
        if let Some(f) = self.fn_stack.last() {
            path.push(f.clone());
        }
        if path.is_empty() {
            "<top-level>".into()
        } else {
            path.join("::")
        }
    }

    fn variant_in_pattern(&self, pat: &syn::Pat) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_variants(pat, &mut out);
        out
    }

    fn collect_variants(&self, pat: &syn::Pat, out: &mut Vec<String>) {
        match pat {
            syn::Pat::Path(p) => self.push_if_match(&p.path, out),
            syn::Pat::TupleStruct(p) => self.push_if_match(&p.path, out),
            syn::Pat::Struct(p) => self.push_if_match(&p.path, out),
            syn::Pat::Or(o) => {
                for c in &o.cases {
                    self.collect_variants(c, out);
                }
            }
            syn::Pat::Reference(r) => self.collect_variants(&r.pat, out),
            syn::Pat::Paren(p) => self.collect_variants(&p.pat, out),
            _ => {}
        }
    }

    fn push_if_match(&self, p: &syn::Path, out: &mut Vec<String>) {
        let segs: Vec<&syn::PathSegment> = p.segments.iter().collect();
        if segs.len() < 2 {
            return;
        }
        if segs[segs.len() - 2].ident != self.target_enum {
            return;
        }
        let last = segs[segs.len() - 1].ident.to_string();
        if self.variant_names.iter().any(|v| v == &last) && !out.contains(&last) {
            out.push(last);
        }
    }

    fn is_wildcard(pat: &syn::Pat) -> bool {
        matches!(pat, syn::Pat::Wild(_))
            || matches!(pat, syn::Pat::Ident(i) if i.subpat.is_none())
    }
}

impl<'ast, 'a> Visit<'ast> for ParaVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.mod_stack.push(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.mod_stack.pop();
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        visit::visit_item_fn(self, i);
        self.fn_stack.pop();
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.impl_stack.push(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.impl_stack.pop();
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        visit::visit_impl_item_fn(self, i);
        self.fn_stack.pop();
    }

    fn visit_expr_match(&mut self, e: &'ast syn::ExprMatch) {
        let mut variants: Vec<String> = Vec::new();
        let mut wildcard = false;
        for arm in &e.arms {
            for v in self.variant_in_pattern(&arm.pat) {
                if !variants.contains(&v) {
                    variants.push(v);
                }
            }
            if Self::is_wildcard(&arm.pat) {
                wildcard = true;
            }
        }
        if !variants.is_empty() {
            variants.sort();
            self.sites.push(Site {
                file: self.file.to_string(),
                line: line_of(&e.match_token),
                context: self.enclosing(),
                variants,
                wildcard,
                is_macro: false,
            });
        }
        visit::visit_expr_match(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if self.include_matches_macro {
            // `matches!(scrutinee, PAT)` — PAT is the only thing that matches; every
            // other variant falls through to an implicit `false`. So a partial
            // pattern is a silent-misclassify just like `match { .. => _ }`.
            if let Body::Matches { pat, .. } = macro_body(m) {
                let mut variants = self.variant_in_pattern(&pat);
                if !variants.is_empty() {
                    variants.sort();
                    variants.dedup();
                    self.sites.push(Site {
                        file: self.file.to_string(),
                        line: line_of(&m.path),
                        context: self.enclosing(),
                        variants,
                        wildcard: true,
                        is_macro: true,
                    });
                }
            }
        }
        visit::visit_macro(self, m);
    }
}

/// Read the target enum's variant names from any definition in the tree.
fn variant_names_of(files: &[ParsedFile], enum_name: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for f in files {
        for item in &f.ast.items {
            if let syn::Item::Enum(e) = item {
                if e.ident == enum_name {
                    for v in &e.variants {
                        names.push(v.ident.to_string());
                    }
                }
            }
        }
    }
    names
}

/// Walk every file and collect the match / `matches!` sites that mention the enum.
fn collect_sites(
    files: &[ParsedFile],
    enum_name: &str,
    variant_names: &[String],
    include_matches_macro: bool,
) -> Vec<Site> {
    let mut all_sites: Vec<Site> = Vec::new();
    for f in files {
        let mut v = ParaVisitor {
            target_enum: enum_name,
            variant_names,
            file: &display_path(&f.path),
            module: &f.module,
            include_matches_macro,
            fn_stack: Vec::new(),
            impl_stack: Vec::new(),
            mod_stack: Vec::new(),
            sites: Vec::new(),
        };
        v.visit_file(&f.ast);
        all_sites.extend(v.sites);
    }
    all_sites
}

/// Variants present in `full` but absent from `covered`, preserving `full`'s order.
fn missing_variants(covered: &[String], full: &[String]) -> Vec<String> {
    full.iter()
        .filter(|v| !covered.contains(v))
        .cloned()
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    files: &[ParsedFile],
    index: &NameIndex,
    enum_name: &str,
    partial_only: bool,
    rank_by_gap: bool,
    show_missing: bool,
    include_matches_macro: bool,
    summary: bool,
) -> anyhow::Result<()> {
    let variant_names = variant_names_of(files, enum_name);
    if variant_names.is_empty() && !index.knows_name(enum_name) {
        eprintln!("no enum named `{}` found in scanned tree", enum_name);
        return Ok(());
    }
    let total = variant_names.len();

    let all_sites = collect_sites(files, enum_name, &variant_names, include_matches_macro);

    // Group by variant set (key = joined sorted variants + wildcard flag).
    let mut groups: BTreeMap<(Vec<String>, bool), Vec<&Site>> = BTreeMap::new();
    for s in &all_sites {
        groups
            .entry((s.variants.clone(), s.wildcard))
            .or_default()
            .push(s);
    }
    let mut rows: Vec<_> = groups.into_iter().collect();

    // A group is "exhaustive" when it names every variant of the enum — those
    // are compiler-protected, so `--partial` drops them.
    let is_exhaustive = |variants: &[String]| total > 0 && variants.len() == total;
    if partial_only {
        rows.retain(|((variants, _), _)| !is_exhaustive(variants));
    }

    // Default ordering: by group size descending (parallel-shot first). With
    // --rank-by-gap, order by coverage ratio descending instead — a 7/8 group
    // (one new variant silently mis-binds) is a louder defect signal than a 1/8.
    if rank_by_gap && total > 0 {
        rows.sort_by(|a, b| {
            let ga = a.0 .0.len() as f64 / total as f64;
            let gb = b.0 .0.len() as f64 / total as f64;
            gb.partial_cmp(&ga)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.1.len().cmp(&a.1.len()))
                .then_with(|| a.0.cmp(&b.0))
        });
    } else {
        rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    }

    if !summary {
        for ((variants, wildcard), sites) in &rows {
            let mut key = format!(
                "{}{}",
                variants.join(","),
                if *wildcard { " | _" } else { "" }
            );
            if rank_by_gap && total > 0 {
                key = format!("[{}/{}] {}", variants.len(), total, key);
            }
            if show_missing && total > 0 {
                let miss = missing_variants(variants, &variant_names);
                let miss = if miss.is_empty() {
                    "(none)".to_string()
                } else {
                    miss.join(",")
                };
                key = format!("{}\tmissing: {}", key, miss);
            }
            println!("group\t{}\t{} site(s)", key, sites.len());
            for s in sites {
                let tag = if s.is_macro { " (matches!)" } else { "" };
                println!("  {}{}\t{}:{}", s.context, tag, s.file, s.line);
            }
        }
    }
    eprintln!(
        "({} match site(s) across {} variant-set group(s) on `{}`{})",
        all_sites.len(),
        rows.len(),
        enum_name,
        if partial_only { "; exhaustive groups hidden" } else { "" }
    );
    Ok(())
}

/// `enum-coverage <Enum>` — synthesis of the partial-enumeration defect class.
/// One row per *partial* match / `matches!` site (exhaustive sites are
/// compiler-protected and hidden), sorted by gap_score = covered/total
/// descending. The top rows — predicates that cover almost every variant —
/// are the sites most likely to silently mis-bind a newly-added variant.
pub fn run_enum_coverage(
    files: &[ParsedFile],
    index: &NameIndex,
    enum_name: &str,
    summary: bool,
) -> anyhow::Result<()> {
    let variant_names = variant_names_of(files, enum_name);
    if variant_names.is_empty() {
        if index.knows_name(enum_name) {
            eprintln!(
                "enum `{}` is named in the tree but its definition (with variants) \
                 wasn't found under --scope; nothing to score",
                enum_name
            );
        } else {
            eprintln!("no enum named `{}` found in scanned tree", enum_name);
        }
        return Ok(());
    }
    let total = variant_names.len();

    // matches!() is guaranteed-supported here — it's the primary vector for
    // this defect, so enum-coverage always includes it.
    let all_sites = collect_sites(files, enum_name, &variant_names, true);

    // One row per site; keep only partials (covered < total).
    struct Row<'s> {
        gap: f64,
        site: &'s Site,
        missing: Vec<String>,
    }
    let mut rows: Vec<Row> = all_sites
        .iter()
        .filter(|s| s.variants.len() < total)
        .map(|s| Row {
            gap: s.variants.len() as f64 / total as f64,
            site: s,
            missing: missing_variants(&s.variants, &variant_names),
        })
        .collect();
    // Highest coverage ratio (smallest gap to full) first — loudest signal on top.
    rows.sort_by(|a, b| {
        b.gap
            .partial_cmp(&a.gap)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.site.file.cmp(&b.site.file))
            .then_with(|| a.site.line.cmp(&b.site.line))
    });

    if !summary {
        for r in &rows {
            let tag = if r.site.is_macro { " (matches!)" } else { "" };
            println!(
                "{:.2}\t{}/{}\t{}\t{}\t{}:{}\t{}{}",
                r.gap,
                r.site.variants.len(),
                total,
                r.site.variants.join(","),
                r.missing.join(","),
                r.site.file,
                r.site.line,
                r.site.context,
                tag
            );
        }
    }
    eprintln!(
        "({} partial site(s) on `{}`; {} total variant(s); exhaustive sites hidden)",
        rows.len(),
        enum_name,
        total
    );
    Ok(())
}
