use syn::visit::{self, Visit};

use crate::ast::{line_of, type_short, ScopeTracker};
use crate::context::AnalysisCtx;
use crate::parse::display_path;

#[derive(Debug, Clone)]
struct VariantDef {
    name: String,
    shape: &'static str, // "unit" | "tuple" | "struct"
    file: String,
    line: usize,
}

#[derive(Debug)]
struct Site {
    kind: &'static str, // "ctor" | "match"
    variant: String,
    file: String,
    line: usize,
    context: String,
}

struct VariantDefVisitor<'a> {
    target_enum: &'a str,
    file: &'a str,
    out: Vec<VariantDef>,
}

impl<'ast, 'a> Visit<'ast> for VariantDefVisitor<'a> {
    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        if i.ident == self.target_enum {
            for v in &i.variants {
                let shape = match &v.fields {
                    syn::Fields::Unit => "unit",
                    syn::Fields::Unnamed(_) => "tuple",
                    syn::Fields::Named(_) => "struct",
                };
                self.out.push(VariantDef {
                    name: v.ident.to_string(),
                    shape,
                    file: self.file.to_string(),
                    line: line_of(&v.ident),
                });
            }
        }
    }
}

struct SiteVisitor<'a> {
    target_enum: &'a str,
    variant_names: &'a [String],
    bare: bool,
    file: &'a str,
    scope: ScopeTracker,
    in_pat: bool,
    sites: Vec<Site>,
}

impl<'a> SiteVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    fn match_path(&self, p: &syn::Path) -> Option<String> {
        let segs: Vec<&syn::PathSegment> = p.segments.iter().collect();
        if segs.is_empty() {
            return None;
        }
        let last = segs[segs.len() - 1].ident.to_string();
        if !self.variant_names.iter().any(|v| v == &last) {
            return None;
        }
        if segs.len() >= 2 {
            let pen = segs[segs.len() - 2].ident.to_string();
            if pen == self.target_enum {
                return Some(last);
            }
        }
        if self.bare && segs.len() == 1 {
            return Some(last);
        }
        None
    }

    fn record(&mut self, kind: &'static str, variant: String, line: usize) {
        let ctx = self.enclosing();
        self.sites.push(Site {
            kind,
            variant,
            file: self.file.to_string(),
            line,
            context: ctx,
        });
    }

    fn line_of_path(p: &syn::Path) -> usize {
        p.segments
            .last()
            .map(|s| line_of(&s.ident))
            .unwrap_or(0)
    }
}

impl<'ast, 'a> Visit<'ast> for SiteVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.scope.enter_mod(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.scope.leave_mod();
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.scope.enter_fn(i.sig.ident.to_string());
        visit::visit_item_fn(self, i);
        self.scope.leave_fn();
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.scope.enter_impl(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.scope.leave_impl();
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.scope.enter_fn(i.sig.ident.to_string());
        visit::visit_impl_item_fn(self, i);
        self.scope.leave_fn();
    }

    fn visit_arm(&mut self, a: &'ast syn::Arm) {
        let saved = self.in_pat;
        self.in_pat = true;
        self.visit_pat(&a.pat);
        self.in_pat = saved;
        if let Some((_, g)) = &a.guard {
            self.visit_expr(g);
        }
        self.visit_expr(&a.body);
    }

    fn visit_local(&mut self, l: &'ast syn::Local) {
        let saved = self.in_pat;
        self.in_pat = true;
        self.visit_pat(&l.pat);
        self.in_pat = saved;
        if let Some(init) = &l.init {
            self.visit_expr(&init.expr);
            if let Some((_, els)) = &init.diverge {
                self.visit_expr(els);
            }
        }
    }

    fn visit_expr_let(&mut self, e: &'ast syn::ExprLet) {
        let saved = self.in_pat;
        self.in_pat = true;
        self.visit_pat(&e.pat);
        self.in_pat = saved;
        self.visit_expr(&e.expr);
    }

    fn visit_pat_tuple_struct(&mut self, p: &'ast syn::PatTupleStruct) {
        if let Some(v) = self.match_path(&p.path) {
            self.record("match", v, Self::line_of_path(&p.path));
        }
        for el in &p.elems {
            self.visit_pat(el);
        }
    }

    fn visit_pat_struct(&mut self, p: &'ast syn::PatStruct) {
        if let Some(v) = self.match_path(&p.path) {
            self.record("match", v, Self::line_of_path(&p.path));
        }
        for f in &p.fields {
            self.visit_pat(&f.pat);
        }
    }

    fn visit_expr_path(&mut self, e: &'ast syn::ExprPath) {
        if let Some(v) = self.match_path(&e.path) {
            let kind = if self.in_pat { "match" } else { "ctor" };
            self.record(kind, v, Self::line_of_path(&e.path));
        }
        visit::visit_expr_path(self, e);
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            if let Some(v) = self.match_path(&p.path) {
                self.record("ctor", v, Self::line_of_path(&p.path));
                for arg in &e.args {
                    self.visit_expr(arg);
                }
                return;
            }
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        if !self.in_pat {
            if let Some(v) = self.match_path(&e.path) {
                self.record("ctor", v, Self::line_of_path(&e.path));
            }
        }
        visit::visit_expr_struct(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        // Skip macros encountered inside a pattern context (e.g. `Pat::Macro`):
        // their tokens are pattern syntax, not expressions.
        if self.in_pat {
            return;
        }
        match crate::macro_scan::macro_body(m) {
            crate::macro_scan::Body::Exprs(es) => {
                for e in es {
                    self.visit_expr(&e);
                }
            }
            crate::macro_scan::Body::Matches { scrutinee, pat } => {
                self.visit_expr(&scrutinee);
                let saved = self.in_pat;
                self.in_pat = true;
                self.visit_pat(&pat);
                self.in_pat = saved;
            }
        }
    }
}

pub fn run(ctx: &AnalysisCtx, enum_name: &str, bare: bool) -> anyhow::Result<()> {
    let files = ctx.files;
    let summary = ctx.summary;
    let mut defs: Vec<VariantDef> = Vec::new();
    for f in files {
        let mut v = VariantDefVisitor {
            target_enum: enum_name,
            file: &display_path(&f.path),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        defs.extend(v.out);
    }
    if defs.is_empty() {
        eprintln!("no enum named `{}` found", enum_name);
        return Ok(());
    }

    let variant_names: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();

    if !summary {
        for d in &defs {
            println!(
                "def\t{}::{}\t{}\t{}:{}",
                enum_name, d.name, d.shape, d.file, d.line
            );
        }
    }

    let mut sites: Vec<Site> = Vec::new();
    for f in files {
        let mut v = SiteVisitor {
            target_enum: enum_name,
            variant_names: &variant_names,
            bare,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()),
            in_pat: false,
            sites: Vec::new(),
        };
        v.visit_file(&f.ast);
        sites.extend(v.sites);
    }

    sites.sort_by(|a, b| {
        a.variant
            .cmp(&b.variant)
            .then_with(|| a.kind.cmp(b.kind))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    if !summary {
        for s in &sites {
            println!(
                "{}\t{}::{}\t{}\t{}:{}",
                s.kind, enum_name, s.variant, s.context, s.file, s.line
            );
        }
    }

    let mut ctors = 0usize;
    let mut matches = 0usize;
    for s in &sites {
        if s.kind == "ctor" {
            ctors += 1;
        } else {
            matches += 1;
        }
    }
    eprintln!(
        "({} variants; {} ctor sites, {} match sites; bare={})",
        defs.len(),
        ctors,
        matches,
        bare
    );
    Ok(())
}
