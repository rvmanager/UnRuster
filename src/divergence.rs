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

use crate::ast::{line_of, scope_visits, ScopeTracker, trait_fn_span};
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
    /// Which enum this pair is about (`--all` mode prints it as a column).
    enum_name: String,
    /// Variant count of the enum, for the `[n/total]` annotation.
    total: usize,
    sealed: bool,
}

/// How many variants both sides name. This is the whole basis of the check:
/// two sites are *disagreeing about a shared job* only if they overlap. Two
/// sites with nothing in common are doing different jobs.
fn intersection(a: &[String], b: &[String]) -> usize {
    a.iter().filter(|v| b.contains(v)).count()
}

/// Rank a divergence. The dominant term is how much of the richer site's job
/// the leaner one already does: two paths covering 5 and 4 of 6 variants differ
/// by one deliberate-looking omission, which is a far louder signal than 5-vs-1
/// (two paths doing genuinely different jobs).
///
/// The first version of this used `|lean| / |rich|` for that term, which is not
/// an overlap at all — it made two *disjoint* single-variant sites score 1.00,
/// the maximum. A codebase's families of single-purpose functions
/// (`paint_cage_corners` / `paint_cage_knots` / `paint_cage_midpoints`, or
/// `as_base_shape` / `as_composite_shape`) then filled the top of the ranking:
/// 70 rows scored >= 0.9 on one real tree, and none of them was a defect.
/// Overlap is now a real set intersection, and a pair with none is not a pair.
fn score(rich: &Site, lean: &Site, delta: usize, kinship: Kinship, sealed: bool) -> f64 {
    let shared = intersection(&lean.variants, &rich.variants) as f64;
    let rich_n = rich.variants.len() as f64;
    let overlap = if rich_n > 0.0 { shared / rich_n } else { 0.0 };
    // A single missing variant is the classic "forgot one" bug; the signal
    // decays as the gap widens into "these are different jobs".
    let gap_penalty = 1.0 / delta as f64;
    let sealed_boost = if sealed { 1.5 } else { 1.0 };
    overlap * gap_penalty * kinship.weight() * sealed_boost
}

/// Pair up one enum's dispatch sites and return the asymmetric ones.
///
/// Collecting rather than printing is what lets `--all` rank across every enum
/// before applying `--top`. Printing here meant the cap stopped the *scan*, so
/// `audit --top 40` only ever reached the first six enums in alphabetical
/// order — every pair in the remaining 164 was unreachable no matter how high
/// it scored.
fn diverge_one<'s>(
    ctx: &AnalysisCtx,
    enum_name: &str,
    variant_names: &[String],
    min_score: f64,
    sites: &'s [Site],
) -> Vec<Pair<'s>> {
    let sealed = enum_sealed(ctx.files, enum_name);
    let total = variant_names.len();
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
            // No shared variant = not a disagreement. `show_new_doc_dialog`
            // checks `ActiveModal::NewDocument`; `show_file_add_dialog` checks
            // `PendingFileAdd`. Neither forgot anything — each dialog asks
            // about itself.
            if intersection(&lean.variants, &rich.variants) == 0 {
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
                enum_name: enum_name.to_string(),
                total,
                sealed,
            });
        }
    }
    pairs
}

/// Order pairs loudest-first, with a stable tiebreak so two runs agree.
fn sort_pairs(pairs: &mut [Pair]) {
    pairs.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.lean.file.cmp(&y.lean.file))
            .then_with(|| x.lean.line.cmp(&y.lean.line))
            .then_with(|| x.enum_name.cmp(&y.enum_name))
    });
}

/// Collapse pairs that differ only in which richer sibling they were compared
/// against.
///
/// The scan is an N×M cross-product by construction: every partial site is
/// paired with every richer partial sibling. But "`insert` omits `Group`" is
/// **one** decision regardless of how many siblings happen to handle `Group` —
/// and on a real 170-enum tree that one decision printed six identical-verdict
/// rows, with three such decisions filling seventeen of the section's rows.
/// Reading effort scaled with the sibling count instead of the decision count.
///
/// The exemplar kept is the highest-scoring pair in the group (input must be
/// sorted), which is also the sibling most worth comparing against.
fn group_by_lean(pairs: Vec<Pair<'_>>) -> Vec<(Pair<'_>, usize)> {
    let mut out: Vec<(Pair, usize)> = Vec::new();
    let mut seen: BTreeMap<(String, usize, String, String), usize> = BTreeMap::new();
    for p in pairs {
        let key = (
            p.lean.file.clone(),
            p.lean.line,
            p.enum_name.clone(),
            p.delta.join(","),
        );
        match seen.get(&key) {
            Some(&i) => out[i].1 += 1,
            None => {
                seen.insert(key, out.len());
                out.push((p, 0));
            }
        }
    }
    out
}

fn print_pair(ctx: &AnalysisCtx, p: &Pair, prefixed: bool, others: usize) {
    let tag = if p.sealed { " SEALED" } else { "" };
    let lean = format!("{}{}", p.lean.context, tag);
    let more = if others > 0 {
        format!(" (+{} more sibling(s))", others)
    } else {
        String::new()
    };
    let vs = format!(
        "{} [{}/{}]{}",
        p.rich.context,
        p.rich.variants.len(),
        p.total,
        more
    );
    if prefixed {
        row!(
            ctx.out,
            "enum" => p.enum_name.clone(),
            "score" => p.score,
            "kin" => p.kinship.as_str(),
            "missing_here" => p.delta.clone(),
            "lean" => lean,
            "at" => site(&p.lean.file, p.lean.line),
            "vs" => vs,
            "vs_at" => site(&p.rich.file, p.rich.line),
        );
    } else {
        row!(
            ctx.out,
            "score" => p.score,
            "kin" => p.kinship.as_str(),
            "missing_here" => p.delta.clone(),
            "lean" => lean,
            "at" => site(&p.lean.file, p.lean.line),
            "vs" => vs,
            "vs_at" => site(&p.rich.file, p.rich.line),
        );
    }
}

/// `divergence <Enum>` — or, with no enum named, every enum in the tree ranked
/// together.
///
/// The whole tree is scanned before anything is printed. `--top` then cuts the
/// *ranking*, not the scan: capping mid-scan meant the sections a reader
/// actually looks at contained whichever enums sorted first alphabetically.
pub fn run(
    ctx: &AnalysisCtx,
    target: Option<&str>,
    min_score: f64,
    top: Option<usize>,
) -> anyhow::Result<usize> {
    // Sites are collected up front and held for the whole run so pairs can
    // borrow them across enums.
    let names: Vec<String> = match target {
        Some(n) => vec![n.to_string()],
        None => ctx.idx.enum_names(),
    };
    let single = target.is_some();

    let mut per_enum: Vec<(String, Vec<String>, Vec<Site>)> = Vec::new();
    let mut waived_sites = 0usize;
    for name in &names {
        let variant_names = variant_names_of(ctx.files, name);
        if variant_names.is_empty() {
            continue;
        }
        let mut sites = collect_sites(ctx.files, name, &variant_names, true, true, ctx.spans);
        ctx.retain_changed(&mut sites, |s| &s.file);
        // Unkeyed pass: `ok(divergence)` retires a dispatch site outright, so
        // it can neither lead nor trail a pair. Variant-keyed waivers are
        // applied to the pair's delta below, where the omission is known.
        waived_sites += ctx.retain_unsuppressed("divergence", &mut sites, |s| {
            crate::suppress::Site::new(s.file.as_str(), s.line)
        });
        per_enum.push((name.clone(), variant_names, sites));
    }

    if single && per_enum.is_empty() {
        let enum_name = &names[0];
        warn_unknown_target("enum", enum_name);
        ctx.out
            .summary(&format!("(0 divergent pair(s) on `{}`)", enum_name));
        return Err(TargetNotFound::err("enum", enum_name));
    }

    let mut all: Vec<Pair> = Vec::new();
    for (name, variant_names, sites) in &per_enum {
        all.extend(diverge_one(ctx, name, variant_names, min_score, sites));
    }
    // Variant-keyed waivers attach to the *lean* side and read "this fn's
    // omission of that variant is deliberate". Because the delta is filtered
    // rather than the pair matched whole, one comment retires the omission
    // against every sibling at once — the arena `NodeContent::Group` case that
    // produced seventeen unwaivable rows — and two waivers can jointly clear a
    // pair whose delta spans two variants.
    let waived_pairs = if ctx.suppressions.is_empty() {
        0
    } else {
        let before = all.len();
        all.retain_mut(|p| {
            p.delta.retain(|v| {
                let qualified = format!("{}::{}", p.enum_name, v);
                !ctx.suppressions.matches(
                    "divergence",
                    crate::suppress::Site::keyed(&p.lean.file, p.lean.line, &qualified),
                )
            });
            !p.delta.is_empty()
        });
        before - all.len()
    };
    let waived = waived_sites + waived_pairs;
    sort_pairs(&mut all);

    let grouped = group_by_lean(all);
    let found = grouped.len();
    let shown = top.map(|n| found.min(n)).unwrap_or(found);
    let today = crate::suppress::Date::today();
    for (p, others) in grouped.iter().take(shown) {
        print_pair(ctx, p, !single, *others);
        for v in &p.delta {
            ctx.suggest(
                "divergence",
                Some(&format!("{}::{}", p.enum_name, v)),
                today,
            );
        }
    }
    if shown < found {
        ctx.out.note(&format!(
            "(note: showing the {} highest-scoring of {} pair(s) — raise --top for the rest; \
             the ranking covers every enum, so these are the loudest in the tree)",
            shown, found
        ));
    }

    let sealed_rows = grouped.iter().take(shown).filter(|(p, _)| p.sealed).count();
    let sealed_note = if sealed_rows > 0 {
        format!("; {} on SEALED enum(s)", sealed_rows)
    } else {
        String::new()
    };
    if single {
        ctx.out.summary(&format!(
            "({} divergent pair(s) on `{}`; {} variant(s); min_score={:.2}{}{}; explain: partial-enumeration)",
            shown,
            per_enum[0].0,
            per_enum[0].1.len(),
            min_score,
            ctx.waived_note(waived),
            sealed_note
        ));
    } else {
        ctx.out.summary(&format!(
            "({} divergent pair(s) across {} enum(s); min_score={:.2}{}{}; explain: partial-enumeration)",
            shown,
            per_enum.len(),
            min_score,
            ctx.waived_note(waived),
            sealed_note
        ));
    }
    Ok(shown)
}

// ─── sibling-handling divergence ────────────────────────────────────────────
//
// The enum pass above only sees `match`. The other two real cases — a poisoned
// lock recovered in one fn and dropped in its neighbour, an index bounds-checked
// in one accessor and not in its siblings — are about how a *call* is handled,
// so they need their own scanner.

/// One call site, tagged with how its result was treated.
struct Handled {
    /// The called thing. For a path call this is the full path
    /// (`Anchor::parse`); for a method call, the method name. A bare method
    /// name is not enough on its own — see [`Handled::subject`].
    callee: String,
    /// The root of the receiver expression (`OPEN_FILE` in `OPEN_FILE.lock()`,
    /// `t` in `t.parse::<f64>()`), or empty for a path call.
    ///
    /// Pairing on the method name alone merged `str::parse` with
    /// `Anchor::parse` and reported an iterator's `filter_map(|t|
    /// t.parse().ok())` as the careless sibling of an `.expect()` three
    /// hundred lines away. Two sites only get compared when they name the same
    /// subject.
    subject: String,
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
    /// Whether the expression currently being visited sits in a closure's tail
    /// position. See [`in_combinator_position`].
    closure_tail: Vec<bool>,
    hits: Vec<Handled>,
}

impl HandlingVisitor<'_> {
    fn push(
        &mut self,
        callee: String,
        subject: String,
        treatment: &'static str,
        care: u8,
        line: usize,
    ) {
        let context = self.scope.enclosing();
        self.hits.push(Handled {
            callee,
            subject,
            treatment,
            care,
            context,
            file: self.file.to_string(),
            line,
        });
    }
}

/// Leftmost identifier of an expression — the "subject" a call is made on.
/// `self.lists[l].closed` → `self`, `OPEN_FILE.lock()` → `OPEN_FILE`.
fn receiver_root(e: &syn::Expr) -> String {
    match e {
        syn::Expr::Path(p) => p
            .path
            .segments
            .first()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        syn::Expr::MethodCall(m) => receiver_root(&m.receiver),
        syn::Expr::Field(f) => receiver_root(&f.base),
        syn::Expr::Index(i) => receiver_root(&i.expr),
        syn::Expr::Call(c) => receiver_root(&c.func),
        syn::Expr::Reference(r) => receiver_root(&r.expr),
        syn::Expr::Paren(p) => receiver_root(&p.expr),
        syn::Expr::Try(t) => receiver_root(&t.expr),
        syn::Expr::Unary(u) => receiver_root(&u.expr),
        _ => String::new(),
    }
}

/// Does this closure body look at the error it was handed — bind and use the
/// parameter, or log? That is the line between "recovered from the failure"
/// and "substituted a constant without looking".
fn closure_inspects_error(args: &[syn::Expr]) -> bool {
    let Some(syn::Expr::Closure(c)) = args.first() else {
        return false;
    };
    // `|_| …` / `|_e| …` discards the error by convention.
    let binds = match c.inputs.first() {
        Some(syn::Pat::Ident(i)) => !i.ident.to_string().starts_with('_'),
        Some(syn::Pat::Wild(_)) | None => false,
        Some(_) => true,
    };
    binds || body_logs(&c.body)
}

/// A log/warn/panic macro or method anywhere in the body makes the failure
/// observable. Matched by name shape, not an allow-list: every project spells
/// its logger differently.
fn body_logs(body: &syn::Expr) -> bool {
    struct V {
        found: bool,
    }
    impl<'ast> Visit<'ast> for V {
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            if let Some(seg) = m.path.segments.last() {
                let n = seg.ident.to_string().to_lowercase();
                if n.contains("log")
                    || n.contains("warn")
                    || n.contains("err")
                    || n.contains("trace")
                    || n.contains("debug")
                    || n.contains("panic")
                    || n.starts_with("eprint")
                {
                    self.found = true;
                }
            }
            visit::visit_macro(self, m);
        }
    }
    let mut v = V { found: false };
    v.visit_expr(body);
    v.found
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

/// How much attention the failure got, on one axis: *did this code notice?*
///
///   3  aborts — `.expect` / `.unwrap`: the failure stops the program.
///   2  inspects — `.unwrap_or_else(|e| …e…)` or a fallback that logs: the
///      error value was read, or the failure was made observable.
///   1  substitutes blindly — `.unwrap_or`, `.unwrap_or_default`,
///      `.unwrap_or_else(|_| CONST)`: a value appears, nobody looked.
///   0  discards — `.ok()`, `if let Ok(..)` with no else.
///
/// The earlier ladder ranked `.unwrap_or_default` above `.ok()`, which made
/// every `read_to_string(p).ok()` the "careless sibling" of every
/// `read_to_string(p).unwrap_or_default()` — two spellings of the same
/// policy, reported as a defect. Tiers 0 and 1 are both silent; the gap that
/// matters is silence versus attention.
fn treatment_of(method: &str, inspects: bool) -> Option<(&'static str, u8)> {
    Some(match method {
        "expect" => ("expect", 3),
        "unwrap" => ("unwrap", 3),
        "unwrap_or_else" if inspects => ("unwrap_or_else(inspects)", 2),
        "unwrap_or_else" => ("unwrap_or_else(const)", 1),
        "unwrap_or_default" => ("unwrap_or_default", 1),
        "unwrap_or" => ("unwrap_or", 1),
        "ok" => ("dropped(.ok)", 0),
        _ => return None,
    })
}

/// Receiver methods that produce an `Option` by *design*, not by failing.
/// `x.map(f).unwrap_or(d)` and `v.first().unwrap_or(&d)` are the ordinary way
/// to spell "use a default"; there is no error, so there is nothing to have
/// handled differently. Left in, `map` was the single loudest remaining row on
/// a real tree — pairing a gradient-drag default against an unrelated
/// `.expect` three modules away.
const NON_FALLIBLE: &[&str] = &[
    "map", "and_then", "filter", "first", "last", "get", "get_mut", "next", "pop", "front",
    "back", "peek", "iter", "into_iter", "checked_add", "checked_sub", "checked_mul", "as_ref",
    "as_mut", "as_deref", "cloned", "copied", "take", "or", "or_else", "find", "position", "min",
    "max", "chars", "bytes", "keys", "values",
];

/// Is this expression the tail of a closure — i.e. its value is being returned
/// into a combinator? `filter_map(|t| t.parse().ok())` converts a Result into
/// an Option *to filter on it*; the error is not dropped, it is the predicate.
/// Counting those made an iterator idiom the loudest finding in the check.
fn in_combinator_position(stack: &[bool]) -> bool {
    stack.last().copied().unwrap_or(false)
}

impl<'ast> Visit<'ast> for HandlingVisitor<'_> {
    scope_visits!(item_mod, item_impl, item_trait, item_fn, impl_item_fn);
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.scope.enter_fn(i.sig.ident.to_string(), trait_fn_span(i));
        visit::visit_trait_item_fn(self, i);
        self.scope.leave_fn();
    }

    fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
        // Mark the closure's own tail position: a `.ok()` there is a
        // Result→Option conversion feeding a combinator, not a dropped error.
        self.closure_tail.push(true);
        visit::visit_expr_closure(self, c);
        self.closure_tail.pop();
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let method = e.method.to_string();
        let inspects = closure_inspects_error(&e.args.iter().cloned().collect::<Vec<_>>());
        if let Some((treatment, care)) = treatment_of(&method, inspects) {
            let combinator_ok = care == 0 && in_combinator_position(&self.closure_tail);
            if !combinator_ok {
                // The receiver's own trailing call names *what* is being
                // handled: in `m.lock().ok()` the subject is `lock`, and the
                // root `m` scopes the comparison to that one subject.
                match &*e.receiver {
                    syn::Expr::MethodCall(inner) => {
                        let callee = inner.method.to_string();
                        if !NON_FALLIBLE.contains(&callee.as_str()) {
                            let subject = receiver_root(&inner.receiver);
                            self.push(callee, subject, treatment, care, line_of(&e.method));
                        }
                    }
                    syn::Expr::Call(inner) => {
                        if let syn::Expr::Path(p) = &*inner.func {
                            // Full path, so `Anchor::parse` and `str::parse`
                            // are never the same callee.
                            let full = p
                                .path
                                .segments
                                .iter()
                                .map(|s| s.ident.to_string())
                                .collect::<Vec<_>>()
                                .join("::");
                            // P1-3: a path call has no receiver, so the
                            // *argument* is what is being converted. Without
                            // it, every `u32::try_from` in the tree shares one
                            // bucket and pairs across unrelated modules.
                            let subject = inner
                                .args
                                .first()
                                .map(receiver_root)
                                .unwrap_or_default();
                            self.push(full, subject, treatment, care, line_of(&e.method));
                        }
                    }
                    _ => {}
                }
            }
        }
        // A closure argument's body is not in *this* call's tail position.
        self.closure_tail.push(false);
        visit::visit_expr_method_call(self, e);
        self.closure_tail.pop();
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
                        let subject = receiver_root(&inner.receiver);
                        self.push(
                            inner.method.to_string(),
                            subject,
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

/// Two sites handling the same callee with different care.
struct HandlingPair<'h> {
    gap: u8,
    careful: &'h Handled,
    careless: &'h Handled,
    kinship: Kinship,
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
            closure_tail: Vec::new(),
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }
    ctx.retain_changed(&mut all, |h| &h.file);
    // A separate check name from `divergence`: this axis reports different
    // rows with a different key (the callee being handled), so an
    // `ok(divergence)` on a match site must not silence it.
    let waived = ctx.retain_unsuppressed("divergence-handling", &mut all, |h| {
        crate::suppress::Site::keyed(h.file.as_str(), h.line, h.callee.as_str())
    });

    // Group by what is being handled; divergence is only meaningful within one
    // callee (comparing how `lock` is handled against how `parse` is handled
    // says nothing).
    let mut by_callee: BTreeMap<&str, Vec<&Handled>> = BTreeMap::new();
    for h in &all {
        by_callee.entry(h.callee.as_str()).or_default().push(h);
    }

    let mut pairs: Vec<HandlingPair> = Vec::new();
    for hits in by_callee.values() {
        for (i, a) in hits.iter().enumerate() {
            for b in &hits[i + 1..] {
                // Same callee name is not enough: the two sites must also
                // name the same subject, or they are handling different things
                // that happen to share a method name.
                if a.subject != b.subject {
                    continue;
                }
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

    // One row per careless site, not per (careless, careful) combination. A
    // site whose four sibling `.expect`s all disagree with it is one decision
    // to make, and printing it four times buried the other decisions: 23 rows
    // on a real tree collapsed to 6 once this was applied. The best-ranked
    // exemplar is kept as the model, with a count of the rest.
    let mut seen: BTreeMap<(&str, usize), usize> = BTreeMap::new();
    let mut unique: Vec<&HandlingPair> = Vec::new();
    for p in &pairs {
        let key = (p.careless.file.as_str(), p.careless.line);
        match seen.get_mut(&key) {
            Some(n) => *n += 1,
            None => {
                seen.insert(key, 1);
                unique.push(p);
            }
        }
    }

    for p in &unique {
        let others = seen
            .get(&(p.careless.file.as_str(), p.careless.line))
            .copied()
            .unwrap_or(1)
            - 1;
        let vs = if others > 0 {
            format!(
                "{} [{}] (+{} more sibling(s))",
                p.careful.context, p.careful.treatment, others
            )
        } else {
            format!("{} [{}]", p.careful.context, p.careful.treatment)
        };
        row!(
            ctx.out,
            "gap" => p.gap as usize,
            "kin" => p.kinship.as_str(),
            "callee" => p.careless.callee.clone(),
            "here" => format!("{} [{}]", p.careless.context, p.careless.treatment),
            "at" => site(&p.careless.file, p.careless.line),
            "vs" => vs,
            "vs_at" => site(&p.careful.file, p.careful.line),
        );
        ctx.suggest(
            "divergence-handling",
            Some(&p.careless.callee),
            crate::suppress::Date::today(),
        );
    }
    ctx.out.summary(&format!(
        "({} careless site(s) across {} callee(s); {} sibling comparison(s); \
         min_care_gap={}{}; explain: silent-fallbacks)",
        unique.len(),
        by_callee.len(),
        pairs.len(),
        min_care_gap,
        ctx.waived_note(waived)
    ));
    Ok(unique.len())
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
