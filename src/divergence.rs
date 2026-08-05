//! `divergence` — sibling paths that treat the same thing differently.
//!
//! This check exists because of a measured result: across two audit passes on
//! a large codebase, most confirmed defects came from one shape — *two pieces
//! of code that should agree, and don't* — while the volume checks (casts,
//! conversions, pass-through) produced none. The shape recurs at three levels:
//!
//! 1. **Enum coverage.** `handle_cage_delete` matched 2 of 3 `AnchorEl`
//!    variants; its animate-mode twin `try_anim_cage_remove_selected_knot`
//!    matched 1. The variant one of them forgot was a live user action that
//!    silently did nothing.
//! 2. **Error handling.** `take_open_file` recovered from a poisoned mutex
//!    with a warning; `handle_open_urls`, in the same file, dropped the value
//!    on the floor.
//! 3. **Bounds discipline.** `restore_enabled` bounds-checked an index;
//!    `is_enabled` and `set_enabled`, its siblings on the same table,
//!    indexed unchecked.
//!
//! Finding these by eye means reading two rows of some other command's output
//! side by side and noticing. This command does the pairing, so the noticing
//! isn't left to luck.
//!
//! Every row is a candidate: divergence is often correct (a read path and a
//! write path legitimately handle different variant sets). The output ranks by
//! how *suspicious* the asymmetry is, and names both sides so the comparison
//! is one read, not two.

use std::collections::BTreeMap;

use syn::visit::{self, Visit};

use crate::ast::{fn_span, line_of, trait_fn_span, type_short, ScopeTracker};
use crate::context::{warn_unknown_target, AnalysisCtx, TargetNotFound};
use crate::emit::{row, site};
use crate::parallel_matches::{collect_sites, enum_sealed, variant_names_of, Site};
use crate::parse::display_path;

/// How the two sides of a pair were judged to be siblings — printed so a reader
/// can discount a weak pairing without opening the files.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kinship {
    /// Same enclosing type or module *and* a shared verb in the fn name
    /// (`handle_cage_delete` / `try_anim_cage_remove_selected_knot` share
    /// `cage`). The strongest signal.
    NameAndScope,
    /// Same enclosing type or module. Weaker, but a module is a deliberate
    /// grouping — code filed together is usually meant to agree.
    Scope,
    /// A shared distinctive word in the fn name across different modules.
    Name,
}

impl Kinship {
    fn as_str(self) -> &'static str {
        match self {
            Kinship::NameAndScope => "name+scope",
            Kinship::Scope => "scope",
            Kinship::Name => "name",
        }
    }

    /// Ranking weight — a pair related two ways outranks one related one way.
    fn weight(self) -> f64 {
        match self {
            Kinship::NameAndScope => 1.0,
            Kinship::Scope => 0.6,
            Kinship::Name => 0.5,
        }
    }
}

/// Words that carry no discriminating power when matching fn names. Pairing on
/// these would relate every handler in the tree to every other one.
const STOPWORDS: &[&str] = &[
    "handle", "try", "get", "set", "is", "as", "to", "from", "new", "with", "on", "do", "run",
    "the", "for", "and", "of", "in", "at", "by", "fn", "self", "impl", "mut", "ref", "render",
    "show", "draw", "paint", "update", "apply", "make", "build", "into", "inner", "impl_",
];

/// Split a qualified fn label (`app::cage_drag::VectorianApp::handle_cage_delete`)
/// into its scope (everything before the last segment) and lowercase name words.
fn split_label(label: &str) -> (String, Vec<String>) {
    // `--spans` appends `@start-end`; strip it so labels pair across modes.
    let label = label.split('@').next().unwrap_or(label);
    let (scope, name) = match label.rsplit_once("::") {
        Some((s, n)) => (s.to_string(), n),
        None => (String::new(), label),
    };
    let words = name
        .split('_')
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() > 2 && !STOPWORDS.contains(&w.as_str()))
        .collect();
    (scope, words)
}

/// Do two sites look like siblings, and how strongly?
///
/// `file_a`/`file_b` matter because the module path alone is not enough: two
/// free fns at a crate root both have an empty scope, and the real
/// poisoned-lock divergence was between two fns in *the same file*. Filing
/// code together is a deliberate grouping.
fn kinship(a: &str, file_a: &str, b: &str, file_b: &str) -> Option<Kinship> {
    let (scope_a, words_a) = split_label(a);
    let (scope_b, words_b) = split_label(b);
    // Same *function* isn't a pair; two sites in one fn are one decision.
    if a == b {
        return None;
    }
    let shared_word = words_a.iter().any(|w| words_b.contains(w));
    // Scope kinship: the same module/impl path, or — when the path is
    // uninformative — the same file.
    let same_scope = (!scope_a.is_empty() && scope_a == scope_b) || file_a == file_b;
    match (shared_word, same_scope) {
        (true, true) => Some(Kinship::NameAndScope),
        (false, true) => Some(Kinship::Scope),
        (true, false) => Some(Kinship::Name),
        (false, false) => None,
    }
}

/// One reported divergence: two sibling sites and what separates them.
struct Pair<'s> {
    /// Higher = more suspicious. Drives the output order.
    score: f64,
    kinship: Kinship,
    /// The side covering more of the enum (or handling the error more
    /// carefully) — the model for what the other side probably should do.
    rich: &'s Site,
    lean: &'s Site,
    /// What `rich` covers that `lean` does not.
    delta: Vec<String>,
}

/// Rank a divergence. The dominant term is the *delta size relative to what
/// both sides already share*: two paths covering 5 and 4 of 6 variants differ
/// by one deliberate-looking omission, which is a far louder signal than 5-vs-1
/// (two paths doing genuinely different jobs).
fn score(rich: &Site, lean: &Site, delta: usize, kinship: Kinship, sealed: bool) -> f64 {
    let shared = lean.variants.len() as f64;
    let rich_n = rich.variants.len() as f64;
    // Overlap ratio: 1.0 when lean is a strict prefix of rich's coverage.
    let overlap = if rich_n > 0.0 { shared / rich_n } else { 0.0 };
    // A single missing variant is the classic "forgot one" bug; the signal
    // decays as the gap widens into "these are different jobs".
    let gap_penalty = 1.0 / delta as f64;
    let sealed_boost = if sealed { 1.5 } else { 1.0 };
    overlap * gap_penalty * kinship.weight() * sealed_boost
}

/// Pair up an enum's dispatch sites and report the asymmetric ones.
/// Returns (rows shown, rows on a sealed enum).
fn diverge_one(
    ctx: &AnalysisCtx,
    enum_name: &str,
    variant_names: &[String],
    min_score: f64,
    prefixed: bool,
) -> (usize, usize) {
    let sealed = enum_sealed(ctx.files, enum_name);
    let total = variant_names.len();
    let mut sites = collect_sites(ctx.files, enum_name, variant_names, true, true, ctx.spans);
    ctx.retain_changed(&mut sites, |s| &s.file);
    ctx.retain_unsuppressed(&mut sites, |s| (s.file.as_str(), s.line));
    // Only partial sites can diverge: an exhaustive match covers everything by
    // construction, so it can't be the lean side, and as the rich side it says
    // nothing the compiler isn't already enforcing.
    let partial: Vec<&Site> = sites.iter().filter(|s| s.variants.len() < total).collect();

    let mut pairs: Vec<Pair> = Vec::new();
    for (i, a) in partial.iter().enumerate() {
        for b in &partial[i + 1..] {
            let Some(k) = kinship(&a.context, &a.file, &b.context, &b.file) else {
                continue;
            };
            // Orient the pair: `rich` covers more.
            let (rich, lean) = if a.variants.len() >= b.variants.len() {
                (*a, *b)
            } else {
                (*b, *a)
            };
            let delta: Vec<String> = rich
                .variants
                .iter()
                .filter(|v| !lean.variants.contains(v))
                .cloned()
                .collect();
            // No delta = the two sides agree; that's the healthy case.
            if delta.is_empty() {
                continue;
            }
            let s = score(rich, lean, delta.len(), k, sealed);
            if s < min_score {
                continue;
            }
            pairs.push(Pair {
                score: s,
                kinship: k,
                rich,
                lean,
                delta,
            });
        }
    }

    pairs.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.lean.file.cmp(&y.lean.file))
            .then_with(|| x.lean.line.cmp(&y.lean.line))
    });

    for p in &pairs {
        let tag = if sealed { " SEALED" } else { "" };
        if prefixed {
            row!(
                ctx.out,
                "enum" => enum_name,
                "score" => p.score,
                "kin" => p.kinship.as_str(),
                "missing_here" => p.delta.clone(),
                "lean" => format!("{}{}", p.lean.context, tag),
                "at" => site(&p.lean.file, p.lean.line),
                "vs" => format!("{} [{}/{}]", p.rich.context, p.rich.variants.len(), total),
                "vs_at" => site(&p.rich.file, p.rich.line),
            );
        } else {
            row!(
                ctx.out,
                "score" => p.score,
                "kin" => p.kinship.as_str(),
                "missing_here" => p.delta.clone(),
                "lean" => format!("{}{}", p.lean.context, tag),
                "at" => site(&p.lean.file, p.lean.line),
                "vs" => format!("{} [{}/{}]", p.rich.context, p.rich.variants.len(), total),
                "vs_at" => site(&p.rich.file, p.rich.line),
            );
        }
    }
    let sealed_rows = if sealed { pairs.len() } else { 0 };
    (pairs.len(), sealed_rows)
}

/// `divergence <Enum>` / `divergence --all`.
pub fn run(
    ctx: &AnalysisCtx,
    target: Option<&str>,
    min_score: f64,
    top: Option<usize>,
) -> anyhow::Result<usize> {
    match target {
        Some(enum_name) => {
            let variant_names = variant_names_of(ctx.files, enum_name);
            if variant_names.is_empty() {
                warn_unknown_target("enum", enum_name);
                ctx.out
                    .summary(&format!("(0 divergent pair(s) on `{}`)", enum_name));
                return Err(TargetNotFound::err("enum", enum_name));
            }
            let (n, sealed) = diverge_one(ctx, enum_name, &variant_names, min_score, false);
            ctx.out.summary(&format!(
                "({} divergent pair(s) on `{}`; {} variant(s); min_score={:.2}{}; explain: partial-enumeration)",
                n,
                enum_name,
                variant_names.len(),
                min_score,
                if sealed > 0 {
                    format!("; {} on a SEALED enum", sealed)
                } else {
                    String::new()
                }
            ));
            Ok(n)
        }
        None => {
            let mut total = 0usize;
            let mut sealed_rows = 0usize;
            let mut scanned = 0usize;
            for name in ctx.idx.enum_names() {
                let variant_names = variant_names_of(ctx.files, &name);
                if variant_names.is_empty() {
                    continue;
                }
                scanned += 1;
                let (n, s) = diverge_one(ctx, &name, &variant_names, min_score, true);
                total += n;
                sealed_rows += s;
                if let Some(cap) = top {
                    if total >= cap {
                        // Truncation must be announced: a silently capped list
                        // reads as "that's everything".
                        ctx.out.note(&format!(
                            "(note: stopped after {} row(s) at --top {}; {} of {} enum(s) scanned — \
                             raise --top or lower --min-score for the rest)",
                            total, cap, scanned, ctx.idx.enum_names().len()
                        ));
                        break;
                    }
                }
            }
            ctx.out.summary(&format!(
                "({} divergent pair(s) across {} enum(s); --all; min_score={:.2}{}; explain: partial-enumeration)",
                total,
                scanned,
                min_score,
                if sealed_rows > 0 {
                    format!("; {} on SEALED enums", sealed_rows)
                } else {
                    String::new()
                }
            ));
            Ok(total)
        }
    }
}

// ─── sibling-handling divergence ────────────────────────────────────────────
//
// The enum pass above only sees `match`. The other two real cases — a poisoned
// lock recovered in one fn and dropped in its neighbour, an index bounds-checked
// in one accessor and not in its siblings — are about how a *call* is handled,
// so they need their own scanner.

/// One call site, tagged with how its result was treated.
struct Handled {
    /// The called thing, by last segment (`lock`, `parent`, `pop_input_at`).
    callee: String,
    /// How the result was handled — the axis siblings are compared on.
    treatment: &'static str,
    /// Weight of the treatment: higher = more careful. Divergence is reported
    /// from the careless side.
    care: u8,
    context: String,
    file: String,
    line: usize,
}

struct HandlingVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    hits: Vec<Handled>,
}

impl HandlingVisitor<'_> {
    fn push(&mut self, callee: String, treatment: &'static str, care: u8, line: usize) {
        let context = self.scope.enclosing();
        self.hits.push(Handled {
            callee,
            treatment,
            care,
            context,
            file: self.file.to_string(),
            line,
        });
    }
}

/// `Ok(..)` head of an `if let` — the pattern that makes the binding an error
/// path rather than an optional lookup.
fn pat_is_ok(p: &syn::Pat) -> bool {
    match p {
        syn::Pat::TupleStruct(ts) => ts
            .path
            .segments
            .last()
            .map(|s| s.ident == "Ok")
            .unwrap_or(false),
        syn::Pat::Reference(r) => pat_is_ok(&r.pat),
        syn::Pat::Paren(p) => pat_is_ok(&p.pat),
        _ => false,
    }
}

/// Classify how a method call's result is treated. Ordered by care so two
/// treatments of the same callee can be compared numerically.
fn treatment_of(method: &str) -> Option<(&'static str, u8)> {
    Some(match method {
        "expect" => ("expect", 4),
        "unwrap_or_else" => ("unwrap_or_else", 3),
        "unwrap_or_default" => ("unwrap_or_default", 2),
        "unwrap_or" => ("unwrap_or", 2),
        "unwrap" => ("unwrap", 1),
        "ok" => ("dropped(.ok)", 0),
        _ => return None,
    })
}

impl<'ast> Visit<'ast> for HandlingVisitor<'_> {
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
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.scope.enter_trait(i.ident.to_string());
        visit::visit_item_trait(self, i);
        self.scope.leave_trait();
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.scope.enter_fn(i.sig.ident.to_string(), trait_fn_span(i));
        visit::visit_trait_item_fn(self, i);
        self.scope.leave_fn();
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        if let Some((treatment, care)) = treatment_of(&e.method.to_string()) {
            // The receiver's own trailing method name identifies *what* is
            // being handled: in `m.lock().ok()` the subject is `lock`.
            if let syn::Expr::MethodCall(inner) = &*e.receiver {
                self.push(
                    inner.method.to_string(),
                    treatment,
                    care,
                    line_of(&e.method),
                );
            } else if let syn::Expr::Call(inner) = &*e.receiver {
                if let syn::Expr::Path(p) = &*inner.func {
                    if let Some(seg) = p.path.segments.last() {
                        self.push(seg.ident.to_string(), treatment, care, line_of(&e.method));
                    }
                }
            }
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_if(&mut self, e: &'ast syn::ExprIf) {
        // `if let Ok(x) = m.lock()` with no else — the error path vanishes.
        // Deliberately `Ok` only: `if let Some(x) = v.last_mut()` is ordinary
        // Option handling, and counting it made every optional lookup in the
        // tree a "careless" sibling of every `.expect()` on the same method.
        if e.else_branch.is_none() {
            if let syn::Expr::Let(le) = &*e.cond {
                if pat_is_ok(&le.pat) {
                    if let syn::Expr::MethodCall(inner) = &*le.expr {
                        self.push(
                            inner.method.to_string(),
                            "dropped(if-let-ok)",
                            0,
                            line_of(&e.if_token),
                        );
                    }
                }
            }
        }
        visit::visit_expr_if(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

/// `divergence --handling` — one callee handled with different care by sibling
/// functions. The row is written from the careless side, because that's the one
/// a reader has to decide about.
pub fn run_handling(ctx: &AnalysisCtx, min_care_gap: u8) -> anyhow::Result<usize> {
    let mut all: Vec<Handled> = Vec::new();
    for f in ctx.files {
        let mut v = HandlingVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }
    ctx.retain_changed(&mut all, |h| &h.file);
    ctx.retain_unsuppressed(&mut all, |h| (h.file.as_str(), h.line));

    // Group by what is being handled; divergence is only meaningful within one
    // callee (comparing how `lock` is handled against how `parse` is handled
    // says nothing).
    let mut by_callee: BTreeMap<&str, Vec<&Handled>> = BTreeMap::new();
    for h in &all {
        by_callee.entry(h.callee.as_str()).or_default().push(h);
    }

    struct HandlingPair<'h> {
        gap: u8,
        careful: &'h Handled,
        careless: &'h Handled,
        kinship: Kinship,
    }
    let mut pairs: Vec<HandlingPair> = Vec::new();
    for hits in by_callee.values() {
        for (i, a) in hits.iter().enumerate() {
            for b in &hits[i + 1..] {
                let (careful, careless) = if a.care >= b.care { (*a, *b) } else { (*b, *a) };
                let gap = careful.care - careless.care;
                if gap < min_care_gap {
                    continue;
                }
                let Some(k) = kinship(
                    &careful.context,
                    &careful.file,
                    &careless.context,
                    &careless.file,
                ) else {
                    continue;
                };
                pairs.push(HandlingPair {
                    gap,
                    careful,
                    careless,
                    kinship: k,
                });
            }
        }
    }
    pairs.sort_by(|x, y| {
        y.gap
            .cmp(&x.gap)
            .then_with(|| y.kinship.weight().total_cmp(&x.kinship.weight()))
            .then_with(|| x.careless.file.cmp(&y.careless.file))
            .then_with(|| x.careless.line.cmp(&y.careless.line))
    });

    for p in &pairs {
        row!(
            ctx.out,
            "gap" => p.gap as usize,
            "kin" => p.kinship.as_str(),
            "callee" => p.careless.callee.clone(),
            "here" => format!("{} [{}]", p.careless.context, p.careless.treatment),
            "at" => site(&p.careless.file, p.careless.line),
            "vs" => format!("{} [{}]", p.careful.context, p.careful.treatment),
            "vs_at" => site(&p.careful.file, p.careful.line),
        );
    }
    ctx.out.summary(&format!(
        "({} handling divergence(s) across {} callee(s); min_care_gap={}; explain: silent-fallbacks)",
        pairs.len(),
        by_callee.len(),
        min_care_gap
    ));
    Ok(pairs.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_scope_and_shared_word_is_the_strongest_kinship() {
        assert_eq!(
            kinship(
                "app::cage::Foo::handle_cage_delete",
                "a.rs",
                "app::cage::Foo::cage_insert",
                "b.rs"
            ),
            Some(Kinship::NameAndScope)
        );
    }

    #[test]
    fn a_site_is_not_its_own_sibling() {
        assert_eq!(kinship("a::b::c", "f.rs", "a::b::c", "f.rs"), None);
    }

    #[test]
    fn stopword_only_overlap_does_not_pair_across_modules() {
        // `handle` and `render` relate nearly every UI fn to every other one;
        // pairing on them would bury the real rows.
        assert_eq!(
            kinship("app::x::handle_alpha", "a.rs", "ui::y::handle_beta", "b.rs"),
            None
        );
    }

    #[test]
    fn spans_suffix_does_not_break_pairing() {
        // With --spans a label is `name@12-40`; pairing must survive it or the
        // two flags silently stop composing.
        assert_eq!(
            kinship(
                "app::cage::delete_knot@10-20",
                "a.rs",
                "app::cage::insert_knot@30-40",
                "b.rs"
            ),
            Some(Kinship::NameAndScope)
        );
    }

    #[test]
    fn two_free_fns_in_one_file_are_siblings_despite_empty_scopes() {
        // Crate-root fns have no module path, so scope comparison alone would
        // never pair them — yet "same file" is exactly how the real
        // poisoned-lock divergence presented.
        assert_eq!(
            kinship("open_the_file", "src/lib.rs", "close_something", "src/lib.rs"),
            Some(Kinship::Scope)
        );
        assert_eq!(
            kinship("open_the_file", "src/a.rs", "close_something", "src/b.rs"),
            None
        );
    }

    #[test]
    fn one_missing_variant_outranks_a_wide_gap() {
        let mk = |vars: &[&str]| Site {
            file: "f.rs".into(),
            line: 1,
            context: "m::f".into(),
            variants: vars.iter().map(|s| s.to_string()).collect(),
            wildcard: false,
            is_macro: false,
            is_if_chain: false,
            trait_routed: false,
        };
        let rich = mk(&["A", "B", "C", "D"]);
        let near = mk(&["A", "B", "C"]);
        let far = mk(&["A"]);
        let near_score = score(&rich, &near, 1, Kinship::NameAndScope, false);
        let far_score = score(&rich, &far, 3, Kinship::NameAndScope, false);
        assert!(
            near_score > far_score,
            "forgot-one ({near_score}) must outrank different-jobs ({far_score})"
        );
    }
}
