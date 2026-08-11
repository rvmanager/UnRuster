use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;

use proc_macro2::{TokenStream, TokenTree};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;

/// Macro bodies whose tokens could not be parsed as expressions — code inside
/// them was NOT analyzed. Surfaced by main as a blind-spot count so "0
/// findings" is never silently mistaken for "0 findings in code we couldn't
/// see". (Quoting macros like `quote!` are skipped on purpose and not counted.)
///
/// Keyed by **file** plus (line, column, token hash). The file used to be
/// absent, so two identical macros at the same line and column in different
/// files collided into one entry and the count silently ran low — an
/// undercount in the one number whose whole job is to bound what the run did
/// not see.
/// (file, line, column, macro name, token hash). The name is carried so the
/// listing can say *which* macro is dark — a bare count is a fact nobody can
/// act on, and on a real 6k-item codebase "45 macro bodies" left the reader
/// writing "a `dbg_log!`/`format!`-heavy region could be hiding something none
/// of this saw" with no way to check.
type BlindSpot = (String, usize, usize, String, u64);
static UNPARSED_MACRO_BODIES: Mutex<Option<HashSet<BlindSpot>>> = Mutex::new(None);

/// Set once [`survey`] has walked the tree. After that the count is a closed
/// property of the scanned files and incidental visits must not extend it.
///
/// Before this existed, the counter accumulated as checks happened to reach
/// macros, so the number was a function of *which checks ran*: the same tree at
/// the same commit reported 21 under `audit`, 18 under `error-swallows` and 17
/// under `enum-coverage`, while the note claimed each was the count of macro
/// bodies in the code. Three answers to one question reads as instability in
/// exactly the number a reader has to trust.
static SURVEYED: Mutex<bool> = Mutex::new(false);

fn record_blind_spot(m: &syn::Macro) {
    // Outside the survey the visit set is arbitrary, so recording would make
    // the total depend on the subcommand. `survey` has already seen every
    // macro in the tree; there is nothing here it missed.
    if *SURVEYED.lock().unwrap() {
        return;
    }
    record_in(m, "");
}

fn record_in(m: &syn::Macro, file: &str) {
    let start = m.path.span().start();
    let mut h = DefaultHasher::new();
    m.tokens.to_string().hash(&mut h);
    let name = m
        .path
        .segments
        .last()
        .map(|s| format!("{}!", s.ident))
        .unwrap_or_else(|| "?!".to_string());
    let key = (file.to_string(), start.line, start.column, name, h.finish());
    let mut guard = UNPARSED_MACRO_BODIES.lock().unwrap();
    guard.get_or_insert_with(HashSet::new).insert(key);
}

/// Walk every macro in the scanned tree and record the ones that resist
/// expression parsing, then seal the count.
///
/// Called once, from `main`, immediately after parsing — so every subcommand
/// reports the same number for the same tree, and that number is the whole
/// tree's rather than whatever subset the chosen checks happened to touch.
pub fn survey(files: &[crate::parse::ParsedFile]) {
    {
        let mut sealed = SURVEYED.lock().unwrap();
        if *sealed {
            return;
        }
        // Recording stays open for the duration of the walk below.
        *sealed = false;
    }
    for f in files {
        let display = crate::parse::display_path(&f.path);
        let mut v = SurveyVisitor { file: &display };
        syn::visit::Visit::visit_file(&mut v, &f.ast);
    }
    *SURVEYED.lock().unwrap() = true;
}

/// Classifies every macro exactly as the checks do — same [`macro_body`]
/// entry point — so the survey can never disagree with them about what is a
/// blind spot.
struct SurveyVisitor<'a> {
    file: &'a str,
}

impl<'ast> syn::visit::Visit<'ast> for SurveyVisitor<'_> {
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if !is_quoting_macro(m) && !is_matches_macro(m) {
            let exprs = parse_exprs(&m.tokens);
            if exprs.is_empty() && tokens_have_substance(&m.tokens) {
                record_in(m, self.file);
            }
        }
        syn::visit::visit_macro(self, m);
    }
}

/// Number of distinct macro bodies in the scanned tree that resisted
/// expression parsing.
/// Every blind spot, as `(file, line, macro)`, sorted for a stable listing.
pub fn blind_spot_sites() -> Vec<(String, usize, String)> {
    let guard = UNPARSED_MACRO_BODIES.lock().unwrap();
    let mut out: Vec<(String, usize, String)> = guard
        .as_ref()
        .map(|s| {
            s.iter()
                .map(|(f, l, _, n, _)| (f.clone(), *l, n.clone()))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

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
    if exprs.is_empty() && tokens_have_substance(&m.tokens) {
        record_blind_spot(m);
    }
    Body::Exprs(exprs)
}

/// True if the token stream contains any identifier or literal — pure
/// punctuation (e.g. the type macro `Token![,]`) can never be an expression
/// and must not count as a blind spot.
fn tokens_have_substance(ts: &TokenStream) -> bool {
    ts.clone().into_iter().any(|tt| match tt {
        TokenTree::Ident(_) | TokenTree::Literal(_) => true,
        TokenTree::Group(g) => tokens_have_substance(&g.stream()),
        TokenTree::Punct(_) => false,
    })
}

/// Backwards-compatible convenience: returns just expressions, ignoring any
/// pattern-shaped portion (e.g. of `matches!`).
pub fn macro_exprs(m: &syn::Macro) -> Vec<syn::Expr> {
    match macro_body(m) {
        Body::Exprs(es) => es,
        Body::Matches { scrutinee, .. } => vec![scrutinee],
    }
}

// unruster: ok(error-swallows/if-let-ok) 2026-08-06 — a chunk that will not
// parse as an expression is skipped on purpose; the caller gets the pieces
// that did parse and every unparsed macro body is reported as a blind spot.
/// Both halves of a `key => value` arm, each parsed as an expression.
///
/// Returns empty when the chunk holds no `=>` or when neither half parses, so a
/// caller can fall through to its other strategies unchanged.
fn split_fat_arrow(chunk: &TokenStream) -> Vec<syn::Expr> {
    let parts = split_at_top_level_fat_arrow(chunk);
    if parts.len() < 2 {
        return Vec::new();
    }
    parts
        .into_iter()
        .filter_map(|p| syn::parse2::<syn::Expr>(p).ok())
        .collect()
}

/// Split on a top-level `=>`, i.e. one not nested inside a delimiter. Written
/// separately from [`split_at_top_level`] because `=>` is two joint punctuation
/// tokens rather than one character.
fn split_at_top_level_fat_arrow(tokens: &TokenStream) -> Vec<TokenStream> {
    let mut parts: Vec<TokenStream> = Vec::new();
    let mut cur: Vec<TokenTree> = Vec::new();
    let mut it = tokens.clone().into_iter().peekable();
    while let Some(tt) = it.next() {
        if let TokenTree::Punct(p) = &tt {
            if p.as_char() == '=' && p.spacing() == proc_macro2::Spacing::Joint {
                if let Some(TokenTree::Punct(n)) = it.peek() {
                    if n.as_char() == '>' {
                        it.next();
                        parts.push(cur.drain(..).collect());
                        continue;
                    }
                }
            }
        }
        cur.push(tt);
    }
    parts.push(cur.into_iter().collect());
    parts
}

fn parse_exprs(tokens: &TokenStream) -> Vec<syn::Expr> {
    if let Ok(list) =
        Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated.parse2(tokens.clone())
    {
        return list.into_iter().collect();
    }
    let mut out = Vec::new();
    for chunk in split_at_top_level(tokens, &[',', ';']) {
        if let Ok(expr) = syn::parse2::<syn::Expr>(chunk.clone()) {
            out.push(expr);
            continue;
        }
        // A `key => value` arm. `row!(out, "at" => at(d, range))` parsed to
        // `out` alone and dropped the arm, so `show::at` was called on that
        // very line and reported zero callers — and `row!` is how every
        // command in this tool emits, so the hole was the width of the
        // codebase. Neither side is an expression on its own *with* the `=>`
        // between them; both are without it.
        out.extend(split_fat_arrow(&chunk));
    }
    if !out.is_empty() {
        return out;
    }
    if let Ok(expr) = syn::parse2::<syn::Expr>(tokens.clone()) {
        return vec![expr];
    }
    // Last resort: a body that is a *statement sequence* rather than an
    // expression list. `tokio::select! { … }`, `bitflags! { … }` and any macro
    // taking a block of code fail every attempt above — a `let` binding is not
    // an expression and splitting on `;` leaves half-statements — so they were
    // recorded as blind spots and analyzed by nothing. Parsing them as
    // statements recovers every expression they contain.
    exprs_of_stmts(tokens)
}

/// Parse `tokens` as a sequence of statements and return the expressions in
/// them. Empty when the stream is not statement-shaped either.
fn exprs_of_stmts(tokens: &TokenStream) -> Vec<syn::Expr> {
    let Ok(stmts) = syn::Block::parse_within.parse2(tokens.clone()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for st in stmts {
        match st {
            syn::Stmt::Expr(e, _) => out.push(e),
            // The initialiser is the part that can contain a call worth seeing;
            // `let x;` contributes nothing and is not a failure to parse.
            syn::Stmt::Local(l) => {
                if let Some(init) = l.init {
                    out.push(*init.expr);
                    if let Some((_, diverge)) = init.diverge {
                        out.push(*diverge);
                    }
                }
            }
            // A nested item or macro: its own body is visited by the AST walk
            // that owns this file, so nothing is lost by not descending here.
            syn::Stmt::Item(_) | syn::Stmt::Macro(_) => {}
        }
    }
    out
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
