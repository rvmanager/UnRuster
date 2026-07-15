use syn::visit::{self, Visit};

use crate::ast::{
    line_of, line_of_span, path_to_string, type_short, type_to_string, vis_str, ScopeTracker,
};
use crate::context::AnalysisCtx;
use crate::parse::display_path;

#[derive(Debug)]
pub struct Item {
    pub kind: &'static str,
    pub name: String,
    pub vis: &'static str,
    pub file: String,
    pub line: usize,
}

/// `--kind` filter values. Kebab-cased by clap (TraitFn → `trait-fn`).
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ItemKind {
    Struct,
    Enum,
    Trait,
    Fn,
    Impl,
    Mod,
    Const,
    Static,
    Type,
    TraitFn,
    ImplFn,
}

impl ItemKind {
    fn as_str(self) -> &'static str {
        match self {
            ItemKind::Struct => "struct",
            ItemKind::Enum => "enum",
            ItemKind::Trait => "trait",
            ItemKind::Fn => "fn",
            ItemKind::Impl => "impl",
            ItemKind::Mod => "mod",
            ItemKind::Const => "const",
            ItemKind::Static => "static",
            ItemKind::Type => "type",
            ItemKind::TraitFn => "trait-fn",
            ItemKind::ImplFn => "impl-fn",
        }
    }
}

/// `--vis` filter values.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum VisFilter {
    Pub,
    Crate,
    Priv,
}

impl VisFilter {
    fn as_str(self) -> &'static str {
        match self {
            VisFilter::Pub => "pub",
            VisFilter::Crate => "pub(crate)",
            VisFilter::Priv => "priv",
        }
    }
}

struct InventoryVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    items: Vec<Item>,
}

impl<'a> InventoryVisitor<'a> {
    fn qualify(&self, name: &str) -> String {
        self.scope.qualify(name)
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
        self.scope.enter_mod(name);
        visit::visit_item_mod(self, i);
        self.scope.leave_mod();
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
        self.scope.enter_trait(name);
        for item in &i.items {
            if let syn::TraitItem::Fn(f) = item {
                let qn = self.qualify(&f.sig.ident.to_string());
                self.push("trait-fn", qn, "pub", line_of(&f.sig.ident));
            }
        }
        self.scope.leave_trait();
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
        self.scope.enter_impl(self_ty);
        for item in &i.items {
            if let syn::ImplItem::Fn(f) = item {
                let qn = self.qualify(&f.sig.ident.to_string());
                // "impl-fn", matching the NameIndex kind — `fn` means free fns.
                self.push("impl-fn", qn, vis_str(&f.vis), line_of(&f.sig.ident));
            }
        }
        self.scope.leave_impl();
    }
}

pub fn run(
    ctx: &AnalysisCtx,
    kind_filter: Option<ItemKind>,
    vis_filter: Option<VisFilter>,
    tree: bool,
) -> anyhow::Result<usize> {
    let files = ctx.files;
    let summary = ctx.summary;
    let mut all = Vec::new();
    for f in files {
        let mut v = InventoryVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()),
            items: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.items);
    }

    if let Some(k) = kind_filter {
        all.retain(|i| i.kind == k.as_str());
    }
    if let Some(v) = vis_filter {
        all.retain(|i| i.vis == v.as_str());
    }

    if tree {
        print_tree(&all, summary);
    } else {
        all.sort_by(|a, b| {
            a.kind
                .cmp(b.kind)
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.line.cmp(&b.line))
        });
        if !summary {
            for it in &all {
                println!(
                    "{}\t{}\t{}\t{}:{}",
                    it.kind, it.vis, it.name, it.file, it.line
                );
            }
        }
    }
    eprintln!("({} items)", all.len());
    Ok(all.len())
}

fn print_tree(items: &[Item], summary: bool) {
    if summary {
        return;
    }
    use std::collections::BTreeMap;
    // Group by leading module path. Items with empty module path go under "<crate>".
    let mut by_mod: BTreeMap<String, Vec<&Item>> = BTreeMap::new();
    for it in items {
        // Use the prefix of `name` before the type name (last segment).
        // e.g. "inventory::InventoryVisitor::push" -> module path = "inventory"
        // For free fns "inventory::run" -> module path = "inventory"
        // For top-level "main" -> "<crate>"
        let module_path = match it.kind {
            "mod" => it.name.clone(),
            _ => {
                let segs: Vec<&str> = it.name.split("::").collect();
                if segs.len() <= 1 {
                    "<crate>".to_string()
                } else {
                    // Drop the trailing item identifier(s).
                    // Impl-fn looks like "module::Type::fn"; we want "module".
                    // Heuristic: keep segments that start with lowercase.
                    let mut keep = Vec::new();
                    for s in &segs[..segs.len() - 1] {
                        let first = s.chars().next().unwrap_or('A');
                        if first.is_ascii_uppercase() {
                            break;
                        }
                        keep.push(*s);
                    }
                    if keep.is_empty() {
                        "<crate>".to_string()
                    } else {
                        keep.join("::")
                    }
                }
            }
        };
        by_mod.entry(module_path).or_default().push(it);
    }

    for (m, items) in &by_mod {
        println!("{}\t({} items)", m, items.len());
        let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
        for it in items {
            *by_kind.entry(it.kind).or_insert(0) += 1;
        }
        for (kind, n) in &by_kind {
            println!("  {}\t{}", n, kind);
        }
        // List items by kind, sorted within each group.
        let mut grouped: BTreeMap<&str, Vec<&Item>> = BTreeMap::new();
        for it in items {
            grouped.entry(it.kind).or_default().push(it);
        }
        for (kind, mut its) in grouped {
            its.sort_by_key(|i| &i.name);
            for it in its {
                println!("    {}\t{}\t{}\t{}:{}", kind, it.vis, it.name, it.file, it.line);
            }
        }
    }
}
