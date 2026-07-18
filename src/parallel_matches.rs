use std::collections::{BTreeMap, HashSet};

use syn::visit::{self, Visit};

use crate::ast::{fn_span, trait_fn_span, enum_variant_of_path, line_of, type_short, ScopeTracker};
use crate::context::{warn_unknown_target, AnalysisCtx, TargetNotFound};
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
        self.scope
            .enter_fn(i.sig.ident.to_string(), trait_fn_span(i));
        visit::visit_trait_item_fn(self, i);
        self.scope.leave_fn();
    }

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
    struct V<'a> {
        target: &'a str,
        out: Vec<String>,
    }
    impl<'ast, 'a> Visit<'ast> for V<'a> {
        fn visit_item_enum(&mut self, e: &'ast syn::ItemEnum) {
            if e.ident == self.target {
                self.out
                    .extend(e.variants.iter().map(|v| v.ident.to_string()));
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
                    eprintln!(
                        "note: `{}` is named in the tree but no enum definition with variants \
                         was found under --scope; nothing to scan",
                        enum_name
                    );
                    eprintln!(
                        "(0 match site(s) across 0 variant-set group(s) on `{}`)",
                        enum_name
                    );
                    return Ok(0);
                }
                warn_unknown_target("enum", enum_name);
                eprintln!(
                    "(0 match site(s) across 0 variant-set group(s) on `{}`)",
                    enum_name
                );
                return Err(TargetNotFound::err("enum", enum_name));
            }
            let (sites, groups) = scan_groups(ctx, enum_name, &variant_names, opts, false);
            eprintln!(
                "({} match site(s) across {} variant-set group(s) on `{}`{})",
                sites,
                groups,
                enum_name,
                if opts.partial_only {
                    "; exhaustive groups hidden"
                } else {
                    ""
                }
            );
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
            eprintln!(
                "({} match site(s) across {} group(s) on {} enum(s); --all{})",
                total_sites,
                total_groups,
                scanned,
                if opts.partial_only {
                    "; exhaustive groups hidden"
                } else {
                    ""
                }
            );
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
    let total = variant_names.len();
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

    // A group is "exhaustive" when it names every variant of the enum — those
    // are compiler-protected, so `--hide-exhaustive` drops them.
    let is_exhaustive = |variants: &[String]| total > 0 && variants.len() == total;
    if opts.partial_only {
        rows.retain(|((variants, _), _)| !is_exhaustive(variants));
    }

    // Default ordering: by group size descending (parallel-shot first). With
    // --rank-by-gap, order by coverage ratio descending instead — a 7/8 group
    // (one new variant silently mis-binds) is a louder defect signal than a 1/8.
    if opts.rank_by_gap && total > 0 {
        // Every group shares the denominator `total`, so ordering by
        // covered/total is exactly ordering by covered-count — no floats.
        rows.sort_by(|a, b| {
            b.0 .0
                .len()
                .cmp(&a.0 .0.len())
                .then_with(|| b.1.len().cmp(&a.1.len()))
                .then_with(|| a.0.cmp(&b.0))
        });
    } else {
        rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    }

    if !summary {
        for ((variants, wildcard), sites) in &rows {
            let mut key = format!(
                "{}{}",
                variants.join(","),
                if *wildcard { " | _" } else { "" }
            );
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
            if prefixed {
                println!("group\t{}\t{}\t{} site(s)", enum_name, key, sites.len());
            } else {
                println!("group\t{}\t{} site(s)", key, sites.len());
            }
            for s in sites {
                let tag = if s.is_macro {
                    " (matches!)"
                } else if s.is_if_chain {
                    " (if-chain)"
                } else {
                    ""
                };
                println!("  {}{}\t{}:{}", s.context, tag, s.file, s.line);
                ctx.print_context(&s.file, s.line);
            }
        }
    }
    (all_sites.len(), rows.len())
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
            if e.ident != self.target {
                return;
            }
            for a in &e.attrs {
                if !a.path().is_ident("doc") {
                    continue;
                }
                if let syn::Meta::NameValue(nv) = &a.meta {
                    if let syn::Expr::Lit(l) = &nv.value {
                        if let syn::Lit::Str(s) = &l.lit {
                            if s.value().contains("unruster: sealed") {
                                self.sealed = true;
                            }
                        }
                    }
                }
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

/// `enum-coverage <Enum>` — synthesis of the partial-enumeration defect class.
/// One row per *partial* match / `matches!` site (exhaustive sites are
/// compiler-protected and hidden), sorted by gap_score = covered/total
/// descending. The top rows — predicates that cover almost every variant —
/// are the sites most likely to silently mis-bind a newly-added variant.
pub fn run_enum_coverage(
    ctx: &AnalysisCtx,
    target: Option<&str>,
    hide_trait_routed: bool,
) -> anyhow::Result<usize> {
    match target {
        Some(enum_name) => {
            let variant_names = variant_names_of(ctx.files, enum_name);
            if variant_names.is_empty() {
                let summary_line = || {
                    eprintln!(
                        "(0 partial site(s) on `{}`; 0 total variant(s); exhaustive sites hidden)",
                        enum_name
                    );
                };
                if ctx.idx.knows_name(enum_name) {
                    eprintln!(
                        "note: `{}` is named in the tree but no enum definition with variants \
                         was found under --scope; nothing to score",
                        enum_name
                    );
                    summary_line();
                    return Ok(0);
                }
                warn_unknown_target("enum", enum_name);
                summary_line();
                return Err(TargetNotFound::err("enum", enum_name));
            }
            let (shown, hidden, sealed_rows) =
                coverage_one(ctx, enum_name, &variant_names, hide_trait_routed, false);
            eprintln!(
                "({} partial site(s) on `{}`; {} total variant(s); exhaustive sites hidden{}{}; explain: partial-enumeration)",
                shown,
                enum_name,
                variant_names.len(),
                if hide_trait_routed {
                    format!("; {} trait-routed catch-all(s) hidden", hidden)
                } else {
                    String::new()
                },
                if sealed_rows > 0 {
                    format!("; {} on a SEALED enum", sealed_rows)
                } else {
                    String::new()
                }
            );
            Ok(shown)
        }
        // `--all`: every enum in the index; rows gain a leading enum column.
        None => {
            let mut shown = 0usize;
            let mut hidden = 0usize;
            let mut sealed_rows = 0usize;
            let mut scanned = 0usize;
            for name in ctx.idx.enum_names() {
                let variant_names = variant_names_of(ctx.files, &name);
                if variant_names.is_empty() {
                    continue;
                }
                scanned += 1;
                let (s, h, sl) = coverage_one(ctx, &name, &variant_names, hide_trait_routed, true);
                shown += s;
                hidden += h;
                sealed_rows += sl;
            }
            eprintln!(
                "({} partial site(s) across {} enum(s); --all; exhaustive sites hidden{}{}; explain: partial-enumeration)",
                shown,
                scanned,
                if hide_trait_routed {
                    format!("; {} trait-routed catch-all(s) hidden", hidden)
                } else {
                    String::new()
                },
                if sealed_rows > 0 {
                    format!("; {} on SEALED enums", sealed_rows)
                } else {
                    String::new()
                }
            );
            Ok(shown)
        }
    }
}

/// Score one enum's partial sites and print its rows. With `prefixed`
/// (--all mode) each row carries a leading enum-name column. Returns
/// (rows shown, trait-routed rows hidden, rows on a sealed enum).
fn coverage_one(
    ctx: &AnalysisCtx,
    enum_name: &str,
    variant_names: &[String],
    hide_trait_routed: bool,
    prefixed: bool,
) -> (usize, usize, usize) {
    let summary = ctx.summary;
    let total = variant_names.len();
    let sealed = enum_sealed(ctx.files, enum_name);

    // matches!() and `==`-if-chains are guaranteed-supported here — both are
    // primary vectors for this defect, so enum-coverage always includes them.
    let mut all_sites = collect_sites(ctx.files, enum_name, variant_names, true, true, ctx.spans);
    ctx.retain_changed(&mut all_sites, |s| &s.file);

    // One row per site; keep only partials (covered < total).
    struct Row<'s> {
        gap: f64,
        site: &'s Site,
        missing: Vec<String>,
    }
    let mut hidden_trait_routed = 0usize;
    let mut rows: Vec<Row> = all_sites
        .iter()
        .filter(|s| s.variants.len() < total)
        .filter(|s| {
            // A catch-all that routes through a method call on the scrutinee is
            // structurally safe (a new variant must implement the trait method).
            // With the flag set, drop those rows; count them for the summary.
            if hide_trait_routed && s.trait_routed {
                hidden_trait_routed += 1;
                false
            } else {
                true
            }
        })
        .map(|s| Row {
            gap: s.variants.len() as f64 / total as f64,
            site: s,
            missing: missing_variants(&s.variants, variant_names),
        })
        .collect();
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

    if !summary {
        for r in &rows {
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
            let body = format!(
                "{:.2}\t{}/{}\t{}\t{}\t{}:{}\t{}{}",
                r.gap,
                r.site.variants.len(),
                total,
                r.site.variants.join(","),
                r.missing.join(","),
                r.site.file,
                r.site.line,
                r.site.context,
                tag
            );
            if prefixed {
                println!("{}\t{}", enum_name, body);
            } else {
                println!("{}", body);
            }
            ctx.print_context(&r.site.file, r.site.line);
        }
    }
    let sealed_rows = if sealed { rows.len() } else { 0 };
    (rows.len(), hidden_trait_routed, sealed_rows)
}
