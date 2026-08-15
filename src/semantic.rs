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
    /// Build a file's use-map, given the file's own module path so that
    /// `crate`, `self` and `super` prefixes become real module paths.
    ///
    /// Without this a glob is stored as written, and `use super::*;` — the
    /// commonest glob in a `mod.rs`-shaped crate — was the literal string
    /// `"super"`. Nothing was ever indexed under `super::…`, so the glob
    /// resolved nothing and `geom::boolean::dist` (the name a reader writes
    /// from a call site *inside* `boolean.rs`, where `dist` is glob-imported
    /// from the parent) reported "no item named". The suggestion list then
    /// offered six near-misses in other modules and not the answer.
    pub fn build_in(file: &syn::File, module: &str) -> Self {
        Self::build_in_items(&file.items, module)
    }

    /// As [`build_in`](Self::build_in), for one *block* of items rather than a
    /// whole file — the contents of an inline `mod foo { … }`.
    ///
    /// A file's use-map reads only its top-level items, which is right for the
    /// one-module-per-file crate this tool mostly meets and wrong for the
    /// commonest exception: `mod tests { use super::*; }`, and any crate that
    /// nests modules inline. `module-uses` on such a crate saw the `use` line
    /// and then missed every bare reference it enabled, because the imports
    /// were a scope down from where anything looked for them.
    ///
    /// Callers that walk into inline modules keep a stack of these and resolve
    /// innermost-first; a caller that only ever sees a file keeps using
    /// `build_in` and is unaffected.
    pub fn build_in_items(items: &[syn::Item], module: &str) -> Self {
        let mut um = UseMap::default();
        for item in items {
            if let syn::Item::Use(u) = item {
                let mut prefix = Vec::new();
                collect_uses(&u.tree, &mut prefix, &mut um);
            }
        }
        for g in &mut um.globs {
            *g = normalize_prefix(g, module);
        }
        for q in um.aliases.values_mut() {
            *q = normalize_prefix(q, module);
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

/// Rewrite a `crate` / `self` / `super` head into the module path it names,
/// from the point of view of a file whose own module is `module`.
///
/// `crate` maps to the empty root because that is how this tool spells qpaths:
/// `geom::dist`, never `crate::geom::dist`. A `super` that walks past the root
/// is left alone rather than guessed at — a wrong path resolves to nothing and
/// says nothing, where the unchanged one at least reads as what was written.
fn normalize_prefix(path: &str, module: &str) -> String {
    let mut segs: Vec<&str> = path.split("::").collect();
    let mut here: Vec<&str> = if module.is_empty() {
        Vec::new()
    } else {
        module.split("::").collect()
    };
    match segs.first() {
        Some(&"crate") => {
            segs.remove(0);
            here.clear();
        }
        Some(&"self") => {
            segs.remove(0);
        }
        Some(&"super") => {
            while segs.first() == Some(&"super") {
                segs.remove(0);
                if here.pop().is_none() {
                    return path.to_string();
                }
            }
        }
        _ => return path.to_string(),
    }
    here.extend(segs);
    here.join("::")
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
        // Forward chain (from `name` through to canonical). Cycle-safe like
        // `canonical`: the scanned tree may not compile, and `type A = B;
        // type B = A;` must not spin this walk forever.
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut cur = name.to_string();
        while let Some(next) = self.aliases.get(&cur) {
            if !seen.insert(cur.clone()) {
                break;
            }
            if !out.contains(&cur) {
                out.push(cur.clone());
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
    /// (struct last-name, field name) → field-type last segment. Lets the
    /// local inferencer resolve `x.field` / `self.field` receivers.
    pub field_types: BTreeMap<(String, String), String>,
}

impl FnSigIndex {
    pub fn build(files: &[ParsedFile]) -> Self {
        let mut idx = FnSigIndex::default();
        for f in files {
            let mut v = SigVisitor { out: &mut idx.by_last };
            v.visit_file(&f.ast);
            let mut fv = FieldTypeVisitor { out: &mut idx.field_types };
            fv.visit_file(&f.ast);
        }
        // Drop ambiguous entries (Some(None) means conflict).
        idx.by_last.retain(|_, v| v.is_some());
        idx
    }

    pub fn return_type(&self, fn_last: &str) -> Option<&str> {
        self.by_last.get(fn_last).and_then(|v| v.as_deref())
    }

    /// Field type (last segment) of `ty_last.field`, if that struct is known.
    pub fn field_type(&self, ty_last: &str, field: &str) -> Option<&str> {
        self.field_types
            .get(&(ty_last.to_string(), field.to_string()))
            .map(String::as_str)
    }
}

struct FieldTypeVisitor<'a> {
    out: &'a mut BTreeMap<(String, String), String>,
}

impl<'ast, 'a> Visit<'ast> for FieldTypeVisitor<'a> {
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        if let syn::Fields::Named(fs) = &i.fields {
            for f in &fs.named {
                if let (Some(id), Some(ty)) = (&f.ident, type_last_segment(&f.ty)) {
                    self.out
                        .insert((i.ident.to_string(), id.to_string()), ty);
                }
            }
        }
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
    /// Bindings whose type came from the *name-only* method-return fallback:
    /// `let w = pixmap.width();` resolves `width` through a tree-wide map of
    /// fn name → return type, with no check that the receiver is the type that
    /// defines it. When the receiver is external (`tiny_skia::Pixmap`) and some
    /// unrelated local type happens to define a same-named method, the answer
    /// is confidently wrong.
    ///
    /// Callers that would *report* the type to a reader (cast classification)
    /// must use [`FnTypes::type_of_grounded`], which refuses to guess. Callers
    /// that only use it to narrow a search (`field-uses via=ti`, already
    /// labelled APPROXIMATE) keep the looser [`FnTypes::type_of`].
    pub guessed: std::collections::BTreeSet<String>,
}

impl FnTypes {
    pub fn build(
        sig: &syn::Signature,
        body: &syn::Block,
        sigs: &FnSigIndex,
        self_ty: Option<&str>,
    ) -> Self {
        let mut ft = FnTypes::default();
        // `self` receiver: typed by the enclosing impl block, when known.
        // `Self` (the type) resolves there too, in any associated fn.
        if let Some(t) = self_ty {
            ft.bindings.insert("Self".to_string(), t.to_string());
            if sig
                .inputs
                .first()
                .map(|i| matches!(i, syn::FnArg::Receiver(_)))
                .unwrap_or(false)
            {
                ft.bindings.insert("self".to_string(), t.to_string());
            }
        }
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

    /// Like [`FnTypes::type_of`], but `None` unless the answer is *grounded* —
    /// traceable to a type annotation, a parameter, a struct literal, an
    /// explicit cast, or arithmetic over those. Never guesses a method's return
    /// type from its bare name. Use this wherever the type appears in output.
    pub fn type_of_grounded(&self, expr: &syn::Expr, sigs: &FnSigIndex) -> Option<String> {
        if !is_grounded(expr, sigs, &self.guessed) {
            return None;
        }
        infer_expr_type(expr, sigs, &self.bindings)
    }
}

struct TypeInferVisitor<'a> {
    ft: &'a mut FnTypes,
    sigs: &'a FnSigIndex,
}

impl<'ast, 'a> Visit<'ast> for TypeInferVisitor<'a> {
    /// `for i in 0..n` types `i` as `n`'s type — usually `usize`, since the
    /// bound is nearly always a `.len()`. Index arithmetic built from loop
    /// variables is the single most common source of the casts this tool is
    /// asked about, and without this arm every one of them was type-unknown.
    fn visit_expr_for_loop(&mut self, f: &'ast syn::ExprForLoop) {
        // `for (i, x) in xs.iter().enumerate()` types `i` as `usize` — the
        // index half of an enumerate tuple is `usize` by definition, and index
        // arithmetic built on it is exactly what later gets cast.
        if let (syn::Pat::Tuple(t), syn::Expr::MethodCall(mc)) = (&*f.pat, &*f.expr) {
            if mc.method == "enumerate" {
                if let Some(idx) = t.elems.first().and_then(pat_first_ident) {
                    self.ft.bindings.insert(idx, "usize".to_string());
                }
            }
        }
        if let (Some(name), syn::Expr::Range(r)) = (pat_first_ident(&f.pat), &*f.expr) {
            let bound = r
                .start
                .as_deref()
                .and_then(|e| infer_expr_type(e, self.sigs, &self.ft.bindings))
                .or_else(|| {
                    r.end
                        .as_deref()
                        .and_then(|e| infer_expr_type(e, self.sigs, &self.ft.bindings))
                });
            if let Some(ty) = bound {
                self.ft.bindings.insert(name, ty);
            }
        }
        visit::visit_expr_for_loop(self, f);
    }

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
                    if !is_grounded(&init.expr, self.sigs, &self.ft.guessed) {
                        self.ft.guessed.insert(name.clone());
                    }
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
        syn::Expr::Call(c) => infer_call_type(c, sigs, bindings),
        // `Type { .. }` literal; `Self { .. }` resolves through the enclosing
        // impl's type (the `Self` binding).
        syn::Expr::Struct(s) => {
            let last = s.path.segments.last().map(|seg| seg.ident.to_string())?;
            if last == "Self" {
                return bindings.get("Self").cloned();
            }
            Some(last)
        }
        // `base.field` — resolve through the struct-field type map.
        syn::Expr::Field(f) => {
            let base = infer_expr_type(&f.base, sigs, bindings)?;
            if let syn::Member::Named(id) = &f.member {
                sigs.field_type(&base, &id.to_string()).map(str::to_string)
            } else {
                None
            }
        }
        // `expr.method()` — best-effort by unique method return type.
        syn::Expr::MethodCall(mc) => {
            let name = mc.method.to_string();
            // `.len()` / `.count()` are `usize` on every std collection, and on
            // essentially every hand-rolled one. Without this, `buf.len() as
            // u32` reported a `_` source and landed in the `unknown` class,
            // which no check reports — the honest-but-useless outcome of
            // refusing to guess at all.
            if is_std_usize_method(&name, sigs) {
                return Some("usize".to_string());
            }
            sigs.return_type(&name).map(str::to_string)
        }
        syn::Expr::Cast(c) => type_last_segment(&c.ty),
        syn::Expr::Reference(r) => infer_expr_type(&r.expr, sigs, bindings),
        syn::Expr::Paren(p) => infer_expr_type(&p.expr, sigs, bindings),
        syn::Expr::Group(g) => infer_expr_type(&g.expr, sigs, bindings),
        syn::Expr::Try(t) => infer_expr_type(&t.expr, sigs, bindings),
        // Arithmetic preserves the operand type in Rust — `a * b` is only
        // well-typed when both sides agree. Without this arm every computed
        // width/stride was type-unknown, so its later cast was reported with a
        // `_` source (or, worse, inherited a guessed type from a sibling let).
        syn::Expr::Binary(b) if is_arith(&b.op) => {
            let l = infer_expr_type(&b.left, sigs, bindings);
            let r = infer_expr_type(&b.right, sigs, bindings);
            match (l, r) {
                (Some(a), Some(c)) if a == c => Some(a),
                // One side unknown (often a `const`): the known side still
                // types the expression, since the other must match it.
                (Some(a), None) => Some(a),
                (None, Some(c)) => Some(c),
                _ => None,
            }
        }
        syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => {
            infer_expr_type(&u.expr, sigs, bindings)
        }
        syn::Expr::Lit(l) => numeric_lit_type(l),
        _ => None,
    }
}

/// Method names that yield `usize` by universal convention. Returns false when
/// the scanned tree defines the same name with a different return type, so a
/// project with its own `fn len(&self) -> f64` is not mis-typed.
fn is_std_usize_method(name: &str, sigs: &FnSigIndex) -> bool {
    if !matches!(name, "len" | "count") {
        return false;
    }
    match sigs.return_type(name) {
        // Tree defines it and agrees, or defines it ambiguously (entry
        // dropped) — the convention holds either way.
        Some(t) => t == "usize",
        None => true,
    }
}

/// Arithmetic operators, whose operands and result share one type. Comparison
/// and logical operators yield `bool` and are deliberately not handled here —
/// they are not what a cast site reads from.
fn is_arith(op: &syn::BinOp) -> bool {
    use syn::BinOp::*;
    matches!(
        op,
        Add(_) | Sub(_) | Mul(_) | Div(_) | Rem(_) | BitAnd(_) | BitOr(_) | BitXor(_) | Shl(_)
            | Shr(_)
    )
}

/// The type of a *suffixed* numeric literal (`4u32`, `1.5f32`). Unsuffixed
/// literals are deliberately unknown: `4` is whatever context demands, and
/// claiming `i32` would misclassify casts in `u64` arithmetic.
fn numeric_lit_type(l: &syn::ExprLit) -> Option<String> {
    match &l.lit {
        syn::Lit::Int(i) if !i.suffix().is_empty() => Some(i.suffix().to_string()),
        syn::Lit::Float(f) if !f.suffix().is_empty() => Some(f.suffix().to_string()),
        _ => None,
    }
}

/// Is `e`'s inferred type traceable to something declared, rather than guessed
/// from a bare method name? See [`FnTypes::guessed`].
fn is_grounded(
    e: &syn::Expr,
    sigs: &FnSigIndex,
    guessed: &std::collections::BTreeSet<String>,
) -> bool {
    match e {
        syn::Expr::Path(p) => p
            .path
            .segments
            .first()
            .map(|s| !guessed.contains(&s.ident.to_string()))
            .unwrap_or(true),
        // The one unsound arm: a method's return type looked up by name alone.
        // `.len()` / `.count()` are grounded by convention (see
        // `is_std_usize_method`); any other method return is a name guess.
        syn::Expr::MethodCall(mc) => is_std_usize_method(&mc.method.to_string(), sigs),
        syn::Expr::Cast(_) | syn::Expr::Struct(_) | syn::Expr::Lit(_) => true,
        syn::Expr::Reference(r) => is_grounded(&r.expr, sigs, guessed),
        syn::Expr::Paren(p) => is_grounded(&p.expr, sigs, guessed),
        syn::Expr::Group(g) => is_grounded(&g.expr, sigs, guessed),
        syn::Expr::Try(t) => is_grounded(&t.expr, sigs, guessed),
        syn::Expr::Unary(u) => is_grounded(&u.expr, sigs, guessed),
        syn::Expr::Binary(b) => {
            is_grounded(&b.left, sigs, guessed)
                && is_grounded(&b.right, sigs, guessed)
        }
        // A field access is grounded when its base is: field types come from
        // the struct definition, not from a name guess.
        syn::Expr::Field(f) => is_grounded(&f.base, sigs, guessed),
        // Free-fn / constructor calls resolve through the same name map as
        // methods, but a path call carries its own module qualification, so a
        // collision is far less likely. Treat as grounded.
        syn::Expr::Call(_) => true,
        _ => true,
    }
}

/// Infer the type of a call expression: `Type::ctor(...)` associated
/// constructors (incl. clap's `Type::parse()`), `Self::ctor(...)` through the
/// enclosing impl, or a bare `fn_name(...)` via its indexed return type.
fn infer_call_type(
    c: &syn::ExprCall,
    sigs: &FnSigIndex,
    bindings: &BTreeMap<String, String>,
) -> Option<String> {
    let syn::Expr::Path(p) = &*c.func else {
        return None;
    };
    let segs: Vec<&syn::PathSegment> = p.path.segments.iter().collect();
    if segs.len() >= 2 {
        let last = segs[segs.len() - 1].ident.to_string();
        let pen = segs[segs.len() - 2].ident.to_string();
        let is_ctor_name = matches!(
            last.as_str(),
            "new" | "default" | "from" | "with_capacity" | "from_str" | "empty" | "parse"
        );
        if is_ctor_name {
            if pen == "Self" {
                return bindings.get("Self").cloned();
            }
            if first_is_uppercase(&pen) {
                return Some(pen);
            }
        }
    }
    // Bare `fn_name(...)`: look up return type.
    if segs.len() == 1 {
        return sigs
            .return_type(&segs[0].ident.to_string())
            .map(str::to_string);
    }
    None
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
            uses.insert(f.path.clone(), UseMap::build_in(&f.ast, &f.module));
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


/// A `let` binding that shares a name with something the caller looked for.
pub struct Binding {
    /// `a closure` / `a local binding` / `a static-like const binding` — worded
    /// to drop straight into a sentence.
    pub kind: &'static str,
    pub file: String,
    pub line: usize,
}

/// Find `name` as a `let` binding somewhere in the tree.
///
/// Only ever called when a lookup has already failed, so the cost is paid on a
/// path that was about to return nothing anyway. It exists to convert the worst
/// answer the tool can give — silence, which reads as "no such concept here" —
/// into "that is a local, and here it is".
///
/// Deliberately *not* part of the index. A local has no callers, no fields, no
/// variants and no visibility; folding it in would make every command that
/// takes a name answer questions it cannot actually answer. This is a
/// signpost, not a new kind of target.
pub fn find_binding(files: &[ParsedFile], name: &str) -> Option<Binding> {
    struct V<'a> {
        want: &'a str,
        file: &'a str,
        hit: Option<Binding>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_local(&mut self, l: &'ast syn::Local) {
            if self.hit.is_none() {
                // `let name = …` and `let mut name = …`; a destructuring
                // pattern binds several, and none of them is what a reader
                // means when they name one thing.
                if let syn::Pat::Ident(i) = &l.pat {
                    if i.ident == self.want {
                        let closure = matches!(
                            l.init.as_ref().map(|i| &*i.expr),
                            Some(syn::Expr::Closure(_))
                        );
                        self.hit = Some(Binding {
                            kind: if closure { "a closure" } else { "a local binding" },
                            file: self.file.to_string(),
                            line: crate::ast::line_of(&i.ident),
                        });
                    }
                }
            }
            visit::visit_local(self, l);
        }
    }
    for f in files {
        let path = crate::parse::display_path(&f.path);
        let mut v = V {
            want: name,
            file: &path,
            hit: None,
        };
        v.visit_file(&f.ast);
        if v.hit.is_some() {
            return v.hit;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pf(path: &str, module: &str, src: &str) -> ParsedFile {
        ParsedFile {
            path: PathBuf::from(path),
            ast: syn::parse_str(src).expect("test source must parse"),
            module: module.to_string(),
        }
    }

    #[test]
    fn a_glob_import_resolves_through_the_name_index() {
        let files = vec![pf("src/geom.rs", "geom", "pub struct Shape;")];
        let idx = NameIndex::build(&files);
        let um = UseMap::build_in(
            &syn::parse_str("use geom::*;\nuse std::fmt::{Display, Write as FmtWrite};").unwrap(),
            "",
        );
        // The glob arm: `Shape` is not aliased, so it resolves only because
        // `geom::Shape` exists in the index.
        assert_eq!(um.resolve("Shape", &idx).as_deref(), Some("geom::Shape"));
        // Group + rename arms of `collect_uses`.
        assert_eq!(um.resolve("Display", &idx).as_deref(), Some("std::fmt::Display"));
        assert_eq!(um.resolve("FmtWrite", &idx).as_deref(), Some("std::fmt::Write"));
        assert_eq!(um.resolve("Nope", &idx), None);
    }

    /// `crate`, `self` and `super` are not module names, and a glob stored as
    /// written resolved nothing: `use super::*;` was the literal `"super"`, so
    /// every name a `mod.rs`-shaped crate reaches through its parent was
    /// invisible to resolution.
    #[test]
    fn a_relative_glob_becomes_the_module_path_it_names() {
        let files = vec![pf("src/geom/mod.rs", "geom", "pub fn dist() {}")];
        let idx = NameIndex::build(&files);
        // As written from `geom::boolean`, one level down.
        let um = UseMap::build_in(&syn::parse_str("use super::*;").unwrap(), "geom::boolean");
        assert_eq!(um.globs, vec!["geom".to_string()]);
        assert_eq!(um.resolve("dist", &idx).as_deref(), Some("geom::dist"));

        // `crate` is the root, which this tool spells as no prefix at all.
        let um = UseMap::build_in(&syn::parse_str("use crate::geom::*;").unwrap(), "deep::down");
        assert_eq!(um.globs, vec!["geom".to_string()]);

        // `self` is the module itself.
        let um = UseMap::build_in(&syn::parse_str("use self::inner::*;").unwrap(), "geom");
        assert_eq!(um.globs, vec!["geom::inner".to_string()]);

        // A `super` that walks past the root is left as written rather than
        // guessed at — a fabricated path resolves to nothing and says nothing.
        let um = UseMap::build_in(&syn::parse_str("use super::super::*;").unwrap(), "geom");
        assert_eq!(um.globs, vec!["super::super".to_string()]);
    }

    #[test]
    fn alias_chains_terminate_on_cycles_and_self_loops() {
        // `type A = B; type B = A;` never compiles, but this tool scans code
        // that may not compile — canonicalisation must not spin on it.
        let g = AliasGraph::build(&[pf("src/lib.rs", "", "type A = B;\ntype B = A;")]);
        assert_eq!(g.canonical("A"), "A");
        // The forward walk of `synonyms` must terminate on the same cycle,
        // and both names still count as one synonym set.
        let mut s = g.synonyms("A");
        s.sort();
        assert_eq!(s, vec!["A".to_string(), "B".to_string()]);
        let selfy = AliasGraph::build(&[pf("src/lib.rs", "", "type S = S;")]);
        assert_eq!(selfy.canonical("S"), "S");
        assert_eq!(selfy.synonyms("S"), vec!["S".to_string()]);
    }

    #[test]
    fn synonyms_walks_both_directions_of_the_alias_chain() {
        let g = AliasGraph::build(&[pf(
            "src/lib.rs",
            "",
            "type Old = Real;\ntype Older = Old;",
        )]);
        let mut s = g.synonyms("Old");
        s.sort();
        assert_eq!(s, vec!["Old".to_string(), "Older".to_string(), "Real".to_string()]);
    }

    #[test]
    fn conflicting_return_types_drop_the_entry_and_field_types_survive() {
        let files = vec![pf(
            "src/lib.rs",
            "",
            "struct Px { w: u32 }\n\
             fn a() -> u32 { 0 }\n\
             fn a() -> u64 { 0 }\n\
             fn b() -> Px { loop {} }",
        )];
        let sigs = FnSigIndex::build(&files);
        assert_eq!(sigs.return_type("a"), None, "ambiguous return must be dropped");
        assert_eq!(sigs.return_type("b"), Some("Px"));
        assert_eq!(sigs.field_type("Px", "w"), Some("u32"));
        assert_eq!(sigs.field_type("Px", "h"), None);
    }

    /// Parse one fn against one tree and hand back its inferred bindings.
    fn infer(tree: &str, self_ty: Option<&str>, body_fn: &str) -> (FnTypes, FnSigIndex) {
        let files = vec![pf("src/lib.rs", "", tree)];
        let sigs = FnSigIndex::build(&files);
        let f: syn::ItemFn = syn::parse_str(body_fn).expect("test fn must parse");
        let ft = FnTypes::build(&f.sig, &f.block, &sigs, self_ty);
        (ft, sigs)
    }

    #[test]
    fn loop_bindings_take_the_type_of_their_bound() {
        let (ft, _) = infer(
            "",
            None,
            "fn f(xs: Vec<u8>, n: u32) {\n\
                 for (i, _x) in xs.iter().enumerate() { let _ = i; }\n\
                 for j in 0..n { let _ = j; }\n\
             }",
        );
        // The enumerate index is usize by definition; the range variable
        // inherits its bound's declared type.
        assert_eq!(ft.bindings.get("i").map(String::as_str), Some("usize"));
        assert_eq!(ft.bindings.get("j").map(String::as_str), Some("u32"));
    }

    #[test]
    fn declared_sources_ground_the_inference_chain() {
        let tree = "pub struct Px { w: u32 }\n\
                    impl Px { pub fn new() -> Px { loop {} } }";
        let (ft, _) = infer(
            tree,
            Some("Px"),
            "fn f(p: Px) {\n\
                 let s = Self {};\n\
                 let c = Self::new();\n\
                 let m = Px::new();\n\
                 let w = p.w;\n\
                 let both = w + w;\n\
                 let half = w * 2;\n\
                 let flip = 2 * w;\n\
                 let neg = -half;\n\
                 let lit = 1.5f32;\n\
                 let (par) = 4u8;\n\
                 let &byref = &p;\n\
             }",
        );
        for (name, ty) in [
            ("s", "Px"),
            ("c", "Px"),
            ("m", "Px"),
            ("w", "u32"),
            ("both", "u32"),
            ("half", "u32"),
            ("flip", "u32"),
            ("neg", "u32"),
            ("lit", "f32"),
            ("par", "u8"),
            ("byref", "Px"),
        ] {
            assert_eq!(
                ft.bindings.get(name).map(String::as_str),
                Some(ty),
                "binding `{}`",
                name
            );
        }
        // Every one of those traces to a declaration — none is a name guess.
        assert!(ft.guessed.is_empty(), "{:?}", ft.guessed);
    }

    #[test]
    fn type_of_grounded_refuses_a_bare_name_guess() {
        let tree = "pub struct Px { w: u32 }\n\
                    impl Px { pub fn area(&self) -> u64 { 0 } }";
        let (ft, sigs) = infer(
            tree,
            None,
            "fn f(p: Px, xs: Vec<u8>) {\n\
                 let a = p.area();\n\
                 let n = xs.len();\n\
             }",
        );
        // `area` resolved by name alone is a guess; `.len()` is usize by
        // convention and stays grounded.
        assert!(ft.guessed.contains("a"), "{:?}", ft.guessed);
        assert!(!ft.guessed.contains("n"));
        let a: syn::Expr = syn::parse_str("a").unwrap();
        assert_eq!(ft.type_of(&a, &sigs).as_deref(), Some("u64"));
        assert_eq!(ft.type_of_grounded(&a, &sigs), None);
        // Grounded compound: field access + literal arithmetic.
        let e: syn::Expr = syn::parse_str("p.w + 2").unwrap();
        assert_eq!(ft.type_of_grounded(&e, &sigs).as_deref(), Some("u32"));
        // A call through a non-path callee types as unknown, quietly.
        let odd: syn::Expr = syn::parse_str("(callback)()").unwrap();
        assert_eq!(ft.type_of(&odd, &sigs), None);
    }

    #[test]
    fn a_projects_own_len_overrides_the_usize_convention() {
        let (ft, sigs) = infer(
            "pub struct Odd; impl Odd { pub fn len(&self) -> f64 { 0.0 } }",
            None,
            "fn f(o: Odd) { let n = o.len(); }",
        );
        // The tree defines `len -> f64`, so the convention yields to it — and
        // a name-resolved return type is a guess, not grounded.
        assert_eq!(ft.bindings.get("n").map(String::as_str), Some("f64"));
        assert!(ft.guessed.contains("n"));
        // And a tree that agrees with the convention keeps it grounded.
        let (ft2, sigs2) = infer(
            "pub struct Buf; impl Buf { pub fn len(&self) -> usize { 0 } }",
            None,
            "fn f(b: Buf) { let n = b.len(); }",
        );
        assert_eq!(ft2.bindings.get("n").map(String::as_str), Some("usize"));
        assert!(!ft2.guessed.contains("n"));
        let _ = (sigs, sigs2);
    }
}
