use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;

use proc_macro2::{TokenStream, TokenTree};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;

/// Macro bodies whose tokens could not be parsed as expressions this run —
/// code inside them was NOT analyzed. Surfaced by main as a blind-spot count
/// so "0 findings" is never silently mistaken for "0 findings in code we
/// couldn't see". Keyed by (line, column, token hash) so a macro visited by
/// several checks in one run counts once. (Quoting macros like `quote!` are
/// skipped on purpose and not counted.)
static UNPARSED_MACRO_BODIES: Mutex<Option<HashSet<(usize, usize, u64)>>> = Mutex::new(None);

fn record_blind_spot(m: &syn::Macro) {
    let start = m.path.span().start();
    let mut h = DefaultHasher::new();
    m.tokens.to_string().hash(&mut h);
    let key = (start.line, start.column, h.finish());
    let mut guard = UNPARSED_MACRO_BODIES.lock().unwrap();
    guard.get_or_insert_with(HashSet::new).insert(key);
}

/// Number of distinct macro bodies that resisted expression parsing this run.
pub fn blind_spots() -> usize {
    UNPARSED_MACRO_BODIES
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.len())
        .unwrap_or(0)
}

/// Structured parse of a macro body, when we can recognize the shape.
// `Body` is a transient per-macro return value, consumed immediately and never
// stored in bulk, so the variant size gap doesn't matter — boxing would only
// add indirection.
#[allow(clippy::large_enum_variant)]
pub enum Body {
    /// Best-effort parse of macro args as a list of expressions.
    Exprs(Vec<syn::Expr>),
    /// `matches!(scrutinee, pat)` — scrutinee is an expression, second arg is a pattern.
    Matches { scrutinee: syn::Expr, pat: syn::Pat },
}

/// Parse a macro body into structured form.
///
/// Strategy for the generic `Exprs` case:
/// 1. Try `Punctuated<Expr, ,>` directly (handles `println!("{}", x.f)`, `dbg!(x, y)`, ...).
/// 2. Else split on top-level `,` and `;`, parse each chunk as `Expr` (handles `vec![x.f; n]`).
/// 3. Else parse the whole stream as a single `Expr`.
/// 4. Pieces that fail are silently dropped.
///
/// Special-cases `matches!(e, p)` so the pattern arg isn't mis-treated as an expression.
///
/// Returns `Exprs(vec![])` for "quoting" macros (`quote!`, `parse_quote!`, ...) where
/// inner tokens are TokenStream construction, not real expressions.
pub fn macro_body(m: &syn::Macro) -> Body {
    if is_quoting_macro(m) {
        return Body::Exprs(Vec::new());
    }
    if is_matches_macro(m) {
        if let Some((e, p)) = parse_matches(m) {
            return Body::Matches { scrutinee: e, pat: p };
        }
    }
    let exprs = parse_exprs(&m.tokens);
    if exprs.is_empty() && !m.tokens.is_empty() {
        record_blind_spot(m);
    }
    Body::Exprs(exprs)
}

/// Backwards-compatible convenience: returns just expressions, ignoring any
/// pattern-shaped portion (e.g. of `matches!`).
pub fn macro_exprs(m: &syn::Macro) -> Vec<syn::Expr> {
    match macro_body(m) {
        Body::Exprs(es) => es,
        Body::Matches { scrutinee, .. } => vec![scrutinee],
    }
}

fn parse_exprs(tokens: &TokenStream) -> Vec<syn::Expr> {
    if let Ok(list) =
        Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated.parse2(tokens.clone())
    {
        return list.into_iter().collect();
    }
    let mut out = Vec::new();
    for chunk in split_at_top_level(tokens, &[',', ';']) {
        if let Ok(expr) = syn::parse2::<syn::Expr>(chunk) {
            out.push(expr);
        }
    }
    if !out.is_empty() {
        return out;
    }
    if let Ok(expr) = syn::parse2::<syn::Expr>(tokens.clone()) {
        return vec![expr];
    }
    Vec::new()
}

fn parse_matches(m: &syn::Macro) -> Option<(syn::Expr, syn::Pat)> {
    let chunks = split_at_top_level(&m.tokens, &[',']);
    if chunks.len() < 2 {
        return None;
    }
    let scrutinee = syn::parse2::<syn::Expr>(chunks[0].clone()).ok()?;
    // The pattern chunk may have a trailing `if <guard>`; strip it before parsing.
    let pat_tokens = strip_match_guard(&chunks[1]);
    let pat = syn::Pat::parse_multi_with_leading_vert
        .parse2(pat_tokens)
        .ok()?;
    Some((scrutinee, pat))
}

fn strip_match_guard(ts: &TokenStream) -> TokenStream {
    let mut acc: Vec<TokenTree> = Vec::new();
    for tt in ts.clone() {
        if let TokenTree::Ident(id) = &tt {
            if id == "if" {
                break;
            }
        }
        acc.push(tt);
    }
    acc.into_iter().collect()
}

fn is_matches_macro(m: &syn::Macro) -> bool {
    m.path
        .segments
        .last()
        .map(|s| s.ident == "matches")
        .unwrap_or(false)
}

pub fn is_quoting_macro(m: &syn::Macro) -> bool {
    let Some(last) = m.path.segments.last() else {
        return false;
    };
    matches!(
        last.ident.to_string().as_str(),
        "quote" | "quote_spanned" | "parse_quote" | "parse_quote_spanned"
    )
}

fn split_at_top_level(ts: &TokenStream, seps: &[char]) -> Vec<TokenStream> {
    let mut acc: Vec<TokenTree> = Vec::new();
    let mut out: Vec<TokenStream> = Vec::new();
    for tt in ts.clone() {
        match &tt {
            TokenTree::Punct(p) if seps.contains(&p.as_char()) => {
                if !acc.is_empty() {
                    out.push(acc.drain(..).collect());
                }
            }
            _ => acc.push(tt),
        }
    }
    if !acc.is_empty() {
        out.push(acc.into_iter().collect());
    }
    out
}
