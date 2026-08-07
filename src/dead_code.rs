use std::collections::BTreeSet;

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::{self, Visit};

use crate::ast::path_to_string;
use crate::context::AnalysisCtx;
use crate::parse::ParsedFile;
use crate::emit::row;

/// Build a set of every "called" last-segment name we observe across the tree.
struct CallSink {
    called: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for CallSink {
    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            let s = path_to_string(&p.path);
            let last = crate::ast::last_segment(&s).to_string();
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
        let last = crate::ast::last_segment(&s).to_string();
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
        // Then every identifier in the raw tokens, whether or not the body
        // parsed as expressions.
        //
        // `macro_exprs` parses chunk-by-chunk and keeps what it can, so a body
        // like `row!(out, "age" => age_str(w))` yields `out` and silently drops
        // the arm containing the call — the fn then looks dead while being
        // called on that very line. It is not even reported as a blind spot,
        // because the parse partially succeeded.
        //
        // A call-set only needs "might this name be called", and the check
        // already declares itself heuristic. Over-collecting costs a missed
        // dead fn; under-collecting sends someone to delete live code. This is
        // the same treatment `visit_item_macro` already gives `macro_rules!`
        // bodies, for the same reason.
        collect_idents(&m.tokens, &mut self.called);
    }

    /// Functions named by a string inside an attribute.
    ///
    /// `#[serde(default = "default_true")]` is a real call — the derive expands
    /// it into one — but the name lives in a string literal, so no amount of
    /// expression walking finds it. Same for `with` / `serialize_with` /
    /// `deserialize_with`, and for the equivalents in other derive ecosystems.
    ///
    /// Rather than allow-list attribute names (the next crate spells it
    /// differently), take every string literal in every attribute and, if it
    /// parses as a path, count its last segment as called. A string that is a
    /// bare identifier is almost never prose, and the cost of being wrong is a
    /// missed dead fn rather than a live one reported dead.
    fn visit_attribute(&mut self, a: &'ast syn::Attribute) {
        match &a.meta {
            // `#[serde(default = "f", with = "m")]` — the interesting case.
            syn::Meta::List(ml) => collect_path_strings(&ml.tokens, &mut self.called),
            // `#[doc = "…"]` lands here too; prose fails the path test.
            syn::Meta::NameValue(nv) => {
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) = &nv.value
                {
                    insert_if_path(&s.value(), &mut self.called);
                }
            }
            syn::Meta::Path(_) => {}
        }
        visit::visit_attribute(self, a);
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

/// Last segments of every string literal in `ts` that looks like a path
/// (`"default_true"`, `"crate::ser::as_secs"`). Non-path strings — sentences,
/// globs, format specs — are skipped.
fn collect_path_strings(ts: &TokenStream, out: &mut BTreeSet<String>) {
    for tt in ts.clone() {
        match tt {
            TokenTree::Literal(l) => {
                let s = l.to_string();
                if let Some(inner) = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
                    insert_if_path(inner, out);
                }
            }
            TokenTree::Group(g) => collect_path_strings(&g.stream(), out),
            _ => {}
        }
    }
}

/// Record `s`'s last segment if `s` is shaped like a Rust path. Prose, globs,
/// and format strings all fail the test, so `#[doc = "…"]` contributes nothing.
fn insert_if_path(s: &str, out: &mut BTreeSet<String>) {
    if s.is_empty() {
        return;
    }
    let is_path = s.split("::").all(|seg| {
        !seg.is_empty()
            && !seg.starts_with(|c: char| c.is_ascii_digit())
            && seg.chars().all(|c| c.is_alphanumeric() || c == '_')
    });
    if is_path {
        if let Some(last) = s.rsplit("::").next() {
            out.insert(last.to_string());
        }
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
    include_trait_impls: bool,
) -> anyhow::Result<usize> {
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
        // Trait-impl methods are skipped by default (dyn dispatch is
        // invisible to us); --include-trait-impls reports them when their
        // method name is never called anywhere in the tree.
        if d.kind == "trait-fn" || (d.in_trait_impl && !include_trait_impls) {
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

    ctx.retain_changed(&mut hits, |h| &h.1.file);
    // Keyed by the item's own name, so `ok(dead-code/default_true)` retires one
    // fn while an unkeyed `ok(dead-code)` above an impl covers everything in it.
    // This check is a *gating* one and its remaining rows on a real codebase are
    // dominated by call paths no syntactic scan can see (serde attribute
    // strings, dyn dispatch, FFI) — without a waiver the audit loop could never
    // reach zero.
    let waived = ctx.retain_unsuppressed("dead-code", &mut hits, |h| {
        crate::suppress::Site::keyed(h.1.file.as_str(), h.1.line, h.1.name.as_str())
    });
    hits.sort_by(|a, b| a.1.file.cmp(&b.1.file).then_with(|| a.1.line.cmp(&b.1.line)));

    if !summary {
        let today = crate::suppress::Date::today();
        for (kind, d) in &hits {
            row!(
                ctx.out,
                "kind" => *kind,
                "vis" => d.vis,
                "qpath" => d.qpath.clone(),
                "at" => ctx.at(&d.file, d.line, d.end),
            );
            ctx.suggest("dead-code", Some(&d.name), today);
        }
    }
    ctx.out.summary(&format!(
        "({} candidate dead fn(s); pub_only={}; include_trait_impls={}{}; heuristic — call-set \
         built from full tree incl. tests; `#[allow(dead_code)]` skipped; pub items may still \
         have external callers we can't see.)",
        hits.len(),
        pub_only,
        include_trait_impls,
        ctx.waived_note(waived)
    ));
    Ok(hits.len())
}
