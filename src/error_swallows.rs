use syn::visit::{self, Visit};

use crate::ast::{fn_span, trait_fn_span, line_of, line_of_span, type_short, ScopeTracker};
use crate::context::AnalysisCtx;
use crate::parse::display_path;

#[derive(Debug)]
struct Hit {
    /// Method-call swallows:
    ///   ".ok" | ".err" | ".unwrap_or_default" | ".unwrap_or_else" |
    ///   ".unwrap_or" | ".map_err(|_|...)"
    /// Syntactic swallows:
    ///   "match-err-wild" | "if-let-ok" | "while-let-ok" | "let-_"
    kind: &'static str,
    file: String,
    line: usize,
    context: String,
}

struct SwallowVisitor<'a> {
    include_unwrap_or: bool,
    file: &'a str,
    scope: ScopeTracker,
    hits: Vec<Hit>,
}

impl<'a> SwallowVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    fn record(&mut self, kind: &'static str, line: usize) {
        let ctx = self.enclosing();
        self.hits.push(Hit {
            kind,
            file: self.file.to_string(),
            line,
            context: ctx,
        });
    }
}

/// True for `_` and underscore-prefixed bindings (`_`, `_e`, `_err`) — the
/// convention for "intentionally discarded." A bare `e` returns false because
/// it may be referenced in the body.
fn pat_is_discarded(p: &syn::Pat) -> bool {
    match p {
        syn::Pat::Wild(_) => true,
        syn::Pat::Ident(i) => {
            i.subpat.is_none() && i.ident.to_string().starts_with('_')
        }
        syn::Pat::Reference(r) => pat_is_discarded(&r.pat),
        syn::Pat::Paren(p) => pat_is_discarded(&p.pat),
        _ => false,
    }
}

/// `Err(_)` / `Err(_e)` — the error contents are discarded by the pattern.
/// `Err(e)` is NOT flagged because the body may reference `e`.
fn pat_is_err_swallow(p: &syn::Pat) -> bool {
    match p {
        syn::Pat::TupleStruct(ts) => {
            let last = ts
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default();
            last == "Err" && ts.elems.iter().all(pat_is_discarded)
        }
        syn::Pat::Or(o) => o.cases.iter().any(pat_is_err_swallow),
        syn::Pat::Reference(r) => pat_is_err_swallow(&r.pat),
        syn::Pat::Paren(p) => pat_is_err_swallow(&p.pat),
        _ => false,
    }
}

/// `Ok(_)` / `Ok(x)` head — used to identify if-let-ok / while-let-ok forms.
fn pat_is_ok(p: &syn::Pat) -> bool {
    match p {
        syn::Pat::TupleStruct(ts) => ts
            .path
            .segments
            .last()
            .map(|s| s.ident == "Ok")
            .unwrap_or(false),
        syn::Pat::Reference(r) => pat_is_ok(&r.pat),
        syn::Pat::Paren(p) => pat_is_ok(&p.pat),
        _ => false,
    }
}

impl<'ast, 'a> Visit<'ast> for SwallowVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.scope.enter_mod(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.scope.leave_mod();
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.scope
            .enter_fn(i.sig.ident.to_string(), fn_span(&i.sig, &i.block));
        visit::visit_item_fn(self, i);
        self.scope.leave_fn();
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.scope.enter_impl(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.scope.leave_impl();
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.scope
            .enter_fn(i.sig.ident.to_string(), fn_span(&i.sig, &i.block));
        visit::visit_impl_item_fn(self, i);
        self.scope.leave_fn();
    }
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.scope.enter_trait(i.ident.to_string());
        visit::visit_item_trait(self, i);
        self.scope.leave_trait();
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.scope
            .enter_fn(i.sig.ident.to_string(), trait_fn_span(i));
        visit::visit_trait_item_fn(self, i);
        self.scope.leave_fn();
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let m = e.method.to_string();
        let kind: Option<&'static str> = match m.as_str() {
            "ok" if e.args.is_empty() => Some(".ok"),
            "err" if e.args.is_empty() => Some(".err"),
            "unwrap_or_default" if e.args.is_empty() => Some(".unwrap_or_default"),
            "unwrap_or_else" => Some(".unwrap_or_else"),
            "unwrap_or" if self.include_unwrap_or => Some(".unwrap_or"),
            "map_err" => {
                // Flag only when the closure's first arg is `_` or `_name` —
                // the error contents are intentionally discarded.
                let mut discarded = false;
                if let Some(syn::Expr::Closure(c)) = e.args.first() {
                    if let Some(first) = c.inputs.first() {
                        discarded = pat_is_discarded(first);
                    }
                }
                if discarded {
                    Some(".map_err(|_|)")
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(k) = kind {
            self.record(k, line_of(&e.method));
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_match(&mut self, e: &'ast syn::ExprMatch) {
        for arm in &e.arms {
            if pat_is_err_swallow(&arm.pat) {
                let line = line_of_span(arm.fat_arrow_token.spans[0]);
                self.record("match-err-wild", line);
                break; // one report per match site
            }
        }
        visit::visit_expr_match(self, e);
    }

    fn visit_expr_if(&mut self, e: &'ast syn::ExprIf) {
        if e.else_branch.is_none() {
            if let syn::Expr::Let(le) = &*e.cond {
                if pat_is_ok(&le.pat) {
                    self.record("if-let-ok", line_of(&e.if_token));
                }
            }
        }
        visit::visit_expr_if(self, e);
    }

    fn visit_expr_while(&mut self, e: &'ast syn::ExprWhile) {
        if let syn::Expr::Let(le) = &*e.cond {
            if pat_is_ok(&le.pat) {
                self.record("while-let-ok", line_of(&e.while_token));
            }
        }
        visit::visit_expr_while(self, e);
    }

    fn visit_local(&mut self, l: &'ast syn::Local) {
        // `let _ = expr;` with init — explicit discard.
        let is_wild = match &l.pat {
            syn::Pat::Wild(_) => true,
            syn::Pat::Type(pt) => matches!(*pt.pat, syn::Pat::Wild(_)),
            _ => false,
        };
        if is_wild && l.init.is_some() {
            self.record("let-_", line_of(&l.let_token));
        }
        visit::visit_local(self, l);
    }
}

pub fn run(ctx: &AnalysisCtx, include_unwrap_or: bool) -> anyhow::Result<usize> {
    let files = ctx.files;
    let summary = ctx.summary;
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = SwallowVisitor {
            include_unwrap_or,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }
    ctx.retain_changed(&mut all, |h| &h.file);
    all.sort_by(|a, b| {
        a.kind
            .cmp(b.kind)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    if !summary {
        for h in &all {
            println!("{}\t{}\t{}:{}", h.kind, h.context, h.file, h.line);
            ctx.print_context(&h.file, h.line);
        }
    }
    use std::collections::BTreeMap;
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for h in &all {
        *by_kind.entry(h.kind).or_insert(0) += 1;
    }
    let breakdown: Vec<String> = by_kind
        .iter()
        .map(|(k, n)| format!("{}={}", k, n))
        .collect();
    eprintln!(
        "({} swallow site(s); {}; include_unwrap_or={}; explain: silent-fallbacks)",
        all.len(),
        breakdown.join(", "),
        include_unwrap_or
    );
    Ok(all.len())
}
