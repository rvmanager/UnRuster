use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{line_of, path_to_string, type_short};
use crate::context::AnalysisCtx;
use crate::parse::display_path;

#[derive(Debug)]
struct Hit {
    qpath: String,
    file: String,
    line: usize,
    loc: usize,
    forwarded_to: String,
}

struct PTVisitor<'a> {
    max_loc: usize,
    file: &'a str,
    module: &'a str,
    impl_stack: Vec<String>,
    mod_stack: Vec<String>,
    hits: Vec<Hit>,
}

impl<'a> PTVisitor<'a> {
    fn qualify(&self, name: &str) -> String {
        let mut path: Vec<String> = Vec::new();
        if !self.module.is_empty() {
            path.push(self.module.to_string());
        }
        path.extend(self.mod_stack.iter().cloned());
        if let Some(t) = self.impl_stack.last() {
            path.push(t.clone());
        }
        path.push(name.to_string());
        path.join("::")
    }

    fn check(&mut self, sig: &syn::Signature, body: &syn::Block) {
        let start = line_of(&sig.ident);
        let end = body.span().end().line.max(start);
        let loc = end.saturating_sub(start) + 1;
        if loc > self.max_loc {
            return;
        }
        let Some(call) = single_call_in_body(body) else {
            return;
        };
        let target = describe_call(&call);
        let qpath = self.qualify(&sig.ident.to_string());
        self.hits.push(Hit {
            qpath,
            file: self.file.to_string(),
            line: start,
            loc,
            forwarded_to: target,
        });
    }
}

/// Return the single call expression in `body` if `body` is "just a call".
/// Accepts:
///   { g(args) }
///   { let x = g(args); x }
///   { g(args); }
///   { return g(args); }
fn single_call_in_body(body: &syn::Block) -> Option<syn::Expr> {
    let stmts = &body.stmts;
    if stmts.len() == 1 { if let syn::Stmt::Expr(e, _) = &stmts[0] {
        if is_call_like(e) {
            return Some(e.clone());
        }
    } }
    None
}

fn is_call_like(e: &syn::Expr) -> bool {
    matches!(
        e,
        syn::Expr::Call(_) | syn::Expr::MethodCall(_) | syn::Expr::Macro(_)
    )
}

fn describe_call(e: &syn::Expr) -> String {
    match e {
        syn::Expr::Call(c) => {
            if let syn::Expr::Path(p) = &*c.func {
                return path_to_string(&p.path);
            }
            "<expr>".into()
        }
        syn::Expr::MethodCall(m) => format!(".{}", m.method),
        syn::Expr::Macro(m) => format!("{}!", path_to_string(&m.mac.path)),
        _ => "<expr>".into(),
    }
}

impl<'ast, 'a> Visit<'ast> for PTVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.mod_stack.push(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.mod_stack.pop();
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.impl_stack.push(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.impl_stack.pop();
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.check(&i.sig, &i.block);
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.check(&i.sig, &i.block);
    }
}

pub fn run(ctx: &AnalysisCtx, max_loc: usize) -> anyhow::Result<()> {
    let files = ctx.files;
    let summary = ctx.summary;
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = PTVisitor {
            max_loc,
            file: &display_path(&f.path),
            module: &f.module,
            impl_stack: Vec::new(),
            mod_stack: Vec::new(),
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }
    all.sort_by(|a, b| a.qpath.cmp(&b.qpath));
    if !summary {
        for h in &all {
            println!(
                "{}\t->\t{}\tloc:{}\t{}:{}",
                h.qpath, h.forwarded_to, h.loc, h.file, h.line
            );
        }
    }
    eprintln!("({} pass-through fn(s); max_loc={})", all.len(), max_loc);
    Ok(())
}
