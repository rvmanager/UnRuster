use syn::visit::{self, Visit};

use crate::ast::{line_of, path_last, type_to_string, vis_str};
use crate::index::NameIndex;
use crate::parse::{display_path, ParsedFile};

#[derive(Debug)]
struct FieldDef {
    name: String,
    ty: String,
    vis: &'static str,
    file: String,
    line: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    reads: usize,
    writes: usize,
    inits: usize,
}

/// Locate field definitions for a given struct (or struct-like enum variant container).
struct FieldDefVisitor<'a> {
    target_type: &'a str,
    file: &'a str,
    out: Vec<FieldDef>,
}

impl<'ast, 'a> Visit<'ast> for FieldDefVisitor<'a> {
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        if i.ident == self.target_type {
            if let syn::Fields::Named(fs) = &i.fields {
                for f in &fs.named {
                    if let Some(id) = &f.ident {
                        self.out.push(FieldDef {
                            name: id.to_string(),
                            ty: type_to_string(&f.ty),
                            vis: vis_str(&f.vis),
                            file: self.file.to_string(),
                            line: line_of(id),
                        });
                    }
                }
            }
        }
    }
}

/// Walk all files counting read/write/init sites of a (Type, field) pair.
/// Strict mode: `self.f` inside `impl Type` and `Type { f: ... }` literals only.
struct CountVisitor<'a> {
    target_type: &'a str,
    target_field: &'a str,
    impl_stack: Vec<String>,
    in_write_lhs: bool,
    counts: Counts,
}

impl<'a> CountVisitor<'a> {
    fn in_target_impl(&self) -> bool {
        self.impl_stack
            .last()
            .map(|t| t == self.target_type)
            .unwrap_or(false)
    }
}

impl<'ast, 'a> Visit<'ast> for CountVisitor<'a> {
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.impl_stack
            .push(crate::ast::type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.impl_stack.pop();
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
                self.in_write_lhs = false;
                if let syn::Expr::Path(p) = &*e.base {
                    if p.path.is_ident("self") && self.in_target_impl() {
                        if is_write {
                            self.counts.writes += 1;
                        } else {
                            self.counts.reads += 1;
                        }
                    }
                }
                self.visit_expr(&e.base);
                return;
            }
        }
        visit::visit_expr_field(self, e);
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        let lit_ty = path_last(&e.path);
        let resolved = if lit_ty == "Self" {
            self.impl_stack.last().cloned().unwrap_or_default()
        } else {
            lit_ty
        };
        if resolved == self.target_type {
            for fv in &e.fields {
                if let syn::Member::Named(id) = &fv.member {
                    if id == self.target_field {
                        self.counts.inits += 1;
                    }
                }
            }
        }
        visit::visit_expr_struct(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

pub fn run(files: &[ParsedFile], _index: &NameIndex, ty: &str, summary: bool) -> anyhow::Result<()> {
    // 1. Collect field definitions for the target type from all files.
    let mut defs: Vec<FieldDef> = Vec::new();
    for f in files {
        let mut v = FieldDefVisitor {
            target_type: ty,
            file: &display_path(&f.path),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        defs.extend(v.out);
    }

    if defs.is_empty() {
        eprintln!("no struct named `{}` with named fields found", ty);
        return Ok(());
    }

    // 2. For each field, count read/write/init sites across all files.
    for fd in &defs {
        let mut counts = Counts::default();
        for f in files {
            let mut cv = CountVisitor {
                target_type: ty,
                target_field: &fd.name,
                impl_stack: Vec::new(),
                in_write_lhs: false,
                counts: Counts::default(),
            };
            cv.visit_file(&f.ast);
            counts.reads += cv.counts.reads;
            counts.writes += cv.counts.writes;
            counts.inits += cv.counts.inits;
        }
        if !summary {
            println!(
                "{}\t{}\t{}\tr:{}\tw:{}\ti:{}\t{}:{}",
                fd.vis, fd.name, fd.ty, counts.reads, counts.writes, counts.inits, fd.file, fd.line
            );
        }
    }
    eprintln!(
        "({} field(s) on `{}`; use `unruster field-uses {} <field>` for site details)",
        defs.len(),
        ty,
        ty
    );
    Ok(())
}
