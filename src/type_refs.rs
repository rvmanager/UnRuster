use syn::visit::{self, Visit};

use crate::ast::{line_of, path_to_string_with_args, type_short, ScopeTracker};
use crate::context::AnalysisCtx;
use crate::parse::display_path;

#[derive(Debug)]
struct Ref {
    file: String,
    line: usize,
    context: String,
    role: &'static str, // "type" | "ctor"
    written: String,    // path as written, e.g. `crate::doc::Document`
    matched_via: &'static str, // "name" | "alias"
}

struct RefVisitor<'a> {
    targets: &'a [String], // primary name + all alias-equivalent names
    primary: &'a str,
    file: &'a str,
    scope: ScopeTracker,
    out: Vec<Ref>,
}

impl<'a> RefVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    fn matches_path_last(&self, p: &syn::Path) -> Option<&'static str> {
        let last = p.segments.last()?.ident.to_string();
        if last == self.primary {
            Some("name")
        } else if self.targets.iter().any(|t| t == &last) {
            Some("alias")
        } else {
            None
        }
    }

    fn record(&mut self, role: &'static str, written: String, line: usize, via: &'static str) {
        let ctx = self.enclosing();
        self.out.push(Ref {
            file: self.file.to_string(),
            line,
            context: ctx,
            role,
            written,
            matched_via: via,
        });
    }
}

impl<'ast, 'a> Visit<'ast> for RefVisitor<'a> {
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

    fn visit_type_path(&mut self, t: &'ast syn::TypePath) {
        if let Some(via) = self.matches_path_last(&t.path) {
            let line = t
                .path
                .segments
                .last()
                .map(|s| line_of(&s.ident))
                .unwrap_or(0);
            self.record("type", path_to_string_with_args(&t.path), line, via);
        }
        visit::visit_type_path(self, t);
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            let segs = &p.path.segments;
            // `Type(arg)` (tuple-struct ctor as call)
            if segs.len() == 1 {
                let id = &segs[0].ident.to_string();
                let via = if id == self.primary {
                    Some("name")
                } else if self.targets.iter().any(|t| t == id) {
                    Some("alias")
                } else {
                    None
                };
                if let Some(via) = via {
                    self.record(
                        "ctor",
                        path_to_string_with_args(&p.path),
                        line_of(&segs[0].ident),
                        via,
                    );
                }
            } else if segs.len() >= 2 {
                let pen = &segs[segs.len() - 2].ident.to_string();
                let via = if pen == self.primary {
                    Some("name")
                } else if self.targets.iter().any(|t| t == pen) {
                    Some("alias")
                } else {
                    None
                };
                if let Some(via) = via {
                    self.record(
                        "ctor",
                        path_to_string_with_args(&p.path),
                        line_of(&segs[segs.len() - 2].ident),
                        via,
                    );
                }
            }
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        if let Some(via) = self.matches_path_last(&e.path) {
            let line = e
                .path
                .segments
                .last()
                .map(|s| line_of(&s.ident))
                .unwrap_or(0);
            self.record("ctor", path_to_string_with_args(&e.path), line, via);
        }
        visit::visit_expr_struct(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

pub fn run(ctx: &AnalysisCtx, ty: &str) -> anyhow::Result<()> {
    let files = ctx.files;
    let index = ctx.idx;
    let aliases = &ctx.sem.aliases;
    let summary = ctx.summary;
    if !index.knows_name(ty) {
        eprintln!(
            "note: `{}` is not a known struct/enum/trait/type-alias in this tree; \
             reporting matches anyway",
            ty
        );
    }

    let targets = aliases.synonyms(ty);
    if targets.len() > 1 {
        eprintln!(
            "note: also matching alias-equivalent names: {}",
            targets
                .iter()
                .filter(|n| n.as_str() != ty)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let mut all: Vec<Ref> = Vec::new();
    for f in files {
        let mut v = RefVisitor {
            targets: &targets,
            primary: ty,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.out);
    }

    all.sort_by(|a, b| {
        a.role
            .cmp(b.role)
            .then_with(|| a.matched_via.cmp(b.matched_via))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    let mut alias_hits = 0usize;
    if !summary {
        for r in &all {
            if r.matched_via == "alias" {
                alias_hits += 1;
            }
            println!(
                "{}\t{}\t{}\t{}\t{}:{}",
                r.role, r.matched_via, r.written, r.context, r.file, r.line
            );
        }
    } else {
        alias_hits = all.iter().filter(|r| r.matched_via == "alias").count();
    }

    let mut by_module = std::collections::BTreeMap::<String, usize>::new();
    for r in &all {
        let module_of = r
            .context
            .split("::")
            .take_while(|s| s.chars().next().map(|c| !c.is_ascii_uppercase()).unwrap_or(true))
            .collect::<Vec<_>>()
            .join("::");
        *by_module.entry(module_of).or_default() += 1;
    }
    eprintln!(
        "({} reference(s) across {} module(s); {} via alias)",
        all.len(),
        by_module.len(),
        alias_hits
    );
    Ok(())
}
