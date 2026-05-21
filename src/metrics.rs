use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{line_of, type_short};
use crate::parse::{display_path, ParsedFile};

#[derive(Debug)]
struct FnMetric {
    qpath: String,
    file: String,
    line: usize,
    loc: usize,
    params: usize,
}

#[derive(Debug)]
struct StructMetric {
    qpath: String,
    file: String,
    line: usize,
    fields: usize,
}

#[derive(Debug)]
struct EnumMetric {
    qpath: String,
    file: String,
    line: usize,
    variants: usize,
}

struct MetricsVisitor<'a> {
    file: &'a str,
    module: &'a str,
    impl_stack: Vec<String>,
    mod_stack: Vec<String>,
    fns: &'a mut Vec<FnMetric>,
    structs: &'a mut Vec<StructMetric>,
    enums: &'a mut Vec<EnumMetric>,
}

impl<'a> MetricsVisitor<'a> {
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

    fn record_fn(&mut self, sig: &syn::Signature, body: &syn::Block) {
        let start = line_of(&sig.ident);
        let end = body.span().end().line.max(start);
        let loc = end.saturating_sub(start) + 1;
        let params = sig
            .inputs
            .iter()
            .filter(|i| !matches!(i, syn::FnArg::Receiver(_)))
            .count();
        let qpath = self.qualify(&sig.ident.to_string());
        self.fns.push(FnMetric {
            qpath,
            file: self.file.to_string(),
            line: start,
            loc,
            params,
        });
    }
}

impl<'ast, 'a> Visit<'ast> for MetricsVisitor<'a> {
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
        self.record_fn(&i.sig, &i.block);
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.record_fn(&i.sig, &i.block);
    }

    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        let fields = match &i.fields {
            syn::Fields::Named(n) => n.named.len(),
            syn::Fields::Unnamed(u) => u.unnamed.len(),
            syn::Fields::Unit => 0,
        };
        let qpath = self.qualify(&i.ident.to_string());
        self.structs.push(StructMetric {
            qpath,
            file: self.file.to_string(),
            line: line_of(&i.ident),
            fields,
        });
    }

    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        let qpath = self.qualify(&i.ident.to_string());
        self.enums.push(EnumMetric {
            qpath,
            file: self.file.to_string(),
            line: line_of(&i.ident),
            variants: i.variants.len(),
        });
    }
}

pub fn run(files: &[ParsedFile], sort: &str, top: usize, summary: bool) -> anyhow::Result<()> {
    let mut fns: Vec<FnMetric> = Vec::new();
    let mut structs: Vec<StructMetric> = Vec::new();
    let mut enums: Vec<EnumMetric> = Vec::new();
    for f in files {
        let mut v = MetricsVisitor {
            file: &display_path(&f.path),
            module: &f.module,
            impl_stack: Vec::new(),
            mod_stack: Vec::new(),
            fns: &mut fns,
            structs: &mut structs,
            enums: &mut enums,
        };
        v.visit_file(&f.ast);
    }

    match sort {
        "loc" => fns.sort_by(|a, b| b.loc.cmp(&a.loc).then_with(|| b.params.cmp(&a.params))),
        "params" => fns.sort_by(|a, b| b.params.cmp(&a.params).then_with(|| b.loc.cmp(&a.loc))),
        _ => {
            eprintln!("unknown --sort `{}`; using `loc`", sort);
            fns.sort_by(|a, b| b.loc.cmp(&a.loc));
        }
    }
    structs.sort_by(|a, b| b.fields.cmp(&a.fields));
    enums.sort_by(|a, b| b.variants.cmp(&a.variants));

    if !summary {
        for m in fns.iter().take(top) {
            println!(
                "fn\tloc:{}\tparams:{}\t{}\t{}:{}",
                m.loc, m.params, m.qpath, m.file, m.line
            );
        }
        for m in structs.iter().take(top) {
            println!(
                "struct\tfields:{}\t{}\t{}:{}",
                m.fields, m.qpath, m.file, m.line
            );
        }
        for m in enums.iter().take(top) {
            println!(
                "enum\tvariants:{}\t{}\t{}:{}",
                m.variants, m.qpath, m.file, m.line
            );
        }
    }

    eprintln!(
        "({} fns, {} structs, {} enums; showing top {} each; sort={})",
        fns.len(),
        structs.len(),
        enums.len(),
        top,
        sort
    );
    Ok(())
}
