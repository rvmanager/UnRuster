//! `tests` subcommand: list `#[test]` / `#[bench]` / `#[tokio::test]` fns
//! with file:start-end and a hint of what each test exercises.
//!
//! Designed to feed agentic workflows like "group these tests by what they
//! cover, find inconsistencies" — the per-test line range lets a reader
//! selectively read the body via `sed -n start,endp file`.

use std::collections::BTreeMap;

use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{line_of, ScopeTracker};
use crate::context::AnalysisCtx;
use crate::parse::{display_path, ParsedFile};

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
}

struct TestVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    grammar: &'a CliGrammar,
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
        self.out.push(TestInfo {
            attr: attr_kind,
            qpath,
            file: self.file.to_string(),
            line_start,
            line_end,
            subcommand,
            hint,
        });
    }
}

impl<'ast, 'a> Visit<'ast> for TestVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.scope.enter_mod(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.scope.leave_mod();
    }

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

fn lit_str(e: &syn::Expr) -> Option<String> {
    if let syn::Expr::Lit(l) = e {
        if let syn::Lit::Str(s) = &l.lit {
            return Some(s.value());
        }
    }
    None
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
pub fn run(
    ctx: &AnalysisCtx,
    full_files: &[ParsedFile],
    with_hint: bool,
    by_subcommand: bool,
    grammar: &CliGrammar,
) -> anyhow::Result<()> {
    let summary = ctx.summary;
    let mut all: Vec<TestInfo> = Vec::new();
    for f in full_files {
        let mut v = TestVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()),
            grammar,
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.out);
    }

    if by_subcommand {
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
                println!("{}\t{}", n, sub);
            }
            if none > 0 {
                println!("{}\t<no detectable subcommand>", none);
            }
        }
        eprintln!(
            "({} test fn(s) across {} distinct subcommand(s){})",
            all.len(),
            rows.len(),
            if none > 0 {
                format!("; {} undetected", none)
            } else {
                String::new()
            }
        );
        return Ok(());
    }

    all.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line_start.cmp(&b.line_start)));

    if !summary {
        for t in &all {
            let range = format!("{}:{}-{}", t.file, t.line_start, t.line_end);
            if with_hint {
                let h = t.hint.as_deref().unwrap_or("");
                println!("{}\t{}\t{}\t{}", t.attr, range, t.qpath, h);
            } else {
                println!("{}\t{}\t{}", t.attr, range, t.qpath);
            }
        }
    }

    use std::collections::BTreeMap as BM;
    let mut by_attr: BM<&str, usize> = BM::new();
    for t in &all {
        *by_attr.entry(t.attr).or_insert(0) += 1;
    }
    let parts: Vec<String> = by_attr.iter().map(|(k, n)| format!("{}={}", k, n)).collect();
    eprintln!("({} test fn(s); {})", all.len(), parts.join(", "));
    Ok(())
}
