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
