use syn::visit::Visit;

use crate::ast::{line_of, type_to_string, vis_str};
use crate::context::AnalysisCtx;
use crate::parse::display_path;
use crate::emit::{row, site};

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

pub fn run(ctx: &AnalysisCtx, ty: &str) -> anyhow::Result<usize> {
    // Targets resolve by last `::` segment throughout this tool — the playbook
    // says so, `impls --of` and `callers` already do it, and `show` prints the
    // *qualified* path in its header row. These three commands compared the raw
    // string against a bare `ident`, so the qualified name a reader had just
    // read off `show` silently matched nothing: `fields index::Defn` answered
    // "(0 field(s))" plus a note contradicting itself ("not as a struct with
    // named fields — it is: struct"). A command that says "none" for a copied
    // name teaches the reader it does not work.
    let ty = crate::ast::last_segment(ty);
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
        ctx.out.summary(&format!("(0 field(s) on `{}`)", ty));
        return Err(ctx.unknown_target("struct with named fields", ty));
    }

    // 2. Count read/write/init sites per field, via the same strict collector
    //    `field-uses` uses — so these counts equal the sum of its rows.
    for fd in &defs {
        let (reads, writes, inits) =
            crate::field_uses::count_kinds(files, ty, &fd.name, &ctx.sem.fn_sigs);
        if !summary {
            row!(
                ctx.out,
                "vis" => fd.vis,
                "name" => fd.name.clone(),
                "type" => fd.ty.clone(),
                "reads" => format!("r:{}", reads),
                "writes" => format!("w:{}", writes),
                "inits" => format!("i:{}", inits),
                "at" => site(&fd.file, fd.line),
            );
        }
    }
    ctx.out.summary(&format!(
        "({} field(s) on `{}`; use `unruster field-uses {} <field>` for site details)",
        defs.len(),
        ty,
        ty
    ));
    Ok(defs.len())
}
