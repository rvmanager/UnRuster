//! `doc-drift` — the documentation and the code disagreeing.
//!
//! # The surface nothing was checking
//!
//! This tool has always indexed doc comments ([`crate::index`] stores a summary
//! for every item) and never *checked* one. That is the cheapest untouched
//! surface it has, and it matters more than it used to: prose is the part of a
//! codebase an agent trusts most and can verify least. A `# Panics` section
//! that survived the refactor which removed the panic is a sentence that will
//! be believed for years.
//!
//! # Four classes, and why each is checkable
//!
//! | kind | the disagreement | precision |
//! |---|---|---|
//! | `panics-doc-unbacked` | a `# Panics` section over a body that cannot panic | APPROXIMATE |
//! | `panics-undocumented` | a documented `pub` fn that panics *explicitly*, with no `# Panics` | APPROXIMATE |
//! | `errors-doc-unbacked` | an `# Errors` section over a fn returning no `Result` | EXACT |
//! | `stale-name` | the docs name an identifier that no longer exists | APPROXIMATE |
//!
//! # `stale-name`, and what measuring it changed
//!
//! `stale-name` is what a rename leaves behind: the signature moves, the
//! sentence above it does not, and the next reader is told to pass a parameter
//! the function has never had.
//!
//! The first cut fired on any backticked bare identifier that was not in the
//! signature and not an item in the tree. Run over *this* codebase it produced
//! **205 rows, essentially all of them wrong** — `` `trait` ``, `` `let` ``,
//! `` `awk` ``, `` `saturating_add` ``, `` `Mask` ``: keywords, tools, and
//! other projects' APIs, because a doc comment is prose and prose backticks
//! everything. The premise that a backticked identifier is probably a parameter
//! is simply false.
//!
//! Two changes followed, and both are the measurement rather than a guess:
//!
//! * The identifier must sit in a doc that **already backticks at least one
//!   real parameter of this function**. That is the evidence that this author
//!   backticks parameters here, which is the only thing that makes a
//!   *non*-parameter in the same doc suspicious. Plus: lower-case only (a
//!   `Type`-shaped name in prose is somebody else's API), three characters or
//!   more, never a Rust keyword, and never a **local binding of the body** — a
//!   doc that explains its own implementation is documentation working.
//! * The class is **off by default** (`--names` opts in) and `audit` does not
//!   run it. A class that cannot survive its own codebase does not get to hold
//!   an agent loop open.
//!
//! # Why the panic predicate here is not `panics`
//!
//! [`crate::panics`] ranks its sites and hides the idiomatic ones — a
//! poisoned-lock `unwrap`, an assertion over a source literal — because those
//! are not the crashes it hunts. That filtering is exactly wrong for a doc
//! check: a `# Panics` section should document a lock unwrap too. So [`Panic`]
//! below is its own predicate, tiered rather than ranked, and the tier each
//! class requires is the asymmetry documented there.

use syn::visit::{self, Visit};

use crate::ast::{doc_lines, fn_visits, line_of, scope_visits, ScopeTracker};
use crate::context::{AnalysisCtx, Counts};
use crate::emit::{row, site};
use crate::parse::{display_path, ParsedFile};

/// The score at or above which a row is a gating `audit` finding.
///
/// Set so the gate is the two classes where the code is the evidence: a section
/// heading with nothing behind it. The undocumented-panic and stale-name
/// classes are advisory — the first is a documentation gap rather than a
/// contradiction, and the second is a heuristic over prose.
pub const GATING_SCORE: f64 = 0.70;

/// Identifiers a doc comment may name that are not items: Rust's own
/// vocabulary. Kept short on purpose — anything longer starts hiding real
/// stale names behind a plausible-looking allow-list.
const RUST_WORDS: &[&str] = &[
    // Keywords. Prose about Rust names these constantly, and every one of them
    // was a row in the first run.
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "union",
    "unsafe", "use", "where", "while",
    // Primitives and the std vocabulary a signature may not mention.
    "str", "bool", "char", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64",
    "i128", "isize", "f32", "f64",
];

#[derive(Debug)]
struct Hit {
    /// `panics-doc-unbacked` | `panics-undocumented` | `errors-doc-unbacked` |
    /// `stale-name`
    kind: &'static str,
    /// The fn the row is about.
    item: String,
    /// The offending name, for `stale-name`; empty otherwise. Also the waiver key.
    key: String,
    detail: String,
    file: String,
    line: usize,
    score: f64,
}

/// How a body can abort, if it can.
///
/// The distinction exists because the two panic classes need different
/// evidence, and running both off one predicate produced fifteen wrong rows on
/// this codebase. `glob_match` slices with a length it just computed;
/// `Date::parse` slices a string it has already checked is ten ASCII bytes;
/// `NameIndex::lookup` indexes by an index it stored itself. Demanding a
/// `# Panics` section from each of those is a style opinion about indexing, not
/// a disagreement between the docs and the code.
///
/// The same reasoning covers `Mutex::lock().unwrap()`, which is the universal
/// Rust idiom and which [`crate::panics`] already declines to report;
/// [`crate::panics::receiver_is_lock`] is shared rather than re-derived here.
///
/// So:
/// * `panics-undocumented` requires [`Panic::Explicit`] — the author wrote
///   `unwrap`, `expect`, `panic!` or an assertion on something that is not a
///   lock, and did not mention it.
/// * `panics-doc-unbacked` accepts either tier, because a `# Panics` section is
///   very often *about* an index or a lock, and calling such a section unbacked
///   would be the same mistake pointed the other way.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Panic {
    /// Nothing in the body can abort.
    None,
    /// A real panic path that convention leaves undocumented: an index or slice
    /// (usually provably in bounds at the site), a poisoned-lock unwrap.
    Incidental,
    /// `unwrap` / `expect` / `panic!` / `unreachable!` / `todo!` / `assert*!`
    /// on anything else.
    Explicit,
}

fn panics(block: &syn::Block) -> Panic {
    struct V(Panic);
    impl<'ast> Visit<'ast> for V {
        fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
            let m = e.method.to_string();
            if m == "unwrap" || m == "expect" || m == "unwrap_err" || m == "expect_err" {
                self.0 = self.0.max(if crate::panics::receiver_is_lock(&e.receiver) {
                    Panic::Incidental
                } else {
                    Panic::Explicit
                });
            }
            visit::visit_expr_method_call(self, e);
        }
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            if let Some(last) = m.path.segments.last() {
                if matches!(
                    last.ident.to_string().as_str(),
                    "panic" | "unreachable" | "todo" | "unimplemented" | "assert" | "assert_eq"
                        | "assert_ne"
                ) {
                    self.0 = Panic::Explicit;
                }
            }
            visit::visit_macro(self, m);
        }
        // Indexing can panic, and a `# Panics` section is often written for
        // exactly that — so it is recorded, at the weaker tier. It never
        // upgrades an `Explicit` finding back down.
        fn visit_expr_index(&mut self, e: &'ast syn::ExprIndex) {
            self.0 = self.0.max(Panic::Incidental);
            visit::visit_expr_index(self, e);
        }
    }
    let mut v = V(Panic::None);
    v.visit_block(block);
    v.0
}

/// Is `ty` a `Result`/`Option` — something an `# Errors` section can describe?
fn is_fallible(sig: &syn::Signature) -> bool {
    let syn::ReturnType::Type(_, t) = &sig.output else {
        return false;
    };
    let text = crate::ast::type_to_string(t);
    text.starts_with("Result") || text.starts_with("Option") || text.contains("Result<")
}

/// Does the doc have a `# <heading>` section?
fn has_section(docs: &[String], heading: &str) -> bool {
    docs.iter().any(|l| {
        let t = l.trim();
        t.strip_prefix('#')
            .map(|r| r.trim_start_matches('#').trim().eq_ignore_ascii_case(heading))
            .unwrap_or(false)
    })
}

/// Backticked single identifiers in a doc comment.
///
/// Only `` `name` `` — never `` `Type::method` ``, `` `foo()` `` or a whole
/// code span. A qualified path or a call is prose about an API, where a bare
/// identifier in backticks is almost always this function's own parameter, and
/// that is the one this check can be right about.
fn backticked_idents(docs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for line in docs {
        // A fenced or indented code block is example code, not a claim about
        // this signature; every identifier in one would be a false positive.
        let t = line.trim_start();
        if t.starts_with("```") || line.starts_with("    ") {
            continue;
        }
        let mut rest = line.as_str();
        while let Some(open) = rest.find('`') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('`') else { break };
            let inner = &after[..close];
            rest = &after[close + 1..];
            if inner.is_empty() || inner.len() > 40 {
                continue;
            }
            let ok = inner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
                && inner.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_');
            if ok && !out.iter().any(|x: &String| x == inner) {
                out.push(inner.to_string());
            }
        }
    }
    out
}

/// Every name the body binds: `let`s, closure arguments, match-arm bindings,
/// loop patterns.
///
/// A doc that explains its own implementation — "without the `seen_names` set a
/// cyclic call graph re-enqueued forever" — is naming a local, and that is
/// documentation working, not drifting. Five of the nine rows that survived the
/// first tightening on this codebase were exactly this.
fn local_bindings(block: &syn::Block) -> std::collections::BTreeSet<String> {
    struct V(std::collections::BTreeSet<String>);
    impl<'ast> Visit<'ast> for V {
        fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
            self.0.insert(p.ident.to_string());
            visit::visit_pat_ident(self, p);
        }
    }
    let mut v = V(Default::default());
    v.visit_block(block);
    v.0
}

/// The parameter names alone — the set `stale-name` compares a doc against.
fn param_names(sig: &syn::Signature) -> Vec<String> {
    let mut out = Vec::new();
    for a in &sig.inputs {
        if let syn::FnArg::Typed(t) = a {
            let mut binds = std::collections::BTreeSet::new();
            crate::callers::pat_idents(&t.pat, &mut binds);
            out.extend(binds);
        }
    }
    out
}

/// Every identifier the signature itself mentions: parameter names, and every
/// segment of every type and generic in it.
fn signature_names(sig: &syn::Signature) -> Vec<String> {
    let mut out = vec![sig.ident.to_string()];
    for g in &sig.generics.params {
        if let syn::GenericParam::Type(t) = g {
            out.push(t.ident.to_string());
        }
    }
    for a in &sig.inputs {
        match a {
            syn::FnArg::Receiver(_) => out.push("self".to_string()),
            syn::FnArg::Typed(t) => {
                let mut binds = std::collections::BTreeSet::new();
                crate::callers::pat_idents(&t.pat, &mut binds);
                out.extend(binds);
                out.extend(type_words(&crate::ast::type_to_string(&t.ty)));
            }
        }
    }
    if let syn::ReturnType::Type(_, t) = &sig.output {
        out.extend(type_words(&crate::ast::type_to_string(t)));
    }
    out
}

/// Identifier-ish fragments of a rendered type.
fn type_words(t: &str) -> Vec<String> {
    t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

struct DocVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    idx: &'a crate::index::NameIndex,
    check_names: bool,
    hits: Vec<Hit>,
}

impl DocVisitor<'_> {
    fn check(&mut self, attrs: &[syn::Attribute], sig: &syn::Signature, body: Option<&syn::Block>, is_pub: bool) {
        let docs = doc_lines(attrs);
        if docs.is_empty() {
            // An undocumented fn is undocumented, not drifted. There is no
            // claim here to contradict.
            return;
        }
        let name = sig.ident.to_string();
        let item = self.scope.qualify(&name);
        let line = line_of(&sig.ident);
        let can_panic = body.map(panics).unwrap_or(Panic::None);

        if has_section(&docs, "Panics") && body.is_some() && can_panic == Panic::None {
            self.hits.push(Hit {
                kind: "panics-doc-unbacked",
                item: item.clone(),
                key: "Panics".to_string(),
                detail: "documents a `# Panics` section over a body with no panic, \
                         index, or assertion site"
                    .to_string(),
                file: self.file.to_string(),
                line,
                score: 0.80,
            });
        }
        if is_pub && can_panic == Panic::Explicit && !has_section(&docs, "Panics") {
            self.hits.push(Hit {
                kind: "panics-undocumented",
                item: item.clone(),
                key: "Panics".to_string(),
                detail: "a documented `pub` fn that can panic, with no `# Panics` section"
                    .to_string(),
                file: self.file.to_string(),
                line,
                score: 0.45,
            });
        }
        if has_section(&docs, "Errors") && !is_fallible(sig) {
            self.hits.push(Hit {
                kind: "errors-doc-unbacked",
                item: item.clone(),
                key: "Errors".to_string(),
                detail: format!(
                    "documents an `# Errors` section, but returns {}",
                    match &sig.output {
                        syn::ReturnType::Default => "nothing".to_string(),
                        syn::ReturnType::Type(_, t) => crate::ast::type_to_string(t),
                    }
                ),
                file: self.file.to_string(),
                line,
                score: 0.85,
            });
        }
        if self.check_names {
            let known = signature_names(sig);
            let params = param_names(sig);
            let ticked = backticked_idents(&docs);
            // The discriminator the first cut lacked: unless this doc already
            // backticks a real parameter, there is no evidence the author
            // backticks parameters at all, and every identifier in it is prose.
            if !ticked.iter().any(|t| params.contains(t)) {
                return;
            }
            let own: Vec<String> = crate::concepts::words_of(&name);
            let locals = body.map(local_bindings).unwrap_or_default();
            for id in ticked {
                // Upper case anywhere means a type or a constant, which is
                // somebody's API and not this function's parameter.
                if id.chars().count() < 3
                    || id.chars().any(|c| c.is_uppercase())
                    || known.contains(&id)
                    || RUST_WORDS.contains(&id.as_str())
                    || own.contains(&id.to_lowercase())
                    || locals.contains(&id)
                    || self.idx.knows_name(&id)
                {
                    continue;
                }
                self.hits.push(Hit {
                    kind: "stale-name",
                    item: item.clone(),
                    key: id.clone(),
                    detail: format!(
                        "docs name `{}`, which is not in the signature and not an item \
                         in the scanned tree",
                        id
                    ),
                    file: self.file.to_string(),
                    line,
                    score: 0.50,
                });
            }
        }
    }
}

impl<'ast> Visit<'ast> for DocVisitor<'_> {
    scope_visits!(item_mod, item_impl, item_trait);

    fn_visits!(before check; item_fn, impl_item_fn);

    /// Written out rather than generated, because the argument it passes is a
    /// decision this check makes and the macro must not make for it: a trait
    /// method's doc describes the *contract*, and its default body is one
    /// implementation of that contract. So `# Panics` over a defaulted method
    /// that does not itself panic is a statement about implementors, not a
    /// contradiction — and the body is deliberately withheld (`None`).
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.check(&i.attrs, &i.sig, None, true);
        visit::visit_trait_item_fn(self, i);
    }
}

pub struct Opts {
    /// Run the `stale-name` class. **Off** by default — see the module header
    /// for the measurement that decided it.
    pub names: bool,
    pub min_score: f64,
}

pub fn run(ctx: &AnalysisCtx, opts: &Opts) -> anyhow::Result<usize> {
    Ok(run_counted(ctx, opts)?.total)
}

pub fn run_counted(ctx: &AnalysisCtx, opts: &Opts) -> anyhow::Result<Counts> {
    let mut hits: Vec<Hit> = Vec::new();
    for f in ctx.files {
        let mut v = DocVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            idx: ctx.idx,
            check_names: opts.names,
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        hits.extend(v.hits);
    }
    let scanned = documented_fns(ctx.files);

    ctx.retain_changed(&mut hits, |h| h.file.as_str());
    let waived = ctx.retain_unsuppressed("doc-drift", &mut hits, |h| {
        crate::suppress::Site::keyed(h.file.as_str(), h.line, &h.key)
    });
    let below = {
        let n = hits.len();
        hits.retain(|h| h.score >= opts.min_score);
        n - hits.len()
    };

    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.key.cmp(&b.key))
    });

    if !ctx.summary {
        let today = crate::suppress::Date::today();
        for h in &hits {
            row!(
                ctx.out,
                "kind" => h.kind,
                "score" => format!("{:.2}", h.score),
                "at" => site(&h.file, h.line),
                "fn" => h.item.clone(),
                "detail" => h.detail.clone(),
            );
            ctx.suggest("doc-drift", Some(&h.key), today);
        }
    }

    let gating = hits.iter().filter(|h| h.score >= GATING_SCORE).count();
    let mut by_kind: std::collections::BTreeMap<&str, usize> = Default::default();
    for h in &hits {
        *by_kind.entry(h.kind).or_insert(0) += 1;
    }
    ctx.out.summary(&format!(
        "({} doc/code disagreement(s){}{}; {}; {} documented fn(s) scanned{}; \
         explain: doc-drift)",
        hits.len(),
        if gating > 0 {
            format!(
                ", {} at score >= {:.2} (the tier `audit` gates on)",
                gating, GATING_SCORE
            )
        } else {
            String::new()
        },
        if below > 0 {
            format!("; {} below --min-score {:.2}", below, opts.min_score)
        } else {
            String::new()
        },
        if by_kind.is_empty() {
            "no classes fired".to_string()
        } else {
            by_kind
                .iter()
                .map(|(k, n)| format!("{}={}", k, n))
                .collect::<Vec<_>>()
                .join(", ")
        },
        scanned,
        ctx.waived_note(waived)
    ));
    Ok(Counts {
        total: hits.len(),
        gating,
    })
}

/// How many fns carry a doc comment — the denominator this check works over.
/// Reported so a zero result reads as "nothing disagreed" rather than
/// "nothing was looked at".
fn documented_fns(files: &[ParsedFile]) -> usize {
    struct V(usize);
    impl V {
        fn count(
            &mut self,
            attrs: &[syn::Attribute],
            _sig: &syn::Signature,
            _body: Option<&syn::Block>,
            _is_pub: bool,
        ) {
            self.0 += usize::from(!doc_lines(attrs).is_empty());
        }
    }
    impl<'ast> Visit<'ast> for V {
        fn_visits!(before count; item_fn, impl_item_fn, trait_item_fn);
    }
    let mut v = V(0);
    for f in files {
        v.visit_file(&f.ast);
    }
    v.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits_of(src: &str, names: bool) -> Vec<Hit> {
        let ast = syn::parse_file(src).expect("parse");
        let pf = ParsedFile {
            path: std::path::PathBuf::from("src/t.rs"),
            ast,
            module: "t".into(),
        };
        let files = vec![pf];
        let idx = crate::index::NameIndex::build(&files);
        let mut v = DocVisitor {
            file: "src/t.rs",
            scope: ScopeTracker::new("t"),
            idx: &idx,
            check_names: names,
            hits: Vec::new(),
        };
        v.visit_file(&files[0].ast);
        v.hits
    }

    fn kinds(src: &str, names: bool) -> Vec<&'static str> {
        hits_of(src, names).into_iter().map(|h| h.kind).collect()
    }

    /// The sentence that survives the refactor that removed the panic.
    #[test]
    fn a_panics_section_over_a_body_that_cannot_panic_is_reported() {
        assert_eq!(
            kinds(
                "/// Does a thing.\n///\n/// # Panics\n///\n/// Never, actually.\n\
                 pub fn f(x: u32) -> u32 { x + 1 }",
                false
            ),
            ["panics-doc-unbacked"]
        );
    }

    #[test]
    fn a_panics_section_with_a_real_panic_is_clean() {
        assert!(kinds(
            "/// Does a thing.\n///\n/// # Panics\n///\n/// On empty input.\n\
             pub fn f(v: &[u32]) -> u32 { v[0] }",
            false
        )
        .is_empty());
    }

    #[test]
    fn a_documented_pub_fn_that_panics_without_saying_so_is_reported() {
        assert_eq!(
            kinds("/// Reads it.\npub fn f(v: &[u32]) -> u32 { *v.first().unwrap() }", false),
            ["panics-undocumented"]
        );
    }

    /// The asymmetry, measured: fifteen rows on this codebase were fns that
    /// slice with a length they just computed. Demanding a `# Panics` section
    /// from those is an opinion about indexing, not a doc/code disagreement.
    #[test]
    fn indexing_alone_does_not_demand_a_panics_section() {
        assert!(kinds("/// Reads it.\npub fn f(v: &[u32]) -> u32 { v[0] }", false).is_empty());
    }

    /// And the two rows that survived it: the poisoned-lock idiom, which
    /// `panics` also declines to report.
    #[test]
    fn a_poisoned_lock_unwrap_does_not_demand_a_panics_section() {
        assert!(kinds(
            "/// Reads it.\npub fn f() -> usize { *STATE.lock().unwrap() }",
            false
        )
        .is_empty());
        // …but an unwrap on anything else still does.
        assert_eq!(
            kinds("/// Reads it.\npub fn f(s: &str) -> u8 { s.parse().unwrap() }", false),
            ["panics-undocumented"]
        );
    }

    /// …but it still backs a `# Panics` section somebody wrote, or the same
    /// mistake fires in the other direction.
    #[test]
    fn indexing_does_back_a_panics_section() {
        assert!(kinds(
            "/// Reads it.\n///\n/// # Panics\n///\n/// On empty input.\n\
             pub fn f(v: &[u32]) -> u32 { v[0] }",
            false
        )
        .is_empty());
    }

    /// An undocumented fn is undocumented, not drifted. There is no claim to
    /// contradict, and flagging it would turn this into a style lint.
    #[test]
    fn an_undocumented_fn_is_not_a_doc_drift_finding() {
        assert!(kinds("pub fn f(v: &[u32]) -> u32 { v[0] }", false).is_empty());
    }

    #[test]
    fn an_errors_section_on_an_infallible_fn_is_reported() {
        assert_eq!(
            kinds("/// Does it.\n///\n/// # Errors\n///\n/// Never.\npub fn f() -> u32 { 1 }", false),
            ["errors-doc-unbacked"]
        );
        assert!(kinds(
            "/// Does it.\n///\n/// # Errors\n///\n/// On IO failure.\n\
             pub fn f() -> Result<u32, E> { Ok(1) }",
            false
        )
        .is_empty());
    }

    /// What a rename leaves behind.
    #[test]
    fn a_doc_naming_a_parameter_that_no_longer_exists_is_reported() {
        let h = hits_of(
            "/// Splits on `sep` and keeps `limit` pieces.\n\
             pub fn split(text: &str, sep: char) -> Vec<&str> { Vec::new() }",
            true,
        );
        assert_eq!(h.len(), 1, "{:?}", h.iter().map(|x| &x.key).collect::<Vec<_>>());
        assert_eq!(h[0].key, "limit");
    }

    #[test]
    fn a_doc_naming_a_real_parameter_or_a_real_item_is_clean() {
        assert!(kinds(
            "pub struct Splitter;\n\
             /// Splits `text` on `sep` using a `Splitter`.\n\
             pub fn split(text: &str, sep: char) -> Vec<&str> { Vec::new() }",
            true
        )
        .is_empty());
    }

    /// Example code inside a doc test names whatever it likes.
    #[test]
    fn identifiers_inside_a_code_block_are_not_stale_names() {
        assert!(kinds(
            "/// Splits.\n///\n/// ```\n/// let whatever_local = 1;\n/// ```\n\
             pub fn split(text: &str) -> Vec<&str> { Vec::new() }",
            true
        )
        .is_empty());
    }

    /// A doc that explains its own implementation is documentation working.
    #[test]
    fn a_doc_naming_a_local_of_its_own_body_is_clean() {
        assert!(kinds(
            "/// Splits `text`, and without the `seen` set it would loop.\n\
             pub fn split(text: &str) -> usize { let seen = 1; seen }",
            true
        )
        .is_empty());
    }

    /// The evidence that makes a non-parameter suspicious at all: this author
    /// backticks parameters here. Without it, every doc comment in a
    /// prose-heavy codebase is a finding — 205 of them on this one.
    #[test]
    fn a_doc_that_backticks_no_parameter_is_not_read_for_stale_names() {
        assert!(kinds(
            "/// Uses `saturating_add` because of how `awk` handles it.\n\
             pub fn inc(x: u32) -> u32 { x + 1 }",
            true
        )
        .is_empty());
    }

    #[test]
    fn a_qualified_path_in_backticks_is_prose_not_a_parameter() {
        assert!(kinds(
            "/// See `Foo::bar` and `baz()` for details.\npub fn f(x: u32) -> u32 { x }",
            true
        )
        .is_empty());
    }

    #[test]
    fn a_trait_methods_default_body_does_not_back_its_panics_section() {
        // The doc describes the contract implementors must meet; the default
        // body is one implementation of it, so there is nothing to contradict.
        assert!(kinds(
            "pub trait T {\n/// Does it.\n///\n/// # Panics\n///\n/// On bad input.\n\
             fn f(&self, x: u32) -> u32 { x }\n}",
            false
        )
        .is_empty());
    }
}
