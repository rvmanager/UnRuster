//! `arith-drift` — one raw operator among checked siblings.
//!
//! The check this tool was missing, found by evaluating it against a real
//! changelog. One fix changed
//!
//! ```ignore
//! corrected_initial_age + resident_age
//! ```
//!
//! to `saturating_add`, in a function where the three adjacent RFC 9111 age
//! terms already saturated. A one-token inconsistency between siblings — which
//! is `divergence`'s entire thesis — and no check in the battery could see it,
//! because `divergence` pairs *enum dispatch sites* and `--handling` pairs
//! *callee error handling*. Nothing looked at expressions.
//!
//! So: inside one function, if some additions are written `saturating_add` and
//! one is written `+`, the odd one out is worth a look. The score is how
//! outnumbered it is, which is the whole signal — an even split is two
//! different jobs in one scope, and three-to-one is someone who missed a line.
//!
//! BEST-EFFORT, and in one specific way: this is a syntactic check with no type
//! information, so a `+` that concatenates `String`s in a function that also
//! saturates integers reads as drift. The obvious spellings of that
//! (`"literal" + …`, `format!(…) + …`, `… .to_string() + …`) are filtered; the
//! rest are candidates for a reader to judge, like every other row this tool
//! prints.

use std::collections::HashMap;

use syn::visit::{self, Visit};

use crate::ast::{line_of_span, peel_grouping, scope_visits, ScopeTracker};
use crate::context::AnalysisCtx;
use crate::emit::{row, site};
use crate::parse::display_path;
use syn::spanned::Spanned;

/// The arithmetic families this check knows: the operator, the name of its
/// checked-method suffix, and the label used in output.
///
/// `%` and the comparison operators are absent on purpose — there is no
/// `saturating_rem` for them to drift from, so a raw one is not an odd one out.
const OPS: &[(&str, &str)] = &[
    ("+", "add"),
    ("-", "sub"),
    ("*", "mul"),
    ("/", "div"),
    ("<<", "shl"),
    (">>", "shr"),
];

/// Prefixes of the methods that say "this author thought about overflow".
const CHECKED_PREFIXES: &[&str] = &["saturating_", "checked_", "wrapping_", "overflowing_"];

#[derive(Debug, Clone)]
struct Site {
    file: String,
    line: usize,
    /// The enclosing fn, which is also the grouping key: drift is only
    /// meaningful between siblings a single author wrote together.
    scope: String,
    /// `add`, `sub`, …
    op: &'static str,
    /// True for `saturating_add(…)`, false for `+`.
    checked: bool,
    /// How it is written, for the row: `+` or `saturating_add`.
    spelling: String,
}

struct ArithVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    sites: Vec<Site>,
}

impl ArithVisitor<'_> {
    fn push(&mut self, op: &'static str, checked: bool, spelling: String, line: usize) {
        let scope = self.scope.enclosing();
        self.sites.push(Site {
            file: self.file.to_string(),
            line,
            scope,
            op,
            checked,
            spelling,
        });
    }
}

impl<'ast> Visit<'ast> for ArithVisitor<'_> {
    scope_visits!(
        item_mod,
        item_impl,
        item_trait,
        item_fn,
        impl_item_fn,
        trait_item_fn
    );

    fn visit_expr_binary(&mut self, e: &'ast syn::ExprBinary) {
        if let Some(op) = op_of_binary(&e.op) {
            if !looks_like_string_concat(e) {
                self.push(
                    op,
                    false,
                    symbol_of(op).to_string(),
                    line_of_span(e.op.span()),
                );
            }
        }
        visit::visit_expr_binary(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let m = e.method.to_string();
        if let Some(op) = checked_call_op(&m) {
            self.push(op, true, m, line_of_span(e.method.span()));
        }
        visit::visit_expr_method_call(self, e);
    }
}

fn op_of_binary(op: &syn::BinOp) -> Option<&'static str> {
    // Compound assignment counts as the same family: `total += n` overflows
    // exactly like `total = total + n`.
    match op {
        syn::BinOp::Add(_) | syn::BinOp::AddAssign(_) => Some("add"),
        syn::BinOp::Sub(_) | syn::BinOp::SubAssign(_) => Some("sub"),
        syn::BinOp::Mul(_) | syn::BinOp::MulAssign(_) => Some("mul"),
        syn::BinOp::Div(_) | syn::BinOp::DivAssign(_) => Some("div"),
        syn::BinOp::Shl(_) | syn::BinOp::ShlAssign(_) => Some("shl"),
        syn::BinOp::Shr(_) | syn::BinOp::ShrAssign(_) => Some("shr"),
        _ => None,
    }
}

fn symbol_of(op: &str) -> &'static str {
    OPS.iter()
        .find(|(_, name)| *name == op)
        .map(|(sym, _)| *sym)
        .unwrap_or("?")
}

/// `saturating_add` → `add`. Anything else → `None`.
fn checked_call_op(method: &str) -> Option<&'static str> {
    let rest = CHECKED_PREFIXES
        .iter()
        .find_map(|p| method.strip_prefix(p))?;
    OPS.iter()
        .find(|(_, name)| *name == rest)
        .map(|(_, name)| *name)
}

/// `"a" + b`, `format!(…) + b`, `x.to_string() + y` — a `String` concatenation,
/// which has no checked sibling and is not drift.
///
/// Only the spellings that are visible without types. This is the check's known
/// blind spot and it is cheaper to say so than to pretend otherwise.
fn looks_like_string_concat(e: &syn::ExprBinary) -> bool {
    fn is_stringy(e: &syn::Expr) -> bool {
        match peel_grouping(e) {
            syn::Expr::Lit(l) => matches!(l.lit, syn::Lit::Str(_)),
            syn::Expr::Macro(m) => m
                .mac
                .path
                .segments
                .last()
                .is_some_and(|s| s.ident == "format"),
            syn::Expr::MethodCall(c) => {
                matches!(c.method.to_string().as_str(), "to_string" | "to_owned" | "into")
            }
            syn::Expr::Reference(r) => is_stringy(&r.expr),
            syn::Expr::Binary(b) => is_stringy(&b.left) || is_stringy(&b.right),
            _ => false,
        }
    }
    is_stringy(&e.left) || is_stringy(&e.right)
}

/// How outnumbered the raw operator is within its scope: `checked / total`.
///
/// One raw among three checked is 0.75; one among one is 0.5, which is where
/// the audit's floor sits, so an even split never reaches the battery.
fn score(checked: usize, raw: usize) -> f64 {
    let total = checked + raw;
    if total == 0 {
        return 0.0;
    }
    checked as f64 / total as f64
}

pub fn run(ctx: &AnalysisCtx, min_score: f64) -> anyhow::Result<usize> {
    let mut sites: Vec<Site> = Vec::new();
    for f in ctx.files {
        let mut v = ArithVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            sites: Vec::new(),
        };
        v.visit_file(&f.ast);
        sites.extend(v.sites);
    }
    ctx.retain_changed(&mut sites, |s| &s.file);

    // (scope, op) — drift is only meaningful between siblings in one fn.
    let mut groups: HashMap<(String, &'static str), Vec<Site>> = HashMap::new();
    for s in sites {
        groups.entry((s.scope.clone(), s.op)).or_default().push(s);
    }

    #[derive(Debug)]
    struct Finding {
        raw: Site,
        witness: Site,
        checked: usize,
        raws: usize,
    }
    let mut findings: Vec<Finding> = Vec::new();
    for ((_, _), members) in groups {
        let (checked, raws): (Vec<Site>, Vec<Site>) = members.into_iter().partition(|s| s.checked);
        // A lone checked call is not a convention, it is one call. The shape
        // this check is named for needs a majority to be the odd one out from.
        if checked.len() < 2 || raws.is_empty() {
            continue;
        }
        let s = score(checked.len(), raws.len());
        if s < min_score {
            continue;
        }
        for raw in raws.iter() {
            findings.push(Finding {
                raw: raw.clone(),
                // The nearest checked sibling: the row is an invitation to
                // compare two lines, so it names the one to compare against.
                witness: nearest(raw, &checked),
                checked: checked.len(),
                raws: raws.len(),
            });
        }
    }

    let waived = ctx.retain_unsuppressed("arith-drift", &mut findings, |f| {
        crate::suppress::Site::keyed(f.raw.file.as_str(), f.raw.line, f.raw.op)
    });

    findings.sort_by(|a, b| {
        score(b.checked, b.raws)
            .partial_cmp(&score(a.checked, a.raws))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.raw.file.cmp(&b.raw.file))
            .then_with(|| a.raw.line.cmp(&b.raw.line))
    });

    if !ctx.summary {
        let today = crate::suppress::Date::today();
        for f in &findings {
            row!(
                ctx.out,
                "op" => f.raw.op,
                "score" => format!("{:.2}", score(f.checked, f.raws)),
                "checked" => f.checked,
                "raw" => f.raws,
                "at" => site(&f.raw.file, f.raw.line),
                "in" => f.raw.scope.clone(),
                "vs" => f.witness.spelling.clone(),
                "vs_at" => site(&f.witness.file, f.witness.line),
            );
            ctx.suggest("arith-drift", Some(f.raw.op), today, (&f.raw.file, f.raw.line));
        }
    }
    ctx.out.summary(&format!(
        "({} raw operator(s) among checked siblings; min_score={:.2}{}; \
         explain: divergence)",
        findings.len(),
        min_score,
        ctx.waived_note(waived)
    ));
    Ok(findings.len())
}

/// The checked sibling closest to `raw` in the file — the one a reader's eye
/// would land on first when asked "why is this one different?".
fn nearest(raw: &Site, checked: &[Site]) -> Site {
    checked
        .iter()
        .min_by_key(|c| raw.line.abs_diff(c.line))
        .cloned()
        .unwrap_or_else(|| raw.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_call_names_map_to_their_operator() {
        assert_eq!(checked_call_op("saturating_add"), Some("add"));
        assert_eq!(checked_call_op("checked_sub"), Some("sub"));
        assert_eq!(checked_call_op("wrapping_mul"), Some("mul"));
        assert_eq!(checked_call_op("overflowing_shl"), Some("shl"));
        // Not an overflow-discipline method at all.
        assert_eq!(checked_call_op("saturating_frobnicate"), None);
        assert_eq!(checked_call_op("add"), None);
    }

    /// The shape the check exists for: three saturating siblings and one raw
    /// `+` must clear the audit's floor, and an even split must not.
    #[test]
    fn one_among_three_scores_above_an_even_split() {
        assert!(score(3, 1) > crate::audit::ARITH_DRIFT_MIN_SCORE);
        assert!(score(1, 1) < crate::audit::ARITH_DRIFT_MIN_SCORE);
        assert!(score(2, 1) > crate::audit::ARITH_DRIFT_MIN_SCORE);
    }

    fn binary(src: &str) -> syn::ExprBinary {
        match syn::parse_str::<syn::Expr>(src).expect("parse") {
            syn::Expr::Binary(b) => b,
            other => panic!("not a binary expr: {:?}", other),
        }
    }

    /// The known blind spot, bounded: `String + &str` has no checked sibling
    /// to drift from, and a function that both concatenates and saturates
    /// would otherwise report the concatenation.
    #[test]
    fn string_concatenation_is_not_arithmetic() {
        assert!(looks_like_string_concat(&binary(r#""a".to_string() + b"#)));
        assert!(looks_like_string_concat(&binary(r#"format!("{}", x) + y"#)));
        assert!(!looks_like_string_concat(&binary("a + b")));
        assert!(!looks_like_string_concat(&binary("age + resident_age")));
    }
}
