use syn::visit::{self, Visit};

use crate::ast::{line_of, type_short};
use crate::index::NameIndex;
use crate::parse::{display_path, ParsedFile};

struct CatchAllVisitor<'a> {
    target_enum: &'a str,
    variant_names: &'a [String],
    file: &'a str,
    module: &'a str,
    fn_stack: Vec<String>,
    impl_stack: Vec<String>,
    mod_stack: Vec<String>,
    hits: Vec<Hit>,
}

#[derive(Debug)]
struct Hit {
    file: String,
    line: usize,
    context: String,
    variants_covered: Vec<String>,
}

impl<'a> CatchAllVisitor<'a> {
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

    fn pattern_targets_variant(&self, pat: &syn::Pat) -> Option<String> {
        match pat {
            syn::Pat::Path(p) => self.match_variant(&p.path),
            syn::Pat::TupleStruct(p) => self.match_variant(&p.path),
            syn::Pat::Struct(p) => self.match_variant(&p.path),
            syn::Pat::Or(o) => o.cases.iter().find_map(|p| self.pattern_targets_variant(p)),
            syn::Pat::Reference(r) => self.pattern_targets_variant(&r.pat),
            syn::Pat::Paren(p) => self.pattern_targets_variant(&p.pat),
            _ => None,
        }
    }

    fn match_variant(&self, p: &syn::Path) -> Option<String> {
        let segs: Vec<&syn::PathSegment> = p.segments.iter().collect();
        if segs.is_empty() {
            return None;
        }
        let last = segs[segs.len() - 1].ident.to_string();
        if !self.variant_names.iter().any(|v| v == &last) {
            return None;
        }
        if segs.len() >= 2 && segs[segs.len() - 2].ident == self.target_enum {
            return Some(last);
        }
        None
    }

    fn is_wildcard(pat: &syn::Pat) -> bool {
        match pat {
            syn::Pat::Wild(_) => true,
            // Plain ident binding (no subpattern, no leading mod path) — also acts as catch-all.
            syn::Pat::Ident(i) => i.subpat.is_none(),
            syn::Pat::Reference(r) => Self::is_wildcard(&r.pat),
            syn::Pat::Paren(p) => Self::is_wildcard(&p.pat),
            _ => false,
        }
    }
}

impl<'ast, 'a> Visit<'ast> for CatchAllVisitor<'a> {
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
        let mut variants_covered: Vec<String> = Vec::new();
        let mut has_wildcard = false;
        for arm in &e.arms {
            if let Some(v) = self.pattern_targets_variant(&arm.pat) {
                if !variants_covered.contains(&v) {
                    variants_covered.push(v);
                }
            }
            if Self::is_wildcard(&arm.pat) {
                has_wildcard = true;
            }
            // `A | B | _` — treat as wildcard too.
            if let syn::Pat::Or(o) = &arm.pat {
                if o.cases.iter().any(Self::is_wildcard) {
                    has_wildcard = true;
                }
                for c in &o.cases {
                    if let Some(v) = self.pattern_targets_variant(c) {
                        if !variants_covered.contains(&v) {
                            variants_covered.push(v);
                        }
                    }
                }
            }
        }
        if !variants_covered.is_empty() && has_wildcard {
            let ctx = self.enclosing();
            self.hits.push(Hit {
                file: self.file.to_string(),
                line: line_of(&e.match_token),
                context: ctx,
                variants_covered,
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
    // Collect variant names from the enum definition(s).
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
    if variant_names.is_empty() {
        if !index.knows_name(enum_name) {
            eprintln!("no enum named `{}` found in scanned tree", enum_name);
            return Ok(());
        }
    }

    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = CatchAllVisitor {
            target_enum: enum_name,
            variant_names: &variant_names,
            file: &display_path(&f.path),
            module: &f.module,
            fn_stack: Vec::new(),
            impl_stack: Vec::new(),
            mod_stack: Vec::new(),
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }

    all.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));

    if !summary {
        for h in &all {
            println!(
                "{}\t{}\t{}:{}",
                h.context,
                h.variants_covered.join(","),
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
