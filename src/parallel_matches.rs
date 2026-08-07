use std::collections::{BTreeMap, HashSet};

use syn::visit::{self, Visit};

use crate::ast::{doc_text, enum_variant_of_path, line_of, scope_visits, ScopeTracker};
use crate::context::{warn_unknown_target, AnalysisCtx, TargetNotFound};
use crate::emit::{row, site as site_cell};
use crate::macro_scan::{macro_body, Body};
use crate::parse::{display_path, ParsedFile};

/// One scanned enum-dispatch site. `pub(crate)` because `catch-all-arms` is a
/// filtered view over the same scanner (see `catch_all::run`).
#[derive(Debug)]
pub(crate) struct Site {
    pub(crate) file: String,
    pub(crate) line: usize,
    pub(crate) context: String,
    /// Names of the target enum's variants that appear in this match site.
    pub(crate) variants: Vec<String>,
    /// Did this site have a wildcard arm? `matches!()` always counts as having
    /// one — the implicit "no-match" branch is exactly the silent-misclassify
    /// risk this tool hunts for.
    pub(crate) wildcard: bool,
    /// True if this site is a `matches!()` invocation rather than a `match` expr.
    pub(crate) is_macro: bool,
    /// True if this site is an `if x == E::A { … } else if x == E::B { … }`
    /// dispatch chain rather than a `match` / `matches!`. Same risk class: the
    /// implicit (or explicit non-If) `else` silently re-bins a new variant.
    pub(crate) is_if_chain: bool,
    /// True if the wildcard / catch-all arm routes through a method call on the
    /// matched scrutinee (e.g. `_ => node.paint_slots()`). Such sites are a
    /// structural false positive for the partial-enumeration defect: a new
    /// variant must implement the trait method, so the catch-all picks up its
    /// behavior automatically. The tool can't see through the method call, so
    /// it would otherwise flag them. Always `false` for `matches!()` (no arm
    /// body to inspect).
    pub(crate) trait_routed: bool,
}

struct ParaVisitor<'a> {
    target_enum: &'a str,
    variant_names: &'a [String],
    file: &'a str,
    scope: ScopeTracker,
    /// Scan `matches!(scrutinee, PAT)` invocations in addition to `match` exprs.
    include_matches_macro: bool,
    /// Scan `if x == E::A { … } else if x == E::B { … }` dispatch chains.
    include_if_chains: bool,
    /// `(line, column)` of the `if` token of every `Expr::If` we have already
    /// absorbed as a non-head arm of some chain. Keeps each chain reported once
    /// from its head while still letting chains nested inside an arm's body be
    /// discovered as their own heads.
    consumed_if_spans: HashSet<(usize, usize)>,
    sites: Vec<Site>,
}

impl<'a> ParaVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    fn variant_in_pattern(&self, pat: &syn::Pat) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_variants(pat, &mut out);
        out
    }

    fn collect_variants(&self, pat: &syn::Pat, out: &mut Vec<String>) {
        match pat {
            syn::Pat::Path(p) => self.push_if_match(&p.path, out),
            // Tuple-struct / struct patterns: check the pattern's own path,
            // then recurse into the payload patterns. The enum dispatch this
            // tool hunts for routinely hides one level down, inside an
            // Option/Result wrapper produced by a lookup:
            //   match doc.find_node(id) { Some(NodeContent::BaseShape(_)) => … }
            // Without the recursion the site scores as "no variants" and the
            // partial-enumeration scanner never sees it.
            syn::Pat::TupleStruct(p) => {
                self.push_if_match(&p.path, out);
                for elem in &p.elems {
                    self.collect_variants(elem, out);
                }
            }
            syn::Pat::Struct(p) => {
                self.push_if_match(&p.path, out);
                for f in &p.fields {
                    self.collect_variants(&f.pat, out);
                }
            }
            // Plain tuple patterns: multi-scrutinee dispatch
            // (`match (kind, other) { (E::A, _) => … }`).
            syn::Pat::Tuple(t) => {
                for elem in &t.elems {
                    self.collect_variants(elem, out);
                }
            }
            syn::Pat::Slice(s) => {
                for elem in &s.elems {
                    self.collect_variants(elem, out);
                }
            }
            // `binding @ E::A(..)` — the subpattern carries the variant.
            syn::Pat::Ident(i) => {
                if let Some((_, sub)) = &i.subpat {
                    self.collect_variants(sub, out);
                }
            }
            syn::Pat::Or(o) => {
                for c in &o.cases {
                    self.collect_variants(c, out);
                }
            }
            syn::Pat::Reference(r) => self.collect_variants(&r.pat, out),
            syn::Pat::Paren(p) => self.collect_variants(&p.pat, out),
            _ => {}
        }
    }

    fn push_if_match(&self, p: &syn::Path, out: &mut Vec<String>) {
        if let Some(v) = self.variant_from_path(p) {
            if !out.contains(&v) {
                out.push(v);
            }
        }
    }

    /// If `p` is `<EnumName>::<Variant>` where `EnumName` matches the target
    /// enum (last-segment rule) and `Variant` is one of its variants, return
    /// the variant ident. Otherwise `None`.
    fn variant_from_path(&self, p: &syn::Path) -> Option<String> {
        enum_variant_of_path(p, self.target_enum, self.variant_names, false)
    }

    /// Pull the target-enum variant ident out of an `==` operand expression.
    /// Handles a bare path (`E::Unit`) and a call to a variant constructor
    /// (`E::Payload(expr)`), peeling borrows/parens. The variant identity is
    /// what coverage scores; any payload is irrelevant.
    fn variant_from_expr(&self, e: &syn::Expr) -> Option<String> {
        match peel_expr(e) {
            syn::Expr::Path(p) => self.variant_from_path(&p.path),
            syn::Expr::Call(c) => match peel_expr(&c.func) {
                syn::Expr::Path(p) => self.variant_from_path(&p.path),
                _ => None,
            },
            _ => None,
        }
    }

    /// If `cond` is `scrutinee == E::Variant` (either operand order), return the
    /// scrutinee expression and the covered variant ident. Skips `!=` and any
    /// comparison where neither (or both) side names a target-enum variant.
    fn eq_arm<'e>(&self, cond: &'e syn::Expr) -> Option<(&'e syn::Expr, String)> {
        let syn::Expr::Binary(b) = peel_expr(cond) else {
            return None;
        };
        if !matches!(b.op, syn::BinOp::Eq(_)) {
            return None;
        }
        let lhs_v = self.variant_from_expr(&b.left);
        let rhs_v = self.variant_from_expr(&b.right);
        match (lhs_v, rhs_v) {
            // Variant on the right: `x == E::A` (the canonical shape).
            (None, Some(v)) => Some((&b.left, v)),
            // Variant on the left: `E::A == x` (reversed).
            (Some(v), None) => Some((&b.right, v)),
            // Neither side is a variant, or both are (ambiguous) → not a dispatch arm.
            _ => None,
        }
    }

    /// Walk an `if x == E::A { … } else if x == E::B { … }` chain from its head,
    /// collecting the covered variant idents. Stops at the first arm that isn't
    /// `<same-scrutinee> == E::Variant` (an explicit non-If `else` marks a
    /// catch-all). Returns a site only for chains of ≥ 2 covered variants;
    /// shorter ones are a single guard, not a dispatch. Records every absorbed
    /// else-if span so the chain is reported once, from its head.
    fn collect_if_chain(&mut self, head: &syn::ExprIf) -> Option<Site> {
        let (scrut_expr, first) = self.eq_arm(&head.cond)?;
        let scrutinee = peel_expr(scrut_expr);
        let mut variants: Vec<String> = vec![first];
        let mut consumed: Vec<(usize, usize)> = Vec::new();
        let mut has_catch_all = false;
        let mut else_block: Option<&syn::Expr> = None;

        let mut cur = head;
        // Implicit `else` (no else_branch) terminates the chain with no
        // catch-all body.
        while let Some((_, else_expr)) = cur.else_branch.as_ref() {
            match else_expr.as_ref() {
                syn::Expr::If(next) => match self.eq_arm(&next.cond) {
                    Some((s2, v2)) if peel_expr(s2) == scrutinee => {
                        consumed.push(span_key(&next.if_token));
                        if !variants.contains(&v2) {
                            variants.push(v2);
                        }
                        cur = next;
                    }
                    // Different scrutinee / negated / non-enum guard: the chain
                    // ends here, and this tail is itself an `if` (not a catch-all
                    // block), so `has_catch_all` stays false.
                    _ => break,
                },
                other => {
                    // Terminal non-If `else { … }` — the explicit catch-all.
                    has_catch_all = true;
                    else_block = Some(other);
                    break;
                }
            }
        }

        if variants.len() < 2 {
            return None;
        }
        for k in consumed {
            self.consumed_if_spans.insert(k);
        }

        // A catch-all that routes through a method call on the scrutinee is
        // structurally safe (a new variant must implement the trait method) —
        // same false-positive class the match scanner already tags.
        let trait_routed = else_block
            .map(|b| arm_routes_through_scrutinee(b, scrutinee))
            .unwrap_or(false);

        variants.sort();
        Some(Site {
            file: self.file.to_string(),
            line: line_of(&head.if_token),
            context: self.enclosing(),
            variants,
            wildcard: has_catch_all,
            is_macro: false,
            is_if_chain: true,
            trait_routed,
        })
    }

    /// Wildcard / catch-all arm: `_`, a plain ident binding, or either of
    /// those inside `|`-cases, references, or parens (`A | B | _`, `&_`).
    fn is_wildcard(pat: &syn::Pat) -> bool {
        match pat {
            syn::Pat::Wild(_) => true,
            syn::Pat::Ident(i) => i.subpat.is_none(),
            syn::Pat::Or(o) => o.cases.iter().any(Self::is_wildcard),
            syn::Pat::Reference(r) => Self::is_wildcard(&r.pat),
            syn::Pat::Paren(p) => Self::is_wildcard(&p.pat),
            _ => false,
        }
    }
}

fn span_key<T: syn::spanned::Spanned>(t: &T) -> (usize, usize) {
    let s = t.span().start();
    (s.line, s.column)
}

/// Peel borrows, derefs, parens, and groups so `&node` / `*node` / `(node)`
/// all compare structurally equal to the bare `node`. Relies on syn's
/// `extra-traits` `PartialEq`, which ignores spans.
fn peel_expr(mut e: &syn::Expr) -> &syn::Expr {
    loop {
        e = match e {
            syn::Expr::Reference(r) => &r.expr,
            syn::Expr::Paren(p) => &p.expr,
            syn::Expr::Group(g) => &g.expr,
            syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => &u.expr,
            other => return other,
        };
    }
}

/// Does `body` contain a method call whose receiver is the match scrutinee
/// (e.g. the catch-all arm `_ => node.paintable_kind() == Path` where the
/// scrutinee was `node`)? If so, the site routes new-variant behavior through
/// a trait method and is a false positive for the partial-enumeration defect.
fn arm_routes_through_scrutinee(body: &syn::Expr, scrutinee: &syn::Expr) -> bool {
    struct V<'s> {
        scrutinee: &'s syn::Expr,
        found: bool,
    }
    impl<'ast, 's> Visit<'ast> for V<'s> {
        fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
            if peel_expr(&c.receiver) == self.scrutinee {
                self.found = true;
            }
            visit::visit_expr_method_call(self, c);
        }
    }
    let mut v = V {
        scrutinee: peel_expr(scrutinee),
        found: false,
    };
    v.visit_expr(body);
    v.found
}

impl<'ast, 'a> Visit<'ast> for ParaVisitor<'a> {
    scope_visits!(item_mod, item_impl, item_trait, item_fn, impl_item_fn, trait_item_fn);

    fn visit_expr_match(&mut self, e: &'ast syn::ExprMatch) {
        let mut variants: Vec<String> = Vec::new();
        let mut wildcard = false;
        let mut trait_routed = false;
        for arm in &e.arms {
            for v in self.variant_in_pattern(&arm.pat) {
                if !variants.contains(&v) {
                    variants.push(v);
                }
            }
            if Self::is_wildcard(&arm.pat) {
                wildcard = true;
                if arm_routes_through_scrutinee(&arm.body, &e.expr) {
                    trait_routed = true;
                }
            }
        }
        if !variants.is_empty() {
            variants.sort();
            self.sites.push(Site {
                file: self.file.to_string(),
                line: line_of(&e.match_token),
                context: self.enclosing(),
                variants,
                wildcard,
                is_macro: false,
                is_if_chain: false,
                trait_routed,
            });
        }
        visit::visit_expr_match(self, e);
    }

    fn visit_expr_if(&mut self, e: &'ast syn::ExprIf) {
        if self.include_if_chains && !self.consumed_if_spans.contains(&span_key(&e.if_token)) {
            if let Some(site) = self.collect_if_chain(e) {
                self.sites.push(site);
            }
        }
        // Always recurse: chains nested inside a then-branch (or deeper) are
        // discovered as their own heads; else-if arms we already absorbed are
        // gated out above via `consumed_if_spans`.
        visit::visit_expr_if(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if self.include_matches_macro {
            // `matches!(scrutinee, PAT)` — PAT is the only thing that matches; every
            // other variant falls through to an implicit `false`. So a partial
            // pattern is a silent-misclassify just like `match { .. => _ }`.
            if let Body::Matches { pat, .. } = macro_body(m) {
                let mut variants = self.variant_in_pattern(&pat);
                if !variants.is_empty() {
                    variants.sort();
                    variants.dedup();
                    self.sites.push(Site {
                        file: self.file.to_string(),
                        line: line_of(&m.path),
                        context: self.enclosing(),
                        variants,
                        wildcard: true,
                        is_macro: true,
                        is_if_chain: false,
                        trait_routed: false,
                    });
                }
            }
        }
        visit::visit_macro(self, m);
    }
}

/// Read the target enum's variant names from any definition in the tree.
/// Uses a visitor so enums declared inside nested inline modules are found
/// too (a plain loop over `f.ast.items` would miss them).
pub(crate) fn variant_names_of(files: &[ParsedFile], enum_name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for set in variant_sets_of(files, enum_name) {
        for v in set {
            if !out.contains(&v) {
                out.push(v);
            }
        }
    }
    out
}

/// One variant list per *definition* of `enum_name` in the tree.
///
/// Targets are matched by last segment, so two enums can share a name —
/// `edit::Op` (26 variants) and `geom::boolean::Op` (4). Flattening both into
/// one list made every exhaustive `match` on the small one look like 4/26
/// coverage: six false-positive rows on one real codebase, and
/// `enum-coverage Op` unable to answer for either enum. Keeping the sets apart
/// lets each site be scored against the definition it actually dispatches on.
pub(crate) fn variant_sets_of(files: &[ParsedFile], enum_name: &str) -> Vec<Vec<String>> {
    struct V<'a> {
        target: &'a str,
        out: Vec<Vec<String>>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_item_enum(&mut self, e: &'ast syn::ItemEnum) {
            if e.ident == self.target {
                self.out
                    .push(e.variants.iter().map(|v| v.ident.to_string()).collect());
            }
        }
    }
    let mut v = V {
        target: enum_name,
        out: Vec::new(),
    };
    for f in files {
        v.visit_file(&f.ast);
    }
    v.out
}

/// The definition a site dispatches on: the smallest variant set that contains
/// every variant the site named. A site can only be scored against an enum
/// whose variants it stays inside, so this is exact whenever the covered set is
/// non-empty and the definitions differ. Falls back to the union (index 0 of
/// the caller's list) when nothing fits — a site mixing variants from two
/// same-named enums is not valid Rust, so this only happens if the scan itself
/// mis-attributed a path.
pub(crate) fn definition_for<'a>(sets: &'a [Vec<String>], covered: &[String]) -> Option<&'a Vec<String>> {
    sets.iter()
        .filter(|set| covered.iter().all(|v| set.contains(v)))
        .min_by_key(|set| set.len())
}

/// Walk every file and collect the match / `matches!` sites that mention the enum.
pub(crate) fn collect_sites(
    files: &[ParsedFile],
    enum_name: &str,
    variant_names: &[String],
    include_matches_macro: bool,
    include_if_chains: bool,
    spans: bool,
) -> Vec<Site> {
    let mut all_sites: Vec<Site> = Vec::new();
    for f in files {
        let mut v = ParaVisitor {
            target_enum: enum_name,
            variant_names,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(spans),
            include_matches_macro,
            include_if_chains,
            consumed_if_spans: HashSet::new(),
            sites: Vec::new(),
        };
        v.visit_file(&f.ast);
        all_sites.extend(v.sites);
    }
    all_sites
}

/// Variants present in `full` but absent from `covered`, preserving `full`'s order.
fn missing_variants(covered: &[String], full: &[String]) -> Vec<String> {
    full.iter()
        .filter(|v| !covered.contains(v))
        .cloned()
        .collect()
}

#[allow(clippy::too_many_arguments)]
/// Flags controlling a `parallel-matches` scan. Grouped into one value so the
/// entrypoint takes a single options argument instead of five positional bools.
#[derive(Default, Clone, Copy)]
pub struct ScanOpts {
    /// Hide compiler-protected exhaustive groups.
    pub partial_only: bool,
    /// Order groups by coverage ratio (covered/total) instead of site count.
    pub rank_by_gap: bool,
    /// Annotate each group with the variants it leaves uncovered.
    pub show_missing: bool,
    /// Also scan `matches!()` invocations.
    pub include_matches_macro: bool,
    /// Also scan `if x == E::A { … } else if … ` dispatch chains.
    pub include_if_chains: bool,
}

pub fn run(
    ctx: &AnalysisCtx,
    target: Option<&str>,
    opts: ScanOpts,
) -> anyhow::Result<usize> {
    match target {
        Some(enum_name) => {
            let variant_names = variant_names_of(ctx.files, enum_name);
            if variant_names.is_empty() {
                if ctx.idx.knows_name(enum_name) {
                    ctx.out.note(&format!(
                        "note: `{}` is named in the tree but no enum definition with variants \
                         was found under --scope; nothing to scan",
                        enum_name
                    ));
                    ctx.out.summary(&format!(
                        "(0 match site(s) across 0 variant-set group(s) on `{}`)",
                        enum_name
                    ));
                    return Ok(0);
                }
                warn_unknown_target("enum", enum_name);
                ctx.out.summary(&format!(
                    "(0 match site(s) across 0 variant-set group(s) on `{}`)",
                    enum_name
                ));
                return Err(TargetNotFound::err("enum", enum_name));
            }
            let (sites, groups) = scan_groups(ctx, enum_name, &variant_names, opts, false);
            ctx.out.summary(&format!(
                "({} match site(s) across {} variant-set group(s) on `{}`{})",
                sites,
                groups,
                enum_name,
                if opts.partial_only {
                    "; exhaustive groups hidden"
                } else {
                    ""
                }
            ));
            Ok(groups)
        }
        // `--all`: every enum in the index; group rows gain a leading enum column.
        None => {
            let mut total_sites = 0usize;
            let mut total_groups = 0usize;
            let mut scanned = 0usize;
            for name in ctx.idx.enum_names() {
                let variant_names = variant_names_of(ctx.files, &name);
                if variant_names.is_empty() {
                    continue;
                }
                scanned += 1;
                let (sites, groups) = scan_groups(ctx, &name, &variant_names, opts, true);
                total_sites += sites;
                total_groups += groups;
            }
            ctx.out.summary(&format!(
                "({} match site(s) across {} group(s) on {} enum(s); --all{})",
                total_sites,
                total_groups,
                scanned,
                if opts.partial_only {
                    "; exhaustive groups hidden"
                } else {
                    ""
                }
            ));
            Ok(total_groups)
        }
    }
}

/// Group, sort, and print the match sites of one enum. With `prefixed`
/// (--all mode) each group row carries a leading enum-name column. Returns
/// (site count, group count shown).
fn scan_groups(
    ctx: &AnalysisCtx,
    enum_name: &str,
    variant_names: &[String],
    opts: ScanOpts,
    prefixed: bool,
) -> (usize, usize) {
    let summary = ctx.summary;
    // Per-definition variant sets, for the same reason `coverage_one` keeps
    // them: two crates in one workspace can both define `enum Side`, and
    // scoring a site against the *union* reports a match the compiler accepts
    // as exhaustive as a partial one — with the other enum's variants listed as
    // "missing". That decision was made and documented on `variant_sets_of`,
    // but only `enum-coverage` ever acted on it; this scan, `catch-all-arms`
    // and `divergence` all kept taking the union. On one real workspace it
    // produced false partial-dispatch rows for seven enum names in a *gating*
    // check, and the reader's fix was to write down the seven names somewhere.
    let sets = variant_sets_of(ctx.files, enum_name);
    let owned_union = variant_names.to_vec();
    // The set a group dispatches on: the smallest definition containing every
    // variant it named. Falls back to the union when nothing fits, which for a
    // real `match` means the scan mis-attributed a path rather than that the
    // code is odd.
    let set_for = |variants: &[String]| -> Vec<String> {
        definition_for(&sets, variants)
            .cloned()
            .unwrap_or_else(|| owned_union.clone())
    };
    let mut all_sites = collect_sites(
        ctx.files,
        enum_name,
        variant_names,
        opts.include_matches_macro,
        opts.include_if_chains,
        ctx.spans,
    );
    ctx.retain_changed(&mut all_sites, |s| &s.file);

    // Group by variant set (key = joined sorted variants + wildcard flag).
    let mut groups: BTreeMap<(Vec<String>, bool), Vec<&Site>> = BTreeMap::new();
    for s in &all_sites {
        groups
            .entry((s.variants.clone(), s.wildcard))
            .or_default()
            .push(s);
    }
    let mut rows: Vec<_> = groups.into_iter().collect();

    // A group is "exhaustive" when it names every variant of the enum it
    // dispatches on — those are compiler-protected, so `--hide-exhaustive`
    // drops them. Measured against that enum, not against the union: a match
    // the compiler accepts must never be reported as partial.
    let is_exhaustive = |variants: &[String]| {
        let n = set_for(variants).len();
        n > 0 && variants.len() == n
    };
    if opts.partial_only {
        rows.retain(|((variants, _), _)| !is_exhaustive(variants));
    }

    // Default ordering: by group size descending (parallel-shot first). With
    // --rank-by-gap, order by coverage ratio descending instead — a 7/8 group
    // (one new variant silently mis-binds) is a louder defect signal than a 1/8.
    if opts.rank_by_gap {
        // Groups no longer share a denominator — with two same-named enums,
        // 2-of-2 and 2-of-9 both have a covered-count of 2 and are not remotely
        // the same signal. Compare `covered/total` as a cross-multiplied
        // integer ratio, which keeps the old ordering exactly when there is one
        // definition (the usual case) and stays exact with no floats.
        rows.sort_by(|a, b| {
            let (ca, cb) = (a.0 .0.len(), b.0 .0.len());
            let (ta, tb) = (set_for(&a.0 .0).len().max(1), set_for(&b.0 .0).len().max(1));
            (cb * ta)
                .cmp(&(ca * tb))
                .then_with(|| b.1.len().cmp(&a.1.len()))
                .then_with(|| a.0.cmp(&b.0))
        });
    } else {
        rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    }

    if !summary {
        for ((variants, wildcard), sites) in &rows {
            // The group's own enum, so `[covered/total]` and `missing:` name
            // variants that exist on the type being matched.
            let set = set_for(variants);
            let key = group_label(variants, *wildcard, opts, set.len(), &set);
            if prefixed {
                row!(
                    ctx.out,
                    "kind" => "group",
                    "enum" => enum_name,
                    "variants" => key,
                    "sites" => format!("{} site(s)", sites.len()),
                );
            } else {
                row!(
                    ctx.out,
                    "kind" => "group",
                    "variants" => key,
                    "sites" => format!("{} site(s)", sites.len()),
                );
            }
            print_group_sites(ctx, sites);
        }
    }
    (all_sites.len(), rows.len())
}

/// One `enum-coverage` output row: a partial site, its gap score, and the
/// variants it leaves uncovered.
struct Row<'s> {
    gap: f64,
    site: &'s Site,
    missing: Vec<String>,
    /// Variant count of the definition *this site* dispatches on, which is not
    /// necessarily the union when two enums in the tree share a name.
    total: usize,
}

/// Print one `enum-coverage` row (with kind/SEALED tags, optional enum-name
/// prefix in --all mode, and the optional --context snippet).
///
/// `compact` drops the covered/missing variant lists from the row. On a wide
/// enum those two columns repeat nearly the whole variant set on every site —
/// a 19-variant enum with 37 sites spent thousands of tokens restating what a
/// single per-enum header line says once.
fn print_coverage_row(
    ctx: &AnalysisCtx,
    r: &Row,
    sealed: bool,
    prefixed: bool,
    enum_name: &str,
    compact: bool,
) {
    let total = r.total;
    let mut tag = if r.site.trait_routed {
        " (catchall→method; likely false positive)".to_string()
    } else if r.site.is_macro {
        " (matches!)".to_string()
    } else if r.site.is_if_chain {
        " (if-chain)".to_string()
    } else {
        String::new()
    };
    if sealed {
        tag.push_str(" SEALED");
    }
    let context = format!("{}{}", r.site.context, tag);
    let covered = format!("{}/{}", r.site.variants.len(), total);
    let at = site_cell(&r.site.file, r.site.line);
    match (prefixed, compact) {
        (true, true) => row!(
            ctx.out,
            "enum" => enum_name,
            "gap" => r.gap,
            "covered" => covered,
            "at" => at,
            "context" => context,
        ),
        (true, false) => row!(
            ctx.out,
            "enum" => enum_name,
            "gap" => r.gap,
            "covered" => covered,
            "variants" => r.site.variants.clone(),
            "missing" => r.missing.clone(),
            "at" => at,
            "context" => context,
        ),
        (false, true) => row!(
            ctx.out,
            "gap" => r.gap,
            "covered" => covered,
            "at" => at,
            "context" => context,
        ),
        (false, false) => row!(
            ctx.out,
            "gap" => r.gap,
            "covered" => covered,
            "variants" => r.site.variants.clone(),
            "missing" => r.missing.clone(),
            "at" => at,
            "context" => context,
        ),
    }
}

/// Print one group's indented site lines (with kind tag and optional
/// --context snippet).
fn print_group_sites(ctx: &AnalysisCtx, sites: &[&Site]) {
    for s in sites {
        let tag = if s.is_macro {
            " (matches!)"
        } else if s.is_if_chain {
            " (if-chain)"
        } else {
            ""
        };
        row!(
            ctx.out,
            "context" => format!("  {}{}", s.context, tag),
            "at" => site_cell(&s.file, s.line),
        );
    }
}

/// Display label of one variant-set group: the covered variants, wildcard
/// marker, and optional `[covered/total]` / `missing:` annotations.
fn group_label(
    variants: &[String],
    wildcard: bool,
    opts: ScanOpts,
    total: usize,
    variant_names: &[String],
) -> String {
    let mut key = format!("{}{}", variants.join(","), if wildcard { " | _" } else { "" });
    if opts.rank_by_gap && total > 0 {
        key = format!("[{}/{}] {}", variants.len(), total, key);
    }
    if opts.show_missing && total > 0 {
        let miss = missing_variants(variants, variant_names);
        let miss = if miss.is_empty() {
            "(none)".to_string()
        } else {
            miss.join(",")
        };
        key = format!("{}\tmissing: {}", key, miss);
    }
    key
}

/// True if any definition of `enum_name` carries the in-source contract
/// marker `unruster: sealed` in its doc comments. Sealed enums must never
/// appear in partial dispatch — `enum-coverage` / `catch-all-arms` tag their
/// findings SEALED and `audit` treats them as highest severity. The marker
/// lives with the code; there is no config file.
pub(crate) fn enum_sealed(files: &[ParsedFile], enum_name: &str) -> bool {
    struct V<'a> {
        target: &'a str,
        sealed: bool,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_item_enum(&mut self, e: &'ast syn::ItemEnum) {
            if e.ident == self.target
                && e.attrs
                    .iter()
                    .filter_map(doc_text)
                    .any(|d| d.contains("unruster: sealed"))
            {
                self.sealed = true;
            }
        }
    }
    let mut v = V {
        target: enum_name,
        sealed: false,
    };
    for f in files {
        v.visit_file(&f.ast);
    }
    v.sealed
}

/// Knobs for an `enum-coverage` scan.
#[derive(Default, Clone, Copy)]
pub struct CoverageOpts {
    /// Drop rows whose `_` arm calls a method on the scrutinee.
    pub hide_trait_routed: bool,
    /// Keep only sites missing at most N variants. `Some(1)` isolates the
    /// "forgot exactly one" shape — the highest-yield subset, and previously
    /// only reachable by piping the rows through `awk -F'\t' '{split($5,a,",")…}'`.
    pub max_missing: Option<usize>,
    /// Drop the covered/missing variant columns and print one header line per
    /// enum instead.
    pub compact: bool,
    /// Instead of per-site rows, print one row per enum: how many partial sites
    /// it has and its worst gap. Replaces the `awk | sort | uniq -c | sort -rn`
    /// pipeline for "which enum should I look at first".
    pub rank_enums: bool,
    /// Skip enums with fewer than this many variants. On a two-variant enum,
    /// "covers 1 of 2" is the definition of an if/else — `is_playing`,
    /// `is_animating`, `is_cage_edit_for` are predicates, not partial dispatch.
    /// Nine of fifteen rows on a real audit were this shape, and every one was
    /// waived. Sweeps (`--all`) default to 3; naming an enum explicitly means
    /// "tell me about *this* one", so that path keeps the floor at 0.
    pub min_variants: usize,
}

/// `enum-coverage <Enum>` — synthesis of the partial-enumeration defect class.
/// One row per *partial* match / `matches!` site (exhaustive sites are
/// compiler-protected and hidden), sorted by gap_score = covered/total
/// descending. The top rows — predicates that cover almost every variant —
/// are the sites most likely to silently mis-bind a newly-added variant.
pub fn run_enum_coverage(
    ctx: &AnalysisCtx,
    target: Option<&str>,
    opts: CoverageOpts,
) -> anyhow::Result<usize> {
    match target {
        Some(enum_name) => {
            let variant_names = variant_names_of(ctx.files, enum_name);
            if variant_names.is_empty() {
                let summary_line = || {
                    ctx.out.summary(&format!(
                        "(0 partial site(s) on `{}`; 0 total variant(s); exhaustive sites hidden)",
                        enum_name
                    ));
                };
                if ctx.idx.knows_name(enum_name) {
                    ctx.out.note(&format!(
                        "note: `{}` is named in the tree but no enum definition with variants \
                         was found under --scope; nothing to score",
                        enum_name
                    ));
                    summary_line();
                    return Ok(0);
                }
                warn_unknown_target("enum", enum_name);
                summary_line();
                return Err(TargetNotFound::err("enum", enum_name));
            }
            let scan = coverage_one(ctx, enum_name, &variant_names, opts, false);
            ctx.out.summary(&format!(
                "({} partial site(s) on `{}`; {} total variant(s); exhaustive sites hidden{}{}{}{}; explain: partial-enumeration)",
                scan.shown,
                enum_name,
                variant_names.len(),
                if opts.hide_trait_routed {
                    format!("; {} trait-routed catch-all(s) hidden", scan.hidden)
                } else {
                    String::new()
                },
                gap_filter_note(opts, scan.filtered_by_gap),
                ctx.waived_note(scan.waived),
                if scan.sealed_rows > 0 {
                    format!("; {} on a SEALED enum", scan.sealed_rows)
                } else {
                    String::new()
                }
            ));
            Ok(scan.shown)
        }
        // `--all`: every enum in the index; rows gain a leading enum column.
        None => {
            let mut shown = 0usize;
            let mut hidden = 0usize;
            let mut filtered = 0usize;
            let mut waived = 0usize;
            let mut small_enums = 0usize;
            let mut sealed_rows = 0usize;
            let mut scanned = 0usize;
            // `--rank-enums` needs every enum's totals before it can order
            // them, so it collects instead of streaming.
            let mut ranked: Vec<(String, usize, f64, bool)> = Vec::new();
            for name in ctx.idx.enum_names() {
                let variant_names = variant_names_of(ctx.files, &name);
                if variant_names.is_empty() {
                    continue;
                }
                if variant_names.len() < opts.min_variants {
                    small_enums += 1;
                    continue;
                }
                scanned += 1;
                let scan = coverage_one(ctx, &name, &variant_names, opts, true);
                shown += scan.shown;
                hidden += scan.hidden;
                filtered += scan.filtered_by_gap;
                waived += scan.waived;
                sealed_rows += scan.sealed_rows;
                if opts.rank_enums && scan.shown > 0 {
                    ranked.push((name, scan.shown, scan.worst_gap, scan.sealed_rows > 0));
                }
            }
            if opts.rank_enums {
                // Most partial sites first: the enum with the widest spread of
                // disagreeing dispatch sites is where a new variant does the
                // most damage.
                ranked.sort_by(|a, b| {
                    b.1.cmp(&a.1)
                        .then_with(|| b.2.total_cmp(&a.2))
                        .then_with(|| a.0.cmp(&b.0))
                });
                for (name, sites, worst, sealed) in &ranked {
                    row!(
                        ctx.out,
                        "enum" => name.clone(),
                        "partial_sites" => *sites,
                        "worst_gap" => *worst,
                        "sealed" => *sealed,
                    );
                }
            }
            ctx.out.summary(&format!(
                "({} partial site(s) across {} enum(s); --all; exhaustive sites hidden{}{}{}{}{}; explain: partial-enumeration)",
                shown,
                scanned,
                if opts.hide_trait_routed {
                    format!("; {} trait-routed catch-all(s) hidden", hidden)
                } else {
                    String::new()
                },
                gap_filter_note(opts, filtered),
                ctx.waived_note(waived),
                if small_enums > 0 {
                    format!(
                        "; {} enum(s) with <{} variants skipped (a 1-of-2 `matches!` is \
                         an if/else, not partial dispatch — `--min-variants 0` to include)",
                        small_enums, opts.min_variants
                    )
                } else {
                    String::new()
                },
                if sealed_rows > 0 {
                    format!("; {} on SEALED enums", sealed_rows)
                } else {
                    String::new()
                }
            ));
            Ok(shown)
        }
    }
}

/// Say what `--max-missing` removed. A filter that silently shrinks the result
/// set reads as a clean codebase.
fn gap_filter_note(opts: CoverageOpts, filtered: usize) -> String {
    match opts.max_missing {
        Some(n) if filtered > 0 => format!(
            "; {} site(s) hidden by --max-missing {} (drop the flag to see them)",
            filtered, n
        ),
        Some(n) => format!("; --max-missing {}", n),
        None => String::new(),
    }
}

/// Outcome of scoring one enum.
struct CoverageScan {
    shown: usize,
    /// Rows dropped as trait-routed catch-alls.
    hidden: usize,
    /// Rows dropped by `--max-missing`.
    filtered_by_gap: usize,
    /// Rows retired by an in-source waiver.
    waived: usize,
    sealed_rows: usize,
    /// Highest coverage ratio among shown rows — the closest-to-exhaustive
    /// site, which is the one a new variant most likely mis-binds.
    worst_gap: f64,
}

/// Score one enum's partial sites and print its rows. With `prefixed`
/// (--all mode) each row carries a leading enum-name column.
fn coverage_one(
    ctx: &AnalysisCtx,
    enum_name: &str,
    variant_names: &[String],
    opts: CoverageOpts,
    prefixed: bool,
) -> CoverageScan {
    let summary = ctx.summary;
    // Per-definition variant sets: with two same-named enums in the tree, a
    // site is scored against the one it actually dispatches on.
    let sets = variant_sets_of(ctx.files, enum_name);
    let total = variant_names.len();
    let sealed = enum_sealed(ctx.files, enum_name);

    // matches!() and `==`-if-chains are guaranteed-supported here — both are
    // primary vectors for this defect, so enum-coverage always includes them.
    let mut all_sites = collect_sites(ctx.files, enum_name, variant_names, true, true, ctx.spans);
    ctx.retain_changed(&mut all_sites, |s| &s.file);
    // Unkeyed pass: `ok(enum-coverage)` retires the whole site. Waivers naming
    // one variant are applied per-row below, once `missing` is known.
    let mut waived = ctx.retain_unsuppressed("enum-coverage", &mut all_sites, |s| {
        crate::suppress::Site::new(s.file.as_str(), s.line)
    });

    // The denominator is per-site: `total_for` resolves which same-named enum
    // this site dispatches on. With one definition (the usual case) it is just
    // that enum's variant count.
    let owned_union = variant_names.to_vec();
    let set_of = |s: &Site| -> Vec<String> {
        definition_for(&sets, &s.variants)
            .cloned()
            .unwrap_or_else(|| owned_union.clone())
    };

    // One row per site; keep only partials (covered < its own enum's total).
    let mut hidden_trait_routed = 0usize;
    let mut filtered_by_gap = 0usize;
    let mut rows: Vec<Row> = all_sites
        .iter()
        .filter(|s| s.variants.len() < set_of(s).len())
        .filter(|s| {
            // A catch-all that routes through a method call on the scrutinee is
            // structurally safe (a new variant must implement the trait method).
            // With the flag set, drop those rows; count them for the summary.
            if opts.hide_trait_routed && s.trait_routed {
                hidden_trait_routed += 1;
                false
            } else {
                true
            }
        })
        .filter(|s| match opts.max_missing {
            Some(n) if set_of(s).len() - s.variants.len() > n => {
                filtered_by_gap += 1;
                false
            }
            _ => true,
        })
        .map(|s| {
            let set = set_of(s);
            Row {
                gap: s.variants.len() as f64 / set.len() as f64,
                site: s,
                missing: missing_variants(&s.variants, &set),
                total: set.len(),
            }
        })
        .collect();
    // Variant-level waivers: `ok(enum-coverage/NodeContent::Group)` says this
    // one omission is deliberate. Drop the waived variants from `missing`; a
    // row whose every gap is accounted for is no longer a finding. Two waivers
    // can therefore jointly clear a site, which one all-or-nothing match on the
    // row could not express.
    if !ctx.suppressions.is_empty() {
        let before = rows.len();
        rows.retain_mut(|r| {
            r.missing.retain(|v| {
                let qualified = format!("{}::{}", enum_name, v);
                !ctx.suppressions.matches(
                    "enum-coverage",
                    crate::suppress::Site::keyed(r.site.file.as_str(), r.site.line, &qualified),
                )
            });
            !r.missing.is_empty()
        });
        waived += before - rows.len();
    }
    // Highest coverage ratio (smallest gap to full) first — loudest signal on
    // top. The denominator `total` is shared, so covered-count ordering is
    // exact; `gap` is computed only for display.
    rows.sort_by(|a, b| {
        b.site
            .variants
            .len()
            .cmp(&a.site.variants.len())
            .then_with(|| a.site.file.cmp(&b.site.file))
            .then_with(|| a.site.line.cmp(&b.site.line))
    });

    // `--compact` drops the per-row variant lists, so the variant set has to be
    // stated once somewhere or the rows become unreadable.
    if !summary && opts.compact && !opts.rank_enums && !rows.is_empty() {
        ctx.out.line(&format!(
            "# {} [{} variants: {}]",
            enum_name,
            total,
            variant_names.join(",")
        ));
    }
    // A name shared by two enums is worth saying out loud: it is the reason a
    // reader might otherwise see an exhaustive match reported as partial.
    if !summary && sets.len() > 1 && !rows.is_empty() {
        ctx.out.note(&format!(
            "(note: `{}` names {} distinct enums in this tree ({}); each site is \
             scored against the definition it dispatches on)",
            enum_name,
            sets.len(),
            sets.iter()
                .map(|s| format!("{} variants", s.len()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !summary && !opts.rank_enums {
        let today = crate::suppress::Date::today();
        for r in &rows {
            print_coverage_row(ctx, r, sealed, prefixed, enum_name, opts.compact);
            // Suggest the narrowest waiver that would retire the row: one per
            // missing variant, so accepting all of them is the same as
            // declaring the site's coverage intentional.
            for v in &r.missing {
                ctx.suggest(
                    "enum-coverage",
                    Some(&format!("{}::{}", enum_name, v)),
                    today,
                );
            }
        }
    }
    CoverageScan {
        shown: rows.len(),
        hidden: hidden_trait_routed,
        filtered_by_gap,
        waived,
        sealed_rows: if sealed { rows.len() } else { 0 },
        worst_gap: rows.first().map(|r| r.gap).unwrap_or(0.0),
    }
}
