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
            TokenTree::Group(g) => {
                if tokens_contain_test(&g.stream()) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// True if any attribute is `#[allow(dead_code)]` (possibly inside
/// `#[allow(unused, dead_code)]` or `#[allow(dead_code, ...)]`).
pub fn has_allow_dead_code(attrs: &[syn::Attribute]) -> bool {
    for a in attrs {
        if !a.path().is_ident("allow") {
            continue;
        }
        if let syn::Meta::List(ml) = &a.meta {
            for tt in ml.tokens.clone() {
                if let TokenTree::Ident(id) = tt {
                    if id == "dead_code" {
                        return true;
                    }
                }
            }
        }
    }
    false
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
