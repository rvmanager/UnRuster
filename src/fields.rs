use syn::visit::Visit;

use crate::ast::{line_of, type_to_string, vis_str};
use crate::context::{warn_unknown_target, AnalysisCtx, TargetNotFound};
use crate::parse::display_path;

#[derive(Debug)]
struct FieldDef {
    name: String,
    ty: String,
    vis: &'static str,
    file: String,
    line: usize,
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

pub fn run(ctx: &AnalysisCtx, ty: &str) -> anyhow::Result<()> {
    let files = ctx.files;
    let summary = ctx.summary;
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
        warn_unknown_target("struct with named fields", ty);
        eprintln!("(0 field(s) on `{}`)", ty);
        return Err(TargetNotFound::err("struct with named fields", ty));
    }

    // 2. Count read/write/init sites per field, via the same strict collector
    //    `field-uses` uses — so these counts equal the sum of its rows.
    for fd in &defs {
        let (reads, writes, inits) =
            crate::field_uses::count_kinds(files, ty, &fd.name, &ctx.sem.fn_sigs);
        if !summary {
            println!(
                "{}\t{}\t{}\tr:{}\tw:{}\ti:{}\t{}:{}",
                fd.vis, fd.name, fd.ty, reads, writes, inits, fd.file, fd.line
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
