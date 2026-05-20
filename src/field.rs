use syn::visit::{self, Visit};

use crate::ast::{line_of, path_last, path_to_string, type_short};
use crate::parse::{display_path, ParsedFile};

#[derive(Debug)]
struct FieldHit {
    kind: &'static str, // "read" | "write" | "init"
    file: String,
    line: usize,
    context: String, // enclosing fn or impl
    note: String,    // "self", "<expr>", base path, ...
}

struct FieldVisitor<'a> {
    file: &'a str,
    module: &'a str,
    target_type: &'a str,
    target_field: &'a str,
    strict: bool,

    impl_stack: Vec<String>,
    fn_stack: Vec<String>,
    mod_stack: Vec<String>,
    in_write_lhs: bool,

    hits: Vec<FieldHit>,
}

impl<'a> FieldVisitor<'a> {
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

    fn in_target_impl(&self) -> bool {
        self.impl_stack
            .last()
            .map(|t| t == self.target_type)
            .unwrap_or(false)
    }

    fn record(&mut self, kind: &'static str, line: usize, note: String) {
        self.hits.push(FieldHit {
            kind,
            file: self.file.to_string(),
            line,
            context: self.enclosing(),
            note,
        });
    }
}

impl<'ast, 'a> Visit<'ast> for FieldVisitor<'a> {
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

    fn visit_expr_assign(&mut self, e: &'ast syn::ExprAssign) {
        self.in_write_lhs = true;
        self.visit_expr(&e.left);
        self.in_write_lhs = false;
        self.visit_expr(&e.right);
    }

    fn visit_expr_binary(&mut self, e: &'ast syn::ExprBinary) {
        let is_compound = matches!(
            e.op,
            syn::BinOp::AddAssign(_)
                | syn::BinOp::SubAssign(_)
                | syn::BinOp::MulAssign(_)
                | syn::BinOp::DivAssign(_)
                | syn::BinOp::RemAssign(_)
                | syn::BinOp::BitXorAssign(_)
                | syn::BinOp::BitAndAssign(_)
                | syn::BinOp::BitOrAssign(_)
                | syn::BinOp::ShlAssign(_)
                | syn::BinOp::ShrAssign(_)
        );
        if is_compound {
            self.in_write_lhs = true;
            self.visit_expr(&e.left);
            self.in_write_lhs = false;
            self.visit_expr(&e.right);
        } else {
            visit::visit_expr_binary(self, e);
        }
    }

    fn visit_expr_reference(&mut self, e: &'ast syn::ExprReference) {
        // `&mut self.f` counts as a write (read-modify-write potential).
        if e.mutability.is_some() {
            let saved = self.in_write_lhs;
            self.in_write_lhs = true;
            self.visit_expr(&e.expr);
            self.in_write_lhs = saved;
        } else {
            visit::visit_expr_reference(self, e);
        }
    }

    fn visit_expr_field(&mut self, e: &'ast syn::ExprField) {
        if let syn::Member::Named(id) = &e.member {
            if id == self.target_field {
                let is_write = self.in_write_lhs;
                // Only the outermost field-access node is the target of the write;
                // its inner base is just read to compute the place.
                let was_write = self.in_write_lhs;
                self.in_write_lhs = false;

                let (note, confirmed) = match &*e.base {
                    syn::Expr::Path(p) if p.path.is_ident("self") => {
                        if self.in_target_impl() {
                            ("self".to_string(), true)
                        } else {
                            (format!("self (in impl {})", self.impl_stack.last().cloned().unwrap_or_default()), false)
                        }
                    }
                    syn::Expr::Path(p) => (path_to_string(&p.path), false),
                    _ => ("<expr>".to_string(), false),
                };

                if confirmed || !self.strict {
                    let kind: &'static str = if is_write { "write" } else { "read" };
                    self.record(kind, line_of(id), note);
                }

                self.visit_expr(&e.base);
                self.in_write_lhs = was_write;
                return;
            }
        }
        visit::visit_expr_field(self, e);
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        let lit_ty = path_last(&e.path);
        let resolved_ty = if lit_ty == "Self" {
            self.impl_stack.last().cloned().unwrap_or_else(|| lit_ty.clone())
        } else {
            lit_ty.clone()
        };
        if resolved_ty == self.target_type {
            for fv in &e.fields {
                if let syn::Member::Named(id) = &fv.member {
                    if id == self.target_field {
                        let line = line_of(id);
                        self.record("init", line, resolved_ty.clone());
                    }
                }
            }
        }
        visit::visit_expr_struct(self, e);
    }
}

pub fn run(files: &[ParsedFile], ty: &str, field: &str, strict: bool) -> anyhow::Result<()> {
    let mut all = Vec::new();
    for f in files {
        let mut v = FieldVisitor {
            file: &display_path(&f.path),
            module: &f.module,
            target_type: ty,
            target_field: field,
            strict,
            impl_stack: Vec::new(),
            fn_stack: Vec::new(),
            mod_stack: Vec::new(),
            in_write_lhs: false,
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

    let (mut reads, mut writes, mut inits) = (0usize, 0usize, 0usize);
    for h in &all {
        match h.kind {
            "read" => reads += 1,
            "write" => writes += 1,
            "init" => inits += 1,
            _ => {}
        }
        println!(
            "{}\t{}\t{}\t{}:{}",
            h.kind, h.note, h.context, h.file, h.line
        );
    }
    eprintln!(
        "({} reads, {} writes, {} inits; strict={})",
        reads, writes, inits, strict
    );
    Ok(())
}
