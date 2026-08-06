//! `builder-drift` — sibling builder chains, one missing a step.
//!
//! [`crate::config_drift`]'s counterpart for method chains. It exists for a
//! defect this tool could not see: three `Command::new(…)` chains in one
//! function, two of them setting `.current_dir()` and one — the one that
//! resolved the repository root — not. The consequence was that
//! `unruster -r ../other-repo/src audit --since HEAD` compared against
//! whichever repository the shell happened to be in. `co-call` looks at whole
//! functions and saw both `Command::new` and `current_dir` called there, so it
//! reported nothing; nothing else looked at chains at all.
//!
//! # Grouping
//!
//! By the constructor path *and its constant arguments*:
//! `Command::new("git")` and `Command::new("tar")` are different operations and
//! comparing their chains is meaningless. That refinement is what turns the
//! motivating case from a muddy three-way spread into a clean pair — two `git`
//! invocations, alike but for one call.
//!
//! # Why "any split" is safe here and is not in `cohort-callees`
//!
//! Relaxing that command's majority rule to "some call it, some don't" produced
//! twenty-four candidates on a correct two-member cohort, because a function
//! body is full of incidental `.count()` and `.filter()`. A builder chain has
//! no incidental links: every method in it is deliberate configuration, three
//! to six of them. So a split is reportable at N=2, which is exactly the size
//! the majority rule cannot serve.

use std::collections::{BTreeMap, BTreeSet};

use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{fn_span, line_of, path_to_string, type_short, ScopeTracker};
use crate::config_drift::render_const;
use crate::context::{warn_unknown_target, AnalysisCtx, TargetNotFound};
use crate::emit::{row, site};
use crate::parse::display_path;

/// One `Type::ctor(args).a().b()` chain.
#[derive(Debug)]
struct Chain {
    /// `Command::new`
    root: String,
    /// Rendered constant arguments of the constructor, or empty when they are
    /// computed. Part of the group key: two chains only compare if they are
    /// configuring the same thing.
    root_args: String,
    methods: BTreeSet<String>,
    file: String,
    line: usize,
    context: String,
}

impl Chain {
    fn group(&self) -> String {
        if self.root_args.is_empty() {
            self.root.clone()
        } else {
            format!("{}({})", self.root, self.root_args)
        }
    }
}

struct ChainVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    /// Spans of method calls already absorbed as a link of some outer chain.
    /// Without this every suffix of `a().b().c()` is reported as its own chain.
    consumed: BTreeSet<(usize, usize)>,
    out: Vec<Chain>,
}

impl<'ast, 'a> Visit<'ast> for ChainVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.scope.enter_mod(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.scope.leave_mod();
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.scope
            .enter_fn(i.sig.ident.to_string(), fn_span(&i.sig, &i.block));
        visit::visit_item_fn(self, i);
        self.scope.leave_fn();
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.scope.enter_impl(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.scope.leave_impl();
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.scope
            .enter_fn(i.sig.ident.to_string(), fn_span(&i.sig, &i.block));
        visit::visit_impl_item_fn(self, i);
        self.scope.leave_fn();
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let here = span_key(e);
        if !self.consumed.contains(&here) {
            // Outermost link of a chain: walk down to the root, marking each
            // inner link so it is not re-reported as a shorter chain.
            let mut methods = BTreeSet::new();
            methods.insert(e.method.to_string());
            self.consumed.insert(span_key(e));
            let mut node: &syn::Expr = &e.receiver;
            let root = loop {
                match peel(node) {
                    syn::Expr::MethodCall(mc) => {
                        methods.insert(mc.method.to_string());
                        self.consumed.insert(span_key(mc));
                        node = &mc.receiver;
                    }
                    other => break other,
                }
            };
            if let Some((path, args)) = ctor_of(root) {
                self.out.push(Chain {
                    root: path,
                    root_args: args,
                    methods,
                    file: self.file.to_string(),
                    line: line_of(&e.method),
                    context: self.scope.enclosing(),
                });
            }
        }
        visit::visit_expr_method_call(self, e);
    }
}

fn span_key<T: Spanned>(t: &T) -> (usize, usize) {
    let s = t.span().start();
    (s.line, s.column)
}

fn peel(e: &syn::Expr) -> &syn::Expr {
    match e {
        syn::Expr::Paren(p) => peel(&p.expr),
        syn::Expr::Group(g) => peel(&g.expr),
        syn::Expr::Reference(r) => peel(&r.expr),
        other => other,
    }
}

/// `Type::ctor(const args…)` at the root of a chain, or `None`.
///
/// Requires a `::`-qualified path: a chain rooted at a local (`self.out.push()`)
/// is not a builder, and comparing those would drown the check.
fn ctor_of(e: &syn::Expr) -> Option<(String, String)> {
    let syn::Expr::Call(c) = peel(e) else {
        return None;
    };
    let syn::Expr::Path(p) = peel(&c.func) else {
        return None;
    };
    let path = path_to_string(&p.path);
    if !path.contains("::") {
        return None;
    }
    // Constant args only. `Command::new(prog)` groups with every other
    // computed-argument chain on the same constructor, which is the honest
    // default: we cannot tell whether they configure the same thing.
    let args: Vec<String> = c.args.iter().filter_map(render_const).collect();
    let rendered = if args.len() == c.args.len() {
        args.join(",")
    } else {
        String::new()
    };
    Some((path, rendered))
}

/// One reported drift: a builder used several ways.
struct Drift<'c> {
    score: f64,
    group: String,
    /// `method{2/3}` — how many chains in the group make that call.
    differing: Vec<String>,
    shared: usize,
    chains: usize,
    a: &'c Chain,
    b: &'c Chain,
}

/// * `breadth` — the share of the chain both sides agree on, floored as in
///   `config-drift` so wholly different uses of one builder still rank.
/// * `gap` — `1/sqrt(differing)`: one missing call is the classic forgotten
///   step; five differences mean two unrelated uses.
/// * `locality` — the inverse of `config-drift`'s. Two config literals in
///   different modules are suspicious because nobody sees them together; two
///   builder chains in *one function* are suspicious because somebody wrote
///   them together and missed one.
fn score(shared: usize, differing: usize, same_fn: bool, same_file: bool) -> f64 {
    let total = (shared + differing) as f64;
    if total == 0.0 || differing == 0 {
        return 0.0;
    }
    let breadth = 0.4 + 0.6 * (shared as f64 / total);
    let gap = 1.0 / (differing as f64).sqrt();
    let locality = if same_fn {
        1.0
    } else if same_file {
        0.6
    } else {
        0.35
    };
    breadth * gap * locality
}

pub fn run(
    ctx: &AnalysisCtx,
    root_filter: Option<&str>,
    min_score: f64,
    top: Option<usize>,
) -> anyhow::Result<usize> {
    let mut all: Vec<Chain> = Vec::new();
    for f in ctx.files {
        let mut v = ChainVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            consumed: BTreeSet::new(),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.out);
    }
    ctx.retain_changed(&mut all, |c| &c.file);

    let mut groups: BTreeMap<String, Vec<&Chain>> = BTreeMap::new();
    for c in &all {
        if root_filter.is_some_and(|r| r != c.root) {
            continue;
        }
        groups.entry(c.group()).or_default().push(c);
    }
    if let Some(r) = root_filter {
        if groups.is_empty() {
            warn_unknown_target("builder chain rooted at", r);
            ctx.out.summary(&format!("(0 drifting chain(s) on `{}`)", r));
            return Err(TargetNotFound::err("builder chain rooted at", r));
        }
    }

    let mut drifts: Vec<Drift> = Vec::new();
    for (group, chains) in &groups {
        if chains.len() < 2 {
            continue;
        }
        let union: BTreeSet<&str> = chains
            .iter()
            .flat_map(|c| c.methods.iter().map(String::as_str))
            .collect();
        let mut differing: Vec<String> = Vec::new();
        let mut shared = 0usize;
        for m in &union {
            let n = chains.iter().filter(|c| c.methods.contains(*m)).count();
            if n == chains.len() {
                shared += 1;
            } else {
                differing.push(format!("{}{{{}/{}}}", m, n, chains.len()));
            }
        }
        if differing.is_empty() {
            continue;
        }
        // Exemplars: the richest chain against the leanest, so the row names
        // the pair whose difference a reader should look at.
        let a = chains.iter().max_by_key(|c| c.methods.len()).copied();
        let b = chains.iter().min_by_key(|c| c.methods.len()).copied();
        let (Some(a), Some(b)) = (a, b) else { continue };
        if std::ptr::eq(a, b) {
            continue;
        }
        let same_fn = chains.iter().all(|c| c.context == chains[0].context);
        let same_file = chains.iter().all(|c| c.file == chains[0].file);
        let s = score(shared, differing.len(), same_fn, same_file);
        if s < min_score {
            continue;
        }
        differing.sort();
        drifts.push(Drift {
            score: s,
            group: group.clone(),
            differing,
            shared,
            chains: chains.len(),
            a,
            b,
        });
    }

    drifts.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.group.cmp(&y.group))
    });
    let waived = ctx.retain_unsuppressed("builder-drift", &mut drifts, |d| {
        crate::suppress::Site::keyed(d.b.file.as_str(), d.b.line, d.group.as_str())
    });

    let found = drifts.len();
    let shown = top.map(|n| found.min(n)).unwrap_or(found);
    if !ctx.summary {
        let today = crate::suppress::Date::today();
        for d in drifts.iter().take(shown) {
            row!(
                ctx.out,
                "builder" => d.group.clone(),
                "score" => d.score,
                "chains" => d.chains,
                "shared" => d.shared,
                "differs" => d.differing.clone(),
                "leanest" => site(&d.b.file, d.b.line),
                "in" => d.b.context.clone(),
                "vs_richest" => site(&d.a.file, d.a.line),
            );
            ctx.suggest("builder-drift", Some(&d.group), today);
        }
    }
    if shown < found {
        ctx.out.note(&format!(
            "(note: showing the {} highest-scoring of {} drifting builder(s) — raise \
             --top for the rest)",
            shown, found
        ));
    }
    ctx.out.summary(&format!(
        "({} drifting builder(s) across {} multi-use constructor(s); min_score={:.2}{}; \
         explain: builder-drift)",
        shown,
        groups.values().filter(|v| v.len() >= 2).count(),
        min_score,
        ctx.waived_note(waived)
    ));
    Ok(shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_missing_call_between_siblings_outranks_a_broad_difference() {
        let near = score(3, 1, true, true);
        let far = score(1, 5, true, true);
        assert!(near > far, "{near} should outrank {far}");
    }

    #[test]
    fn siblings_in_one_fn_outrank_the_same_split_across_the_tree() {
        // Inverted from `config-drift`: two config literals in different
        // modules are suspicious because nobody sees them together, but two
        // builder chains in one function are suspicious because somebody wrote
        // them together and missed one.
        let together = score(3, 1, true, true);
        let scattered = score(3, 1, false, false);
        assert!(together > scattered, "{together} should outrank {scattered}");
    }

    #[test]
    fn agreement_alone_is_not_a_finding() {
        assert_eq!(score(4, 0, true, true), 0.0);
    }

    fn chains_of(src: &str) -> Vec<Chain> {
        let f: syn::File = syn::parse_str(src).unwrap();
        let mut v = ChainVisitor {
            file: "t.rs",
            scope: ScopeTracker::new("t"),
            consumed: BTreeSet::new(),
            out: Vec::new(),
        };
        v.visit_file(&f);
        v.out
    }

    #[test]
    fn a_chain_is_reported_once_not_once_per_suffix() {
        let c = chains_of("fn f() { let _ = Command::new(\"git\").a().b().c(); }");
        assert_eq!(c.len(), 1, "got {c:#?}");
        assert_eq!(c[0].root, "Command::new");
        assert_eq!(
            c[0].methods.iter().cloned().collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn constant_constructor_args_separate_the_groups() {
        // `Command::new("git")` and `Command::new("tar")` configure different
        // operations; comparing their chains would be noise.
        let c = chains_of(
            "fn f() { let _ = Command::new(\"git\").a(); let _ = Command::new(\"tar\").b(); }",
        );
        assert_eq!(c.len(), 2);
        assert_ne!(c[0].group(), c[1].group());
        assert_eq!(c[0].group(), "Command::new(\"git\")");
    }

    #[test]
    fn a_chain_rooted_at_a_local_is_not_a_builder() {
        let c = chains_of("fn f(v: Vec<u8>) { let _ = v.iter().count(); }");
        assert!(c.is_empty(), "got {c:#?}");
    }
}
