use syn::visit::{self, Visit};

use crate::ast::{line_of, path_to_string_with_args, type_short};
use crate::index::NameIndex;
use crate::parse::{display_path, ParsedFile};

#[derive(Debug)]
struct Ref {
    file: String,
    line: usize,
    context: String,
    role: &'static str, // "type" | "ctor" | "path"
    written: String,    // path as written, e.g. `crate::doc::Document`
}

struct RefVisitor<'a> {
    target: &'a str,
    file: &'a str,
    module: &'a str,
    fn_stack: Vec<String>,
    impl_stack: Vec<String>,
    mod_stack: Vec<String>,
    out: Vec<Ref>,
}

impl<'a> RefVisitor<'a> {
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

    fn matches_path_last(&self, p: &syn::Path) -> bool {
        p.segments
            .last()
            .map(|s| s.ident == self.target)
            .unwrap_or(false)
    }

    fn record(&mut self, role: &'static str, written: String, line: usize) {
        let ctx = self.enclosing();
        self.out.push(Ref {
            file: self.file.to_string(),
            line,
            context: ctx,
            role,
            written,
        });
    }
}

impl<'ast, 'a> Visit<'ast> for RefVisitor<'a> {
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

    fn visit_type_path(&mut self, t: &'ast syn::TypePath) {
        if self.matches_path_last(&t.path) {
            let line = t
                .path
                .segments
                .last()
                .map(|s| line_of(&s.ident))
                .unwrap_or(0);
            self.record("type", path_to_string_with_args(&t.path), line);
        }
        visit::visit_type_path(self, t);
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            // Detect `Type::new(...)`, `Type::default(...)`, `Type(arg)` (tuple struct ctor)
            let segs = &p.path.segments;
            if segs.len() == 1 && segs[0].ident == self.target {
                let line = line_of(&segs[0].ident);
                self.record("ctor", path_to_string_with_args(&p.path), line);
            } else if segs.len() >= 2 && segs[segs.len() - 2].ident == self.target {
                let line = line_of(&segs[segs.len() - 2].ident);
                self.record("ctor", path_to_string_with_args(&p.path), line);
            }
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        if self.matches_path_last(&e.path) {
            let line = e
                .path
                .segments
                .last()
                .map(|s| line_of(&s.ident))
                .unwrap_or(0);
            self.record("ctor", path_to_string_with_args(&e.path), line);
        }
        visit::visit_expr_struct(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

pub fn run(files: &[ParsedFile], index: &NameIndex, ty: &str, summary: bool) -> anyhow::Result<()> {
    if !index.knows_name(ty) {
        eprintln!(
            "note: `{}` is not a known struct/enum/trait/type-alias in this tree; \
             reporting matches anyway",
            ty
        );
    }

    let mut all: Vec<Ref> = Vec::new();
    for f in files {
        let mut v = RefVisitor {
            target: ty,
            file: &display_path(&f.path),
            module: &f.module,
            fn_stack: Vec::new(),
            impl_stack: Vec::new(),
            mod_stack: Vec::new(),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.out);
    }

    all.sort_by(|a, b| {
        a.role
            .cmp(b.role)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    if !summary {
        for r in &all {
            println!(
                "{}\t{}\t{}\t{}:{}",
                r.role, r.written, r.context, r.file, r.line
            );
        }
    }

    let mut by_module = std::collections::BTreeMap::<String, usize>::new();
    for r in &all {
        let module_of = r
            .context
            .split("::")
            .take_while(|s| s.chars().next().map(|c| !c.is_ascii_uppercase()).unwrap_or(true))
            .collect::<Vec<_>>()
            .join("::");
        *by_module.entry(module_of).or_default() += 1;
    }
    eprintln!("({} reference(s) across {} module(s))", all.len(), by_module.len());
    Ok(())
}
