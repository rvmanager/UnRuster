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

