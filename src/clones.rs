//! Duplicated function bodies — the same code written out more than once.
//!
//! This is the `replication` thesis at its most literal, and it was the tool's
//! largest blind spot. On a twelve-crate workspace an agent working the audit
//! loop found, by hand-grepping, seven byte-identical copies of `caller<T>`,
//! five of `parse_uuid`, four of `ts`, and six inline re-spellings of the same
//! UUID parse — sixteen copies of three concepts, all inside one directory.
//! Consolidating them deleted eleven other findings outright rather than
//! waiving them, which made it the highest-leverage change of that session.
//! `conversion-pairs` reported zero, `pass-through` reported zero, and nothing
//! else looked.
//!
//! # What counts as a clone
//!
//! Bodies are compared after **alpha-renaming**: every binding a function
//! introduces — parameters, `let` patterns, closure arguments, match arm
//! bindings, loop patterns — is rewritten to a positional placeholder in
//! order of first appearance. Everything else is compared verbatim, including
//! called method and path names, literals, and control flow.
//!
//! That cut is deliberate. Renaming *called* names would group any two
//! functions with the same shape (`fn a() { x.foo() }` and `fn b() { y.bar() }`),
//! which is a similarity metric, not a defect report. Keeping them means a
//! group is a set of functions that do the same thing to the same APIs, and
//! differ only in what the locals are called — which is what a copy-paste
//! actually looks like.
//!
//! Two copies of a three-token accessor are not a finding, so bodies below
//! `--min-tokens` are ignored.
//!
//! # Precision
//!
//! EXACT. Two bodies in one group are token-identical after renaming; there is
//! no similarity threshold and no fuzzy match. What the check cannot tell you
//! is whether consolidating is *right* — a trait impl that must repeat a
//! default, a macro-generated shape, two modules deliberately kept
//! independent. Those are judgment calls, which is what the waiver is for.

use std::collections::HashMap;

use quote::ToTokens;
use syn::visit::{self, Visit};

use crate::ast::{line_of, scope_visits, ScopeTracker};
use crate::context::{AnalysisCtx, Counts};
use crate::emit::{row, site};
use crate::parse::display_path;

/// One function body, canonicalized.
#[derive(Debug)]
struct Body {
    /// Alpha-renamed token text — the grouping key.
    canon: String,
    /// Fully-qualified path as the scope tracker sees it.
    qpath: String,
    /// The declared name alone, for the "same name too" signal.
    name: String,
    file: String,
    line: usize,
    /// Tokens in the canonical body. Stands in for size; a copy-pasted
    /// 40-token helper is a bigger deal than a copy-pasted 12-token one.
    tokens: usize,
}

/// A set of functions sharing one canonical body.
#[derive(Debug)]
struct Group {
    members: Vec<Body>,
    tokens: usize,
    /// Every member declares the same identifier. The strongest form: the same
    /// helper, written out N times, under N different roofs.
    same_name: bool,
    /// Every member lives under the same directory — copies that a single
    /// `mod` could hold, which is where consolidation is cheapest and safest.
    same_dir: bool,
}

impl Group {
    /// Rank: how much is duplicated, how many times, and how easy the fix is.
    ///
    /// Size and copy count multiply because they compound — four copies of a
    /// 40-token body is not twice the problem of two copies, it is the same
    /// helper having drifted four ways. The two booleans are what turn a
    /// merely-identical pair into an obviously-extractable one, and they are
    /// what put `parse_uuid`-shaped findings on top.
    fn score(&self) -> f64 {
        let copies = self.members.len() as f64;
        // Saturating at 40 tokens and at 5 copies: past those the finding is
        // already "yes, extract this", and letting either run away would sort
        // one enormous group above three obvious ones.
        let bulk = (self.tokens as f64 / 40.0).min(1.0);
        // Zero at exactly two copies. A pair is the smallest group that can
        // exist, so it earns no credit for being one — otherwise every pair
        // clears the gate and the gate is the list.
        let spread = ((copies - 2.0) / 3.0).clamp(0.0, 1.0);
        let named = if self.same_name { 0.20 } else { 0.0 };
        // Weakest of the signals, and weighted like it: in a crate whose
        // modules all sit directly in `src/`, "same directory" is free.
        let local = if self.same_dir { 0.05 } else { 0.0 };
        (0.25 + 0.20 * bulk + 0.175 * spread + named + local).min(1.0)
    }

    /// The shortest description of what is duplicated.
    fn label(&self) -> String {
        if self.same_name {
            self.members[0].name.clone()
        } else {
            // Distinct names: name the group by its members so the row is
            // still greppable.
            let mut names: Vec<&str> =
                self.members.iter().map(|m| m.name.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            names.join("/")
        }
    }
}

/// Rewrites every binding the function introduces to `_0`, `_1`, … in order of
/// first appearance, so two copies that differ only in local naming collapse to
/// one key.
///
/// Collected from patterns rather than from every identifier: a pattern is
/// exactly where Rust introduces a name, so this renames what the author chose
/// and leaves what they called alone.
struct Renamer {
    map: HashMap<String, String>,
}

impl Renamer {
    fn new() -> Self {
        Renamer {
            map: HashMap::new(),
        }
    }

    fn bind(&mut self, ident: &syn::Ident) {
        let k = ident.to_string();
        if !self.map.contains_key(&k) {
            let n = self.map.len();
            self.map.insert(k, format!("_{n}"));
        }
    }

    /// Walk a pattern and bind every identifier it introduces.
    fn bind_pat(&mut self, p: &syn::Pat) {
        struct V<'a>(&'a mut Renamer);
        impl<'ast> Visit<'ast> for V<'_> {
            fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
                self.0.bind(&p.ident);
                visit::visit_pat_ident(self, p);
            }
            // A struct pattern's *field* names are API, not bindings. The
            // shorthand `Foo { bar }` does bind `bar`, and `visit_pat_ident`
            // above catches it; the explicit `Foo { bar: x }` binds `x` only,
            // which is also what the default walk reaches.
        }
        V(self).visit_pat(p);
    }
}

/// Collects the bindings of one function, in source order.
struct BindingCollector<'a> {
    r: &'a mut Renamer,
}

impl<'ast> Visit<'ast> for BindingCollector<'_> {
    fn visit_local(&mut self, l: &'ast syn::Local) {
        self.r.bind_pat(&l.pat);
        visit::visit_local(self, l);
    }
    fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
        for i in &c.inputs {
            self.r.bind_pat(i);
        }
        visit::visit_expr_closure(self, c);
    }
    fn visit_arm(&mut self, a: &'ast syn::Arm) {
        self.r.bind_pat(&a.pat);
        visit::visit_arm(self, a);
    }
    fn visit_expr_for_loop(&mut self, e: &'ast syn::ExprForLoop) {
        self.r.bind_pat(&e.pat);
        visit::visit_expr_for_loop(self, e);
    }
    fn visit_expr_let(&mut self, e: &'ast syn::ExprLet) {
        self.r.bind_pat(&e.pat);
        visit::visit_expr_let(self, e);
    }
}

/// Canonical token text of a body, with bindings alpha-renamed.
///
/// Renders through the token stream rather than the AST so the result is
/// whitespace- and formatting-independent: two copies that `rustfmt` broke
/// differently still hash the same.
fn canonicalize(sig: &syn::Signature, body: &syn::Block) -> (String, usize) {
    let mut r = Renamer::new();
    // Parameters first, so the placeholder order matches the call shape and two
    // copies with reordered `let`s still agree on their argument names.
    for arg in &sig.inputs {
        match arg {
            syn::FnArg::Receiver(_) => {}
            syn::FnArg::Typed(t) => r.bind_pat(&t.pat),
        }
    }
    BindingCollector { r: &mut r }.visit_block(body);

    let mut out = String::new();
    let mut n = 0usize;
    render(body.to_token_stream(), &r, &mut out, &mut n);
    (out, n)
}

/// Flatten a token stream to a canonical string, substituting renamed
/// bindings. Delimiters are emitted so `f(a)(b)` and `f(a, b)` never collide.
fn render(
    ts: proc_macro2::TokenStream,
    r: &Renamer,
    out: &mut String,
    n: &mut usize,
) {
    use proc_macro2::TokenTree;
    for tt in ts {
        *n += 1;
        match tt {
            TokenTree::Ident(i) => {
                let s = i.to_string();
                out.push_str(r.map.get(&s).map_or(s.as_str(), |v| v.as_str()));
                out.push(' ');
            }
            TokenTree::Literal(l) => {
                out.push_str(&l.to_string());
                out.push(' ');
            }
            TokenTree::Punct(p) => {
                out.push(p.as_char());
            }
            TokenTree::Group(g) => {
                let (open, close) = match g.delimiter() {
                    proc_macro2::Delimiter::Parenthesis => ('(', ')'),
                    proc_macro2::Delimiter::Brace => ('{', '}'),
                    proc_macro2::Delimiter::Bracket => ('[', ']'),
                    proc_macro2::Delimiter::None => ('\u{2039}', '\u{203a}'),
                };
                out.push(open);
                render(g.stream(), r, out, n);
                out.push(close);
            }
        }
    }
}

struct CloneVisitor<'a> {
    min_tokens: usize,
    file: &'a str,
    scope: ScopeTracker,
    bodies: Vec<Body>,
}

impl CloneVisitor<'_> {
    fn check(&mut self, sig: &syn::Signature, body: &syn::Block) {
        let (canon, tokens) = canonicalize(sig, body);
        if tokens < self.min_tokens {
            return;
        }
        let name = sig.ident.to_string();
        self.bodies.push(Body {
            canon,
            qpath: self.scope.qualify(&name),
            name,
            file: self.file.to_string(),
            line: line_of(&sig.ident),
            tokens,
        });
    }
}

impl<'ast> Visit<'ast> for CloneVisitor<'_> {
    scope_visits!(item_mod, item_impl, item_trait);
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.check(&i.sig, &i.block);
        visit::visit_item_fn(self, i);
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.check(&i.sig, &i.block);
        visit::visit_impl_item_fn(self, i);
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        if let Some(body) = &i.default {
            self.check(&i.sig, body);
        }
        visit::visit_trait_item_fn(self, i);
    }
}

/// Directory of a display path, for the `same_dir` signal.
fn dir_of(file: &str) -> &str {
    file.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Minimum canonical tokens before a body can be part of a group. A `{ self.0 }`
/// accessor repeated thirty times is a data point about the language, not about
/// the codebase.
pub const DEFAULT_MIN_TOKENS: usize = 24;

/// The score at or above which a clone group is a gating audit finding.
///
/// Set so that the class it admits is "the same named helper, written out more
/// than twice, in one directory" — the `parse_uuid` shape, where the fix is a
/// `mod` and a `use` and it deletes findings rather than moving them.
///
/// Two identical copies of one helper in two modules lands at 0.65 and stays
/// advisory on purpose. It is a real lead — the pair can drift, and on this
/// codebase `pat_is_ok` and `peel` are exactly that — but a pair is also how a
/// deliberate boundary between two crates looks, and gating on it would put the
/// whole list in the gate. At 0.75 the gate is the groups where copy count or
/// bulk has already answered the question.
pub const GATING_SCORE: f64 = 0.75;

pub fn run(ctx: &AnalysisCtx, min_tokens: usize, top: Option<usize>) -> anyhow::Result<usize> {
    Ok(run_counted(ctx, min_tokens, top)?.total)
}

/// As [`run`], but also reporting how many groups clear [`GATING_SCORE`].
pub fn run_counted(
    ctx: &AnalysisCtx,
    min_tokens: usize,
    top: Option<usize>,
) -> anyhow::Result<Counts> {
    let mut bodies: Vec<Body> = Vec::new();
    for f in ctx.files {
        let mut v = CloneVisitor {
            min_tokens,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            bodies: Vec::new(),
        };
        v.visit_file(&f.ast);
        bodies.extend(v.bodies);
    }
    let scanned = bodies.len();

    let mut by_canon: HashMap<String, Vec<Body>> = HashMap::new();
    for b in bodies {
        by_canon.entry(b.canon.clone()).or_default().push(b);
    }

    let mut groups: Vec<Group> = by_canon
        .into_values()
        .filter(|m| m.len() > 1)
        .map(|mut members| {
            members.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));
            let tokens = members[0].tokens;
            let same_name = members.iter().all(|m| m.name == members[0].name);
            let d = dir_of(&members[0].file);
            let same_dir = members.iter().all(|m| dir_of(&m.file) == d);
            Group {
                members,
                tokens,
                same_name,
                same_dir,
            }
        })
        .collect();

    // `--changed-since` keeps a group when *any* copy is in the changed set:
    // the finding is the duplication, and you can act on it from either end.
    if ctx.changed.is_some() {
        groups.retain(|g| {
            let mut files: Vec<String> = g.members.iter().map(|m| m.file.clone()).collect();
            ctx.retain_changed(&mut files, |f| f.as_str());
            !files.is_empty()
        });
    }

    // Keyed on the group label so a waiver above one copy names what it is
    // retiring. Anchored at the *first* member: a group has several sites and
    // the waiver has to attach to a specific one, so it attaches to the one the
    // row leads with.
    let waived = ctx.retain_unsuppressed("clones", &mut groups, |g| {
        crate::suppress::Site::keyed(g.members[0].file.as_str(), g.members[0].line, "")
    });

    groups.sort_by(|a, b| {
        b.score()
            .partial_cmp(&a.score())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.members.len().cmp(&a.members.len()))
            .then_with(|| a.members[0].file.cmp(&b.members[0].file))
            .then_with(|| a.members[0].line.cmp(&b.members[0].line))
    });

    let shown = top.map_or(groups.len(), |n| n.min(groups.len()));
    if !ctx.summary {
        if shown < groups.len() {
            ctx.out.note(&format!(
                "(note: showing {} of {} group(s) — raise or drop --top for the rest)",
                shown,
                groups.len()
            ));
        }
        let today = crate::suppress::Date::today();
        for g in &groups[..shown] {
            let label = g.label();
            let first = &g.members[0];
            // `at` stays a real site so `--json` keeps file and line as fields
            // and `--context` can quote the source. The remaining copies ride in
            // one text column: a group has a variable number of members and the
            // row shape has to be fixed.
            row!(
                ctx.out,
                "what" => label.clone(),
                "score" => format!("{:.2}", g.score()),
                "copies" => g.members.len().to_string(),
                "tokens" => g.tokens.to_string(),
                "at" => site(&first.file, first.line),
                "fn" => first.qpath.clone(),
                "others" => g.members[1..]
                    .iter()
                    .map(|m| format!("{} {}:{}", m.qpath, m.file, m.line))
                    .collect::<Vec<_>>()
                    .join("  "),
            );
            ctx.suggest("clones", Some(&label), today);
        }
    }

    let copies: usize = groups.iter().map(|g| g.members.len()).sum();
    let gating = groups.iter().filter(|g| g.score() >= GATING_SCORE).count();
    ctx.out.summary(&format!(
        "({} duplicated bod(ies) across {} group(s){}; {} fn(s) scanned; \
         min_tokens={}{}; explain: replication)",
        copies,
        groups.len(),
        if gating > 0 {
            format!(
                ", {} at score >= {:.2} (the tier `audit` gates on)",
                gating, GATING_SCORE
            )
        } else {
            String::new()
        },
        scanned,
        min_tokens,
        ctx.waived_note(waived)
    ));
    Ok(Counts {
        total: groups.len(),
        gating,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon_of(src: &str) -> String {
        let f: syn::ItemFn = syn::parse_str(src).expect("parse");
        canonicalize(&f.sig, &f.block).0
    }

    /// The copy-paste this check exists for: same body, locals renamed.
    #[test]
    fn alpha_renaming_collapses_renamed_locals() {
        let a = canon_of(
            r#"fn parse_uuid(bytes: &[u8], field: &'static str) -> Result<Uuid, Status> {
                   Uuid::from_slice(bytes).map_err(|_| Status::invalid_argument(field))
               }"#,
        );
        let b = canon_of(
            r#"fn parse_uuid(raw: &[u8], name: &'static str) -> Result<Uuid, Status> {
                   Uuid::from_slice(raw).map_err(|_| Status::invalid_argument(name))
               }"#,
        );
        assert_eq!(a, b);
    }

    /// Formatting is not identity. Two copies rustfmt broke differently are
    /// still two copies.
    #[test]
    fn whitespace_and_line_breaks_do_not_matter() {
        let a = canon_of("fn f(x: u32) -> u32 { let y = x + 1; y * 2 }");
        let b = canon_of(
            "fn f(x: u32) -> u32 {\n    let y = x + 1;\n\n    y * 2\n}",
        );
        assert_eq!(a, b);
    }

    /// Called names are API, not local naming. Renaming them too would turn
    /// this into a shape-similarity metric and group everything.
    #[test]
    fn different_callees_are_not_clones() {
        let a = canon_of("fn f(x: T) -> u32 { x.width() + x.height() }");
        let b = canon_of("fn g(y: T) -> u32 { y.rows() + y.cols() }");
        assert_ne!(a, b);
    }

    /// Literals carry meaning. Two functions that differ only in the table they
    /// write to are not the same function.
    #[test]
    fn different_literals_are_not_clones() {
        let a = canon_of(r#"fn f(d: &D) { d.exec("DELETE FROM users"); }"#);
        let b = canon_of(r#"fn g(d: &D) { d.exec("DELETE FROM orders"); }"#);
        assert_ne!(a, b);
    }

    /// Structure has to survive flattening: same tokens, different nesting.
    #[test]
    fn delimiters_are_part_of_the_key() {
        let a = canon_of("fn f(x: T) { g(h(x)); }");
        let b = canon_of("fn f(x: T) { g(h, x); }");
        assert_ne!(a, b);
    }

    fn body(name: &str, file: &str, tokens: usize) -> Body {
        Body {
            canon: "c".into(),
            qpath: format!("m::{name}"),
            name: name.into(),
            file: file.into(),
            line: 1,
            tokens,
        }
    }

    fn group(members: Vec<Body>) -> Group {
        let tokens = members[0].tokens;
        let same_name = members.iter().all(|m| m.name == members[0].name);
        let d = dir_of(&members[0].file);
        let same_dir = members.iter().all(|m| dir_of(&m.file) == d);
        Group {
            members,
            tokens,
            same_name,
            same_dir,
        }
    }

    /// The `parse_uuid` shape — one named helper, five copies, one directory —
    /// must gate. That is the finding the check was built to produce.
    #[test]
    fn the_same_helper_copied_across_one_directory_gates() {
        let g = group(
            (0..5)
                .map(|i| body("parse_uuid", &format!("src/services/s{i}.rs"), 30))
                .collect(),
        );
        assert!(
            g.score() >= GATING_SCORE,
            "score {:.2} should gate",
            g.score()
        );
    }

    /// Two small, differently-named bodies in unrelated directories are a lead,
    /// not a gate.
    #[test]
    fn an_incidental_pair_does_not_gate() {
        let g = group(vec![
            body("encode", "src/a/x.rs", 24),
            body("write_tag", "src/b/y.rs", 24),
        ]);
        assert!(
            g.score() < GATING_SCORE,
            "score {:.2} should not gate",
            g.score()
        );
    }

    /// One helper duplicated in exactly two modules is a real lead that can
    /// drift, and it stays advisory: a pair is also how a deliberate boundary
    /// between two crates looks, and gating on pairs puts the whole list in
    /// the gate.
    #[test]
    fn a_named_pair_reports_but_does_not_gate() {
        let g = group(vec![
            body("pat_is_ok", "src/divergence.rs", 81),
            body("pat_is_ok", "src/error_swallows.rs", 81),
        ]);
        assert!(g.score() > 0.5, "a named pair is still worth reading");
        assert!(
            g.score() < GATING_SCORE,
            "score {:.2} should not gate",
            g.score()
        );
    }

    /// More copies of more code always ranks higher, so the top of the list is
    /// the biggest lever.
    #[test]
    fn score_rises_with_bulk_and_copies() {
        let small = group(vec![body("f", "src/a.rs", 24), body("f", "src/b.rs", 24)]);
        let bigger = group(vec![body("f", "src/a.rs", 80), body("f", "src/b.rs", 80)]);
        let more = group(
            (0..4)
                .map(|i| body("f", &format!("src/{i}.rs"), 80))
                .collect(),
        );
        assert!(bigger.score() > small.score());
        assert!(more.score() > bigger.score());
    }

    #[test]
    fn label_names_the_helper_when_every_copy_agrees() {
        let g = group(vec![
            body("parse_uuid", "src/a.rs", 30),
            body("parse_uuid", "src/b.rs", 30),
        ]);
        assert_eq!(g.label(), "parse_uuid");
        let g = group(vec![
            body("to_pb", "src/a.rs", 30),
            body("into_proto", "src/b.rs", 30),
        ]);
        assert_eq!(g.label(), "into_proto/to_pb");
    }
}
