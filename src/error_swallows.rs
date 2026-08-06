use syn::visit::{self, Visit};

use crate::ast::{fn_span, trait_fn_span, line_of, line_of_span, type_short, ScopeTracker};
use crate::context::AnalysisCtx;
use crate::parse::display_path;
use crate::emit::{row, site};

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
    /// True when the site is one of the two families that are idiomatic rather
    /// than defective: an infallible in-memory write, or a fallback that logs.
    /// Kept as a flag rather than dropped at scan time so the summary can say
    /// how many were filtered and `--include-*` can restore them.
    benign: Option<&'static str>,
}

struct SwallowVisitor<'a> {
    include_unwrap_or: bool,
    file: &'a str,
    scope: ScopeTracker,
    /// Whether the expression being visited sits in a closure's tail position.
    /// `filter_map(|t| t.parse().ok())` turns a Result into an Option *so the
    /// iterator can filter on it* — the error is the predicate, not something
    /// dropped. On one real codebase this idiom was most of the `.ok` bucket.
    closure_tail: Vec<bool>,
    hits: Vec<Hit>,
}

/// Macros whose `Result` is infallible when the target is an in-memory
/// `String`/`Vec` — `write!`/`writeln!` into a `fmt::Write` buffer cannot fail,
/// so `let _ = write!(s, …)` is the idiomatic spelling, not a swallowed error.
/// These dominated the `let-_` bucket on a real codebase (a large majority of
/// 116 rows) while producing no defects.
const INFALLIBLE_WRITE_MACROS: &[&str] = &["write", "writeln"];

/// Does this `let _ = …;` discard an infallible in-memory write?
fn is_infallible_write(init: &syn::Expr) -> bool {
    let syn::Expr::Macro(m) = init else {
        return false;
    };
    let Some(name) = m.mac.path.segments.last() else {
        return false;
    };
    INFALLIBLE_WRITE_MACROS.contains(&name.ident.to_string().as_str())
}

/// Does a fallback closure body make the failure observable — a log, a warn, a
/// debug macro, an `eprintln!`? `\u{2e}unwrap_or_else(|| { log!(…); default })`
/// is a *handled* fallback: the error was noticed and a policy applied. Rows
/// like these were ~half the `.unwrap_or_else` bucket and none were defects.
fn fallback_is_logged(e: &syn::ExprMethodCall) -> bool {
    struct V {
        found: bool,
    }
    impl<'ast> Visit<'ast> for V {
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            if let Some(seg) = m.path.segments.last() {
                let n = seg.ident.to_string().to_lowercase();
                // Match by name shape rather than an allow-list: every project
                // spells its logger differently (`dbg_log`, `macos_warn`,
                // `tracing::warn`), and an allow-list would silently fail on
                // the next one.
                if n.contains("log")
                    || n.contains("warn")
                    || n.contains("err")
                    || n.contains("trace")
                    || n.contains("debug")
                    || n.contains("panic")
                    || n.starts_with("eprint")
                {
                    self.found = true;
                }
            }
            visit::visit_macro(self, m);
        }
        fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
            let n = c.method.to_string().to_lowercase();
            if n.contains("log") || n.contains("warn") || n.contains("report") {
                self.found = true;
            }
            visit::visit_expr_method_call(self, c);
        }
    }
    let mut v = V { found: false };
    for a in &e.args {
        v.visit_expr(a);
    }
    v.found
}

impl<'a> SwallowVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    fn record(&mut self, kind: &'static str, line: usize) {
        self.record_tagged(kind, line, None);
    }

    fn record_tagged(&mut self, kind: &'static str, line: usize, benign: Option<&'static str>) {
        let ctx = self.enclosing();
        self.hits.push(Hit {
            kind,
            file: self.file.to_string(),
            line,
            context: ctx,
            benign,
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

/// `.map_err(|_| …)` / `.map_err(|_e| …)` — the closure's first arg is a
/// discard binding, so the error contents are intentionally dropped.
fn map_err_discards(e: &syn::ExprMethodCall) -> bool {
    let Some(syn::Expr::Closure(c)) = e.args.first() else {
        return false;
    };
    c.inputs.first().map(pat_is_discarded).unwrap_or(false)
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
            "map_err" if map_err_discards(e) => Some(".map_err(|_|)"),
            _ => None,
        };
        if let Some(k) = kind {
            let benign = if k == ".unwrap_or_else" && fallback_is_logged(e) {
                Some("logged-fallback")
            } else if k == ".ok" && self.closure_tail.last().copied().unwrap_or(false) {
                Some("combinator-ok")
            } else {
                None
            };
            self.record_tagged(k, line_of(&e.method), benign);
        }
        // A closure passed as an argument is not in *this* call's tail slot.
        self.closure_tail.push(false);
        visit::visit_expr_method_call(self, e);
        self.closure_tail.pop();
    }

    fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
        self.closure_tail.push(true);
        visit::visit_expr_closure(self, c);
        self.closure_tail.pop();
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

    // Every sibling site-scanner walks macro bodies; without this, swallows
    // inside macro args (e.g. `.ok()` in a `writeln!`) were invisible —
    // flagged by `cohort-callees visit_macro`.
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }

    fn visit_local(&mut self, l: &'ast syn::Local) {
        // `let _ = expr;` with init — explicit discard.
        let is_wild = match &l.pat {
            syn::Pat::Wild(_) => true,
            syn::Pat::Type(pt) => matches!(*pt.pat, syn::Pat::Wild(_)),
            _ => false,
        };
        if is_wild {
            if let Some(init) = &l.init {
                let benign = if is_infallible_write(&init.expr) {
                    Some("infallible-write")
                } else {
                    None
                };
                self.record_tagged("let-_", line_of(&l.let_token), benign);
            }
        }
        visit::visit_local(self, l);
    }
}

/// Which families of swallow site to report.
#[derive(Clone, Copy)]
pub struct SwallowOpts {
    /// `.unwrap_or(…)` with any argument. Noisy; off by default.
    pub include_unwrap_or: bool,
    /// `let _ = write!(buf, …)` into an in-memory buffer.
    pub include_infallible: bool,
    /// `.unwrap_or_else(|| { log!(…); default })` — failure already observable.
    pub include_logged: bool,
}

impl Default for SwallowOpts {
    /// The bare `error-swallows` command keeps every family: the dedicated
    /// command is where someone goes to see everything. `audit` opts out of
    /// the benign families, since it is read for defects.
    fn default() -> Self {
        SwallowOpts {
            include_unwrap_or: false,
            include_infallible: true,
            include_logged: true,
        }
    }
}

pub fn run(ctx: &AnalysisCtx, opts: SwallowOpts) -> anyhow::Result<usize> {
    let include_unwrap_or = opts.include_unwrap_or;
    let files = ctx.files;
    let summary = ctx.summary;
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = SwallowVisitor {
            include_unwrap_or,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            closure_tail: Vec::new(),
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }
    ctx.retain_changed(&mut all, |h| &h.file);
    ctx.retain_unsuppressed(&mut all, |h| (h.file.as_str(), h.line));
    let before = all.len();
    all.retain(|h| match h.benign {
        Some("infallible-write") => opts.include_infallible,
        Some("logged-fallback") | Some("combinator-ok") => opts.include_logged,
        _ => true,
    });
    let benign_hidden = before - all.len();
    all.sort_by(|a, b| {
        a.kind
            .cmp(b.kind)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    if !summary {
        for h in &all {
            row!(
                ctx.out,
                "kind" => h.kind,
                "context" => h.context.clone(),
                "at" => site(&h.file, h.line),
            );
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
    ctx.out.summary(&format!(
        "({} swallow site(s); {}; include_unwrap_or={}{}; explain: silent-fallbacks)",
        all.len(),
        breakdown.join(", "),
        include_unwrap_or,
        if benign_hidden > 0 {
            format!(
                "; {} benign site(s) hidden (infallible writes / logged fallbacks — \
                 `--include-infallible` / `--include-logged` to restore)",
                benign_hidden
            )
        } else {
            String::new()
        }
    ));
    Ok(all.len())
}
