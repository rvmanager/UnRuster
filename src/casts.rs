use syn::visit::{self, Visit};

use crate::ast::{print_grouped_counts, top_module_of, type_short, type_to_string};
use crate::parse::{display_path, ParsedFile};
use crate::semantic::{FnSigIndex, FnTypes};

#[derive(Debug)]
struct Hit {
    class: &'static str,
    src: String, // "_" if unknown
    dst: String,
    context: String,
    file: String,
    line: usize,
}

struct CastVisitor<'a> {
    file: &'a str,
    module: &'a str,
    fn_stack: Vec<String>,
    impl_stack: Vec<String>,
    mod_stack: Vec<String>,
    fn_types_stack: Vec<FnTypes>,
    fn_sigs: &'a FnSigIndex,
    hits: Vec<Hit>,
}

impl<'a> CastVisitor<'a> {
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

fn int_width_signed(t: &str) -> Option<(u16, bool)> {
    Some(match t {
        "u8" => (8, false),
        "u16" => (16, false),
        "u32" => (32, false),
        "u64" => (64, false),
        "u128" => (128, false),
        "i8" => (8, true),
        "i16" => (16, true),
        "i32" => (32, true),
        "i64" => (64, true),
        "i128" => (128, true),
        _ => return None,
    })
}

fn is_float(t: &str) -> bool {
    t == "f32" || t == "f64"
}

fn is_usize_family(t: &str) -> bool {
    t == "usize" || t == "isize"
}

fn classify(src: Option<&str>, dst: &str) -> &'static str {
    if dst.starts_with("*const") || dst.starts_with("*mut") {
        return "ptr";
    }
    let src_is_usize = src.map(is_usize_family).unwrap_or(false);
    let dst_is_usize = is_usize_family(dst);
    if src_is_usize && !dst_is_usize && int_width_signed(dst).is_some() {
        return "usize-cross";
    }
    if dst_is_usize && src.is_some() && !src_is_usize {
        return "usize-cross";
    }
    let dst_int = int_width_signed(dst);
    let src_int = src.and_then(int_width_signed);
    if let (Some((sw, sgn_s)), Some((dw, sgn_d))) = (src_int, dst_int) {
        if sw == dw && sgn_s != sgn_d {
            return "signed-flip";
        }
        if dw < sw {
            return "narrow-int";
        }
        if dw > sw {
            return "widen-int";
        }
        return "widen-int"; // same width, same signedness = no-op-ish; bucket as widen
    }
    if let Some(s) = src {
        if is_float(s) && dst_int.is_some() {
            return "float-int";
        }
        if int_width_signed(s).is_some() && is_float(dst) {
            return "int-float";
        }
        if is_float(s) && is_float(dst) {
            return if s == "f64" && dst == "f32" {
                "narrow-float"
            } else {
                "widen-float"
            };
        }
    }
    if src.is_none() {
        return "unknown";
    }
    "other"
}

impl<'ast, 'a> Visit<'ast> for CastVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.mod_stack.push(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.mod_stack.pop();
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        self.fn_types_stack
            .push(FnTypes::build(&i.sig, &i.block, self.fn_sigs));
        visit::visit_item_fn(self, i);
        self.fn_types_stack.pop();
        self.fn_stack.pop();
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.impl_stack.push(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.impl_stack.pop();
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        self.fn_types_stack
            .push(FnTypes::build(&i.sig, &i.block, self.fn_sigs));
        visit::visit_impl_item_fn(self, i);
        self.fn_types_stack.pop();
        self.fn_stack.pop();
    }

    fn visit_expr_cast(&mut self, e: &'ast syn::ExprCast) {
        let dst = type_to_string(&e.ty);
        let src = self
            .fn_types_stack
            .last()
            .and_then(|ft| ft.type_of(&e.expr, self.fn_sigs));
        let class = classify(src.as_deref(), &dst);
        self.hits.push(Hit {
            class,
            src: src.unwrap_or_else(|| "_".into()),
            dst,
            context: self.enclosing(),
            file: self.file.to_string(),
            line: e.as_token.span.start().line,
        });
        visit::visit_expr_cast(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

pub fn run(
    files: &[ParsedFile],
    fn_sigs: &FnSigIndex,
    class_filter: Option<&str>,
    by: Option<&str>,
    no_widen: bool,
    summary: bool,
) -> anyhow::Result<()> {
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = CastVisitor {
            file: &display_path(&f.path),
            module: &f.module,
            fn_stack: Vec::new(),
            impl_stack: Vec::new(),
            mod_stack: Vec::new(),
            fn_types_stack: Vec::new(),
            fn_sigs,
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }

    if let Some(cf) = class_filter {
        let wanted: Vec<&str> = cf.split(',').map(str::trim).collect();
        all.retain(|h| wanted.contains(&h.class));
    }
    if no_widen {
        all.retain(|h| !matches!(h.class, "widen-int" | "widen-float"));
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
                        "{}\t{}\t{}\t{}\t{}:{}",
                        h.class, h.src, h.dst, h.context, h.file, h.line
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
    let break_str: Vec<String> = by_class.iter().map(|(k, n)| format!("{}={}", k, n)).collect();
    eprintln!(
        "({} cast(s); {}; design-smell hint: a hot fn with many casts is usually shape-juggling — pick one type at the boundary, do the cast once, then pass the typed value through.)",
        all.len(),
        break_str.join(", ")
    );
    Ok(())
}
