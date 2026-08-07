//! `config-drift` — the same struct built two ways.
//!
//! This is the tool's central thesis (two things that should agree, and don't)
//! applied to *values* rather than enum variants. It exists because of a
//! measured miss: two functions in this codebase built a `CoverageOpts` to
//! configure the same check battery, one for `audit` and one for the waiver
//! hit-count probe. They drifted — `min_variants: 3` against `min_variants: 0`,
//! `hide_trait_routed: true` against `false` — and the consequence was that
//! orphan detection answered a different question than the audit line, on the
//! same run, in opposite directions. Every existing check looked straight past
//! it: `divergence` reads enum dispatch, not struct literals.
//!
//! # What counts as a comparable value
//!
//! Only constant-shaped field expressions: literals, qualified or SCREAMING
//! paths (`CastClass::Ptr`, `MAX_DEPTH`), and calls / references / tuples /
//! arrays built from those. A field set from a local (`min_variants: a.min`)
//! is computed, not configured, so that *site* simply does not vote on that
//! field — the field is still compared across the sites that do spell out a
//! constant. Without that rule a single CLI-plumbing site, which sets every
//! field from a flag, would suppress the whole comparison.
//!
//! A field omitted behind `..Default::default()` votes as `(default)`. That is
//! load-bearing: in the real defect one side left `compact` to the default and
//! the other set it explicitly, which is exactly the kind of difference a
//! reader skims past.
//!
//! # Ranking
//!
//! Same shape as `divergence`: two literals alike but for one field are a far
//! louder signal than two alike in one field out of six. Three terms multiply —
//! how much of the struct the sites agree on, how narrow the disagreement is,
//! and how few distinct configurations exist. The last one is what separates a
//! drifted constant from a builder that is *supposed* to vary per call.

use std::collections::{BTreeMap, BTreeSet};

use syn::visit::{self, Visit};

use crate::ast::{line_of, path_to_string, peel_grouping, scope_visits, trait_fn_span, ScopeTracker};
use crate::context::{AnalysisCtx, TargetNotFound};
use crate::emit::{row, site};
use crate::parse::display_path;

/// Value recorded for a field a literal leaves to `..Default::default()`.
/// Rendered rather than skipped: "one side spelled it out, the other did not"
/// is a real difference and the easiest kind to miss by eye.
const DEFAULTED: &str = "(default)";

/// Fields whose whole purpose is to differ between instances. Five of the ten
/// top rows on a real codebase were wgpu descriptors differing only in
/// `label` — `Some("glass")` against `Some("glass-hl")` — which is two
/// pipelines correctly naming themselves, not drift. Excluded from the
/// comparison rather than merely down-weighted: a name that matched would be
/// the surprising thing.
const NAMING_FIELDS: &[&str] = &["label", "name", "id", "title", "debug_name", "tag", "key"];

/// One `Foo { .. }` expression.
#[derive(Debug)]
struct Literal {
    ty: String,
    file: String,
    line: usize,
    context: String,
    /// Field → rendered constant. Fields whose value is computed are absent.
    fields: BTreeMap<String, String>,
    /// Field names present in the source but not constant-shaped. Kept so a
    /// site can be shown to have *had* an opinion the scan could not read.
    computed: BTreeSet<String>,
    has_rest: bool,
}

impl Literal {
    /// How this site votes on `field`: a constant, `(default)` when the field
    /// is absent behind a rest-expression, or `None` when the value is computed
    /// (the site abstains).
    fn vote(&self, field: &str) -> Option<&str> {
        if let Some(v) = self.fields.get(field) {
            return Some(v);
        }
        if self.computed.contains(field) {
            return None;
        }
        if self.has_rest {
            return Some(DEFAULTED);
        }
        None
    }
}

struct DriftVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    out: Vec<Literal>,
}

impl<'ast, 'a> Visit<'ast> for DriftVisitor<'a> {
    scope_visits!(item_mod, item_impl, item_trait, item_fn, impl_item_fn);
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        // A trait fn with no default body has nothing to walk. Asked as a
        // question rather than bound as `let Some(body) = …`, which forced a
        // `let _ = body;` underneath purely to silence the unused warning — and
        // that discard then read as a swallowed value to this tool's own check.
        if i.default.is_none() {
            return;
        }
        self.scope
            .enter_fn(i.sig.ident.to_string(), trait_fn_span(i));
        visit::visit_trait_item_fn(self, i);
        self.scope.leave_fn();
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        let ty = crate::ast::last_segment(&path_to_string(&e.path)).to_string();
        let mut fields = BTreeMap::new();
        let mut computed = BTreeSet::new();
        for f in &e.fields {
            let syn::Member::Named(name) = &f.member else {
                continue; // tuple-struct literals carry no field names to compare
            };
            let name = name.to_string();
            match render_const(&f.expr) {
                Some(v) => {
                    fields.insert(name, v);
                }
                None => {
                    computed.insert(name);
                }
            }
        }
        // A literal built inside `impl ThatType` is one of the type's own
        // constructors. Variation between `Out::new` and `Out::silent` is the
        // API, not drift — and every type with two constructors would otherwise
        // top this ranking forever.
        let own_constructor = self
            .scope
            .impl_stack
            .last()
            .is_some_and(|imp| *imp == ty);
        // A literal with no readable constant says nothing about configuration.
        if !fields.is_empty() && !own_constructor {
            self.out.push(Literal {
                ty,
                file: self.file.to_string(),
                line: line_of(&e.path),
                context: self.scope.enclosing(),
                fields,
                computed,
                has_rest: e.rest.is_some(),
            });
        }
        visit::visit_expr_struct(self, e);
    }
}

/// Everything before the last `::` of an enclosing-fn label — the module (and
/// impl) the literal was written in.
pub(crate) fn module_of(context: &str) -> &str {
    context.rsplit_once("::").map(|(m, _)| m).unwrap_or("")
}


/// A path that names a constant rather than a local: qualified
/// (`CastClass::Ptr`), SCREAMING_SNAKE (`MAX_DEPTH`), or a bare CamelCase
/// ident, which in value position is a unit variant or unit struct (`None`,
/// `Some`, `Ordering`). A `snake_case` ident is a binding, and its value is not
/// knowable from one expression.
fn path_is_constish(s: &str) -> bool {
    if s.contains("::") {
        return true;
    }
    s.starts_with(|c: char| c.is_ascii_uppercase())
}

/// `crate::a::b::Enum::Variant` → `Enum::Variant`; `Some` → `Some`.
fn last_two_segments(s: &str) -> String {
    let segs: Vec<&str> = s.split("::").collect();
    match segs.len() {
        0 | 1 => s.to_string(),
        n => segs[n - 2..].join("::"),
    }
}

fn lit_str(l: &syn::Lit) -> String {
    match l {
        syn::Lit::Bool(b) => b.value.to_string(),
        syn::Lit::Int(i) => i.base10_digits().to_string(),
        syn::Lit::Float(f) => f.base10_digits().to_string(),
        syn::Lit::Str(s) => format!("{:?}", s.value()),
        syn::Lit::Char(c) => format!("{:?}", c.value()),
        syn::Lit::Byte(b) => b.value().to_string(),
        syn::Lit::ByteStr(_) | syn::Lit::CStr(_) => "b\"…\"".to_string(),
        _ => "?".to_string(),
    }
}

/// Render `e` if — and only if — it is constant-shaped. Validation and
/// rendering in one pass, so the two can never disagree about what is
/// comparable.
pub(crate) fn render_const(e: &syn::Expr) -> Option<String> {
    Some(match peel_grouping(e) {
        syn::Expr::Lit(l) => lit_str(&l.lit),
        syn::Expr::Path(p) => {
            let s = path_to_string(&p.path);
            if !path_is_constish(&s) {
                return None;
            }
            // Compare the item, not how it was imported. On a real codebase
            // `DxfMargin::Percent(0.0)` and
            // `crate::app::app_state::DxfMargin::Percent(0.0)` were reported as
            // a 0.56 drift: the *same value*, written two ways. Keeping the
            // last two segments preserves `Enum::Variant` and `Type::CONST`
            // while dropping the module path that says nothing about the value.
            last_two_segments(&s)
        }
        syn::Expr::Unary(u) => {
            let op = match u.op {
                syn::UnOp::Neg(_) => "-",
                syn::UnOp::Not(_) => "!",
                _ => return None,
            };
            format!("{}{}", op, render_const(&u.expr)?)
        }
        syn::Expr::Reference(r) => format!("&{}", render_const(&r.expr)?),
        syn::Expr::Call(c) => {
            let f = render_const(&c.func)?;
            let args = c.args.iter().map(render_const).collect::<Option<Vec<_>>>()?;
            format!("{}({})", f, args.join(","))
        }
        syn::Expr::Tuple(t) => {
            let items = t.elems.iter().map(render_const).collect::<Option<Vec<_>>>()?;
            format!("({})", items.join(","))
        }
        syn::Expr::Array(a) => {
            let items = a.elems.iter().map(render_const).collect::<Option<Vec<_>>>()?;
            format!("[{}]", items.join(","))
        }
        _ => return None,
    })
}

/// One reported drift: a struct type whose literals disagree.
struct Drift<'l> {
    score: f64,
    ty: String,
    /// `field{valueA|valueB}`, narrowest disagreements first.
    differing: Vec<String>,
    /// Fields every voting site agreed on — the denominator of `agreement`.
    agreed: usize,
    sites: usize,
    configs: usize,
    a: &'l Literal,
    b: &'l Literal,
}

/// Rank a drift.
///
/// * `breadth` — how much of the struct the sites concur on, **floored at
///   0.4**. The floor is not cosmetic: the defect this check was written for
///   was two `CoverageOpts` literals that agreed on *nothing*, and a plain
///   `agreement` multiplier scored that exactly 0.0 and dropped it. "Alike but
///   for one field" is the loudest drift; "wholly different configurations of
///   the same struct" is still drift.
/// * `gap` — `1/sqrt(differing)`, so a single disagreement outranks a broad one
///   without erasing it. `1/differing` was too steep for the same reason.
/// * `focus` — `1/(configs-1)`. Two distinct configurations is drift; twenty is
///   a builder doing its job.
/// * `scatter` — distinct enclosing modules over sites. Two literals in one
///   module are a local pair a reader sees at once; the same struct assembled
///   in two modules is where configurations quietly diverge, because nobody
///   ever has both on screen.
fn score(agreed: usize, differing: usize, configs: usize, modules: usize, sites: usize) -> f64 {
    let total = (agreed + differing) as f64;
    if total == 0.0 || differing == 0 {
        return 0.0;
    }
    let breadth = 0.4 + 0.6 * (agreed as f64 / total);
    let gap = 1.0 / (differing as f64).sqrt();
    let focus = 1.0 / (configs.saturating_sub(1).max(1)) as f64;
    let scatter = modules as f64 / sites.max(1) as f64;
    breadth * gap * focus * scatter
}

/// Collect literals, group by type, and report the types whose sites disagree.
pub fn run(
    ctx: &AnalysisCtx,
    ty_filter: Option<&str>,
    min_score: f64,
    top: Option<usize>,
) -> anyhow::Result<usize> {
    let mut all: Vec<Literal> = Vec::new();
    for f in ctx.files {
        let mut v = DriftVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.out);
    }
    ctx.retain_changed(&mut all, |l| &l.file);

    let mut by_ty: BTreeMap<&str, Vec<&Literal>> = BTreeMap::new();
    for l in &all {
        if ty_filter.is_some_and(|t| t != l.ty) {
            continue;
        }
        by_ty.entry(l.ty.as_str()).or_default().push(l);
    }
    if let Some(t) = ty_filter {
        if by_ty.is_empty() {
            ctx.warn_unknown("struct literal of type", t);
            ctx.out
                .summary(&format!("(0 drifting field(s) on `{}`)", t));
            return Err(TargetNotFound::err("struct literal of type", t));
        }
    }

    let mut drifts: Vec<Drift> = Vec::new();
    // Types whose only disagreement was a name — reported as a count, never
    // silently dropped.
    let mut naming_only = 0usize;
    for (ty, lits) in &by_ty {
        if lits.len() < 2 {
            continue;
        }
        let names: BTreeSet<&str> = lits
            .iter()
            .flat_map(|l| l.fields.keys().map(String::as_str))
            .collect();

        let mut differing: Vec<(usize, String)> = Vec::new();
        let mut agreed = 0usize;
        let mut naming = 0usize;
        for name in &names {
            // Only sites that actually vote count; a field set from a local
            // abstains rather than blocking the comparison.
            let votes: Vec<&str> = lits.iter().filter_map(|l| l.vote(name)).collect();
            if votes.len() < 2 {
                continue;
            }
            let distinct: BTreeSet<&str> = votes.iter().copied().collect();
            if distinct.len() == 1 {
                agreed += 1;
            } else if NAMING_FIELDS.contains(name) {
                naming += 1;
            } else {
                let joined = distinct.into_iter().collect::<Vec<_>>().join("|");
                differing.push((votes.len(), format!("{}{{{}}}", name, joined)));
            }
        }
        if differing.is_empty() {
            naming_only += usize::from(naming > 0);
            continue;
        }
        // Distinct configurations across the whole type: the builder test.
        let configs = lits
            .iter()
            .map(|l| {
                names
                    .iter()
                    .map(|n| l.vote(n).unwrap_or("?"))
                    .collect::<Vec<_>>()
                    .join("\u{1}")
            })
            .collect::<BTreeSet<_>>()
            .len();

        // Exemplars: the first two sites whose signatures differ, so the row
        // names a concrete pair to open rather than "somewhere among 5 sites".
        let (a, b) = match pick_pair(lits, &names) {
            Some(p) => p,
            None => continue,
        };
        // Module, not file: two presets in one file but different modules are
        // still two places a reader never sees together.
        let modules = lits
            .iter()
            .map(|l| module_of(&l.context))
            .collect::<BTreeSet<_>>()
            .len();
        let s = score(agreed, differing.len(), configs, modules, lits.len());
        if s < min_score {
            continue;
        }
        differing.sort();
        drifts.push(Drift {
            score: s,
            ty: (*ty).to_string(),
            differing: differing.into_iter().map(|(_, d)| d).collect(),
            agreed,
            sites: lits.len(),
            configs,
            a,
            b,
        });
    }

    drifts.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.ty.cmp(&y.ty))
    });

    let waived = ctx.retain_unsuppressed("config-drift", &mut drifts, |d| {
        crate::suppress::Site::keyed(d.a.file.as_str(), d.a.line, d.ty.as_str())
    });

    let found = drifts.len();
    let shown = top.map(|n| found.min(n)).unwrap_or(found);
    if !ctx.summary {
        let today = crate::suppress::Date::today();
        for d in drifts.iter().take(shown) {
            row!(
                ctx.out,
                "type" => d.ty.clone(),
                "score" => d.score,
                "sites" => d.sites,
                "configs" => d.configs,
                "agreed" => d.agreed,
                "differs" => d.differing.clone(),
                "at" => site(&d.a.file, d.a.line),
                "in" => d.a.context.clone(),
                "vs_at" => site(&d.b.file, d.b.line),
                "vs_in" => d.b.context.clone(),
            );
            ctx.suggest("config-drift", Some(&d.ty), today);
        }
    }
    if shown < found {
        ctx.out.note(&format!(
            "(note: showing the {} highest-scoring of {} drifting type(s) — raise --top \
             for the rest)",
            shown, found
        ));
    }
    ctx.out.summary(&format!(
        "({} drifting type(s) across {} multi-site type(s); min_score={:.2}{}{}; \
         explain: config-drift)",
        shown,
        by_ty.values().filter(|v| v.len() >= 2).count(),
        min_score,
        ctx.waived_note(waived),
        if naming_only > 0 {
            format!(
                "; {} type(s) differed only in a naming field (label/name/id/…), which is \
                 what those fields are for",
                naming_only
            )
        } else {
            String::new()
        }
    ));
    Ok(shown)
}

/// Two sites with different signatures — the pair a reader should diff.
fn pick_pair<'l>(
    lits: &[&'l Literal],
    names: &BTreeSet<&str>,
) -> Option<(&'l Literal, &'l Literal)> {
    let sig = |l: &Literal| -> String {
        names
            .iter()
            .map(|n| l.vote(n).unwrap_or("?"))
            .collect::<Vec<_>>()
            .join("\u{1}")
    };
    let first = lits.first()?;
    let base = sig(first);
    let other = lits.iter().find(|l| sig(l) != base)?;
    Some((*first, *other))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(src: &str) -> syn::Expr {
        syn::parse_str(src).unwrap()
    }

    #[test]
    fn constants_render_and_locals_abstain() {
        assert_eq!(render_const(&expr("true")).as_deref(), Some("true"));
        assert_eq!(render_const(&expr("3")).as_deref(), Some("3"));
        assert_eq!(render_const(&expr("Some(1)")).as_deref(), Some("Some(1)"));
        assert_eq!(render_const(&expr("None")).as_deref(), Some("None"));
        assert_eq!(
            render_const(&expr("CastClass::Ptr")).as_deref(),
            Some("CastClass::Ptr")
        );
        assert_eq!(render_const(&expr("MAX_DEPTH")).as_deref(), Some("MAX_DEPTH"));
        // A local is computed, not configured — the site abstains rather than
        // blocking every other site's comparison.
        assert_eq!(render_const(&expr("a.min_variants")), None);
        assert_eq!(render_const(&expr("threshold")), None);
        assert_eq!(render_const(&expr("compute(x)")), None);
    }

    #[test]
    fn one_field_apart_outranks_six() {
        // The ordering the check exists to produce.
        let near = score(6, 1, 2, 2, 2);
        let far = score(1, 6, 2, 2, 2);
        assert!(near > far, "{near} should outrank {far}");
    }

    #[test]
    fn total_disagreement_still_scores() {
        // The defect this check was written for: two `CoverageOpts` literals
        // configuring the same battery, agreeing on *nothing*. An `agreement`
        // multiplier scored it 0.0 and dropped it silently — the check missed
        // its own motivating case until the term was floored.
        let s = score(0, 5, 2, 2, 2);
        assert!(s > 0.0, "wholly divergent configs must still rank, got {s}");
        // …but below a near-identical pair.
        assert!(score(4, 1, 2, 2, 2) > s);
    }

    #[test]
    fn a_builder_is_demoted_below_a_drifted_constant() {
        let drift = score(4, 1, 2, 2, 2);
        // The same disagreement spread over ten configurations is a builder
        // varying on purpose.
        let builder = score(4, 1, 10, 2, 2);
        assert!(drift > builder, "{drift} should outrank {builder}");
    }

    #[test]
    fn module_of_strips_the_fn_name() {
        assert_eq!(module_of("audit::run"), "audit");
        assert_eq!(module_of("emit::Out::new"), "emit::Out");
        assert_eq!(module_of("bare"), "");
    }

    #[test]
    fn two_literals_in_one_module_rank_below_two_across_modules() {
        let scattered = score(4, 1, 2, 2, 2);
        let local = score(4, 1, 2, 1, 2);
        assert!(scattered > local, "{scattered} should outrank {local}");
    }

    #[test]
    fn agreement_is_zero_when_nothing_differs() {
        assert_eq!(score(5, 0, 1, 1, 2), 0.0);
    }
}
