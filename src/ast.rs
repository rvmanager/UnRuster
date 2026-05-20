use proc_macro2::Span;
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
                format!("<{}>::{}", type_to_string(&q.ty), path_to_string(&p.path))
            } else {
                path_to_string(&p.path)
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
        _ => "_".to_string(),
    }
}

pub fn type_short(t: &syn::Type) -> String {
    match t {
        syn::Type::Path(p) => path_last(&p.path),
        syn::Type::Reference(r) => type_short(&r.elem),
        _ => type_to_string(t),
    }
}
