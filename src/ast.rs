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

    /// Source extent of the innermost enclosing fn, regardless of `--spans`.
    ///
    /// [`span_suffix`](Self::span_suffix) renders the same pair into a label
    /// only when the flag is set. A caller that needs to *read* the enclosing
    /// body — rather than to label it — needs the numbers unconditionally.
    pub fn fn_span(&self) -> Option<(usize, usize)> {
        self.fn_span_stack.last().copied()
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
                        // Spelled, not elided: `Matrix<4>` and `Matrix<32>` are
                        // different types, and this string is what `index` and
                        // `fields` key on. See `type_to_string`.
                        syn::GenericArgument::Const(c) => const_arg_to_string(c),
                        syn::GenericArgument::AssocType(a) => {
                            format!("{} = {}", a.ident, type_to_string(&a.ty))
                        }
                        _ => "?".to_string(),
                    })
                    .collect();
                s.push('<');
                s.push_str(&args.join(", "));
                s.push('>');
            }
            // `Fn(A) -> B`. The old `(...)` merged every `Fn`/`FnMut`/`FnOnce`
            // bound onto one spelling, the same identity collision the const
            // arm above had.
            syn::PathArguments::Parenthesized(p) => {
                let args: Vec<String> = p.inputs.iter().map(type_to_string).collect();
                s.push('(');
                s.push_str(&args.join(", "));
                s.push(')');
                if let syn::ReturnType::Type(_, t) = &p.output {
                    s.push_str(&format!(" -> {}", type_to_string(t)));
                }
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

/// Last-segment glob match. `*` matches any (possibly empty) run of chars.
/// No other metacharacters. `name` is the bare last segment of an item.
///
/// One implementation for every command that takes a name pattern, so `*`
/// means the same thing in `callers --among` and `inventory --name`. It is
/// deliberately not a regex: the patterns people actually write for this are
/// `wrap_in_*` and `*_opts`, and a regex dialect would have to be documented,
/// escaped and got wrong.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    // Fast path: no wildcard means exact match.
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    // Anchored prefix (text before the first `*`).
    let mut rest = name;
    if let Some(first) = parts.first() {
        if !rest.starts_with(first) {
            return false;
        }
        rest = &rest[first.len()..];
    }
    // Anchored suffix (text after the last `*`).
    if let Some(last) = parts.last() {
        if !rest.ends_with(last) {
            return false;
        }
        rest = &rest[..rest.len() - last.len()];
    }
    // Interior literals must appear in order.
    for mid in &parts[1..parts.len().saturating_sub(1)] {
        if mid.is_empty() {
            continue;
        }
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// [`glob_match`] with smartcase: an all-lowercase pattern matches
/// case-insensitively, a pattern carrying any uppercase matches exactly.
///
/// The ripgrep/vim convention, adopted because the case it serves is the one
/// that actually showed up. A session hunting the mask machinery wrote
/// `inventory | grep -i mask`, and neither `--name '*mask*'` (misses `Mask`,
/// `MaskArgs`, `load_mask_for`'s type) nor `--name '*Mask*'` (misses the
/// snake_case fns) covers it — `*` is the only metacharacter, so there is no
/// character class to fall back on. `--name mask` now does what the `-i` did.
///
/// Deliberately *not* used by `callers --among`: a cohort is a naming
/// convention, `wrap_in_*` is already lowercase, and quietly widening it to
/// match `Wrap_In_*` would change which functions a divergence report compares
/// without anyone asking.
pub fn glob_match_smart(pattern: &str, name: &str) -> bool {
    if pattern.chars().any(|c| c.is_ascii_uppercase()) {
        return glob_match(pattern, name);
    }
    glob_match(pattern, &name.to_ascii_lowercase())
}

pub fn path_last(p: &syn::Path) -> String {
    p.segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_default()
}

/// Render a type as text.
///
/// # This string is an identity, not a label
///
/// Callers key on it: `index` builds an impl block's `qpath` out of it,
/// `fields` stores it as `FieldDef.ty`, `casts` compares a cast's source
/// against its target. So two *different* types must not render alike, or
/// those consumers conflate them silently — which is exactly what happened
/// while `Array` rendered `[T; _]`: `impl T for [u8; 4]` and
/// `impl T for [u8; 32]` produced one and the same `qpath`, and two struct
/// fields of those types were indistinguishable in `fields`.
///
/// Elision is still fine where it loses nothing that distinguishes: a const
/// generic length that is neither a literal nor a path is `_` because there is
/// nothing shorter to say about it. What is not fine is a whole *class* —
/// every `impl Trait`, every `dyn Trait`, every fn pointer — collapsing onto
/// one spelling.
///
/// The catch-all renders `?`, not `_`: `Type::Infer` *is* `_` in the source,
/// and a variant this function cannot render is a different fact from a type
/// the author wrote as inferred.
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
        syn::Type::Array(a) => format!(
            "[{}; {}]",
            type_to_string(&a.elem),
            const_arg_to_string(&a.len)
        ),
        syn::Type::ImplTrait(i) => format!("impl {}", bounds_to_string(&i.bounds)),
        syn::Type::TraitObject(o) => format!("dyn {}", bounds_to_string(&o.bounds)),
        syn::Type::BareFn(f) => bare_fn_to_string(f),
        syn::Type::Macro(m) => format!("{}!", path_to_string(&m.mac.path)),
        syn::Type::Infer(_) => "_".to_string(),
        syn::Type::Never(_) => "!".to_string(),
        syn::Type::Paren(p) => type_to_string(&p.elem),
        syn::Type::Group(g) => type_to_string(&g.elem),
        // Not `_`: see the note on this fn. A variant syn added since this
        // match was written is not a type the author elided.
        _ => "?".to_string(),
    }
}

/// `Trait + Send + 'a` for an `impl`/`dyn` bound list. Empty renders `_`, which
/// is unreachable in valid source but keeps `impl `/`dyn ` from ending bare.
fn bounds_to_string(bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, syn::Token![+]>) -> String {
    let parts: Vec<String> = bounds
        .iter()
        .map(|b| match b {
            syn::TypeParamBound::Trait(t) => {
                let q = if matches!(t.modifier, syn::TraitBoundModifier::Maybe(_)) {
                    "?"
                } else {
                    ""
                };
                format!("{}{}", q, path_to_string_with_args(&t.path))
            }
            syn::TypeParamBound::Lifetime(l) => format!("'{}", l.ident),
            _ => "?".to_string(),
        })
        .collect();
    if parts.is_empty() {
        "_".to_string()
    } else {
        parts.join(" + ")
    }
}

/// `fn(A, B) -> C`. The arity alone already separates fn pointers that the old
/// single `fn(_)` spelling merged.
fn bare_fn_to_string(f: &syn::TypeBareFn) -> String {
    let args: Vec<String> = f.inputs.iter().map(|i| type_to_string(&i.ty)).collect();
    let variadic = if f.variadic.is_some() { ", ..." } else { "" };
    let ret = match &f.output {
        syn::ReturnType::Default => String::new(),
        syn::ReturnType::Type(_, t) => format!(" -> {}", type_to_string(t)),
    };
    format!("fn({}{}){}", args.join(", "), variadic, ret)
}

/// An array length or const generic argument. A literal and a named constant
/// are both worth spelling — `[u8; 4]` and `[u8; MAX]` are different types, and
/// so are `[u8; 4]` and `[u8; 32]`. Anything computed stays `_`: there is
/// nothing shorter than the expression itself to say about it.
pub fn const_arg_to_string(e: &syn::Expr) -> String {
    match e {
        syn::Expr::Lit(l) => match &l.lit {
            syn::Lit::Int(i) => i.base10_digits().to_string(),
            syn::Lit::Bool(b) => b.value.to_string(),
            syn::Lit::Str(s) => format!("{:?}", s.value()),
            syn::Lit::Char(c) => format!("{:?}", c.value()),
            _ => "_".to_string(),
        },
        syn::Expr::Path(p) => path_to_string(&p.path),
        syn::Expr::Group(g) => const_arg_to_string(&g.expr),
        syn::Expr::Paren(p) => const_arg_to_string(&p.expr),
        syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => {
            format!("-{}", const_arg_to_string(&u.expr))
        }
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
pub fn print_grouped_counts<T, F>(out: &crate::emit::Out, items: &[T], key: F)
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
    // Through `out`, not `println!`. Writing straight to stdout meant
    // `--by <x> --json` emitted raw TSV *and then* the JSON envelope — a
    // document no parser accepts — on all four grouping commands (`casts`,
    // `stringly`, `conversions`, `callers`). `--fingerprints` was dropped the
    // same way. A helper that renders its own output cannot honour the global
    // output flags, so it does not render its own output.
    for (k, n) in rows {
        crate::emit::row!(out, "count" => n.to_string(), "group" => k);
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
    // As above, but also maintains `self.fn_types_stack` — the local-type
    // inference the receiver-typed checks need. Requires `self.fn_types_stack`
    // and `self.fn_sigs`. A trait fn with no default body has nothing to infer
    // over, so it is skipped rather than pushed empty.
    (@emit trait_item_fn_typed) => {
        fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
            // Trait default-method bodies count like any other fn body.
            let Some(body) = &i.default else { return };
            self.scope
                .enter_fn(i.sig.ident.to_string(), $crate::ast::fn_span(&i.sig, body));
            self.fn_types_stack.push(
                $crate::semantic::FnTypes::build(&i.sig, body, self.fn_sigs, None),
            );
            syn::visit::visit_trait_item_fn(self, i);
            self.fn_types_stack.pop();
            self.scope.leave_fn();
        }
    };
    // Mark the closure's own tail position: a `.ok()` there is a Result→Option
    // conversion feeding a combinator, not a dropped error. Requires
    // `self.closure_tail: Vec<bool>`.
    (@emit expr_closure_tail) => {
        fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
            self.closure_tail.push(true);
            syn::visit::visit_expr_closure(self, c);
            self.closure_tail.pop();
        }
    };
}

pub(crate) use scope_visits;

// ---------------------------------------------------------------------------
// Shared syntax predicates
//
// Each of these existed in two or three copies before `unruster clones` ranked
// them among its own top findings. They are collected here not to save lines
// but because every one of them is a *definition* two checks must agree on: if
// one copy learns about a new shape and the other does not, the two checks
// silently disagree about the same question, which is the exact defect class
// this tool was built to find.
// ---------------------------------------------------------------------------

/// Peel parentheses and invisible groups: pure syntax noise, no change of value.
pub fn peel_grouping(e: &syn::Expr) -> &syn::Expr {
    match e {
        syn::Expr::Paren(p) => peel_grouping(&p.expr),
        syn::Expr::Group(g) => peel_grouping(&g.expr),
        other => other,
    }
}

/// Is every input to this expression written in the source?
///
/// `Regex::new("^v[0-9]+$")`, `"3".parse::<u8>()`, `"/tmp".to_string()` — the
/// value cannot vary at runtime on any input the program did not already
/// contain, so it is a constant however many calls it is spelled with.
///
/// Two checks need this and they need to agree: `panics` uses it to clear
/// `Regex::new("…").unwrap()` as an assertion about the source file rather than
/// about data, and `error-swallows` uses it to keep `.unwrap_or_else(|_|
/// "/tmp".to_string())` classified as a *default* rather than as a substituted
/// value. Two copies that learned about different shapes would make the two
/// checks disagree about the same question.
///
/// BEST-EFFORT: it looks for literals and rejects anything that names a binding,
/// so a `const PATTERN: &str` reads as variable and stays in the listing.
pub fn is_literal_only(e: &syn::Expr) -> bool {
    match peel_grouping(e) {
        syn::Expr::Lit(_) => true,
        syn::Expr::Reference(r) => is_literal_only(&r.expr),
        syn::Expr::Unary(u) => is_literal_only(&u.expr),
        syn::Expr::MethodCall(c) => {
            is_literal_only(&c.receiver) && c.args.iter().all(is_literal_only)
        }
        // A no-argument call has nothing written in it to be constant *from*:
        // `Instant::now()` and `Default::default()` are both spelled this way.
        syn::Expr::Call(c) => !c.args.is_empty() && c.args.iter().all(is_literal_only),
        _ => false,
    }
}

/// Peel grouping *and* borrows and derefs, so `&node` / `*node` / `(node)` all
/// compare structurally equal to the bare `node`. For comparing two expressions
/// for identity; use [`peel_grouping`] when only the noise should go.
pub fn peel_expr(mut e: &syn::Expr) -> &syn::Expr {
    loop {
        e = match e {
            syn::Expr::Reference(r) => &r.expr,
            syn::Expr::Paren(p) => &p.expr,
            syn::Expr::Group(g) => &g.expr,
            syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => &u.expr,
            other => return other,
        };
    }
}

/// An `Ok(..)` pattern head, through references and parens — what makes an
/// `if let` / `while let` an error path rather than an optional lookup.
pub fn pat_is_ok(p: &syn::Pat) -> bool {
    match p {
        syn::Pat::TupleStruct(ts) => ts
            .path
            .segments
            .last()
            .map(|s| s.ident == "Ok")
            .unwrap_or(false),
        syn::Pat::Reference(r) => pat_is_ok(&r.pat),
        syn::Pat::Paren(p) => pat_is_ok(&p.pat),
        _ => false,
    }
}

/// The value of a string-literal expression, if it is one.
pub fn lit_str(e: &syn::Expr) -> Option<String> {
    match e {
        syn::Expr::Lit(l) => match &l.lit {
            syn::Lit::Str(s) => Some(s.value()),
            _ => None,
        },
        _ => None,
    }
}

/// Does this name look like logging, warning, or panicking?
///
/// Matched by name *shape* rather than an allow-list: every project spells its
/// logger differently (`dbg_log`, `macos_warn`, `tracing::warn`), and an
/// allow-list silently fails on the next one.
pub fn looks_like_logging(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("log")
        || n.contains("warn")
        || n.contains("err")
        || n.contains("trace")
        || n.contains("debug")
        || n.contains("panic")
        || n.contains("report")
        || n.starts_with("eprint")
}

/// Does anything in this subtree make a failure observable — a logging macro,
/// or a call to a logging-shaped method?
///
/// One definition rather than two. `divergence --handling` used to look only at
/// macros while `error-swallows` looked at macros *and* method names, so the
/// two checks disagreed about whether `.unwrap_or_else(|e| self.log_err(e))`
/// counted as handled. Nothing chose that difference; it is what two copies of
/// a heuristic do when only one of them is edited.
pub fn mentions_logging(node: &syn::Expr) -> bool {
    struct V {
        found: bool,
    }
    impl<'ast> syn::visit::Visit<'ast> for V {
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            if let Some(seg) = m.path.segments.last() {
                if looks_like_logging(&seg.ident.to_string()) {
                    self.found = true;
                }
            }
            syn::visit::visit_macro(self, m);
        }
        fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
            if looks_like_logging(&c.method.to_string()) {
                self.found = true;
            }
            syn::visit::visit_expr_method_call(self, c);
        }
    }
    let mut v = V { found: false };
    syn::visit::Visit::visit_expr(&mut v, node);
    v.found
}

#[cfg(test)]
mod shared_predicate_tests {
    use super::*;

    fn expr(src: &str) -> syn::Expr {
        syn::parse_str(src).expect("test expr must parse")
    }

    #[test]
    fn grouping_peels_noise_but_not_borrows() {
        // The distinction the two levels exist for: `peel_grouping` removes
        // what the parser inserted, `peel_expr` also removes what changes the
        // *reference* but not the value.
        assert!(matches!(peel_grouping(&expr("((x))")), syn::Expr::Path(_)));
        assert!(matches!(peel_grouping(&expr("&x")), syn::Expr::Reference(_)));
        assert!(matches!(peel_expr(&expr("&(*x)")), syn::Expr::Path(_)));
        assert!(matches!(peel_expr(&expr("*(&x)")), syn::Expr::Path(_)));
    }

    #[test]
    fn ok_patterns_are_seen_through_refs_and_parens() {
        // `syn::Pat` has no `Parse` impl (patterns are ambiguous standalone),
        // so lift each one out of an `if let`, which is where they occur here.
        let pat = |s: &str| -> syn::Pat {
            let e: syn::Expr = syn::parse_str(&format!("if let {} = x {{}}", s)).unwrap();
            match e {
                syn::Expr::If(i) => match *i.cond {
                    syn::Expr::Let(l) => *l.pat,
                    _ => unreachable!("built an if-let"),
                },
                _ => unreachable!("built an if"),
            }
        };
        assert!(pat_is_ok(&pat("Ok(v)")));
        assert!(pat_is_ok(&pat("&Ok(v)")));
        assert!(pat_is_ok(&pat("io::Result::Ok(v)")));
        assert!(!pat_is_ok(&pat("Err(e)")));
        assert!(!pat_is_ok(&pat("Some(v)")));
    }

    #[test]
    fn a_string_literal_yields_its_value_and_nothing_else_does() {
        assert_eq!(lit_str(&expr("\"hi\"")), Some("hi".to_string()));
        assert_eq!(lit_str(&expr("42")), None);
        assert_eq!(lit_str(&expr("name")), None);
    }

    #[test]
    fn logging_is_recognised_as_a_macro_and_as_a_method() {
        // The reason this is one function: `divergence --handling` looked only
        // at macros while `error-swallows` looked at macros and methods, so the
        // two disagreed about whether the same fallback was "handled".
        assert!(mentions_logging(&expr("{ eprintln!(\"{e}\"); 0 }")));
        assert!(mentions_logging(&expr("{ tracing::warn!(\"x\"); 0 }")));
        assert!(mentions_logging(&expr("{ self.log_err(e); 0 }")));
        assert!(mentions_logging(&expr("{ ctx.report_failure(e); 0 }")));
        assert!(mentions_logging(&expr("{ panic!(\"no\") }")));
    }

    #[test]
    fn a_silent_fallback_is_not_mistaken_for_a_logged_one() {
        assert!(!mentions_logging(&expr("{ 0 }")));
        assert!(!mentions_logging(&expr("{ String::new() }")));
        assert!(!mentions_logging(&expr("{ self.count += 1; 0 }")));
    }

    #[test]
    fn the_logging_name_test_is_shape_based_not_an_allow_list() {
        // Every project spells its logger differently; an allow-list silently
        // fails on the next one.
        for n in ["log", "dbg_log", "macos_warn", "warn", "report_err", "eprintln", "panic"] {
            assert!(looks_like_logging(n), "{} should read as logging", n);
        }
        for n in ["push", "insert", "len", "collect", "name"] {
            assert!(!looks_like_logging(n), "{} should not", n);
        }
    }
}
