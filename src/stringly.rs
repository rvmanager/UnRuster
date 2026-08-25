use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{lit_str, print_grouped_counts, scope_visits, top_module_of, ScopeTracker};
use crate::context::{AnalysisCtx, GroupBy};
use crate::parse::display_path;
use crate::emit::{row, site};

#[derive(Debug)]
struct Hit {
    /// "cmp-eq" | "cmp-method" | "match-lit" | "substr" | "map-lit-key"
    class: &'static str,
    literal: String,
    context: String,
    file: String,
    line: usize,
}

struct StringlyVisitor<'a> {
    include_substring: bool,
    include_map_keys: bool,
    file: &'a str,
    scope: ScopeTracker,
    hits: Vec<Hit>,
}

impl<'a> StringlyVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    fn record(&mut self, class: &'static str, literal: String, line: usize) {
        let ctx = self.enclosing();
        self.hits.push(Hit {
            class,
            literal,
            context: ctx,
            file: self.file.to_string(),
            line,
        });
    }
}


fn collect_str_lits_in_pat(p: &syn::Pat, out: &mut Vec<(String, usize)>) {
    match p {
        syn::Pat::Lit(el) => {
            if let syn::Lit::Str(s) = &el.lit {
                out.push((s.value(), s.span().start().line));
            }
        }
        syn::Pat::Or(o) => {
            for c in &o.cases {
                collect_str_lits_in_pat(c, out);
            }
        }
        syn::Pat::Reference(r) => collect_str_lits_in_pat(&r.pat, out),
        syn::Pat::Paren(p) => collect_str_lits_in_pat(&p.pat, out),
        _ => {}
    }
}

fn truncate_lit(s: &str, max: usize) -> String {
    let escaped = s.replace('\n', "\\n").replace('\t', "\\t");
    let chars: Vec<char> = escaped.chars().collect();
    if chars.len() <= max {
        format!("\"{}\"", escaped)
    } else {
        let head: String = chars.into_iter().take(max).collect();
        format!("\"{}…\"", head)
    }
}

impl<'ast, 'a> Visit<'ast> for StringlyVisitor<'a> {
    scope_visits!(item_mod, item_impl, item_trait, item_fn, impl_item_fn, trait_item_fn);

    fn visit_expr_binary(&mut self, e: &'ast syn::ExprBinary) {
        if matches!(e.op, syn::BinOp::Eq(_) | syn::BinOp::Ne(_)) {
            if let Some(s) = lit_str(&e.left) {
                self.record("cmp-eq", truncate_lit(&s, 32), e.left.span().start().line);
            } else if let Some(s) = lit_str(&e.right) {
                self.record("cmp-eq", truncate_lit(&s, 32), e.right.span().start().line);
            }
        }
        visit::visit_expr_binary(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let m = e.method.to_string();
        let class: Option<&'static str> = match m.as_str() {
            "eq" | "ne" | "eq_ignore_ascii_case" | "eq_ignore_case" => Some("cmp-method"),
            "starts_with" | "ends_with" | "contains" if self.include_substring => Some("substr"),
            "get" | "contains_key" | "remove" | "entry" if self.include_map_keys => Some("map-lit-key"),
            _ => None,
        };
        if let Some(c) = class {
            if let Some(arg) = e.args.first() {
                if let Some(s) = lit_str(arg) {
                    self.record(c, truncate_lit(&s, 32), e.method.span().start().line);
                }
            }
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_match(&mut self, e: &'ast syn::ExprMatch) {
        for arm in &e.arms {
            let mut found = Vec::new();
            collect_str_lits_in_pat(&arm.pat, &mut found);
            for (v, line) in found {
                self.record("match-lit", truncate_lit(&v, 32), line);
            }
        }
        visit::visit_expr_match(self, e);
    }

    fn visit_expr_let(&mut self, e: &'ast syn::ExprLet) {
        // `if let "foo" = x.as_str()` etc.
        let mut found = Vec::new();
        collect_str_lits_in_pat(&e.pat, &mut found);
        for (v, line) in found {
            self.record("match-lit", truncate_lit(&v, 32), line);
        }
        visit::visit_expr_let(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        // Special-case assert_eq!/assert_ne!/debug_assert_eq!/debug_assert_ne! so we
        // catch `assert_eq!(role, "admin")` which is morally `role == "admin"`.
        let mac_name = m
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        let is_assert_cmp = matches!(
            mac_name.as_str(),
            "assert_eq" | "assert_ne" | "debug_assert_eq" | "debug_assert_ne"
        );
        let exprs = crate::macro_scan::macro_exprs(m);
        if is_assert_cmp {
            // First two args are the operands; either being a str literal is a hit.
            for arg in exprs.iter().take(2) {
                if let Some(s) = lit_str(arg) {
                    self.record(
                        "cmp-eq",
                        truncate_lit(&s, 32),
                        arg.span().start().line,
                    );
                }
            }
        }
        for expr in exprs {
            self.visit_expr(&expr);
        }
    }
}

pub fn run(
    ctx: &AnalysisCtx,
    include_substring: bool,
    include_map_keys: bool,
    by: Option<GroupBy>,
) -> anyhow::Result<usize> {
    let files = ctx.files;
    let summary = ctx.summary;
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = StringlyVisitor {
            include_substring,
            include_map_keys,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }

    ctx.retain_changed(&mut all, |h| &h.file);
    // Keyed on the literal, so waiving the Stripe event name on a line leaves
    // any other literal branch on it flagged. An unkeyed `ok(stringly)` above
    // a fn retires the whole cluster in one comment, which is the shape most
    // of these come in: a `match` over a wire protocol's vocabulary is one
    // judgment, not eight.
    //
    // The check had no waiver mechanism at all until now. That was load-bearing
    // in the wrong direction: `stringly` hits are frequently correct (external
    // protocol strings genuinely are strings), so its count could never reach
    // zero, so it could never gate, so the audit's advisory tier stayed
    // permanently non-empty and there was nothing a reader could do about it.
    let waived = ctx.retain_unsuppressed("stringly", &mut all, |h| {
        crate::suppress::Site::keyed(h.file.as_str(), h.line, h.literal.as_str())
    });
    all.sort_by(|a, b| {
        a.class
            .cmp(b.class)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    if !summary {
        match by {
            Some(GroupBy::Fn) => print_grouped_counts(ctx.out, &all, |h| h.context.clone()),
            Some(GroupBy::File) => print_grouped_counts(ctx.out, &all, |h| h.file.clone()),
            Some(GroupBy::Module) => {
                print_grouped_counts(ctx.out, &all, |h| top_module_of(&h.context).to_string())
            }
            None => {
                let today = crate::suppress::Date::today();
                for h in &all {
                    row!(
                        ctx.out,
                        "class" => h.class,
                        "literal" => h.literal.clone(),
                        "in_fn" => h.context.clone(),
                        "at" => site(&h.file, h.line),
                    );
                    ctx.suggest("stringly", Some(&h.literal), today, (&h.file, h.line));
                }
            }
        }
    }

    use std::collections::BTreeMap;
    let mut by_class: BTreeMap<&str, usize> = BTreeMap::new();
    for h in &all {
        *by_class.entry(h.class).or_insert(0) += 1;
    }
    let break_str: Vec<String> = by_class
        .iter()
        .map(|(k, n)| format!("{}={}", k, n))
        .collect();
    ctx.out.summary(&format!(
        "({} stringly hit(s); {}; include_substring={}, include_map_keys={}{}; explain: stringly)",
        all.len(),
        break_str.join(", "),
        include_substring,
        include_map_keys,
        ctx.waived_note(waived)
    ));
    Ok(all.len())
}
