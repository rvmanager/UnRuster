use std::collections::BTreeSet;

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::{self, Visit};

use crate::ast::{fn_visits, path_to_string, scope_visits, ScopeTracker};
use crate::context::AnalysisCtx;
use crate::parse::ParsedFile;
use crate::emit::row;

/// Build a set of every "called" last-segment name we observe across the tree,
/// attributed to the item whose body named it.
///
/// The attribution is what `--transitive` runs on. A call set that is one flat
/// set of names cannot answer "and what would be dead once these three go",
/// because it cannot subtract the names those three contributed. Keeping the
/// per-item breakdown costs one map and makes the fixed point a set union.
struct CallSink {
    /// Names used outside any fn body: module-level consts and statics, struct
    /// field attributes, `macro_rules!` definitions. These never go away as a
    /// consequence of deleting a fn.
    outside: BTreeSet<String>,
    /// Per fn, the names its own body used.
    per_item: std::collections::BTreeMap<String, BTreeSet<String>>,
    /// Qualified path of the fn currently being walked, innermost last.
    stack: Vec<String>,
    scope: ScopeTracker,
}

impl CallSink {
    fn new() -> Self {
        CallSink {
            outside: BTreeSet::new(),
            per_item: std::collections::BTreeMap::new(),
            stack: Vec::new(),
            scope: ScopeTracker::new(""),
        }
    }

    fn saw(&mut self, name: String) {
        match self.stack.last() {
            Some(owner) => {
                self.per_item.entry(owner.clone()).or_default().insert(name);
            }
            None => {
                self.outside.insert(name);
            }
        }
    }

    /// Every name in play once the bodies of `gone` are removed from the tree.
    fn called_without(&self, gone: &BTreeSet<String>) -> BTreeSet<String> {
        let mut out = self.outside.clone();
        for (owner, names) in &self.per_item {
            if gone.contains(owner) {
                continue;
            }
            out.extend(names.iter().cloned());
        }
        out
    }

    fn called(&self) -> BTreeSet<String> {
        self.called_without(&BTreeSet::new())
    }

    /// Open a fn: everything walked from here belongs to it until [`Self::leave_fn`].
    /// Shared by every fn-shaped visit method — see [`fn_visits`].
    fn enter_fn(&mut self, sig: &syn::Signature, _block: Option<&syn::Block>) {
        let q = self.scope.qualify(&sig.ident.to_string());
        self.stack.push(q);
    }

    /// Close it.
    fn leave_fn(&mut self, _sig: &syn::Signature, _block: Option<&syn::Block>) {
        self.stack.pop();
    }
}

impl<'ast> Visit<'ast> for CallSink {
    scope_visits!(item_mod, item_impl, item_trait);
    // The three fn shapes differ only in the `syn` type they carry, which is
    // what this macro is for. Written out, they were three bodies one edit
    // apart and `near-clones` said so on the first run over this file.
    fn_visits!(around enter_fn, leave_fn; item_fn, impl_item_fn, trait_item_fn);

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            let s = path_to_string(&p.path);
            let last = crate::ast::last_segment(&s).to_string();
            self.saw(last);
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        self.saw(e.method.to_string());
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_path(&mut self, e: &'ast syn::ExprPath) {
        // Track fn-references-as-values (`let f = some_fn; f();`) too.
        let s = path_to_string(&e.path);
        let last = crate::ast::last_segment(&s).to_string();
        self.saw(last);
        visit::visit_expr_path(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if let Some(last) = m.path.segments.last() {
            self.saw(last.ident.to_string());
        }
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
        // Then every identifier in the raw tokens, whether or not the body
        // parsed as expressions.
        //
        // `macro_exprs` parses chunk-by-chunk and keeps what it can, so a body
        // like `row!(out, "age" => age_str(w))` yields `out` and silently drops
        // the arm containing the call — the fn then looks dead while being
        // called on that very line. It is not even reported as a blind spot,
        // because the parse partially succeeded.
        //
        // A call-set only needs "might this name be called", and the check
        // already declares itself heuristic. Over-collecting costs a missed
        // dead fn; under-collecting sends someone to delete live code. This is
        // the same treatment `visit_item_macro` already gives `macro_rules!`
        // bodies, for the same reason.
        let mut names = BTreeSet::new();
        collect_idents(&m.tokens, &mut names);
        for n in names {
            self.saw(n);
        }
    }

    /// Functions named inside an attribute — as a string, or as an expression.
    ///
    /// `#[serde(default = "default_true")]` is a real call — the derive expands
    /// it into one — but the name lives in a string literal, so no amount of
    /// expression walking finds it. Same for `with` / `serialize_with` /
    /// `deserialize_with`, and for the equivalents in other derive ecosystems.
    ///
    /// Rather than allow-list attribute names (the next crate spells it
    /// differently), take every string literal in every attribute and, if it
    /// parses as a path, count its last segment as called. A string that is a
    /// bare identifier is almost never prose, and the cost of being wrong is a
    /// missed dead fn rather than a live one reported dead.
    ///
    /// The other half is bare expressions. `clap` spells the same idea without
    /// quotes:
    ///
    /// ```ignore
    /// #[arg(long, default_value_t = default_points())]
    /// pub points: usize,
    /// ```
    ///
    /// The derive expands that into a call, but it lives in an attribute's
    /// token stream, which the AST walk does not enter — so on a real
    /// `clap`-derive CLI all four default helpers were reported dead, in a
    /// *gating* check, holding the agent loop open on code where deleting any
    /// of them is a compile error. Every identifier in the token stream is
    /// counted, the same treatment macro bodies already get above: the bias is
    /// deliberately toward missing a dead fn over reporting a live one.
    fn visit_attribute(&mut self, a: &'ast syn::Attribute) {
        match &a.meta {
            // `#[serde(default = "f", with = "m")]` — the interesting case.
            syn::Meta::List(ml) => {
                let mut names = BTreeSet::new();
                collect_path_strings(&ml.tokens, &mut names);
                // `#[arg(default_value_t = f())]`, `#[arg(value_parser = f)]`.
                // Doc comments are `Meta::NameValue`, so no prose reaches here.
                collect_idents(&ml.tokens, &mut names);
                for n in names {
                    self.saw(n);
                }
            }
            // `#[doc = "…"]` lands here too; prose fails the path test.
            syn::Meta::NameValue(nv) => {
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) = &nv.value
                {
                    let mut names = BTreeSet::new();
                    insert_if_path(&s.value(), &mut names);
                    for n in names {
                        self.saw(n);
                    }
                }
            }
            syn::Meta::Path(_) => {}
        }
        visit::visit_attribute(self, a);
    }

    fn visit_item_macro(&mut self, im: &'ast syn::ItemMacro) {
        // `macro_rules! foo { ... }` definitions: walk the body tokens and
        // treat every identifier as "potentially called." Otherwise a fn
        // referenced only from a custom macro expansion would look dead.
        let is_macro_rules = im
            .mac
            .path
            .segments
            .last()
            .map(|s| s.ident == "macro_rules")
            .unwrap_or(false);
        if is_macro_rules {
            let mut names = BTreeSet::new();
            collect_idents(&im.mac.tokens, &mut names);
            for n in names {
                self.saw(n);
            }
        }
        visit::visit_item_macro(self, im);
    }
}

/// Last segments of every string literal in `ts` that looks like a path
/// (`"default_true"`, `"crate::ser::as_secs"`). Non-path strings — sentences,
/// globs, format specs — are skipped.
fn collect_path_strings(ts: &TokenStream, out: &mut BTreeSet<String>) {
    for tt in ts.clone() {
        match tt {
            TokenTree::Literal(l) => {
                let s = l.to_string();
                if let Some(inner) = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
                    insert_if_path(inner, out);
                }
            }
            TokenTree::Group(g) => collect_path_strings(&g.stream(), out),
            _ => {}
        }
    }
}

/// Record `s`'s last segment if `s` is shaped like a Rust path. Prose, globs,
/// and format strings all fail the test, so `#[doc = "…"]` contributes nothing.
fn insert_if_path(s: &str, out: &mut BTreeSet<String>) {
    if s.is_empty() {
        return;
    }
    let is_path = s.split("::").all(|seg| {
        !seg.is_empty()
            && !seg.starts_with(|c: char| c.is_ascii_digit())
            && seg.chars().all(|c| c.is_alphanumeric() || c == '_')
    });
    if is_path {
        if let Some(last) = s.rsplit("::").next() {
            out.insert(last.to_string());
        }
    }
}

fn collect_idents(ts: &TokenStream, out: &mut BTreeSet<String>) {
    for tt in ts.clone() {
        match tt {
            TokenTree::Ident(id) => {
                out.insert(id.to_string());
            }
            TokenTree::Group(g) => collect_idents(&g.stream(), out),
            _ => {}
        }
    }
}

/// Candidate defns come from `ctx.idx` (built over the user-scoped files);
/// `call_source` is the FULL tree so production items called only from tests
/// aren't false-flagged as dead.
/// Every name `dead-code` believes is called, by its own independent
/// mechanism: raw identifier collection rather than call-site matching.
///
/// Exposed so `self-check` can hold it against the AST call-site path. That
/// disagreement is not academic — it is what exposed a fn used only as
/// `.map(f)` and a fn called only from inside a `row!(… => f(x))` arm, both of
/// which the AST path reported as having zero callers while this set contained
/// them all along.
pub(crate) fn called_names(call_source: &[ParsedFile]) -> BTreeSet<String> {
    sink_over(call_source).called()
}

/// One walk of the whole tree, with per-item attribution.
fn sink_over(call_source: &[ParsedFile]) -> CallSink {
    let mut sink = CallSink::new();
    for f in call_source {
        sink.scope = ScopeTracker::new(f.module.as_str());
        sink.visit_file(&f.ast);
    }
    sink
}

/// How many rounds of "remove these, look again" `--transitive` will run.
///
/// A cap rather than a true fixed point only in the pathological case: each
/// round can only shrink the call set, so it converges, and four rounds covered
/// the deepest real cascade seen — three dead `pub fn`s exposing four private
/// orphans over four build-delete-rebuild cycles driven by a Python loop over
/// `cargo build` warnings.
const TRANSITIVE_ROUNDS: usize = 16;

pub fn run(
    ctx: &AnalysisCtx,
    call_source: &[ParsedFile],
    vis: Option<crate::inventory::VisFilter>,
    include_trait_impls: bool,
    transitive: bool,
) -> anyhow::Result<usize> {
    let index = ctx.idx;
    let summary = ctx.summary;
    let sink = sink_over(call_source);

    // Everything about an item except whether anything calls it. Split out so
    // the transitive rounds re-ask only the one question that can change.
    let reportable = |d: &crate::index::Defn| -> bool {
        if !matches!(d.kind, "fn" | "impl-fn" | "trait-fn") {
            return false;
        }
        if let Some(v) = vis {
            if d.vis != v.as_str() {
                return false;
            }
        }
        if matches!(d.name.as_str(), "main" | "start") {
            return false;
        }
        // Trait-impl methods are skipped by default (dyn dispatch is
        // invisible to us); --include-trait-impls reports them when their
        // method name is never called anywhere in the tree.
        if d.kind == "trait-fn" || (d.in_trait_impl && !include_trait_impls) {
            return false;
        }
        if d.allow_dead {
            return false;
        }
        // The harness calls a `#[test]` fn, and the harness is in no call site.
        // Without this, `--scope all` — the scope every command's own note
        // recommends — answered with 600 rows of which every single one was a
        // test fn.
        !d.is_test
    };

    let called = sink.called();
    // `(kind, defn, what would have to go first)`. `None` means nothing:
    // the item is dead as the tree stands.
    let mut hits: Vec<(&str, &crate::index::Defn, Option<String>)> = index
        .iter()
        .filter(|d| reportable(d) && !called.contains(&d.name))
        .map(|d| (d.kind, d, None))
        .collect();

    // Deleting a dead `pub fn` exposes the private helpers only it called, and
    // deleting those exposes theirs. Verified locally that rustc's own
    // `dead_code` lint reports *zero* for a dead `pub fn` in a lib crate — it
    // cannot, since `pub` is API surface — so the first round of this cascade
    // is only visible here, and the rest of it cost four build-delete-rebuild
    // cycles to find by hand.
    if transitive {
        let mut gone: BTreeSet<String> = hits.iter().map(|h| h.1.qpath.clone()).collect();
        let mut frontier: Vec<String> = gone.iter().cloned().collect();
        for _ in 0..TRANSITIVE_ROUNDS {
            let called = sink.called_without(&gone);
            let fresh: Vec<&crate::index::Defn> = index
                .iter()
                .filter(|d| {
                    !gone.contains(&d.qpath) && reportable(d) && !called.contains(&d.name)
                })
                .collect();
            if fresh.is_empty() {
                break;
            }
            let next: Vec<String> = fresh.iter().map(|d| d.qpath.clone()).collect();
            for d in fresh {
                // Which of the just-removed items was naming it — the answer to
                // "what would have to go first".
                let blocker = frontier
                    .iter()
                    .find(|q| {
                        sink.per_item
                            .get(*q)
                            .is_some_and(|names| names.contains(&d.name))
                    })
                    .cloned();
                hits.push((d.kind, d, Some(blocker.unwrap_or_else(|| "—".to_string()))));
            }
            gone.extend(next.iter().cloned());
            frontier = next;
        }
    }

    ctx.retain_changed(&mut hits, |h| &h.1.file);
    // Keyed by the item's own name, so `ok(dead-code/default_true)` retires one
    // fn while an unkeyed `ok(dead-code)` above an impl covers everything in it.
    // This check is a *gating* one and its remaining rows on a real codebase are
    // dominated by call paths no syntactic scan can see (serde attribute
    // strings, dyn dispatch, FFI) — without a waiver the audit loop could never
    // reach zero.
    //
    // A transitive row is not a gating finding: it is dead *conditionally*, and
    // the condition is an edit nobody has made yet.
    let waived = ctx.retain_unsuppressed_tiered(
        "dead-code",
        &mut hits,
        |h| crate::suppress::Site::keyed(h.1.file.as_str(), h.1.line, h.1.name.as_str()),
        |h| h.2.is_none(),
    );
    hits.sort_by(|a, b| a.1.file.cmp(&b.1.file).then_with(|| a.1.line.cmp(&b.1.line)));

    // The summary counts the whole result set; `--top` only bounds the list.
    let total = hits.len();
    let cascaded = hits.iter().filter(|h| h.2.is_some()).count();
    if !summary {
        let today = crate::suppress::Date::today();
        for (kind, d, after) in &hits {
            // The `via` column exists only under `--transitive`: appending it
            // unconditionally would move every existing reader's `awk`, and it
            // says nothing when every row is direct.
            match after {
                None if !transitive => row!(
                    ctx.out,
                    "kind" => *kind,
                    "vis" => d.vis,
                    "qpath" => d.qpath.clone(),
                    "at" => ctx.at(&d.file, d.line, d.end),
                ),
                None => row!(
                    ctx.out,
                    "kind" => *kind,
                    "vis" => d.vis,
                    "qpath" => d.qpath.clone(),
                    "at" => ctx.at(&d.file, d.line, d.end),
                    "via" => "direct",
                ),
                Some(blocker) => row!(
                    ctx.out,
                    "kind" => *kind,
                    "vis" => d.vis,
                    "qpath" => d.qpath.clone(),
                    "at" => ctx.at(&d.file, d.line, d.end),
                    "via" => format!("transitive after {}", blocker),
                ),
            }
            ctx.suggest("dead-code", Some(&d.name), today, (&d.file, d.line));
        }
    }
    // Named where the deletion is about to happen. This check's call set is
    // identifier-based, so a fn a test only names *inside a string* — a fixture
    // literal, an expected-output assertion — is not a call and does not save
    // it from this list. `tests --mentions` reads those, and reads macro bodies
    // where an AST walk stops, which is where most assertions live.
    if let Some((_, first, _)) = hits.first() {
        ctx.out.note(&format!(
            "(note: `tests --mentions {}` lists the tests that name it — including inside \
             string literals and macro bodies, which are not call sites and so are not in \
             this check's call set. Worth one look before deleting.)",
            first.name
        ));
    }
    ctx.out.summary(&format!(
        "({} candidate dead fn(s){}; vis={}; include_trait_impls={}{}; heuristic — call-set \
         built from full tree incl. tests; `#[allow(dead_code)]` skipped; pub items may still \
         have external callers we can\'t see.{})",
        total,
        if cascaded > 0 {
            format!(", {} of them only once the others are gone", cascaded)
        } else {
            String::new()
        },
        vis.map_or("any", crate::inventory::VisFilter::as_str),
        include_trait_impls,
        ctx.waived_note(waived),
        if transitive {
            ""
        } else {
            " `--transitive` also reports the private orphans each deletion would expose."
        }
    ));
    Ok(total)
}
