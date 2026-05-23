use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{print_grouped_counts, top_module_of, type_short};
use crate::parse::{display_path, ParsedFile};

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
    module: &'a str,
    fn_stack: Vec<String>,
    impl_stack: Vec<String>,
    mod_stack: Vec<String>,
    hits: Vec<Hit>,
}

impl<'a> StringlyVisitor<'a> {
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

fn lit_str_value(e: &syn::Expr) -> Option<String> {
    if let syn::Expr::Lit(el) = e {
        if let syn::Lit::Str(s) = &el.lit {
            return Some(s.value());
        }
    }
    None
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

    fn visit_expr_binary(&mut self, e: &'ast syn::ExprBinary) {
        if matches!(e.op, syn::BinOp::Eq(_) | syn::BinOp::Ne(_)) {
            if let Some(s) = lit_str_value(&e.left) {
                self.record("cmp-eq", truncate_lit(&s, 32), e.left.span().start().line);
            } else if let Some(s) = lit_str_value(&e.right) {
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
                if let Some(s) = lit_str_value(arg) {
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
                if let Some(s) = lit_str_value(arg) {
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
    files: &[ParsedFile],
    include_substring: bool,
    include_map_keys: bool,
    by: Option<&str>,
    summary: bool,
) -> anyhow::Result<()> {
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = StringlyVisitor {
            include_substring,
            include_map_keys,
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
        a.class
            .cmp(b.class)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    if !summary {
        match by {
            Some("fn") => print_grouped_counts(&all, None, |h| h.context.clone()),
            Some("file") => print_grouped_counts(&all, None, |h| h.file.clone()),
            Some("module") => {
                print_grouped_counts(&all, None, |h| top_module_of(&h.context).to_string())
            }
            _ => {
                for h in &all {
                    println!(
                        "{}\t{}\t{}\t{}:{}",
                        h.class, h.literal, h.context, h.file, h.line
                    );
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
    eprintln!(
        "({} stringly hit(s); {}; design-smell hint: branching on string literals couples logic to spelling — replace with an enum (or a newtype like `pub struct ActionId(&'static str)`) so the compiler catches typos and missing cases.)",
        all.len(),
        break_str.join(", ")
    );
    Ok(())
}
