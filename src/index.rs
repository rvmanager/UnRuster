use std::collections::HashMap;

use syn::visit::{self, Visit};

use crate::ast::{
    has_allow_dead_code, line_of, line_of_span, path_to_string, type_short, type_to_string,
    vis_str, ScopeTracker,
};
use crate::parse::{display_path, ParsedFile};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Defn {
    /// "struct" | "enum" | "trait" | "fn" | "const" | "static" | "type" |
    /// "mod" | "impl" | "impl-fn" | "trait-fn"
    pub kind: &'static str,
    pub name: String,
    pub qpath: String,
    pub file: String,
    pub line: usize,
    pub vis: &'static str,
    pub module: String,
    /// For impl-fn: the self-type name. For trait-fn: the trait name. For "impl"
    /// header: the self-type. None for free items.
    pub owner: Option<String>,
    /// For "impl" headers: the trait being implemented, if any.
    pub trait_name: Option<String>,
    /// True for impl-fn defns whose enclosing `impl` block is a trait impl
    /// (i.e. `impl SomeTrait for T { fn ... }`). False for inherent impls and
    /// for non-fn defns. Used by `dead-code` to skip dynamically-dispatched
    /// trait methods.
    pub in_trait_impl: bool,
    /// True if the item (or its enclosing impl block) carries `#[allow(dead_code)]`.
    /// `dead-code` skips these to respect the author's explicit opt-out.
    pub allow_dead: bool,
}

pub struct NameIndex {
    pub defns: Vec<Defn>,
    by_last: HashMap<String, Vec<usize>>,
}

#[allow(dead_code)]
impl NameIndex {
    pub fn build(files: &[ParsedFile]) -> Self {
        let mut defns: Vec<Defn> = Vec::new();
        for f in files {
            let mut v = IndexVisitor {
                file: &display_path(&f.path),
                scope: ScopeTracker::new(f.module.as_str()),
                out: &mut defns,
            };
            v.visit_file(&f.ast);
        }
        let mut by_last: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, d) in defns.iter().enumerate() {
            by_last.entry(d.name.clone()).or_default().push(i);
        }
        NameIndex { defns, by_last }
    }

    /// Lookup by bare name or qualified suffix (`Type` or `module::Type`).
    /// For bare names, returns all defns whose last segment matches.
    /// For qualified, returns defns whose qpath ends with the query.
    pub fn lookup(&self, query: &str) -> Vec<&Defn> {
        let last = query.rsplit("::").next().unwrap_or(query);
        let Some(ids) = self.by_last.get(last) else {
            return Vec::new();
        };
        if query.contains("::") {
            let suffix = format!("::{}", query);
            ids.iter()
                .filter_map(|&i| {
                    let d = &self.defns[i];
                    if d.qpath == query || d.qpath.ends_with(&suffix) {
                        Some(d)
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            ids.iter().map(|&i| &self.defns[i]).collect()
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Defn> {
        self.defns.iter()
    }

    /// True if any definition with the given last-segment name exists.
    pub fn knows_name(&self, last: &str) -> bool {
        self.by_last.contains_key(last)
    }

    /// Sorted, deduplicated names of every enum defined in the tree.
    /// Used by the `--all` modes of the enum-target commands.
    pub fn enum_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .defns
            .iter()
            .filter(|d| d.kind == "enum")
            .map(|d| d.name.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

struct IndexVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    out: &'a mut Vec<Defn>,
}

impl<'a> IndexVisitor<'a> {
    fn qualify(&self, name: &str) -> String {
        self.scope.qualify(name)
    }

    // `push()` is only invoked for top-level items (struct/enum/trait/fn/etc.),
    // never during impl/trait iteration — those construct Defns directly. So
    // the "current owner" at push time is always None.
    fn current_owner(&self) -> Option<String> {
        None
    }

    fn current_module(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.scope.module.is_empty() {
            parts.push(self.scope.module.clone());
        }
        parts.extend(self.scope.mod_stack.iter().cloned());
        parts.join("::")
    }

    fn push(&mut self, kind: &'static str, name: String, vis: &'static str, line: usize) {
        self.push_with(kind, name, vis, line, false);
    }

    fn push_with(
        &mut self,
        kind: &'static str,
        name: String,
        vis: &'static str,
        line: usize,
        allow_dead: bool,
    ) {
        let qpath = self.qualify(&name);
        self.out.push(Defn {
            kind,
            name,
            qpath,
            file: self.file.to_string(),
            line,
            vis,
            module: self.current_module(),
            owner: self.current_owner(),
            trait_name: None,
            in_trait_impl: false,
            allow_dead,
        });
    }
}

impl<'ast, 'a> Visit<'ast> for IndexVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        let name = i.ident.to_string();
        self.push("mod", name.clone(), vis_str(&i.vis), line_of(&i.ident));
        self.scope.enter_mod(name);
        visit::visit_item_mod(self, i);
        self.scope.leave_mod();
    }

    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.push(
            "struct",
            i.ident.to_string(),
            vis_str(&i.vis),
            line_of(&i.ident),
        );
    }

    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.push(
            "enum",
            i.ident.to_string(),
            vis_str(&i.vis),
            line_of(&i.ident),
        );
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        let name = i.ident.to_string();
        self.push("trait", name.clone(), vis_str(&i.vis), line_of(&i.ident));
        self.scope.enter_trait(name);
        for item in &i.items {
            if let syn::TraitItem::Fn(f) = item {
                let fname = f.sig.ident.to_string();
                let qpath = self.qualify(&fname);
                self.out.push(Defn {
                    kind: "trait-fn",
                    name: fname,
                    qpath,
                    file: self.file.to_string(),
                    line: line_of(&f.sig.ident),
                    vis: "pub",
                    module: self.current_module(),
                    owner: self.scope.trait_stack.last().cloned(),
                    trait_name: None,
                    in_trait_impl: false,
                    allow_dead: has_allow_dead_code(&f.attrs),
                });
            }
        }
        self.scope.leave_trait();
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.push_with(
            "fn",
            i.sig.ident.to_string(),
            vis_str(&i.vis),
            line_of(&i.sig.ident),
            has_allow_dead_code(&i.attrs),
        );
    }

    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.push(
            "const",
            i.ident.to_string(),
            vis_str(&i.vis),
            line_of(&i.ident),
        );
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.push(
            "static",
            i.ident.to_string(),
            vis_str(&i.vis),
            line_of(&i.ident),
        );
    }

    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        self.push(
            "type",
            i.ident.to_string(),
            vis_str(&i.vis),
            line_of(&i.ident),
        );
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let self_ty = type_short(&i.self_ty);
        let trait_name = i.trait_.as_ref().and_then(|(_, p, _)| {
            p.segments.last().map(|s| s.ident.to_string())
        });
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
        let module = self.current_module();
        let is_trait_impl = trait_name.is_some();
        let impl_block_allow = has_allow_dead_code(&i.attrs);
        self.out.push(Defn {
            kind: "impl",
            name: self_ty.clone(),
            qpath: header,
            file: self.file.to_string(),
            line: line_of_span(i.impl_token.span),
            vis: "—",
            module: module.clone(),
            owner: Some(self_ty.clone()),
            trait_name,
            in_trait_impl: false,
            allow_dead: impl_block_allow,
        });
        self.scope.enter_impl(self_ty);
        for item in &i.items {
            if let syn::ImplItem::Fn(f) = item {
                let fname = f.sig.ident.to_string();
                let qpath = self.qualify(&fname);
                self.out.push(Defn {
                    kind: "impl-fn",
                    name: fname,
                    qpath,
                    file: self.file.to_string(),
                    line: line_of(&f.sig.ident),
                    vis: vis_str(&f.vis),
                    module: module.clone(),
                    owner: self.scope.impl_stack.last().cloned(),
                    trait_name: None,
                    in_trait_impl: is_trait_impl,
                    allow_dead: impl_block_allow || has_allow_dead_code(&f.attrs),
                });
            }
        }
        self.scope.leave_impl();
    }
}
