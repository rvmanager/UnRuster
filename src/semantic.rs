//! Lightweight semantic layer.
//!
//! Three pieces, all syntax-only (no type checking):
//! - `UseMap`: per-file `use` resolution: bare name → qualified path.
//! - `AliasGraph`: `type Foo = Bar;` chains across the tree.
//! - `FnSigIndex` + `FnTypes`: function-local type inference for bindings.
//!
//! Everything here is **approximate** and best-effort. The intent is to close
//! the obvious gaps (re-exports, type aliases, simple local lets) without
//! pulling in a real type system. Anything we can't infer stays as `Unknown`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use syn::visit::{self, Visit};

use crate::ast::type_last_segment;
use crate::index::NameIndex;
use crate::parse::ParsedFile;

// ──────────────────────────────────────────────────────────────────────────
// UseMap — per-file bare-name → qualified path

#[derive(Debug, Default, Clone)]
pub struct UseMap {
    /// `Foo` → `crate::foo::Foo` (or whatever the use brought in).
    pub aliases: BTreeMap<String, String>,
    /// Module prefixes from `use foo::bar::*;` (without trailing `::`).
    pub globs: Vec<String>,
}

impl UseMap {
    pub fn build(file: &syn::File) -> Self {
        let mut um = UseMap::default();
        for item in &file.items {
            if let syn::Item::Use(u) = item {
                let mut prefix = Vec::new();
                collect_uses(&u.tree, &mut prefix, &mut um);
            }
        }
        um
    }

    /// Resolve a bare name to its qualified path, or None if not in scope.
    pub fn resolve(&self, name: &str, index: &NameIndex) -> Option<String> {
        if let Some(q) = self.aliases.get(name) {
            return Some(q.clone());
        }
        for g in &self.globs {
            let candidate = format!("{}::{}", g, name);
            if !index.lookup(&candidate).is_empty() {
                return Some(candidate);
            }
        }
        None
    }
}

fn collect_uses(tree: &syn::UseTree, prefix: &mut Vec<String>, um: &mut UseMap) {
    match tree {
        syn::UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            collect_uses(&p.tree, prefix, um);
            prefix.pop();
        }
        syn::UseTree::Name(n) => {
            let name = n.ident.to_string();
            let q = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}::{}", prefix.join("::"), name)
            };
            um.aliases.insert(name, q);
        }
        syn::UseTree::Rename(r) => {
            let alias = r.rename.to_string();
            let original = r.ident.to_string();
            let q = if prefix.is_empty() {
                original
            } else {
                format!("{}::{}", prefix.join("::"), original)
            };
            um.aliases.insert(alias, q);
        }
        syn::UseTree::Glob(_) => {
            if !prefix.is_empty() {
                um.globs.push(prefix.join("::"));
            }
        }
        syn::UseTree::Group(g) => {
            for inner in &g.items {
                collect_uses(inner, prefix, um);
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// AliasGraph — type aliases

#[derive(Debug, Default, Clone)]
pub struct AliasGraph {
    /// alias-last-name → target-last-name (one hop)
    pub aliases: BTreeMap<String, String>,
}

impl AliasGraph {
    pub fn build(files: &[ParsedFile]) -> Self {
        let mut g = AliasGraph::default();
        for f in files {
            let mut v = AliasVisitor { out: &mut g.aliases };
            v.visit_file(&f.ast);
        }
        g
    }

    /// Follow alias chain to canonical last-name. Cycle-safe.
    pub fn canonical(&self, name: &str) -> String {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut cur = name.to_string();
        while let Some(next) = self.aliases.get(&cur) {
            if !seen.insert(cur.clone()) {
                break;
            }
            cur = next.clone();
        }
        cur
    }

    /// Return `name` plus all aliases that resolve to the same canonical target.
    pub fn synonyms(&self, name: &str) -> Vec<String> {
        let canon = self.canonical(name);
        let mut out: Vec<String> = vec![canon.clone()];
        // Forward chain (from `name` through to canonical).
        let mut cur = name.to_string();
        while let Some(next) = self.aliases.get(&cur) {
            if !out.contains(&cur) {
                out.push(cur.clone());
            }
            if next == &cur {
                break;
            }
            cur = next.clone();
        }
        // Reverse: BFS over `alias → target == current`.
        let mut work = vec![canon.clone()];
        let mut visited: BTreeSet<String> = BTreeSet::new();
        while let Some(t) = work.pop() {
            if !visited.insert(t.clone()) {
                continue;
            }
            for (alias, target) in &self.aliases {
                if target == &t {
                    if !out.contains(alias) {
                        out.push(alias.clone());
                    }
                    if !visited.contains(alias) {
                        work.push(alias.clone());
                    }
                }
            }
        }
        out
    }
}

struct AliasVisitor<'a> {
    out: &'a mut BTreeMap<String, String>,
}

impl<'ast, 'a> Visit<'ast> for AliasVisitor<'a> {
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        if let Some(target) = type_last_segment(&i.ty) {
            self.out.insert(i.ident.to_string(), target);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// FnSigIndex — fn name → return-type last segment (best-effort)

#[derive(Debug, Default, Clone)]
pub struct FnSigIndex {
    /// fn last-name → return-type last segment. If a name has multiple defns
    /// with conflicting return types, the entry is removed (ambiguous).
    pub by_last: BTreeMap<String, Option<String>>,
}

impl FnSigIndex {
    pub fn build(files: &[ParsedFile]) -> Self {
        let mut idx = FnSigIndex::default();
        for f in files {
            let mut v = SigVisitor { out: &mut idx.by_last };
            v.visit_file(&f.ast);
        }
        // Drop ambiguous entries (Some(None) means conflict).
        idx.by_last.retain(|_, v| v.is_some());
        idx
    }

    pub fn return_type(&self, fn_last: &str) -> Option<&str> {
        self.by_last.get(fn_last).and_then(|v| v.as_deref())
    }
}

struct SigVisitor<'a> {
    out: &'a mut BTreeMap<String, Option<String>>,
}

impl<'a> SigVisitor<'a> {
    fn record(&mut self, name: String, ret: Option<String>) {
        match self.out.get(&name) {
            None => {
                self.out.insert(name, ret);
            }
            Some(Some(existing)) if Some(existing) == ret.as_ref() => {}
            Some(_) => {
                // Conflict (different return types) — mark ambiguous.
                self.out.insert(name, None);
            }
        }
    }
}

impl<'ast, 'a> Visit<'ast> for SigVisitor<'a> {
    fn visit_signature(&mut self, sig: &'ast syn::Signature) {
        let ret = match &sig.output {
            syn::ReturnType::Default => None,
            syn::ReturnType::Type(_, ty) => type_last_segment(ty),
        };
        self.record(sig.ident.to_string(), ret);
    }
}

// ──────────────────────────────────────────────────────────────────────────
// FnTypes — per-function-body binding → type-name map

#[derive(Debug, Default, Clone)]
pub struct FnTypes {
    /// binding-name → type last-name
    pub bindings: BTreeMap<String, String>,
}

impl FnTypes {
    pub fn build(sig: &syn::Signature, body: &syn::Block, sigs: &FnSigIndex) -> Self {
        let mut ft = FnTypes::default();
        // Parameters.
        for input in &sig.inputs {
            if let syn::FnArg::Typed(t) = input {
                if let Some(name) = pat_first_ident(&t.pat) {
                    if let Some(last) = type_last_segment(&t.ty) {
                        ft.bindings.insert(name, last);
                    }
                }
            }
        }
        // Walk body for `let`s.
        let mut v = TypeInferVisitor { ft: &mut ft, sigs };
        v.visit_block(body);
        ft
    }

    /// Best-effort type of `expr`. Returns the last-segment type name, or None.
    pub fn type_of(&self, expr: &syn::Expr, sigs: &FnSigIndex) -> Option<String> {
        infer_expr_type(expr, sigs, &self.bindings)
    }
}

struct TypeInferVisitor<'a> {
    ft: &'a mut FnTypes,
    sigs: &'a FnSigIndex,
}

impl<'ast, 'a> Visit<'ast> for TypeInferVisitor<'a> {
    fn visit_local(&mut self, l: &'ast syn::Local) {
        let name_opt = pat_first_ident(&l.pat);

        // Type annotation (`let x: Type = ...;` — Pat::Type)
        if let syn::Pat::Type(pt) = &l.pat {
            if let Some(name) = pat_first_ident(&pt.pat) {
                if let Some(ty) = type_last_segment(&pt.ty) {
                    self.ft.bindings.insert(name, ty);
                }
            }
        } else if let Some(name) = name_opt {
            if let Some(init) = &l.init {
                if let Some(ty) = infer_expr_type(&init.expr, self.sigs, &self.ft.bindings) {
                    self.ft.bindings.insert(name, ty);
                }
            }
        }
        visit::visit_local(self, l);
    }
}

fn pat_first_ident(p: &syn::Pat) -> Option<String> {
    match p {
        syn::Pat::Ident(i) => Some(i.ident.to_string()),
        syn::Pat::Type(pt) => pat_first_ident(&pt.pat),
        syn::Pat::Reference(r) => pat_first_ident(&r.pat),
        syn::Pat::Paren(p) => pat_first_ident(&p.pat),
        _ => None,
    }
}

fn infer_expr_type(
    e: &syn::Expr,
    sigs: &FnSigIndex,
    bindings: &BTreeMap<String, String>,
) -> Option<String> {
    match e {
        syn::Expr::Path(p) => {
            if p.path.segments.len() == 1 {
                let name = p.path.segments[0].ident.to_string();
                return bindings.get(&name).cloned();
            }
            None
        }
        syn::Expr::Call(c) => {
            if let syn::Expr::Path(p) = &*c.func {
                let segs: Vec<&syn::PathSegment> = p.path.segments.iter().collect();
                // `Type::new(...)` / `Type::default()` / `Type::from(...)` / `Type::with_capacity(...)`
                if segs.len() >= 2 {
                    let last = &segs[segs.len() - 1].ident;
                    let pen = &segs[segs.len() - 2].ident;
                    if first_is_uppercase(&pen.to_string())
                        && matches!(
                            last.to_string().as_str(),
                            "new" | "default" | "from" | "with_capacity" | "from_str" | "empty"
                        )
                    {
                        return Some(pen.to_string());
                    }
                }
                // Bare `fn_name(...)`: look up return type.
                if segs.len() == 1 {
                    return sigs
                        .return_type(&segs[0].ident.to_string())
                        .map(str::to_string);
                }
            }
            None
        }
        syn::Expr::Struct(s) => s.path.segments.last().map(|seg| seg.ident.to_string()),
        syn::Expr::Cast(c) => type_last_segment(&c.ty),
        syn::Expr::Reference(r) => infer_expr_type(&r.expr, sigs, bindings),
        syn::Expr::Paren(p) => infer_expr_type(&p.expr, sigs, bindings),
        syn::Expr::Group(g) => infer_expr_type(&g.expr, sigs, bindings),
        syn::Expr::Try(t) => infer_expr_type(&t.expr, sigs, bindings),
        _ => None,
    }
}

fn first_is_uppercase(s: &str) -> bool {
    s.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
}

// ──────────────────────────────────────────────────────────────────────────
// Bundle — built once at startup, threaded into commands that benefit.

pub struct Semantic {
    pub uses: BTreeMap<PathBuf, UseMap>,
    pub aliases: AliasGraph,
    pub fn_sigs: FnSigIndex,
}

impl Semantic {
    pub fn build(files: &[ParsedFile]) -> Self {
        let mut uses = BTreeMap::new();
        for f in files {
            uses.insert(f.path.clone(), UseMap::build(&f.ast));
        }
        Semantic {
            uses,
            aliases: AliasGraph::build(files),
            fn_sigs: FnSigIndex::build(files),
        }
    }

    pub fn uses_for(&self, path: &std::path::Path) -> Option<&UseMap> {
        self.uses.get(path)
    }
}

