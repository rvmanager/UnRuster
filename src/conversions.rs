use syn::visit::{self, Visit};

use crate::ast::{line_of, print_grouped_counts, top_module_of, type_short, ScopeTracker};
use crate::context::AnalysisCtx;
use crate::parse::display_path;

#[derive(Debug)]
struct Hit {
    kind: &'static str,
    target: String, // dst type when visible (e.g. `::from` target, `::<T>` turbofish)
    context: String,
    file: String,
    line: usize,
}

struct ConvVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    hits: Vec<Hit>,
}

impl<'a> ConvVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    fn record(&mut self, kind: &'static str, target: String, line: usize) {
        let ctx = self.enclosing();
        self.hits.push(Hit {
            kind,
            target,
            context: ctx,
            file: self.file.to_string(),
            line,
        });
    }
}

fn first_is_uppercase(s: &str) -> bool {
    s.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
}

impl<'ast, 'a> Visit<'ast> for ConvVisitor<'a> {
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

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let m = e.method.to_string();
        let kind: Option<&'static str> = match m.as_str() {
            "into" if e.args.is_empty() => Some(".into"),
            "try_into" if e.args.is_empty() => Some(".try_into"),
            "to_string" if e.args.is_empty() => Some(".to_string"),
            "to_owned" if e.args.is_empty() => Some(".to_owned"),
            "to_vec" if e.args.is_empty() => Some(".to_vec"),
            "as_str" if e.args.is_empty() => Some(".as_str"),
            "as_bytes" if e.args.is_empty() => Some(".as_bytes"),
            "as_ref" if e.args.is_empty() => Some(".as_ref"),
            "as_mut" if e.args.is_empty() => Some(".as_mut"),
            "parse" if e.args.is_empty() => Some(".parse"),
            "cloned" if e.args.is_empty() => Some(".cloned"),
            "copied" if e.args.is_empty() => Some(".copied"),
            "collect" if e.args.is_empty() => Some(".collect"),
            _ => None,
        };
        if let Some(k) = kind {
            // Extract turbofish target if present.
            let target = e
                .turbofish
                .as_ref()
                .and_then(|t| t.args.first())
                .and_then(|arg| match arg {
                    syn::GenericArgument::Type(t) => Some(crate::ast::type_to_string(t)),
                    _ => None,
                })
                .unwrap_or_else(|| "_".into());
            self.record(k, target, line_of(&e.method));
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            let segs: Vec<&syn::PathSegment> = p.path.segments.iter().collect();
            if segs.len() >= 2 {
                let last = segs[segs.len() - 1].ident.to_string();
                let pen = segs[segs.len() - 2].ident.to_string();
                if first_is_uppercase(&pen) {
                    let kind = match last.as_str() {
                        "from" => Some("::from"),
                        "try_from" => Some("::try_from"),
                        _ => None,
                    };
                    if let Some(k) = kind {
                        self.record(k, pen, line_of(&segs[segs.len() - 1].ident));
                    }
                }
            }
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

pub fn run(
    ctx: &AnalysisCtx,
    kind_filter: Option<&str>,
    by: Option<&str>,
    top: Option<usize>,
) -> anyhow::Result<()> {
    let files = ctx.files;
    let summary = ctx.summary;
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = ConvVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()),
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }

    if let Some(k) = kind_filter {
        let wanted: Vec<&str> = k.split(',').map(str::trim).collect();
        all.retain(|h| wanted.contains(&h.kind));
    }

    if !summary {
        match by {
            Some("fn") => print_grouped_counts(&all, top, |h| h.context.clone()),
            Some("file") => print_grouped_counts(&all, top, |h| h.file.clone()),
            Some("module") => {
                print_grouped_counts(&all, top, |h| top_module_of(&h.context).to_string())
            }
            _ => {
                all.sort_by(|a, b| {
                    a.kind
                        .cmp(b.kind)
                        .then_with(|| a.file.cmp(&b.file))
                        .then_with(|| a.line.cmp(&b.line))
                });
                let rows: &[Hit] = if let Some(n) = top { &all[..all.len().min(n)] } else { &all };
                for h in rows {
                    println!("{}\t{}\t{}\t{}:{}", h.kind, h.target, h.context, h.file, h.line);
                }
            }
        }
    }

    use std::collections::BTreeMap;
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for h in &all {
        *by_kind.entry(h.kind).or_insert(0) += 1;
    }
    let break_str: Vec<String> = by_kind.iter().map(|(k, n)| format!("{}={}", k, n)).collect();
    eprintln!(
        "({} conversion call(s); {}; design-smell hint: a fn with many conversion calls is reshaping the same value repeatedly — usually a sign the wrong type was chosen at the boundary.)",
        all.len(),
        break_str.join(", ")
    );
    Ok(())
}
