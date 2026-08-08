use syn::visit::{self, Visit};

use crate::ast::{line_of, path_to_string_with_args, scope_visits, ScopeTracker};
use crate::context::{AnalysisCtx, Confidence, TargetNotFound};
use crate::parse::display_path;
use crate::emit::{row, site};

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

    /// How `name` matches the queried type: directly, via a type alias, or
    /// not at all. Single implementation for type positions and ctor paths.
    fn via_of(&self, name: &str) -> Option<&'static str> {
        if name == self.primary {
            Some("name")
        } else if self.targets.iter().any(|t| t == name) {
            Some("alias")
        } else {
            None
        }
    }

    fn matches_path_last(&self, p: &syn::Path) -> Option<&'static str> {
        let last = p.segments.last()?.ident.to_string();
        self.via_of(&last)
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
    scope_visits!(item_mod, item_impl, item_trait, item_fn, impl_item_fn, trait_item_fn);

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
            let segs: Vec<_> = p.path.segments.iter().collect();
            // Ctor position: the sole segment (`Type(arg)` tuple-struct call)
            // or the penultimate one (`Type::new(..)` associated ctor).
            let ctor_seg = match segs.len() {
                0 => None,
                1 => Some(segs[0]),
                n => Some(segs[n - 2]),
            };
            if let Some(seg) = ctor_seg {
                if let Some(via) = self.via_of(&seg.ident.to_string()) {
                    self.record(
                        "ctor",
                        path_to_string_with_args(&p.path),
                        line_of(&seg.ident),
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

pub fn run(
    ctx: &AnalysisCtx,
    ty: &str,
    min_confidence: Option<Confidence>,
) -> anyhow::Result<usize> {
    let files = ctx.files;
    let index = ctx.idx;
    let aliases = &ctx.sem.aliases;
    let summary = ctx.summary;
    let known = index.knows_name(ty);
    if !known {
        ctx.warn_unknown("type", ty);
    }

    let targets = aliases.synonyms(ty);
    if targets.len() > 1 {
        ctx.out.note(&format!(
            "note: also matching alias-equivalent names: {}",
            targets
                .iter()
                .filter(|n| n.as_str() != ty)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // A name with exactly one definition in the tree can't be confused with
    // a same-named type elsewhere — its matches are resolved, not heuristic.
    let unique_name = index
        .lookup(ty)
        .iter()
        .filter(|d| matches!(d.kind, "struct" | "enum" | "trait" | "type"))
        .count()
        == 1;
    let conf_of = |via: &str| -> Confidence {
        match via {
            "alias" => Confidence::Inferred,
            _ if unique_name => Confidence::Resolved,
            _ => Confidence::Heuristic,
        }
    };

    let mut all: Vec<Ref> = Vec::new();
    for f in files {
        let mut v = RefVisitor {
            targets: &targets,
            primary: ty,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.out);
    }

    if let Some(min) = min_confidence {
        all.retain(|r| conf_of(r.matched_via) >= min);
    }
    ctx.retain_changed(&mut all, |r| &r.file);
    all.sort_by(|a, b| {
        a.role
            .cmp(b.role)
            .then_with(|| a.matched_via.cmp(b.matched_via))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    // Every summary statistic is taken from the full result set, before the
    // cap: `--top 5` bounds what is listed, not what was found. Counting
    // inside the print loop (as `alias_hits` used to) makes the two disagree
    // the moment a cap is applied — and `--summary` already had its own
    // second copy of the count for exactly that reason.
    let total = all.len();
    let alias_hits = all.iter().filter(|r| r.matched_via == "alias").count();
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
    if !summary {
        for r in &all {
            row!(
                ctx.out,
                "role" => r.role,
                "via" => r.matched_via,
                "confidence" => conf_of(r.matched_via).as_str(),
                "written" => r.written.clone(),
                "context" => r.context.clone(),
                "at" => site(&r.file, r.line),
            );
        }
    }
    ctx.out.summary(&format!(
        "({} reference(s) across {} module(s); {} via alias)",
        total,
        by_module.len(),
        alias_hits
    ));
    if !known && total == 0 {
        return Err(TargetNotFound::err("type", ty));
    }
    Ok(total)
}
