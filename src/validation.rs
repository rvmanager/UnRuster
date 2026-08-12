//! `asserts` and `validation-drift` — where inputs are checked, and where a
//! sibling forgot to.
//!
//! # Two commands over one scan
//!
//! `asserts` is the catalogue: every place this codebase states a condition it
//! requires. `validation-drift` is the judgment built on it: a cohort of
//! sibling functions where most check their inputs and one does not.
//!
//! The second is `arith-drift`'s thesis pointed at validation rather than
//! arithmetic, and it is ranked the same way — a lone unchecked function among
//! three checked siblings is somebody who missed a line, while a one-to-one
//! split is two different jobs sharing a scope.
//!
//! # What counts as validation
//!
//! Three forms, all exact:
//!
//! * an assertion macro — `assert!`, `assert_eq!`, `assert_ne!`, `debug_assert*`
//! * `ensure!` — the `anyhow`/`snafu` spelling of the same thing
//! * a **guard**: `if <cond> { return Err(…) }`, `if <cond> { bail!(…) }`, or
//!   `if <cond> { return None }`, including the `else` form. This is the one
//!   that matters, because it is how most Rust actually validates, and no check
//!   here could see it before.
//!
//! The `None` form was missing from the first cut, and a run over a real
//! codebase named the cost: `Arena::pop_input_at` and `pop_shape_id_at` reject
//! an out-of-range index by returning `None`, and this check called them
//! unvalidated siblings of functions that reject with `Err`. Returning `None`
//! for input the function refuses is the same decision spelled for a fallible
//! *lookup* rather than a fallible *operation*.
//!
//! Deliberately excluded: `?`, `.ok_or(…)`, `.expect(…)`. Those propagate or
//! assert a failure someone *else* detected; they are not this function
//! deciding what it will accept. Counting them would make almost every function
//! "validating" and the divergence check would report nothing.
//!
//! # Cohorts
//!
//! A cohort is *one enclosing scope* (an `impl` block or a module) plus *one
//! shared word* of the members' names — `parse_header`/`parse_body` in one
//! impl, not `parse` on nine unrelated types across the tree. Sibling-ness has
//! to be local for the comparison to mean anything: two constructors on two
//! unrelated types differ for reasons that have nothing to do with rigour.
//!
//! The shared word must also be a domain word rather than Rust API vocabulary
//! ([`crate::concepts::is_generic_api_word`]). Before that filter, the top of
//! this check's output on its own codebase was seven `run`/`run_*` pairs — one
//! entry point and one variant of it, which is not a cohort that should agree
//! about anything.

use std::collections::BTreeMap;

use syn::visit::{self, Visit};

use crate::ast::{fn_visits, line_of_span, scope_visits, ScopeTracker};
use crate::context::{AnalysisCtx, Counts, GroupBy};
use crate::emit::{row, site};
use crate::parse::display_path;
use syn::spanned::Spanned;

/// The score at or above which a divergence row is a gating `audit` finding.
///
/// Three checked siblings and one unchecked scores 0.75; an even two-to-two
/// split lands at 0.45 and stays advisory, because that shape is usually two
/// jobs rather than one oversight.
pub const GATING_SCORE: f64 = 0.70;

/// One place a function states what it requires.
#[derive(Debug, Clone)]
pub struct Site {
    /// `assert` | `debug-assert` | `ensure` | `guard-return-err` |
    /// `guard-return-none` | `guard-bail`
    pub kind: &'static str,
    /// Qualified path of the enclosing fn. Cohorts are formed from the `fns`
    /// list rather than from here, so a site needs no more identity than the
    /// function it sits in.
    pub owner: String,
    pub file: String,
    pub line: usize,
}

/// One function the scan walked past, whether or not it validated anything.
///
/// A named struct rather than the five-tuple this started as, for the reason
/// `index::Spot` gives about its own: five positional strings is a call nobody
/// can read, and two of them swapped is a defect that shows as a wrong row
/// rather than as a compile error.
#[derive(Debug, Clone)]
struct FnSeen {
    /// Bare name, for cohort formation.
    name: String,
    qpath: String,
    /// The impl block or module it sits in.
    scope: String,
    file: String,
    line: usize,
}

struct Collector<'a> {
    file: &'a str,
    scope: ScopeTracker,
    /// Bare name and qualified path of the fn currently being walked.
    current: Vec<(String, String, usize)>,
    sites: Vec<Site>,
    /// Every fn seen, so `validation-drift` knows which ones validated *nothing*.
    fns: Vec<FnSeen>,
}

impl Collector<'_> {
    /// The impl/module the current fn belongs to: everything up to the last
    /// `::` of its qualified path.
    fn scope_of(qpath: &str) -> String {
        qpath.rsplit_once("::").map(|(s, _)| s.to_string()).unwrap_or_default()
    }

    fn enter_fn(&mut self, sig: &syn::Signature, _block: Option<&syn::Block>) {
        let name = sig.ident.to_string();
        let q = self.scope.qualify(&name);
        self.current.push((name, q, crate::ast::line_of(&sig.ident)));
    }

    fn leave_fn(&mut self, _sig: &syn::Signature, _block: Option<&syn::Block>) {
        if let Some((name, qpath, line)) = self.current.pop() {
            let scope = Self::scope_of(&qpath);
            self.fns.push(FnSeen {
                name,
                qpath,
                scope,
                file: self.file.to_string(),
                line,
            });
        }
    }

    fn hit(&mut self, kind: &'static str, line: usize) {
        // Only the innermost fn: a guard inside a closure belongs to the fn
        // that wrote the closure.
        let Some((_, qpath, _)) = self.current.last() else {
            return;
        };
        self.sites.push(Site {
            kind,
            owner: qpath.clone(),
            file: self.file.to_string(),
            line,
        });
    }
}

/// Does this block do nothing but leave the function empty-handed?
///
/// `return Err(…)`, `bail!(…)`, `return None`, or any of those as the block's
/// tail expression. The tail forms are what `if … { Err(e) } else { Ok(v) }`
/// and `if … { None } else { Some(v) }` look like, and missing them would drop
/// the commonest guard in expression-style Rust.
fn error_exit(b: &syn::Block) -> Option<&'static str> {
    fn is_err_call(e: &syn::Expr) -> bool {
        match crate::ast::peel_expr(e) {
            syn::Expr::Call(c) => matches!(
                crate::ast::peel_expr(&c.func),
                syn::Expr::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "Err")
            ),
            _ => false,
        }
    }
    /// A bare `None`. Only ever a guard here because the block is the body of
    /// an `if` whose other branch produced a value — a fn that returns `None`
    /// unconditionally is not validating, it is a stub, and has no `if` for
    /// this to be reached from.
    fn is_none(e: &syn::Expr) -> bool {
        matches!(
            crate::ast::peel_expr(e),
            syn::Expr::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "None")
        )
    }
    for st in &b.stmts {
        let e = match st {
            syn::Stmt::Expr(e, _) => e,
            syn::Stmt::Macro(m) => {
                if m.mac
                    .path
                    .segments
                    .last()
                    .is_some_and(|s| s.ident == "bail")
                {
                    return Some("guard-bail");
                }
                continue;
            }
            _ => continue,
        };
        match crate::ast::peel_expr(e) {
            syn::Expr::Return(r) => {
                let Some(v) = r.expr.as_deref() else { continue };
                if is_err_call(v) {
                    return Some("guard-return-err");
                }
                if is_none(v) {
                    return Some("guard-return-none");
                }
            }
            syn::Expr::Macro(m) => {
                if m.mac.path.segments.last().is_some_and(|s| s.ident == "bail") {
                    return Some("guard-bail");
                }
            }
            other if is_err_call(other) => return Some("guard-return-err"),
            other if is_none(other) => return Some("guard-return-none"),
            _ => {}
        }
    }
    None
}

impl<'ast> Visit<'ast> for Collector<'_> {
    scope_visits!(item_mod, item_impl, item_trait);

    fn_visits!(around enter_fn, leave_fn; item_fn, impl_item_fn);

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if let Some(last) = m.path.segments.last() {
            let n = last.ident.to_string();
            let kind = match n.as_str() {
                "assert" | "assert_eq" | "assert_ne" => Some("assert"),
                "debug_assert" | "debug_assert_eq" | "debug_assert_ne" => Some("debug-assert"),
                "ensure" => Some("ensure"),
                _ => None,
            };
            if let Some(k) = kind {
                self.hit(k, line_of_span(m.path.span()));
            }
        }
        visit::visit_macro(self, m);
    }

    fn visit_expr_if(&mut self, e: &'ast syn::ExprIf) {
        if let Some(k) = error_exit(&e.then_branch) {
            self.hit(k, line_of_span(e.if_token.span));
        }
        if let Some(syn::Expr::Block(b)) = e.else_branch.as_ref().map(|(_, b)| b.as_ref()) {
            if let Some(k) = error_exit(&b.block) {
                self.hit(k, line_of_span(e.if_token.span));
            }
        }
        visit::visit_expr_if(self, e);
    }
}

/// `(validation sites, every fn seen)` across the scanned tree.
fn scan(ctx: &AnalysisCtx) -> (Vec<Site>, Vec<FnSeen>) {
    let mut sites = Vec::new();
    let mut fns = Vec::new();
    for f in ctx.files {
        let mut c = Collector {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            current: Vec::new(),
            sites: Vec::new(),
            fns: Vec::new(),
        };
        c.visit_file(&f.ast);
        sites.extend(c.sites);
        fns.extend(c.fns);
    }
    (sites, fns)
}

// ──────────────────────────────────────────────────────────────────────────
// `asserts` — the inventory

pub fn run_asserts(ctx: &AnalysisCtx, by: Option<GroupBy>) -> anyhow::Result<usize> {
    let (mut sites, _) = scan(ctx);
    ctx.retain_changed(&mut sites, |s| s.file.as_str());
    sites.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));

    if let Some(by) = by {
        crate::ast::print_grouped_counts(ctx.out, &sites, |s| match by {
            GroupBy::Fn => s.owner.clone(),
            GroupBy::File => s.file.clone(),
            GroupBy::Module => crate::ast::top_module_of(&s.owner).to_string(),
        });
    } else if !ctx.summary {
        for s in &sites {
            row!(
                ctx.out,
                "kind" => s.kind,
                "at" => site(&s.file, s.line),
                "fn" => s.owner.clone(),
            );
        }
    }
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for s in &sites {
        *by_kind.entry(s.kind).or_insert(0) += 1;
    }
    ctx.out.summary(&format!(
        "({} validation site(s) in {} fn(s); {}; `?`, `.ok_or` and `.expect` are \
         deliberately not counted — they propagate a failure someone else detected)",
        sites.len(),
        sites
            .iter()
            .map(|s| s.owner.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        if by_kind.is_empty() {
            "none".to_string()
        } else {
            by_kind
                .iter()
                .map(|(k, n)| format!("{}={}", k, n))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    Ok(sites.len())
}

// ──────────────────────────────────────────────────────────────────────────
// `validation-drift` — the judgment

struct Drift {
    /// The unchecked sibling.
    name: String,
    owner: String,
    file: String,
    line: usize,
    /// Cohort members that do validate.
    checked: Vec<String>,
    /// Cohort members that do not, this one included.
    unchecked: usize,
    /// The word the cohort shares.
    word: String,
    scope: String,
}

impl Drift {
    /// Rank by how outnumbered the unchecked sibling is.
    ///
    /// The same shape `arith-drift` uses, and for the same reason: three
    /// checked siblings and one raw is somebody who missed a line, where a
    /// one-to-one split is two different jobs that happen to share a scope.
    fn score(&self) -> f64 {
        let c = self.checked.len() as f64;
        let u = self.unchecked as f64;
        let ratio = c / (c + u);
        // Two checked siblings say more than one; saturating at four, past
        // which the answer is already yes.
        let weight = ((c - 1.0) / 3.0).clamp(0.0, 1.0);
        (0.25 + 0.45 * ratio + 0.30 * weight).min(1.0)
    }
}

pub fn run_drift(ctx: &AnalysisCtx, min_score: f64) -> anyhow::Result<usize> {
    Ok(run_drift_counted(ctx, min_score)?.total)
}

pub fn run_drift_counted(ctx: &AnalysisCtx, min_score: f64) -> anyhow::Result<Counts> {
    let (sites, fns) = scan(ctx);
    let validating: std::collections::BTreeSet<&str> =
        sites.iter().map(|s| s.owner.as_str()).collect();

    // Cohort key: (enclosing scope, shared name word). A fn joins every cohort
    // its name has a word for, so `parse_header` sits in both the `parse` and
    // the `header` cohort of its impl — and a genuine oversight shows up in
    // whichever of the two its siblings actually share.
    let mut cohorts: BTreeMap<(String, String), Vec<&FnSeen>> = BTreeMap::new();
    for f in &fns {
        for w in crate::concepts::words_of(&f.name) {
            // Rust API vocabulary is not a shared concept. Without this the
            // top of the list on this codebase was `run`/`run_handling`,
            // `run`/`run_playbook`, `run`/`run_candidates` — one entry point
            // and one variant of it, in seven modules, none of them a cohort
            // whose members should agree about validating their inputs.
            // Three characters, unlike `concepts --kind newtype` where a
            // two-letter word (`id`, `db`) usually *is* the concept. A cohort
            // is a claim that two functions do the same kind of work, and `co`
            // shared by `partition_co` and `run_co_call` does not support one.
            if w.chars().count() < 3 || crate::concepts::is_generic_api_word(&w) {
                continue;
            }
            cohorts.entry((f.scope.clone(), w)).or_default().push(f);
        }
    }

    let mut drifts: Vec<Drift> = Vec::new();
    for ((scope, word), members) in cohorts {
        if members.len() < 2 {
            continue;
        }
        let (checked, unchecked): (Vec<&&FnSeen>, Vec<&&FnSeen>) = members
            .iter()
            .partition(|m| validating.contains(m.qpath.as_str()));
        if checked.is_empty() || unchecked.is_empty() {
            continue;
        }
        for u in &unchecked {
            drifts.push(Drift {
                name: u.name.clone(),
                owner: u.qpath.clone(),
                file: u.file.clone(),
                line: u.line,
                checked: checked.iter().map(|c| c.qpath.clone()).collect(),
                unchecked: unchecked.len(),
                word: word.clone(),
                scope: scope.clone(),
            });
        }
    }

    // One fn can land in several cohorts; report it once, under the cohort that
    // makes the strongest case. Reporting all of them would count a single
    // missing check three times.
    drifts.sort_by(|a, b| {
        a.owner
            .cmp(&b.owner)
            .then_with(|| b.score().total_cmp(&a.score()))
    });
    drifts.dedup_by(|a, b| a.owner == b.owner && a.file == b.file && a.line == b.line);

    if ctx.changed.is_some() {
        drifts.retain(|d| ctx.in_scope(&d.file));
    }
    let waived = ctx.retain_unsuppressed("validation-drift", &mut drifts, |d| {
        crate::suppress::Site::keyed(d.file.as_str(), d.line, &d.name)
    });
    let below = {
        let n = drifts.len();
        drifts.retain(|d| d.score() >= min_score);
        n - drifts.len()
    };

    drifts.sort_by(|a, b| {
        b.score()
            .total_cmp(&a.score())
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    if !ctx.summary {
        let today = crate::suppress::Date::today();
        for d in &drifts {
            row!(
                ctx.out,
                "score" => format!("{:.2}", d.score()),
                "cohort" => format!("{}::*{}*", d.scope, d.word),
                "at" => site(&d.file, d.line),
                "fn" => d.owner.clone(),
                "checked_siblings" => d.checked.join("  "),
            );
            ctx.suggest("validation-drift", Some(&d.name), today);
        }
    }

    let gating = drifts.iter().filter(|d| d.score() >= GATING_SCORE).count();
    ctx.out.summary(&format!(
        "({} unchecked sibling(s){}{}; {} validation site(s) across {} fn(s) scanned; \
         explain: validation-drift)",
        drifts.len(),
        if gating > 0 {
            format!(
                ", {} at score >= {:.2} (the tier `audit` gates on)",
                gating, GATING_SCORE
            )
        } else {
            String::new()
        },
        if below > 0 {
            format!("; {} below --min-score {:.2}", below, min_score)
        } else {
            String::new()
        },
        sites.len(),
        fns.len(),
    ));
    let _ = waived;
    Ok(Counts {
        total: drifts.len(),
        gating,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sites_of(src: &str) -> Vec<Site> {
        let ast = syn::parse_file(src).expect("parse");
        let mut c = Collector {
            file: "src/t.rs",
            scope: ScopeTracker::new("t"),
            current: Vec::new(),
            sites: Vec::new(),
            fns: Vec::new(),
        };
        c.visit_file(&ast);
        c.sites
    }

    #[test]
    fn assertion_macros_are_collected_and_classified() {
        let s = sites_of(
            "fn f(n: usize) { assert!(n > 0); debug_assert_eq!(n, n); assert_ne!(n, 9); }",
        );
        let kinds: Vec<&str> = s.iter().map(|x| x.kind).collect();
        assert_eq!(kinds, ["assert", "debug-assert", "assert"]);
    }

    /// The form that matters: most Rust validates with a guard, not an assert,
    /// and nothing in this tool could see one before.
    #[test]
    fn an_early_return_err_guard_counts_as_validation() {
        let s = sites_of(
            "fn f(n: usize) -> Result<(), E> { if n == 0 { return Err(E::Empty); } Ok(()) }",
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, "guard-return-err");
    }

    /// The form a fallible *lookup* uses. A run over a real codebase reported
    /// `Arena::pop_input_at` as an unvalidated sibling because it rejects an
    /// out-of-range index with `None` rather than `Err`.
    #[test]
    fn a_return_none_guard_counts_as_validation() {
        let s = sites_of("fn f(v: &[u8], i: usize) -> Option<u8> { if i >= v.len() { return None; } Some(v[i]) }");
        assert_eq!(s.iter().map(|x| x.kind).collect::<Vec<_>>(), ["guard-return-none"]);
        let t = sites_of("fn f(v: &[u8], i: usize) -> Option<u8> { if i >= v.len() { None } else { Some(v[i]) } }");
        assert_eq!(t.len(), 1, "the expression form counts too");
    }

    #[test]
    fn an_expression_style_guard_counts_too() {
        let s = sites_of(
            "fn f(n: usize) -> Result<u8, E> { if n == 0 { Err(E::Empty) } else { Ok(1) } }",
        );
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn a_bail_guard_counts() {
        let s = sites_of("fn f(n: usize) -> Result<(), E> { if n == 0 { bail!(\"no\"); } Ok(()) }");
        assert_eq!(s.iter().map(|x| x.kind).collect::<Vec<_>>(), ["guard-bail"]);
    }

    /// Propagation is not validation. Counting `?` would make nearly every
    /// function "validating" and leave the divergence check with nothing to say.
    #[test]
    fn propagation_and_assertion_of_someone_elses_failure_are_not_validation() {
        assert!(sites_of("fn f(s: &str) -> Result<u8, E> { Ok(s.parse()?) }").is_empty());
        assert!(sites_of("fn f(s: &str) -> u8 { s.parse().expect(\"num\") }").is_empty());
        assert!(sites_of("fn f(o: Option<u8>) -> Result<u8, E> { o.ok_or(E::None) }").is_empty());
    }

    /// A guard inside a closure belongs to the fn that wrote the closure.
    #[test]
    fn a_site_is_attributed_to_the_innermost_enclosing_fn() {
        let s = sites_of(
            "fn outer(v: &[u8]) -> Result<(), E> { \
             let g = |n: u8| -> Result<(), E> { if n == 0 { return Err(E::Z); } Ok(()) }; \
             g(v[0]) }",
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].owner, "t::outer");
    }

    fn drift(checked: usize, unchecked: usize) -> Drift {
        Drift {
            name: "f".into(),
            owner: "m::f".into(),
            file: "src/t.rs".into(),
            line: 1,
            checked: (0..checked).map(|i| format!("m::c{i}")).collect(),
            unchecked,
            word: "parse".into(),
            scope: "m".into(),
        }
    }

    /// The `arith-drift` shape: one unchecked sibling among three checked ones
    /// is somebody who missed a line.
    #[test]
    fn a_lone_unchecked_sibling_among_three_gates() {
        assert!(
            drift(3, 1).score() >= GATING_SCORE,
            "score {:.2}",
            drift(3, 1).score()
        );
    }

    /// An even split is two different jobs sharing a scope, not an oversight.
    #[test]
    fn an_even_split_does_not_gate() {
        assert!(
            drift(2, 2).score() < GATING_SCORE,
            "score {:.2}",
            drift(2, 2).score()
        );
        assert!(drift(1, 1).score() < GATING_SCORE);
    }

    #[test]
    fn being_more_outnumbered_ranks_higher() {
        assert!(drift(4, 1).score() > drift(2, 1).score());
        assert!(drift(2, 1).score() > drift(1, 1).score());
    }
}
