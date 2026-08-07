use proc_macro2::{Span, TokenStream, TokenTree};
use syn::spanned::Spanned;

pub fn line_of<T: Spanned>(t: &T) -> usize {
    t.span().start().line
}

pub fn line_of_span(s: Span) -> usize {
    s.start().line
}

pub fn vis_str(v: &syn::Visibility) -> &'static str {
    match v {
        syn::Visibility::Public(_) => "pub",
        syn::Visibility::Restricted(r) => {
            if r.path.is_ident("crate") {
                "pub(crate)"
            } else if r.path.is_ident("super") {
                "pub(super)"
            } else if r.path.is_ident("self") {
                "pub(self)"
            } else {
                "pub(in ...)"
            }
        }
        syn::Visibility::Inherited => "priv",
    }
}

pub fn path_to_string(p: &syn::Path) -> String {
    let mut out = String::new();
    if p.leading_colon.is_some() {
        out.push_str("::");
    }
    for (i, seg) in p.segments.iter().enumerate() {
        if i > 0 {
            out.push_str("::");
        }
        out.push_str(&seg.ident.to_string());
    }
    out
}

/// Shared item-path prefix: `module::mods::(impl|trait)`. The type segment is
/// the innermost `impl` self-type, falling back to the innermost `trait`.
fn type_prefix(
    module: &str,
    mod_stack: &[String],
    impl_stack: &[String],
    trait_stack: &[String],
) -> Vec<String> {
    let mut path: Vec<String> = Vec::new();
    if !module.is_empty() {
        path.push(module.to_string());
    }
    path.extend(mod_stack.iter().cloned());
    if let Some(t) = impl_stack.last() {
        path.push(t.clone());
    } else if let Some(t) = trait_stack.last() {
        path.push(t.clone());
    }
    path
}

/// Fully-qualified name of a defined item: `module::mods::Type::name`. Pass an
/// empty `trait_stack` for visitors that don't distinguish trait context.
pub fn qualify(
    module: &str,
    mod_stack: &[String],
    impl_stack: &[String],
    trait_stack: &[String],
    name: &str,
) -> String {
    let mut path = type_prefix(module, mod_stack, impl_stack, trait_stack);
    path.push(name.to_string());
    path.join("::")
}

/// Label for the fn enclosing a call/site: `module::mods::Type::fn`. With no
/// enclosing fn the prefix alone is returned (or `<top-level>` if empty). When
/// `toplevel_segment` is true, a missing fn inside a non-empty prefix renders as
/// a trailing `<top-level>` segment — the `callers` convention, which marks
/// call sites that sit at module/impl top level rather than inside a fn.
pub fn enclosing(
    module: &str,
    mod_stack: &[String],
    impl_stack: &[String],
    fn_stack: &[String],
    toplevel_segment: bool,
) -> String {
    let mut path = type_prefix(module, mod_stack, impl_stack, &[]);
    if let Some(f) = fn_stack.last() {
        path.push(f.clone());
    } else if toplevel_segment {
        if path.is_empty() {
            return "<top-level>".to_string();
        }
        path.push("<top-level>".to_string());
    }
    if path.is_empty() {
        "<top-level>".to_string()
    } else {
        path.join("::")
    }
}

/// Tracks the lexical scope a `syn` visitor is currently inside — the file's
/// top-level module, plus stacks of nested `mod`s, `impl`/`trait` blocks, and
/// `fn`s. Every analysis visitor needs this to qualify the items and call sites
/// it finds; embedding one `ScopeTracker` replaces the four parallel stacks
/// (and their push/pop boilerplate) that were previously copy-pasted per
/// visitor. Visitors `enter_*` on the way down and `leave_*` on the way back
/// up, then call `qualify`/`enclosing` to render a path.
#[derive(Default)]
pub struct ScopeTracker {
    pub module: String,
    pub mod_stack: Vec<String>,
    pub impl_stack: Vec<String>,
    pub trait_stack: Vec<String>,
    pub fn_stack: Vec<String>,
    /// (start, end) source lines of each fn on the stack; parallel to
    /// `fn_stack`. Rendered as `@start-end` when `spans` is set.
    fn_span_stack: Vec<(usize, usize)>,
    /// Render the enclosing fn as `name@start-end` (the global `--spans`
    /// flag) so a reader can fetch exactly the relevant body.
    spans: bool,
}

impl ScopeTracker {
    pub fn new(module: impl Into<String>) -> Self {
        Self {
            module: module.into(),
            ..Default::default()
        }
    }

    /// Enable `@start-end` span rendering on the enclosing-fn label.
    pub fn with_spans(mut self, on: bool) -> Self {
        self.spans = on;
        self
    }

    pub fn enter_mod(&mut self, name: impl Into<String>) {
        self.mod_stack.push(name.into());
    }
    pub fn leave_mod(&mut self) {
        self.mod_stack.pop();
    }
    pub fn enter_impl(&mut self, ty: impl Into<String>) {
        self.impl_stack.push(ty.into());
    }
    pub fn leave_impl(&mut self) {
        self.impl_stack.pop();
    }
    pub fn enter_trait(&mut self, name: impl Into<String>) {
        self.trait_stack.push(name.into());
    }
    pub fn leave_trait(&mut self) {
        self.trait_stack.pop();
    }
    pub fn enter_fn(&mut self, name: impl Into<String>, span: (usize, usize)) {
        self.fn_stack.push(name.into());
        self.fn_span_stack.push(span);
    }
    pub fn leave_fn(&mut self) {
        self.fn_stack.pop();
        self.fn_span_stack.pop();
    }

    /// `module::mods::Type::name` for a defined item in the current scope.
    pub fn qualify(&self, name: &str) -> String {
        qualify(
            &self.module,
            &self.mod_stack,
            &self.impl_stack,
            &self.trait_stack,
            name,
        )
    }

    /// With `--spans`, `@start-end` of the innermost enclosing fn.
    fn span_suffix(&self) -> String {
        if !self.spans {
            return String::new();
        }
        match (self.fn_stack.last(), self.fn_span_stack.last()) {
            (Some(_), Some((s, e))) => format!("@{}-{}", s, e),
            _ => String::new(),
        }
    }

    /// Label for the fn enclosing the current site: `module::mods::Type::fn`,
    /// or the prefix alone / `<top-level>` when not inside a fn. With
    /// `--spans` the fn segment carries `@start-end` source lines.
    pub fn enclosing(&self) -> String {
        format!(
            "{}{}",
            enclosing(
                &self.module,
                &self.mod_stack,
                &self.impl_stack,
                &self.fn_stack,
                false,
            ),
            self.span_suffix()
        )
    }

    /// Like [`enclosing`](Self::enclosing) but renders a module/impl top-level
    /// site (no enclosing fn) as a trailing `<top-level>` segment — the
    /// `callers` convention for labelling call sites.
    pub fn enclosing_with_toplevel(&self) -> String {
        format!(
            "{}{}",
            enclosing(
                &self.module,
                &self.mod_stack,
                &self.impl_stack,
                &self.fn_stack,
                true,
            ),
            self.span_suffix()
        )
    }
}

/// Where an item begins, where it is declared, and where it ends.
///
/// Three numbers rather than one because the three questions a reader asks are
/// different: *where do I read from* includes the doc comment and attributes,
/// *which line is the item* is the declaration, and *where do I stop* is the
/// closing brace. Collapsing them is how `grep -n 'fn foo' | sed -n "$n,+70p"`
/// gets to be off by a line — every consumer picks the wrong one of the three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    /// First line including `///` docs and `#[attrs]`.
    pub doc_start: usize,
    /// The `fn`/`struct`/`impl` line itself.
    pub decl: usize,
    /// Last line of the item, closing brace included.
    pub end: usize,
}

/// The [`Extent`] of a syn node whose declaration sits at `decl`.
///
/// `attrs` is passed separately rather than read back off `node` because syn
/// has no one trait for "this item's attributes" — [`item_attrs`] covers
/// `syn::Item` and nothing else, while impl/trait members carry their own.
pub fn extent_of<T: Spanned>(node: &T, attrs: &[syn::Attribute], decl: usize) -> Extent {
    let doc_start = attrs
        .iter()
        .map(line_of)
        .min()
        .unwrap_or(decl)
        .min(decl);
    Extent {
        doc_start,
        decl,
        // `max(decl)`: a span that ends before it starts is not a range any
        // reader can use, and an empty `sed` range reads as "nothing here".
        end: node.span().end().line.max(decl),
    }
}

/// The first line of an item's `///` doc comment, trimmed. `None` when the
/// item has no doc — an outline row says nothing rather than inventing a
/// summary from the code.
pub fn doc_summary(attrs: &[syn::Attribute]) -> Option<String> {
    let line = attrs.iter().find_map(doc_text)?;
    let t = line.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Last line of a fn signature — through the return type and where-clause, so
/// `show --sig` prints a complete header and none of the body.
pub fn sig_end(sig: &syn::Signature, decl: usize) -> usize {
    sig.span().end().line.max(decl)
}

/// (start, end) source lines of a fn: signature ident line through body end.
pub fn fn_span(sig: &syn::Signature, block: &syn::Block) -> (usize, usize) {
    let start = line_of(&sig.ident);
    let end = block.span().end().line.max(start);
    (start, end)
}

/// Span of a trait fn: through its default body if present, else the
/// signature line alone.
pub fn trait_fn_span(f: &syn::TraitItemFn) -> (usize, usize) {
    match &f.default {
        Some(b) => fn_span(&f.sig, b),
        None => {
            let l = line_of(&f.sig.ident);
            (l, l)
        }
    }
}

pub fn path_to_string_with_args(p: &syn::Path) -> String {
    let mut s = String::new();
    if p.leading_colon.is_some() {
        s.push_str("::");
    }
    for (i, seg) in p.segments.iter().enumerate() {
        if i > 0 {
            s.push_str("::");
        }
        s.push_str(&seg.ident.to_string());
        match &seg.arguments {
            syn::PathArguments::None => {}
            syn::PathArguments::AngleBracketed(a) => {
                let args: Vec<String> = a
                    .args
                    .iter()
                    .map(|arg| match arg {
                        syn::GenericArgument::Type(t) => type_to_string(t),
                        syn::GenericArgument::Lifetime(l) => format!("'{}", l.ident),
                        syn::GenericArgument::Const(_) => "_".to_string(),
                        _ => "_".to_string(),
                    })
                    .collect();
                s.push('<');
                s.push_str(&args.join(", "));
                s.push('>');
            }
            syn::PathArguments::Parenthesized(_) => {
                s.push_str("(...)");
            }
        }
    }
    s
}

/// If `p` is `<Enum>::<Variant>` — the penultimate segment equals
/// `target_enum` (last-segment rule) and the final segment names one of
/// `variant_names` — return the variant ident. With `allow_bare`, a
/// single-segment path naming a variant also matches (for callers that
/// `use Enum::*;` — noisier). Single shared implementation for every
/// enum-site scanner (`variants`, `catch-all-arms`, `parallel-matches`,
/// `enum-coverage`) so the matching rule can't drift between them.
pub fn enum_variant_of_path(
    p: &syn::Path,
    target_enum: &str,
    variant_names: &[String],
    allow_bare: bool,
) -> Option<String> {
    let segs: Vec<&syn::PathSegment> = p.segments.iter().collect();
    let last = segs.last()?.ident.to_string();
    if !variant_names.iter().any(|v| v == &last) {
        return None;
    }
    if segs.len() >= 2 && segs[segs.len() - 2].ident == target_enum {
        return Some(last);
    }
    if allow_bare && segs.len() == 1 {
        return Some(last);
    }
    None
}

/// Last `::`-segment of a path string (the bare name). One shared
/// implementation for a pattern that had drifted into 18 copies.
pub fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

pub fn path_last(p: &syn::Path) -> String {
    p.segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_default()
}

pub fn type_to_string(t: &syn::Type) -> String {
    match t {
        syn::Type::Path(p) => {
            if let Some(q) = &p.qself {
                format!(
                    "<{}>::{}",
                    type_to_string(&q.ty),
                    path_to_string_with_args(&p.path)
                )
            } else {
                path_to_string_with_args(&p.path)
            }
        }
        syn::Type::Reference(r) => format!(
            "&{}{}",
            if r.mutability.is_some() { "mut " } else { "" },
            type_to_string(&r.elem)
        ),
        syn::Type::Ptr(p) => format!(
            "*{}{}",
            if p.mutability.is_some() { "mut " } else { "const " },
            type_to_string(&p.elem)
        ),
        syn::Type::Tuple(t) => {
            let inner: Vec<_> = t.elems.iter().map(type_to_string).collect();
            format!("({})", inner.join(", "))
        }
        syn::Type::Slice(s) => format!("[{}]", type_to_string(&s.elem)),
        syn::Type::Array(a) => format!("[{}; _]", type_to_string(&a.elem)),
        syn::Type::ImplTrait(_) => "impl _".to_string(),
        syn::Type::TraitObject(_) => "dyn _".to_string(),
        syn::Type::BareFn(_) => "fn(_)".to_string(),
        syn::Type::Infer(_) => "_".to_string(),
        syn::Type::Never(_) => "!".to_string(),
        syn::Type::Paren(p) => type_to_string(&p.elem),
        syn::Type::Group(g) => type_to_string(&g.elem),
        _ => "_".to_string(),
    }
}

/// Last segment of a `&[mut] T` or plain `T`, peeling through references.
pub fn type_last_segment(t: &syn::Type) -> Option<String> {
    match t {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        syn::Type::Reference(r) => type_last_segment(&r.elem),
        syn::Type::Paren(p) => type_last_segment(&p.elem),
        syn::Type::Group(g) => type_last_segment(&g.elem),
        _ => None,
    }
}

/// True if `t` (possibly through `&`/`Paren`/`Group`) is `&mut <something>`.
pub fn is_mut_ref(t: &syn::Type) -> bool {
    match t {
        syn::Type::Reference(r) => r.mutability.is_some(),
        syn::Type::Paren(p) => is_mut_ref(&p.elem),
        syn::Type::Group(g) => is_mut_ref(&g.elem),
        _ => false,
    }
}

/// True if any attribute is `#[cfg(test)]`, `#[cfg(any(test, ...))]`,
/// `#[cfg(all(test, ...))]`, `#[test]`, or a known test attribute like
/// `#[tokio::test]`/`#[test_case::*]`.
pub fn has_test_attr(attrs: &[syn::Attribute]) -> bool {
    for a in attrs {
        let p = a.path();
        // Direct `#[test]` / `#[bench]` / `#[tokio::test]` etc.
        let last = p.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
        if last == "test" || last == "bench" {
            return true;
        }
        // `#[cfg(test)]` and friends.
        if p.is_ident("cfg") {
            if let syn::Meta::List(ml) = &a.meta {
                if tokens_contain_test(&ml.tokens) {
                    return true;
                }
            }
        }
    }
    false
}

fn tokens_contain_test(ts: &TokenStream) -> bool {
    for tt in ts.clone() {
        match tt {
            TokenTree::Ident(id) if id == "test" => return true,
            TokenTree::Group(g) if tokens_contain_test(&g.stream()) => return true,
            _ => {}
        }
    }
    false
}

/// True if any attribute is `#[allow(dead_code)]` (possibly inside
/// `#[allow(unused, dead_code)]` or `#[allow(dead_code, ...)]`).
pub fn has_allow_dead_code(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        if !a.path().is_ident("allow") {
            return false;
        }
        let syn::Meta::List(ml) = &a.meta else {
            return false;
        };
        ml.tokens
            .clone()
            .into_iter()
            .any(|tt| matches!(tt, TokenTree::Ident(id) if id == "dead_code"))
    })
}

/// The string value of a `#[doc = "..."]` attribute (one doc-comment line).
pub fn doc_text(attr: &syn::Attribute) -> Option<String> {
    if !attr.path().is_ident("doc") {
        return None;
    }
    let syn::Meta::NameValue(nv) = &attr.meta else {
        return None;
    };
    let syn::Expr::Lit(l) = &nv.value else {
        return None;
    };
    match &l.lit {
        syn::Lit::Str(s) => Some(s.value()),
        _ => None,
    }
}

/// Group `items` by `key`, sort by count desc, optionally truncate to top N,
/// print one row per group as `<count>\t<key>` on stdout.
///
/// Shared by commands that support `--by fn|file|module`.
pub fn print_grouped_counts<T, F>(items: &[T], top: Option<usize>, key: F)
where
    F: Fn(&T) -> String,
{
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for item in items {
        *counts.entry(key(item)).or_insert(0) += 1;
    }
    let mut rows: Vec<(String, usize)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if let Some(n) = top {
        rows.truncate(n);
    }
    for (k, n) in rows {
        println!("{}\t{}", n, k);
    }
}

/// Top-level module of a `qpath` like `inventory::Visitor::record`. Returns
/// the whole string if there's no `::`.
pub fn top_module_of(qpath: &str) -> &str {
    qpath.split("::").next().unwrap_or(qpath)
}

/// Get the attributes list for any `syn::Item`. Returns None for variants
/// without attrs (rare/forbidden ones).
pub fn item_attrs(item: &syn::Item) -> Option<&[syn::Attribute]> {
    Some(match item {
        syn::Item::Const(i) => &i.attrs,
        syn::Item::Enum(i) => &i.attrs,
        syn::Item::ExternCrate(i) => &i.attrs,
        syn::Item::Fn(i) => &i.attrs,
        syn::Item::ForeignMod(i) => &i.attrs,
        syn::Item::Impl(i) => &i.attrs,
        syn::Item::Macro(i) => &i.attrs,
        syn::Item::Mod(i) => &i.attrs,
        syn::Item::Static(i) => &i.attrs,
        syn::Item::Struct(i) => &i.attrs,
        syn::Item::Trait(i) => &i.attrs,
        syn::Item::TraitAlias(i) => &i.attrs,
        syn::Item::Type(i) => &i.attrs,
        syn::Item::Union(i) => &i.attrs,
        syn::Item::Use(i) => &i.attrs,
        _ => return None,
    })
}

pub fn type_short(t: &syn::Type) -> String {
    match t {
        syn::Type::Path(p) => path_last(&p.path),
        syn::Type::Reference(r) => type_short(&r.elem),
        _ => type_to_string(t),
    }
}

// ---------------------------------------------------------------------------
// Scope-tracking visitor boilerplate
// ---------------------------------------------------------------------------

/// Emit the `syn::visit::Visit` methods that do nothing but keep a
/// [`ScopeTracker`] in step with the traversal.
///
/// Every check that reports an *enclosing* item — which is nearly all of them —
/// needs the same six methods, and each body is push, recurse, pop. Written out
/// by hand that came to 77 identical bodies across 17 files: 18 copies of
/// `visit_item_mod`, 17 of `visit_item_impl`, 14 of `visit_item_trait`, 10 each
/// of `visit_item_fn` and `visit_impl_item_fn`, 8 of `visit_trait_item_fn`.
/// `unruster clones` ranked them as its own top six findings, at 0.82–0.88.
///
/// The hazard is drift, not volume: a visitor that pushes without popping, or
/// that omits one of the six, reports qualified paths that are silently wrong
/// for every row it emits — and there is nothing in the output to say so.
///
/// Name the methods you want, inside the `impl Visit` block:
///
/// ```ignore
/// impl<'ast> Visit<'ast> for MyVisitor<'_> {
///     scope_visits!(item_mod, item_impl, item_trait);
///     fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) { /* custom */ }
/// }
/// ```
///
/// A visitor that needs to do work *around* the scope push — `casts` tracks
/// unsafe depth, `clones` records the body — writes that method out and takes
/// the rest from here. Paths are fully qualified so the call site needs no
/// imports beyond what it already has.
///
/// # The trade this makes
///
/// `unruster` is syntactic: it reads source, not expansions. Moving these
/// bodies into a macro therefore hides them from the tool's own checks — this
/// definition is one of the two `blind spots` a self-audit now reports, and the
/// 74 fn bodies it replaces no longer appear in `clones`' scanned count.
///
/// That is the right trade here and it is worth being explicit about why:
/// the code is now written once, in one place, where a reviewer can check the
/// push/pop pairing by eye — which is the only property that mattered. Reaching
/// for a macro to make a finding go away, on code that genuinely differs per
/// call site, would be the same move with none of the benefit.
macro_rules! scope_visits {
    ($($which:ident),+ $(,)?) => {
        $(scope_visits!(@emit $which);)+
    };

    (@emit item_mod) => {
        fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
            self.scope.enter_mod(i.ident.to_string());
            syn::visit::visit_item_mod(self, i);
            self.scope.leave_mod();
        }
    };
    (@emit item_impl) => {
        fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
            self.scope.enter_impl($crate::ast::type_short(&i.self_ty));
            syn::visit::visit_item_impl(self, i);
            self.scope.leave_impl();
        }
    };
    (@emit item_trait) => {
        fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
            self.scope.enter_trait(i.ident.to_string());
            syn::visit::visit_item_trait(self, i);
            self.scope.leave_trait();
        }
    };
    (@emit item_fn) => {
        fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
            self.scope.enter_fn(
                i.sig.ident.to_string(),
                $crate::ast::fn_span(&i.sig, &i.block),
            );
            syn::visit::visit_item_fn(self, i);
            self.scope.leave_fn();
        }
    };
    (@emit impl_item_fn) => {
        fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
            self.scope.enter_fn(
                i.sig.ident.to_string(),
                $crate::ast::fn_span(&i.sig, &i.block),
            );
            syn::visit::visit_impl_item_fn(self, i);
            self.scope.leave_fn();
        }
    };
    (@emit trait_item_fn) => {
        fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
            self.scope
                .enter_fn(i.sig.ident.to_string(), $crate::ast::trait_fn_span(i));
            syn::visit::visit_trait_item_fn(self, i);
            self.scope.leave_fn();
        }
    };
}

pub(crate) use scope_visits;
