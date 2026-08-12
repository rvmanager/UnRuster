use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{line_of, scope_visits, ScopeTracker};
use crate::context::AnalysisCtx;
use crate::parse::display_path;
use crate::emit::row;

#[derive(Debug)]
struct FnMetric {
    qpath: String,
    file: String,
    line: usize,
    /// Last line of the body. Already computed to derive `loc`; kept so
    /// `--spans` can report `file:start-end` instead of making the reader
    /// re-derive it from `loc:`.
    end: usize,
    loc: usize,
    params: usize,
    cyclo: usize,
    nesting: usize,
}

#[derive(Debug)]
struct StructMetric {
    qpath: String,
    file: String,
    line: usize,
    end: usize,
    fields: usize,
}

#[derive(Debug)]
struct EnumMetric {
    qpath: String,
    file: String,
    line: usize,
    end: usize,
    variants: usize,
}

struct MetricsVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    fns: &'a mut Vec<FnMetric>,
    structs: &'a mut Vec<StructMetric>,
    enums: &'a mut Vec<EnumMetric>,
}

impl<'a> MetricsVisitor<'a> {
    fn qualify(&self, name: &str) -> String {
        self.scope.qualify(name)
    }

    fn record_fn(&mut self, sig: &syn::Signature, body: &syn::Block) {
        let start = line_of(&sig.ident);
        let end = body.span().end().line.max(start);
        let loc = end.saturating_sub(start) + 1;
        let params = sig
            .inputs
            .iter()
            .filter(|i| !matches!(i, syn::FnArg::Receiver(_)))
            .count();
        let (cyclo, nesting) = compute_complexity(body);
        let qpath = self.qualify(&sig.ident.to_string());
        self.fns.push(FnMetric {
            qpath,
            file: self.file.to_string(),
            line: start,
            end,
            loc,
            params,
            cyclo,
            nesting,
        });
    }
}

impl<'ast, 'a> Visit<'ast> for MetricsVisitor<'a> {
    scope_visits!(item_mod, item_impl, item_trait);


    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.record_fn(&i.sig, &i.block);
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.record_fn(&i.sig, &i.block);
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        // Trait default-method bodies are measurable fns like any other.
        if let Some(body) = &i.default {
            self.record_fn(&i.sig, body);
        }
    }

    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        let fields = match &i.fields {
            syn::Fields::Named(n) => n.named.len(),
            syn::Fields::Unnamed(u) => u.unnamed.len(),
            syn::Fields::Unit => 0,
        };
        let qpath = self.qualify(&i.ident.to_string());
        let line = line_of(&i.ident);
        self.structs.push(StructMetric {
            qpath,
            file: self.file.to_string(),
            line,
            end: i.span().end().line.max(line),
            fields,
        });
    }

    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        let qpath = self.qualify(&i.ident.to_string());
        let line = line_of(&i.ident);
        self.enums.push(EnumMetric {
            qpath,
            file: self.file.to_string(),
            line,
            end: i.span().end().line.max(line),
            variants: i.variants.len(),
        });
    }
}

// ─── complexity ────────────────────────────────────────────────────────────

/// Cyclomatic complexity = 1 + N(decision points). Decision points:
/// - each `if` / `else if` (one per ExprIf node)
/// - each non-wildcard match arm
/// - each `while`, `while let`, `for`, `loop`
/// - each `&&` / `||` in expressions
/// - each `?` operator
///
/// Nesting depth = max depth of nested control-flow bodies (if / else / match
/// arm / while / for / loop). Closures and plain blocks don't count.
fn compute_complexity(body: &syn::Block) -> (usize, usize) {
    let mut v = ComplexityVisitor {
        cyclo: 1, // base path
        nest_current: 0,
        nest_max: 0,
    };
    v.visit_block(body);
    (v.cyclo, v.nest_max)
}

struct ComplexityVisitor {
    cyclo: usize,
    nest_current: usize,
    nest_max: usize,
}

impl ComplexityVisitor {
    /// One branch, one nesting level, then the head expression and the body —
    /// the shape `while` and `for` share.
    ///
    /// `near-clones` reported the two as a one-leaf divergence (`cond` against
    /// `expr`) and it was right: the difference is only which syn field holds
    /// the head, and Rust spells that differently for the two loops. `loop`
    /// keeps its own method because it genuinely has no head.
    fn loop_with_head(&mut self, head: &syn::Expr, body: &syn::Block) {
        self.cyclo += 1;
        self.enter();
        self.visit_expr(head);
        for stmt in &body.stmts {
            self.visit_stmt(stmt);
        }
        self.exit();
    }

    fn enter(&mut self) {
        self.nest_current += 1;
        if self.nest_current > self.nest_max {
            self.nest_max = self.nest_current;
        }
    }
    fn exit(&mut self) {
        self.nest_current = self.nest_current.saturating_sub(1);
    }

    fn is_wildcard_pat(p: &syn::Pat) -> bool {
        matches!(p, syn::Pat::Wild(_))
            || matches!(p, syn::Pat::Ident(i) if i.subpat.is_none())
    }
}

impl<'ast> Visit<'ast> for ComplexityVisitor {
    fn visit_expr_if(&mut self, e: &'ast syn::ExprIf) {
        self.cyclo += 1;
        self.enter();
        self.visit_expr(&e.cond);
        for stmt in &e.then_branch.stmts {
            self.visit_stmt(stmt);
        }
        if let Some((_, else_expr)) = &e.else_branch {
            self.visit_expr(else_expr);
        }
        self.exit();
    }

    fn visit_expr_match(&mut self, e: &'ast syn::ExprMatch) {
        for arm in &e.arms {
            if !Self::is_wildcard_pat(&arm.pat) {
                self.cyclo += 1;
            }
        }
        self.enter();
        self.visit_expr(&e.expr);
        for arm in &e.arms {
            if let Some((_, g)) = &arm.guard {
                self.cyclo += 1;
                self.visit_expr(g);
            }
            self.visit_expr(&arm.body);
        }
        self.exit();
    }

    fn visit_expr_while(&mut self, e: &'ast syn::ExprWhile) {
        self.loop_with_head(&e.cond, &e.body);
    }

    fn visit_expr_for_loop(&mut self, e: &'ast syn::ExprForLoop) {
        self.loop_with_head(&e.expr, &e.body);
    }

    fn visit_expr_loop(&mut self, e: &'ast syn::ExprLoop) {
        self.cyclo += 1;
        self.enter();
        for stmt in &e.body.stmts {
            self.visit_stmt(stmt);
        }
        self.exit();
    }

    fn visit_expr_binary(&mut self, e: &'ast syn::ExprBinary) {
        if matches!(e.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) {
            self.cyclo += 1;
        }
        visit::visit_expr_binary(self, e);
    }

    fn visit_expr_try(&mut self, e: &'ast syn::ExprTry) {
        self.cyclo += 1;
        visit::visit_expr_try(self, e);
    }

    // Decision points inside macro args (`&&`, `?`, `if` in a `format!`)
    // count too — flagged by `cohort-callees visit_macro`: every sibling
    // scanner walks macro bodies.
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

// ─── run ────────────────────────────────────────────────────────────────────

/// `--sort` key for the fn table. Parsed by clap (value_enum).
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum SortKey {
    Loc,
    Params,
    Cyclo,
    Nesting,
}

impl SortKey {
    fn as_str(self) -> &'static str {
        match self {
            SortKey::Loc => "loc",
            SortKey::Params => "params",
            SortKey::Cyclo => "cyclo",
            SortKey::Nesting => "nesting",
        }
    }
}

/// Drop fns whose sort metric is below the threshold.
fn apply_threshold(fns: &mut Vec<FnMetric>, sort: SortKey, t: usize) {
    match sort {
        SortKey::Loc => fns.retain(|m| m.loc >= t),
        SortKey::Params => fns.retain(|m| m.params >= t),
        SortKey::Cyclo => fns.retain(|m| m.cyclo >= t),
        SortKey::Nesting => fns.retain(|m| m.nesting >= t),
    }
}

/// Sort the fn table by the key, descending, with stable tie-breakers.
fn sort_fns(fns: &mut [FnMetric], sort: SortKey) {
    match sort {
        SortKey::Loc => fns.sort_by(|a, b| b.loc.cmp(&a.loc).then_with(|| b.cyclo.cmp(&a.cyclo))),
        SortKey::Params => {
            fns.sort_by(|a, b| b.params.cmp(&a.params).then_with(|| b.loc.cmp(&a.loc)))
        }
        SortKey::Cyclo => fns.sort_by(|a, b| {
            b.cyclo
                .cmp(&a.cyclo)
                .then_with(|| b.nesting.cmp(&a.nesting))
                .then_with(|| b.loc.cmp(&a.loc))
        }),
        SortKey::Nesting => fns.sort_by(|a, b| {
            b.nesting
                .cmp(&a.nesting)
                .then_with(|| b.cyclo.cmp(&a.cyclo))
                .then_with(|| b.loc.cmp(&a.loc))
        }),
    }
}

pub fn run(
    ctx: &AnalysisCtx,
    sort: SortKey,
    threshold: Option<usize>,
    fns_only: bool,
) -> anyhow::Result<usize> {
    let files = ctx.files;
    let summary = ctx.summary;
    let mut fns: Vec<FnMetric> = Vec::new();
    let mut structs: Vec<StructMetric> = Vec::new();
    let mut enums: Vec<EnumMetric> = Vec::new();
    for f in files {
        let mut v = MetricsVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()),
            fns: &mut fns,
            structs: &mut structs,
            enums: &mut enums,
        };
        v.visit_file(&f.ast);
    }

    ctx.retain_changed(&mut fns, |m| &m.file);
    ctx.retain_changed(&mut structs, |m| &m.file);
    ctx.retain_changed(&mut enums, |m| &m.file);

    if let Some(t) = threshold {
        apply_threshold(&mut fns, sort, t);
    }
    sort_fns(&mut fns, sort);
    structs.sort_by_key(|s| std::cmp::Reverse(s.fields));
    enums.sort_by_key(|e| std::cmp::Reverse(e.variants));

    if !summary {
        for m in fns.iter() {
            row!(
                ctx.out,
                "kind" => "fn",
                "loc" => format!("loc:{}", m.loc),
                "params" => format!("params:{}", m.params),
                "cyclo" => format!("cyclo:{}", m.cyclo),
                "nesting" => format!("nesting:{}", m.nesting),
                "qpath" => m.qpath.clone(),
                "at" => ctx.at(&m.file, m.line, m.end),
            );
        }
        for m in structs.iter().take(if fns_only { 0 } else { usize::MAX }) {
            row!(
                ctx.out,
                "kind" => "struct",
                "fields" => format!("fields:{}", m.fields),
                "qpath" => m.qpath.clone(),
                "at" => ctx.at(&m.file, m.line, m.end),
            );
        }
        for m in enums.iter().take(if fns_only { 0 } else { usize::MAX }) {
            row!(
                ctx.out,
                "kind" => "enum",
                "variants" => format!("variants:{}", m.variants),
                "qpath" => m.qpath.clone(),
                "at" => ctx.at(&m.file, m.line, m.end),
            );
        }
    }

    ctx.out.summary(&format!(
        "({} fns, {} structs, {} enums; sort={}{})",
        fns.len(),
        structs.len(),
        enums.len(),
        sort.as_str(),
        threshold
            .map(|t| format!("; threshold={}", t))
            // unruster: ok(error-swallows/.unwrap_or_default) 2026-08-06 — `threshold` is an
            // Option<usize> flag; an absent flag renders as the empty string by design.
            .unwrap_or_default()
    ));
    // With --threshold the fn table is the findings set; otherwise everything shown counts.
    Ok(if threshold.is_some() || fns_only {
        fns.len()
    } else {
        fns.len() + structs.len() + enums.len()
    })
}
