use syn::visit::{self, Visit};

use crate::ast::{line_of, path_to_string, type_short};
use crate::parse::{display_path, ParsedFile};

#[derive(Debug)]
struct CallSite {
    file: String,
    line: usize,
    caller: String,
    target: String,
}

struct CallVisitor<'a> {
    file: &'a str,
    module: &'a str,
    fn_stack: Vec<String>,
    impl_stack: Vec<String>,
    mod_stack: Vec<String>,
    sites: Vec<CallSite>,
}

impl<'a> CallVisitor<'a> {
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
        } else if path.is_empty() {
            return "<top-level>".to_string();
        } else {
            path.push("<top-level>".to_string());
        }
        path.join("::")
    }

    fn record(&mut self, target: String, line: usize) {
        self.sites.push(CallSite {
            file: self.file.to_string(),
            line,
            caller: self.enclosing(),
            target,
        });
    }
}

impl<'ast, 'a> Visit<'ast> for CallVisitor<'a> {
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

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        visit::visit_trait_item_fn(self, i);
        self.fn_stack.pop();
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            let target = path_to_string(&p.path);
            self.record(target, line_of(&e.func));
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let target = format!(".{}", e.method);
        self.record(target, line_of(&e.method));
        visit::visit_expr_method_call(self, e);
    }
}

fn collect_sites(files: &[ParsedFile]) -> Vec<CallSite> {
    let mut all = Vec::new();
    for f in files {
        let mut v = CallVisitor {
            file: &display_path(&f.path),
            module: &f.module,
            fn_stack: Vec::new(),
            impl_stack: Vec::new(),
            mod_stack: Vec::new(),
            sites: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.sites);
    }
    all
}

fn matches_target(call_target: &str, query: &str) -> bool {
    // query forms:
    //   `foo`              -> matches path ending in `foo`, or method `.foo`
    //   `Foo::bar`         -> matches path ending in `Foo::bar`
    //   `.foo` (explicit)  -> matches method `.foo` only
    //   `::foo` (explicit) -> matches free-fn last segment `foo` only (skip methods)
    if let Some(method) = query.strip_prefix('.') {
        return call_target == format!(".{}", method);
    }
    if let Some(rest) = query.strip_prefix("::") {
        if call_target.starts_with('.') {
            return false;
        }
        let last = rest.rsplit("::").next().unwrap_or(rest);
        let target_last = call_target.rsplit("::").next().unwrap_or(call_target);
        return target_last == last;
    }
    if query.contains("::") {
        return call_target.ends_with(query);
    }
    // bare name: match either a path with that last segment, or a method of that name
    let target_last = if let Some(m) = call_target.strip_prefix('.') {
        m
    } else {
        call_target.rsplit("::").next().unwrap_or(call_target)
    };
    target_last == query
}

pub fn run_callers(files: &[ParsedFile], query: &str) -> anyhow::Result<()> {
    let sites = collect_sites(files);
    let mut hits: Vec<&CallSite> = sites
        .iter()
        .filter(|s| matches_target(&s.target, query))
        .collect();
    hits.sort_by(|a, b| {
        a.caller
            .cmp(&b.caller)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    for s in &hits {
        println!("{}\t{}\t{}:{}", s.caller, s.target, s.file, s.line);
    }
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for h in &hits {
        *counts.entry(h.caller.as_str()).or_insert(0) += 1;
    }
    eprintln!(
        "({} call sites across {} caller(s))",
        hits.len(),
        counts.len()
    );
    Ok(())
}

pub fn run_callees(files: &[ParsedFile], query: &str) -> anyhow::Result<()> {
    let sites = collect_sites(files);
    let last = query.rsplit("::").next().unwrap_or(query);

    let in_target = |caller: &str| -> bool {
        if query.contains("::") {
            // Qualified: match path suffix only.
            caller == query || caller.ends_with(&format!("::{}", query))
        } else {
            // Bare name: match last segment.
            caller.rsplit("::").next().unwrap_or(caller) == last
        }
    };

    let hits: Vec<&CallSite> = sites.iter().filter(|s| in_target(&s.caller)).collect();
    if hits.is_empty() {
        eprintln!("no fn matching `{}` found (or it makes no calls)", query);
        return Ok(());
    }

    let mut counts = std::collections::BTreeMap::<String, (usize, String, usize)>::new();
    for h in &hits {
        let e = counts
            .entry(h.target.clone())
            .or_insert((0, h.file.clone(), h.line));
        e.0 += 1;
    }
    let mut rows: Vec<_> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| a.0.cmp(&b.0)));
    for (target, (n, file, line)) in &rows {
        println!("{}\t{}\t{}:{}", n, target, file, line);
    }
    eprintln!("({} distinct callees)", rows.len());
    Ok(())
}
