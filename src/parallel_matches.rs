use std::collections::BTreeMap;

use syn::visit::{self, Visit};

use crate::ast::{line_of, type_short};
use crate::index::NameIndex;
use crate::parse::{display_path, ParsedFile};

#[derive(Debug)]
struct Site {
    file: String,
    line: usize,
    context: String,
    /// Names of the target enum's variants that appear in this match site.
    variants: Vec<String>,
    /// Did this site have a wildcard arm?
    wildcard: bool,
}

struct ParaVisitor<'a> {
    target_enum: &'a str,
    variant_names: &'a [String],
    file: &'a str,
    module: &'a str,
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
            });
        }
        visit::visit_expr_match(self, e);
    }
}

pub fn run(
    files: &[ParsedFile],
    index: &NameIndex,
    enum_name: &str,
    summary: bool,
) -> anyhow::Result<()> {
    let mut variant_names: Vec<String> = Vec::new();
    for f in files {
        for item in &f.ast.items {
            if let syn::Item::Enum(e) = item {
                if e.ident == enum_name {
                    for v in &e.variants {
                        variant_names.push(v.ident.to_string());
                    }
                }
            }
        }
    }
    if variant_names.is_empty() && !index.knows_name(enum_name) {
        eprintln!("no enum named `{}` found in scanned tree", enum_name);
        return Ok(());
    }

    let mut all_sites: Vec<Site> = Vec::new();
    for f in files {
        let mut v = ParaVisitor {
            target_enum: enum_name,
            variant_names: &variant_names,
            file: &display_path(&f.path),
            module: &f.module,
            fn_stack: Vec::new(),
            impl_stack: Vec::new(),
            mod_stack: Vec::new(),
            sites: Vec::new(),
        };
        v.visit_file(&f.ast);
        all_sites.extend(v.sites);
    }

    // Group by variant set (key = joined sorted variants + wildcard flag).
    let mut groups: BTreeMap<(Vec<String>, bool), Vec<&Site>> = BTreeMap::new();
    for s in &all_sites {
        groups
            .entry((s.variants.clone(), s.wildcard))
            .or_default()
            .push(s);
    }
    // Rank groups by size descending (parallel-shot first).
    let mut rows: Vec<_> = groups.into_iter().collect();
    rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    if !summary {
        for ((variants, wildcard), sites) in &rows {
            let key = format!(
                "{}{}",
                variants.join(","),
                if *wildcard { " | _" } else { "" }
            );
            println!("group\t{}\t{} site(s)", key, sites.len());
            for s in sites {
                println!("  {}\t{}:{}", s.context, s.file, s.line);
            }
        }
    }
    eprintln!(
        "({} match site(s) across {} variant-set group(s) on `{}`)",
        all_sites.len(),
        rows.len(),
        enum_name
    );
    Ok(())
}
