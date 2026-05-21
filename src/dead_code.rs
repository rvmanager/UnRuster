use std::collections::BTreeSet;

use syn::visit::{self, Visit};

use crate::ast::path_to_string;
use crate::index::NameIndex;
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
}

pub fn run(
    files: &[ParsedFile],
    index: &NameIndex,
    pub_only: bool,
    summary: bool,
) -> anyhow::Result<()> {
    let mut sink = CallSink {
        called: BTreeSet::new(),
    };
    for f in files {
        sink.visit_file(&f.ast);
    }
    // Index of all defined fns by last name → file/line for the report.
    let mut hits: Vec<(&str, &crate::index::Defn)> = Vec::new();
    for d in index.iter() {
        match d.kind {
            "fn" | "impl-fn" | "trait-fn" => {}
            _ => continue,
        }
        if pub_only && d.vis != "pub" {
            continue;
        }
        // `main` / `start` are entry points — never dead.
        if matches!(d.name.as_str(), "main" | "start") {
            continue;
        }
        // Trait methods whose owning trait is implemented somewhere are likely
        // dispatched dynamically — skip to avoid false positives.
        if d.kind == "trait-fn" {
            continue;
        }
        // Default impls of common traits (Display, Debug, Drop, Clone, ...) get
        // called by `{}`/`{:?}`/auto-drop and are easy to miss-name. Skip fns
        // whose name is a known trait-method convention.
        if matches!(
            d.name.as_str(),
            "fmt"
                | "from"
                | "into"
                | "as_ref"
                | "as_mut"
                | "deref"
                | "deref_mut"
                | "drop"
                | "clone"
                | "default"
                | "hash"
                | "eq"
                | "ne"
                | "cmp"
                | "partial_cmp"
                | "next"
                | "size_hint"
        ) {
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
        "({} candidate dead fn(s); pub_only={}; heuristic — pub items may have external callers, trait fns are skipped)",
        hits.len(),
        pub_only
    );
    Ok(())
}
