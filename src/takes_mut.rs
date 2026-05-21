use syn::visit::{self, Visit};

use crate::ast::{is_mut_ref, line_of, type_last_segment, type_short, type_to_string};
use crate::index::NameIndex;
use crate::parse::{display_path, ParsedFile};

#[derive(Debug)]
struct Hit {
    file: String,
    line: usize,
    qpath: String,
    params: Vec<String>, // formatted as `name: &mut Type`
}

struct TakesMutVisitor<'a> {
    target: &'a str,
    file: &'a str,
    module: &'a str,
    impl_stack: Vec<String>,
    mod_stack: Vec<String>,
    out: Vec<Hit>,
}

impl<'a> TakesMutVisitor<'a> {
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

    fn check_sig(&mut self, sig: &syn::Signature) {
        let mut hits: Vec<String> = Vec::new();
        for input in &sig.inputs {
            match input {
                syn::FnArg::Receiver(r) => {
                    // &mut self — only relevant if the impl is on the target type.
                    if r.mutability.is_some() && r.reference.is_some() {
                        if self
                            .impl_stack
                            .last()
                            .map(|t| t == self.target)
                            .unwrap_or(false)
                        {
                            hits.push("&mut self".to_string());
                        }
                    }
                }
                syn::FnArg::Typed(t) => {
                    if is_mut_ref(&t.ty) {
                        if let Some(last) = type_last_segment(&t.ty) {
                            if last == self.target {
                                let pname = match &*t.pat {
                                    syn::Pat::Ident(p) => p.ident.to_string(),
                                    _ => "_".to_string(),
                                };
                                hits.push(format!("{}: {}", pname, type_to_string(&t.ty)));
                            }
                        }
                    }
                }
            }
        }
        if !hits.is_empty() {
            let qpath = self.qualify(&sig.ident.to_string());
            self.out.push(Hit {
                file: self.file.to_string(),
                line: line_of(&sig.ident),
                qpath,
                params: hits,
            });
        }
    }
}

impl<'ast, 'a> Visit<'ast> for TakesMutVisitor<'a> {
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
        self.check_sig(&i.sig);
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.check_sig(&i.sig);
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.check_sig(&i.sig);
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
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = TakesMutVisitor {
            target: ty,
            file: &display_path(&f.path),
            module: &f.module,
            impl_stack: Vec::new(),
            mod_stack: Vec::new(),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.out);
    }
    all.sort_by(|a, b| a.qpath.cmp(&b.qpath).then_with(|| a.file.cmp(&b.file)));

    if !summary {
        for h in &all {
            println!("{}\t{}\t{}:{}", h.qpath, h.params.join(", "), h.file, h.line);
        }
    }
    eprintln!("({} fn(s) take `&mut {}`)", all.len(), ty);
    Ok(())
}
