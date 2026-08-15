//! `tests` subcommand: list `#[test]` / `#[bench]` / `#[tokio::test]` fns
//! with file:start-end and a hint of what each test exercises.
//!
//! Designed to feed agentic workflows like "group these tests by what they
//! cover, find inconsistencies". Three steps, each answering the question the
//! previous one raises: `--by subcommand` says how many tests cover each,
//! `--subcommand <name>` says which those are, and `--context N` prints their
//! bodies. The range column is a real span cell, so the last step needs no
//! second command — and the fingerprint no longer moves when a test shifts
//! down the file.

use std::collections::BTreeMap;

use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{line_of, lit_str, scope_visits, ScopeTracker};
use crate::context::AnalysisCtx;
use crate::parse::{display_path, ParsedFile};
use crate::emit::row;

#[derive(Debug)]
struct TestInfo {
    attr: &'static str, // "test" | "bench" | "test-async" | "test-other"
    qpath: String,
    file: String,
    line_start: usize,
    line_end: usize,
    /// First sub-command-shaped string literal found inside an `.args([...])`
    /// call in the body. None when we can't detect one.
    subcommand: Option<String>,
    /// Compact reconstruction of the args invocation: drops `--root <path>`
    /// and `--scope <value>`, keeps the subcommand and following flags. None
    /// when no args call detected.
    hint: Option<String>,
    /// How `--mentions <ident>` matched this body, if it did. `None` under no
    /// `--mentions`, and also for a body that does not name it.
    mention: Option<&'static str>,
}

struct TestVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    grammar: &'a CliGrammar,
    mentions: Option<&'a str>,
    out: Vec<TestInfo>,
}

impl<'a> TestVisitor<'a> {
    fn qualify(&self, name: &str) -> String {
        self.scope.qualify(name)
    }

    fn handle_fn(&mut self, attrs: &[syn::Attribute], sig: &syn::Signature, body: &syn::Block) {
        let Some(attr_kind) = classify_test_attr(attrs) else {
            return;
        };
        let line_start = line_of(&sig.ident);
        let line_end = body.span().end().line.max(line_start);
        let qpath = self.qualify(&sig.ident.to_string());
        let (subcommand, hint) = scan_body_for_args(body, self.grammar);
        let mention = self.mentions.and_then(|m| mention_of(body, m));
        self.out.push(TestInfo {
            attr: attr_kind,
            qpath,
            file: self.file.to_string(),
            line_start,
            line_end,
            subcommand,
            hint,
            mention,
        });
    }
}

/// How a test body names `want`: as an identifier, inside a string literal, or
/// both. `None` when it does not name it at all.
///
/// Both, and labelled, because they are different evidence. A test that writes
/// `Kind::Circle` breaks when the variant goes; a test whose fixture string
/// contains `circle r=50` breaks when the *language* drops the word, which is a
/// different migration on a different day. The session that needed this counted
/// them together —
///
/// ```text
/// for s in circle ellipse rect …; do grep -cE "(^|[^a-z-])$s( |=|\")" tests/render.rs; done
/// ```
///
/// — over an 18k-line file, and got one number per word with no way to tell the
/// two apart, no test names, and a regex hand-tuned to avoid matching
/// `rectangle`.
///
/// Walks the block's raw token stream rather than the AST, which is what makes
/// it complete: a test's assertions live inside `assert_eq!` and `matches!`,
/// and a `Visit` impl does not descend into a macro's tokens. Whole-word
/// matching inside literals, so `rect` does not match `rectangle`.
fn mention_of(body: &syn::Block, want: &str) -> Option<&'static str> {
    use quote::ToTokens;
    fn walk(ts: proc_macro2::TokenStream, want: &str, ident: &mut bool, string: &mut bool) {
        for t in ts {
            match t {
                proc_macro2::TokenTree::Ident(i) if i == want => *ident = true,
                proc_macro2::TokenTree::Literal(l) => {
                    if !*string && literal_names(&l.to_string(), want) {
                        *string = true;
                    }
                }
                proc_macro2::TokenTree::Group(g) => walk(g.stream(), want, ident, string),
                _ => {}
            }
        }
    }
    let (mut ident, mut string) = (false, false);
    walk(body.to_token_stream(), want, &mut ident, &mut string);
    match (ident, string) {
        (true, true) => Some("both"),
        (true, false) => Some("ident"),
        (false, true) => Some("string"),
        (false, false) => None,
    }
}

/// Whether a literal token's text contains `want` as a whole word.
///
/// A word boundary here is "not a character an identifier can continue with",
/// and `-` counts as one so a KDL/CSS-ish `corner-tl` is found by `corner`
/// while `rectangle` is not found by `rect`.
fn literal_names(lit: &str, want: &str) -> bool {
    let boundary = |c: Option<char>| !c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut from = 0usize;
    while let Some(i) = lit[from..].find(want) {
        let at = from + i;
        let before = lit[..at].chars().next_back();
        let after = lit[at + want.len()..].chars().next();
        if boundary(before) && boundary(after) {
            return true;
        }
        from = at + want.len();
    }
    false
}

impl<'ast, 'a> Visit<'ast> for TestVisitor<'a> {
    scope_visits!(item_mod);

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.handle_fn(&i.attrs, &i.sig, &i.block);
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.scope.enter_impl(crate::ast::type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.scope.leave_impl();
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.handle_fn(&i.attrs, &i.sig, &i.block);
    }
}

/// Classify a fn's attributes as a known test attribute, or None.
fn classify_test_attr(attrs: &[syn::Attribute]) -> Option<&'static str> {
    for a in attrs {
        let path = a.path();
        let last = path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        match last.as_str() {
            "test" => {
                // `#[test]` or `#[tokio::test]` / `#[async_std::test]` etc.
                if path.segments.len() >= 2 {
                    return Some("test-async");
                }
                return Some("test");
            }
            "bench" => return Some("bench"),
            _ => {}
        }
    }
    None
}

/// The CLI's own grammar, derived from clap introspection in `main.rs` (never
/// hand-maintained — a hand-written copy of this list once drifted and left the
/// three newest subcommands undetected). `subcommands` are the known
/// subcommand names; `value_flags` are the flags that consume the next
/// argument as their value.
pub struct CliGrammar {
    pub subcommands: Vec<String>,
    pub value_flags: std::collections::BTreeSet<String>,
}

/// Walk a test body for the first `.args([...])` method call (or `.arg(...)`)
/// and extract the embedded subcommand + compact hint.
fn scan_body_for_args(body: &syn::Block, grammar: &CliGrammar) -> (Option<String>, Option<String>) {
    let mut s = ArgScanner {
        first_args_literals: None,
        seen: false,
    };
    s.visit_block(body);
    let Some(lits) = s.first_args_literals else {
        return (None, None);
    };
    // Walk pairs: skip flag-and-value pairs (--root <val>, --scope <val>, ...).
    // First non-flag, non-value lit is the candidate subcommand. Cross-check
    // against the known list to filter false positives ("all" / "production"
    // / "unix" / cfg values that happen to look subcommand-shaped).
    let subcommand = detect_subcommand(&lits, grammar);
    let hint = build_hint(&lits);
    (subcommand, hint)
}

fn detect_subcommand(lits: &[String], grammar: &CliGrammar) -> Option<String> {
    let mut i = 0;
    while i < lits.len() {
        let cur = &lits[i];
        if grammar.value_flags.contains(cur.as_str()) {
            i += 2; // skip flag + its value
            continue;
        }
        if cur.starts_with("--") || cur.starts_with('-') {
            i += 1; // bool flag, no value
            continue;
        }
        // First non-flag string. Match against the known list to avoid
        // misreading flag values that happen to look subcommand-shaped.
        if grammar.subcommands.iter().any(|s| s == cur) {
            return Some(cur.clone());
        }
        // Looks subcommand-shaped but unknown — bail rather than guess.
        if looks_like_subcommand(cur) {
            return None;
        }
        i += 1;
    }
    None
}

struct ArgScanner {
    first_args_literals: Option<Vec<String>>,
    seen: bool,
}

impl<'ast> Visit<'ast> for ArgScanner {
    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        if !self.seen && (e.method == "args" || e.method == "arg") {
            if let Some(arg) = e.args.first() {
                if let Some(lits) = extract_string_array(arg) {
                    if !lits.is_empty() {
                        self.first_args_literals = Some(lits);
                        self.seen = true;
                    }
                }
            }
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        // Also catch helper-fn calls like `assert_summary_silent_stdout(&[...])`
        // whose first arg is a string array — the helper wraps `Command::args`.
        if !self.seen {
            if let Some(arg) = e.args.first() {
                if let Some(lits) = extract_string_array(arg) {
                    if !lits.is_empty() {
                        self.first_args_literals = Some(lits);
                        self.seen = true;
                    }
                }
            }
        }
        visit::visit_expr_call(self, e);
    }
}

/// Returns one entry per array element. Non-literal elements (e.g. `FIXTURE`
/// constants) come through as `"<expr>"` so positional pairing with flags
/// like `--root` survives.
fn extract_string_array(e: &syn::Expr) -> Option<Vec<String>> {
    match e {
        syn::Expr::Array(arr) => {
            let mut out = Vec::with_capacity(arr.elems.len());
            for el in &arr.elems {
                out.push(lit_str(el).unwrap_or_else(|| "<expr>".to_string()));
            }
            Some(out)
        }
        syn::Expr::Reference(r) => extract_string_array(&r.expr),
        syn::Expr::Lit(_) => lit_str(e).map(|s| vec![s]),
        _ => None,
    }
}


/// A subcommand looks like `lowercase-with-hyphens`, doesn't start with `-`,
/// and contains no `/` or `.`.
fn looks_like_subcommand(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.contains('/')
        && !s.contains('.')
        && s.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// Build a compact hint from a list of `.args([...])` string literals.
/// Drops the `--root <path>` and `--scope <val>` pairs (and `<expr>`
/// placeholders that came from non-literal Rust constants); keeps the
/// subcommand and meaningful flags so the fingerprint is grep-able.
fn build_hint(lits: &[String]) -> Option<String> {
    let mut kept = Vec::new();
    let mut i = 0;
    while i < lits.len() {
        let cur = &lits[i];
        if cur == "--root" || cur == "--scope" {
            i += 2; // drop flag + value entirely
            continue;
        }
        if cur == "<expr>" {
            i += 1;
            continue;
        }
        if cur == "--cfg" && i + 1 < lits.len() {
            kept.push(format!("--cfg {}", lits[i + 1]));
            i += 2;
            continue;
        }
        kept.push(cur.clone());
        i += 1;
    }
    if kept.is_empty() {
        None
    } else {
        Some(kept.join(" "))
    }
}

/// `full_files` is the FULL tree (tests included) — under `--scope production`
/// the tests this command enumerates would be stripped from `ctx.files`, so
/// never read files from `ctx` here.
/// The `--subcommand` value that selects the tests whose subcommand could not
/// be detected — the bucket `--by subcommand` reports as
/// `<no detectable subcommand>`, which is not a string anyone will type.
/// Safe as a literal because `unruster` has no subcommand called `none`, and
/// [`run`] asserts that rather than trusting it.
pub const NO_SUBCOMMAND: &str = "none";

/// What `tests` was asked for. A struct rather than four more positional
/// arguments, matching `FieldUsesOpts` / `SwallowOpts` / `ScanOpts` — three
/// adjacent `bool`s in a call are three chances to swap two of them, and
/// nothing at the call site would say so.
pub struct TestsOpts<'a> {
    /// Append each test's `.args([...])` fingerprint.
    pub with_hint: bool,
    /// Print the per-subcommand histogram instead of the per-test listing.
    pub by_subcommand: bool,
    /// List only the tests invoking this subcommand ([`NO_SUBCOMMAND`] for the
    /// ones whose subcommand could not be detected).
    pub only: Option<&'a str>,
    /// List only the tests whose body names this identifier — the blast radius
    /// of removing it. Adds a `via` column saying how each one names it.
    pub mentions: Option<&'a str>,
}

pub fn run(
    ctx: &AnalysisCtx,
    full_files: &[ParsedFile],
    opts: &TestsOpts,
    grammar: &CliGrammar,
) -> anyhow::Result<usize> {
    let summary = ctx.summary;
    let mut all: Vec<TestInfo> = Vec::new();
    for f in full_files {
        let mut v = TestVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()),
            grammar,
            mentions: opts.mentions,
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.out);
    }

    // Before the subcommand views, so `--mentions` composes with them: "which
    // of the tests that name `Kind` drive the `trace` subcommand" is one
    // question, and answering it needed two greps and an eyeball join.
    if let Some(want) = opts.mentions {
        let scanned = all.len();
        all.retain(|t| t.mention.is_some());
        if all.is_empty() {
            // Not an error: "no test names this" is a real and useful answer
            // before a removal — it is the cheap case, and reporting it as a
            // failed lookup would make the reader doubt the spelling instead.
            ctx.out.summary(&format!(
                "(0 of {} test fn(s) name `{}` — nothing in the test suite depends on it \
                 by that spelling)",
                scanned, want
            ));
            return Ok(0);
        }
    }

    // The histogram says *how many* tests cover each subcommand; this says
    // *which*. Without it the only way from `6  impls` to those six tests was
    // to grep the test file for the subcommand string and read what came back
    // — which is the same locate-by-guessing that `show` exists to end.
    if let Some(want) = opts.only {
        return listing(
            ctx,
            filter_to(ctx, all, want)?,
            opts.with_hint,
            Some(want),
            opts.mentions,
        );
    }

    if opts.by_subcommand {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut none = 0usize;
        for t in &all {
            match &t.subcommand {
                Some(s) => *counts.entry(s.clone()).or_insert(0) += 1,
                None => none += 1,
            }
        }
        let mut rows: Vec<_> = counts.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if !summary {
            for (sub, n) in &rows {
                row!(ctx.out, "count" => *n, "subcommand" => sub.clone());
            }
            if none > 0 {
                row!(
                    ctx.out,
                    "count" => none,
                    "subcommand" => "<no detectable subcommand>",
                );
            }
        }
        ctx.out.summary(&format!(
            "({} test fn(s) across {} distinct subcommand(s){})",
            all.len(),
            rows.len(),
            if none > 0 {
                format!("; {} undetected", none)
            } else {
                String::new()
            }
        ));
        return Ok(all.len());
    }

    listing(ctx, all, opts.with_hint, None, opts.mentions)
}

/// Keep only the tests that exercise `want`, or fail with the alternatives.
///
/// An empty listing would be the wrong answer twice over: it cannot distinguish
/// "no test covers this subcommand" (worth knowing, and actionable) from "you
/// typed a subcommand that does not exist" (a typo), and it offers nothing
/// either way.
fn filter_to(
    ctx: &AnalysisCtx,
    all: Vec<TestInfo>,
    want: &str,
) -> anyhow::Result<Vec<TestInfo>> {
    // Owned: the failure path needs this list after `all` has been consumed by
    // the filter, and it is only built when the filter is in play.
    let covered: Vec<String> = {
        let mut v: Vec<String> = all
            .iter()
            .filter_map(|t| t.subcommand.clone())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let hits: Vec<TestInfo> = all
        .into_iter()
        .filter(|t| match t.subcommand.as_deref() {
            Some(s) => s == want,
            None => want == NO_SUBCOMMAND,
        })
        .collect();
    if !hits.is_empty() {
        return Ok(hits);
    }
    // Nothing matched. Say which of the two reasons it was.
    let near = crate::index::closest(want, covered.iter().map(String::as_str), 5);
    if near.is_empty() {
        ctx.out.note(&format!(
            "note: no test invokes `{}`. Subcommands with tests: {} (or `--subcommand {}` \
             for the tests whose subcommand could not be detected)",
            want,
            covered.join(", "),
            NO_SUBCOMMAND
        ));
    } else {
        ctx.out.note(&format!(
            "note: no test invokes `{}`. Did you mean: {}",
            want,
            near.join(", ")
        ));
    }
    Err(crate::context::TargetNotFound::err("tested subcommand", want))
}

/// One row per test: attr, `file:start-end`, qpath, and optionally the hint.
fn listing(
    ctx: &AnalysisCtx,
    mut all: Vec<TestInfo>,
    with_hint: bool,
    only: Option<&str>,
    mentions: Option<&str>,
) -> anyhow::Result<usize> {
    all.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line_start.cmp(&b.line_start)));

    if !ctx.summary {
        for t in &all {
            // Cells built rather than four `row!` arms over two independent
            // optional columns: that shape was already two near-copies before
            // `--mentions` and would have been four, which is what
            // `near-clones` reports on other people's code.
            //
            // Both optional columns follow the same rule — a column appears
            // only when its flag asked for it, so an existing `tests`
            // invocation's TSV keeps its shape. `via` leads because it is what
            // the reader scans under `--mentions`.
            let mut cells: Vec<(&'static str, crate::emit::Val)> = Vec::with_capacity(5);
            if let Some(via) = t.mention {
                cells.push(("via", via.into()));
            }
            cells.push(("attr", t.attr.into()));
            // A typed span cell, not the `format!("{}:{}-{}")` string this
            // built by hand. Same TSV text, but `--context N` can now find the
            // site and print the body, and `--json` gets real
            // `file`/`line`/`end_line` fields instead of one opaque string —
            // which is the whole reason the column existed.
            cells.push((
                "range",
                crate::emit::span_site(&t.file, t.line_start, t.line_end),
            ));
            cells.push(("qpath", t.qpath.clone().into()));
            if with_hint {
                cells.push(("hint", t.hint.as_deref().unwrap_or("").into()));
            }
            ctx.out.row(cells);
        }
    }

    use std::collections::BTreeMap as BM;
    let mut by_attr: BM<&str, usize> = BM::new();
    for t in &all {
        *by_attr.entry(t.attr).or_insert(0) += 1;
    }
    let parts: Vec<String> = by_attr.iter().map(|(k, n)| format!("{}={}", k, n)).collect();
    let scope = match (only, mentions) {
        (Some(s), Some(m)) => format!(" invoking `{}` and naming `{}`", s, m),
        (Some(s), None) => format!(" invoking `{}`", s),
        (None, Some(m)) => format!(" naming `{}`", m),
        (None, None) => String::new(),
    };
    ctx.out.summary(&format!(
        "({} test fn(s){}; {}{})",
        all.len(),
        scope,
        parts.join(", "),
        // The two ways a body can name an identifier are different evidence and
        // break on different days — see `mention_of`.
        match mentions {
            Some(_) => {
                let by = |k: &str| all.iter().filter(|t| t.mention == Some(k)).count();
                format!(
                    "; via: {} ident, {} string, {} both",
                    by("ident"),
                    by("string"),
                    by("both")
                )
            }
            None => String::new(),
        }
    ));
    Ok(all.len())
}
