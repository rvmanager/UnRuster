use syn::visit::{self, Visit};

use crate::ast::{line_of, line_of_span, path_to_string, type_short, type_to_string, vis_str};
use crate::parse::{display_path, ParsedFile};

#[derive(Debug)]
pub struct Item {
    pub kind: &'static str,
    pub name: String,
    pub vis: &'static str,
    pub file: String,
    pub line: usize,
}

struct InventoryVisitor<'a> {
    file: &'a str,
    module: &'a str,
    items: Vec<Item>,
    impl_stack: Vec<String>,
    trait_stack: Vec<String>,
    mod_stack: Vec<String>,
}

impl<'a> InventoryVisitor<'a> {
    fn qualify(&self, name: &str) -> String {
        let mut path: Vec<String> = Vec::new();
        if !self.module.is_empty() {
            path.push(self.module.to_string());
        }
        path.extend(self.mod_stack.iter().cloned());
        if let Some(t) = self.impl_stack.last() {
            path.push(t.clone());
        } else if let Some(t) = self.trait_stack.last() {
            path.push(t.clone());
        }
        path.push(name.to_string());
        path.join("::")
    }

    fn push(&mut self, kind: &'static str, name: String, vis: &'static str, line: usize) {
        self.items.push(Item {
            kind,
            name,
            vis,
            file: self.file.to_string(),
            line,
        });
    }
}

impl<'ast, 'a> Visit<'ast> for InventoryVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        let name = i.ident.to_string();
        self.push("mod", self.qualify(&name), vis_str(&i.vis), line_of(&i.ident));
        self.mod_stack.push(name);
        visit::visit_item_mod(self, i);
        self.mod_stack.pop();
    }

    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        let name = i.ident.to_string();
        self.push("struct", self.qualify(&name), vis_str(&i.vis), line_of(&i.ident));
    }

    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        let name = i.ident.to_string();
        self.push("enum", self.qualify(&name), vis_str(&i.vis), line_of(&i.ident));
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        let name = i.ident.to_string();
        self.push("trait", self.qualify(&name), vis_str(&i.vis), line_of(&i.ident));
        self.trait_stack.push(name);
        for item in &i.items {
            if let syn::TraitItem::Fn(f) = item {
                let qn = self.qualify(&f.sig.ident.to_string());
                self.push("trait-fn", qn, "pub", line_of(&f.sig.ident));
            }
        }
        self.trait_stack.pop();
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        let name = i.sig.ident.to_string();
        self.push("fn", self.qualify(&name), vis_str(&i.vis), line_of(&i.sig.ident));
    }

    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        let name = i.ident.to_string();
        self.push("const", self.qualify(&name), vis_str(&i.vis), line_of(&i.ident));
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        let name = i.ident.to_string();
        self.push("static", self.qualify(&name), vis_str(&i.vis), line_of(&i.ident));
    }

    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        let name = i.ident.to_string();
        self.push("type", self.qualify(&name), vis_str(&i.vis), line_of(&i.ident));
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let self_ty = type_short(&i.self_ty);
        let header = match &i.trait_ {
            Some((bang, trait_path, _)) => {
                let prefix = if bang.is_some() { "!" } else { "" };
                format!(
                    "impl {}{} for {}",
                    prefix,
                    path_to_string(trait_path),
                    type_to_string(&i.self_ty)
                )
            }
            None => format!("impl {}", type_to_string(&i.self_ty)),
        };
        self.push("impl", header, "—", line_of_span(i.impl_token.span));
        self.impl_stack.push(self_ty);
        for item in &i.items {
            if let syn::ImplItem::Fn(f) = item {
                let qn = self.qualify(&f.sig.ident.to_string());
                self.push("fn", qn, vis_str(&f.vis), line_of(&f.sig.ident));
            }
        }
        self.impl_stack.pop();
    }
}

pub fn run(files: &[ParsedFile], kind_filter: Option<&str>) -> anyhow::Result<()> {
    let mut all = Vec::new();
    for f in files {
        let mut v = InventoryVisitor {
            file: &display_path(&f.path),
            module: &f.module,
            items: Vec::new(),
            impl_stack: Vec::new(),
            trait_stack: Vec::new(),
            mod_stack: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.items);
    }

    if let Some(k) = kind_filter {
        all.retain(|i| i.kind == k);
    }

    all.sort_by(|a, b| {
        a.kind
            .cmp(b.kind)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    for it in &all {
        println!(
            "{}\t{}\t{}\t{}:{}",
            it.kind, it.vis, it.name, it.file, it.line
        );
    }
    eprintln!("({} items)", all.len());
    Ok(())
}
