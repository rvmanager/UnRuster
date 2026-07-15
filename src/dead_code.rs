use std::collections::BTreeSet;

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::{self, Visit};

use crate::ast::path_to_string;
use crate::context::AnalysisCtx;
use crate::parse::ParsedFile;

/// Build a set of every "called" last-segment name we observe across the tree.
struct CallSink {
    called: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for CallSink {
    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            let s = path_to_string(&p.path);
            let last = s.rsplit("::").next().unwrap_or(&s).to_string();
            self.called.insert(last);
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        self.called.insert(e.method.to_string());
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_path(&mut self, e: &'ast syn::ExprPath) {
        // Track fn-references-as-values (`let f = some_fn; f();`) too.
        let s = path_to_string(&e.path);
        let last = s.rsplit("::").next().unwrap_or(&s).to_string();
        self.called.insert(last);
        visit::visit_expr_path(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if let Some(last) = m.path.segments.last() {
            self.called.insert(last.ident.to_string());
        }
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }

    fn visit_item_macro(&mut self, im: &'ast syn::ItemMacro) {
        // `macro_rules! foo { ... }` definitions: walk the body tokens and
        // treat every identifier as "potentially called." Otherwise a fn
        // referenced only from a custom macro expansion would look dead.
        let is_macro_rules = im
            .mac
            .path
            .segments
            .last()
            .map(|s| s.ident == "macro_rules")
            .unwrap_or(false);
        if is_macro_rules {
            collect_idents(&im.mac.tokens, &mut self.called);
        }
        visit::visit_item_macro(self, im);
    }
}

fn collect_idents(ts: &TokenStream, out: &mut BTreeSet<String>) {
    for tt in ts.clone() {
        match tt {
            TokenTree::Ident(id) => {
                out.insert(id.to_string());
            }
            TokenTree::Group(g) => collect_idents(&g.stream(), out),
            _ => {}
        }
    }
}

/// Candidate defns come from `ctx.idx` (built over the user-scoped files);
/// `call_source` is the FULL tree so production items called only from tests
/// aren't false-flagged as dead.
pub fn run(
    ctx: &AnalysisCtx,
    call_source: &[ParsedFile],
    pub_only: bool,
) -> anyhow::Result<()> {
    let index = ctx.idx;
    let summary = ctx.summary;
    let mut sink = CallSink {
        called: BTreeSet::new(),
    };
    for f in call_source {
        sink.visit_file(&f.ast);
    }

    let mut hits: Vec<(&str, &crate::index::Defn)> = Vec::new();
    for d in index.iter() {
        match d.kind {
            "fn" | "impl-fn" | "trait-fn" => {}
            _ => continue,
        }
        if pub_only && d.vis != "pub" {
            continue;
        }
        if matches!(d.name.as_str(), "main" | "start") {
            continue;
        }
        if d.kind == "trait-fn" || d.in_trait_impl {
            continue;
        }
        if d.allow_dead {
            continue;
        }
        if sink.called.contains(&d.name) {
            continue;
        }
        hits.push((d.kind, d));
    }

    hits.sort_by(|a, b| a.1.file.cmp(&b.1.file).then_with(|| a.1.line.cmp(&b.1.line)));

    if !summary {
        for (kind, d) in &hits {
            println!("{}\t{}\t{}\t{}:{}", kind, d.vis, d.qpath, d.file, d.line);
        }
    }
    eprintln!(
        "({} candidate dead fn(s); pub_only={}; heuristic — call-set built from full tree \
         incl. tests; trait impls + `#[allow(dead_code)]` skipped; pub items may still have \
         external callers we can't see.)",
        hits.len(),
        pub_only
    );
    Ok(())
}
