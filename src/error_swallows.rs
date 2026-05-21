use syn::visit::{self, Visit};

use crate::ast::{line_of, type_short};
use crate::parse::{display_path, ParsedFile};

#[derive(Debug)]
struct Hit {
    kind: &'static str, // ".ok" | ".unwrap_or_default" | ".unwrap_or_else" | ".unwrap_or"
    file: String,
    line: usize,
    context: String,
}

struct SwallowVisitor<'a> {
    include_unwrap_or: bool,
    file: &'a str,
    module: &'a str,
    fn_stack: Vec<String>,
    impl_stack: Vec<String>,
    mod_stack: Vec<String>,
    hits: Vec<Hit>,
}

impl<'a> SwallowVisitor<'a> {
    fn enclosing(&self) -> String {
        let mut path: Vec<String> = Vec::new();
        if !self.module.is_empty() {
            path.push(self.module.to_string());
        }
        path.extend(self.mod_stack.iter().cloned());
        if let Some(t) = self.impl_stack.last() {
            path.push(t.clone());
        }
        if let Some(f) = self.fn_stack.last() {
            path.push(f.clone());
        }
        if path.is_empty() {
            "<top-level>".into()
        } else {
            path.join("::")
        }
    }
}

impl<'ast, 'a> Visit<'ast> for SwallowVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.mod_stack.push(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.mod_stack.pop();
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        visit::visit_item_fn(self, i);
        self.fn_stack.pop();
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.impl_stack.push(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.impl_stack.pop();
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        visit::visit_impl_item_fn(self, i);
        self.fn_stack.pop();
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let m = e.method.to_string();
        let kind: Option<&'static str> = match m.as_str() {
            "ok" if e.args.is_empty() => Some(".ok"),
            "err" if e.args.is_empty() => Some(".err"),
            "unwrap_or_default" if e.args.is_empty() => Some(".unwrap_or_default"),
            "unwrap_or_else" => Some(".unwrap_or_else"),
            "unwrap_or" if self.include_unwrap_or => Some(".unwrap_or"),
            "ok_or_default" => Some(".ok_or_default"),
            _ => None,
        };
        if let Some(k) = kind {
            let ctx = self.enclosing();
            self.hits.push(Hit {
                kind: k,
                file: self.file.to_string(),
                line: line_of(&e.method),
                context: ctx,
            });
        }
        visit::visit_expr_method_call(self, e);
    }
}

pub fn run(
    files: &[ParsedFile],
    include_unwrap_or: bool,
    summary: bool,
) -> anyhow::Result<()> {
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = SwallowVisitor {
            include_unwrap_or,
            file: &display_path(&f.path),
            module: &f.module,
            fn_stack: Vec::new(),
            impl_stack: Vec::new(),
            mod_stack: Vec::new(),
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }
    all.sort_by(|a, b| {
        a.kind
            .cmp(b.kind)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    if !summary {
        for h in &all {
            println!("{}\t{}\t{}:{}", h.kind, h.context, h.file, h.line);
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
        "({} swallow site(s); {}; include_unwrap_or={})",
        all.len(),
        breakdown.join(", "),
        include_unwrap_or
    );
    Ok(())
}
