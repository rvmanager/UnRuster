use std::collections::{BTreeMap, BTreeSet, VecDeque};

use syn::visit::{self, Visit};

use crate::ast::{line_of, path_to_string, print_grouped_counts, scope_visits, ScopeTracker};
use crate::context::{warn_unknown_target, AnalysisCtx, Confidence, GroupBy, TargetNotFound};

use crate::index::NameIndex;
use crate::parse::{display_path, ParsedFile};
use crate::semantic::{Semantic, UseMap};
use crate::emit::{row, site};

#[derive(Debug, Clone)]
pub(crate) struct CallSite {
    pub(crate) file: String,
    pub(crate) line: usize,
    pub(crate) caller: String,
    pub(crate) target: String,
    /// Target as resolved through the calling file's `use` map, if different
    /// from `target`. Used as a secondary key for `matches_target`. Approximate.
    pub(crate) target_resolved: Option<String>,
    /// The callee is a bare name bound *locally* at the call site — a `let`
    /// closure, a closure parameter, or a fn parameter — so it cannot be the
    /// item that shares the name. `let grow = |lo, hi| …; grow(a)` calls the
    /// closure, and attributing it to `hull::grow` misled a real session.
    /// Best-effort: bindings introduced by `match` arms or `if let` patterns
    /// are not tracked.
    shadowed: bool,
    /// The receiver was literally `self`. Enough to attribute `self.push(…)`
    /// inside `impl ArithVisitor` to `ArithVisitor::push` without inferring a
    /// type — and without mistaking `self.sites.push(…)`, whose receiver is a
    /// field, for the same thing.
    pub(crate) receiver_is_self: bool,
}

struct CallVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    sites: Vec<CallSite>,
    /// Local-binding scopes, innermost last. Names land here when their
    /// binding is visited, so a call *before* the `let` that shadows it still
    /// reads as a call to the item — which is what the compiler resolves too.
    locals: Vec<BTreeSet<String>>,
    /// Parameter names from the most recent signature, claimed by the fn
    /// body's block when it opens.
    pending_params: BTreeSet<String>,
}

impl<'a> CallVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing_with_toplevel()
    }

    fn local_shadows(&self, name: &str) -> bool {
        self.locals.iter().any(|frame| frame.contains(name))
    }

    fn record(&mut self, target: String, line: usize, shadowed: bool) {
        self.record_full(target, line, shadowed, false);
    }

    fn record_full(&mut self, target: String, line: usize, shadowed: bool, on_self: bool) {
        self.sites.push(CallSite {
            file: self.file.to_string(),
            line,
            caller: self.enclosing(),
            target,
            target_resolved: None,
            shadowed,
            receiver_is_self: on_self,
        });
    }
}

/// The names a signature's parameters bind, for a visitor that must know which
/// bare names the body about to open shadows.
///
/// Returned rather than extended into place: a body-less trait signature must
/// not leak its params into the next body that happens to open, and an
/// overwrite makes that the only possible behaviour.
pub(crate) fn params_of(s: &syn::Signature) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for input in &s.inputs {
        if let syn::FnArg::Typed(t) = input {
            pat_idents(&t.pat, &mut out);
        }
    }
    out
}

/// A path handed to another call *as a value* — `.map(arg_shape)`,
/// `sort_by_key(width_of)`, `filter(is_gating)`.
///
/// These are uses with no call expression anywhere, so nothing recorded them
/// and every usage command reported a confident zero. `contract_drift`'s own
/// `arg_shape` has two `.map(arg_shape)` uses and answered "0 caller(s);
/// nothing to infer a contract from" — the same shape of wrong answer as a
/// qualified query that matches no literal path. `dead-code` was never fooled,
/// because it collects bare identifiers rather than call sites; that is the
/// divergence this closes.
///
/// A leading-uppercase segment is a constructor, not a fn — `.map(Some)` and
/// `.map(Ok)` are the common case and must not be recorded as uses of anything.
pub(crate) fn fn_ref_path(e: &syn::Expr) -> Option<String> {
    let syn::Expr::Path(p) = crate::ast::peel_grouping(e) else {
        return None;
    };
    let last = p.path.segments.last()?;
    let name = last.ident.to_string();
    if !name.starts_with(|c: char| c.is_lowercase() || c == '_') {
        return None;
    }
    Some(path_to_string(&p.path))
}

/// Every name a pattern binds, recursively. Used to learn which bare names a
/// `let`, closure head, or fn signature shadows.
pub(crate) fn pat_idents(p: &syn::Pat, out: &mut BTreeSet<String>) {
    match p {
        syn::Pat::Ident(i) => {
            out.insert(i.ident.to_string());
            if let Some((_, sub)) = &i.subpat {
                pat_idents(sub, out);
            }
        }
        syn::Pat::Type(t) => pat_idents(&t.pat, out),
        syn::Pat::Reference(r) => pat_idents(&r.pat, out),
        syn::Pat::Paren(p) => pat_idents(&p.pat, out),
        syn::Pat::Tuple(t) => t.elems.iter().for_each(|e| pat_idents(e, out)),
        syn::Pat::TupleStruct(t) => t.elems.iter().for_each(|e| pat_idents(e, out)),
        syn::Pat::Struct(s) => s.fields.iter().for_each(|f| pat_idents(&f.pat, out)),
        syn::Pat::Slice(s) => s.elems.iter().for_each(|e| pat_idents(e, out)),
        syn::Pat::Or(o) => o.cases.iter().for_each(|c| pat_idents(c, out)),
        _ => {}
    }
}

impl<'ast, 'a> Visit<'ast> for CallVisitor<'a> {
    scope_visits!(item_mod, item_impl, item_fn, impl_item_fn, trait_item_fn);





    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            let target = path_to_string(&p.path);
            // Only a bare single-segment name can be captured by a local
            // binding; `hull::grow(…)` always names the item.
            let shadowed =
                p.path.segments.len() == 1 && self.local_shadows(&target);
            self.record(target, line_of(&e.func), shadowed);
        }
        for a in &e.args {
            if let Some(t) = fn_ref_path(a) {
                self.record(t, line_of(a), false);
            }
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let target = format!(".{}", e.method);
        let on_self = matches!(crate::ast::peel_grouping(&e.receiver), syn::Expr::Path(p) if p.path.is_ident("self"));
        self.record_full(target, line_of(&e.method), false, on_self);
        for a in &e.args {
            if let Some(t) = fn_ref_path(a) {
                self.record(t, line_of(a), false);
            }
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if let Some(last) = m.path.segments.last() {
            let target = format!("{}!", path_to_string(&m.path));
            self.record(target, line_of(&last.ident), false);
        }
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }

    fn visit_signature(&mut self, s: &'ast syn::Signature) {
        self.pending_params = params_of(s);
        visit::visit_signature(self, s);
    }

    fn visit_block(&mut self, b: &'ast syn::Block) {
        let frame = std::mem::take(&mut self.pending_params);
        self.locals.push(frame);
        visit::visit_block(self, b);
        self.locals.pop();
    }

    fn visit_local(&mut self, l: &'ast syn::Local) {
        // Initializer first: in `let grow = |x| grow(x)` the body's call
        // still names the outer item, exactly as the compiler resolves it.
        visit::visit_local(self, l);
        let mut names = BTreeSet::new();
        pat_idents(&l.pat, &mut names);
        if let Some(frame) = self.locals.last_mut() {
            frame.extend(names);
        }
    }

    fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
        let mut frame = BTreeSet::new();
        for p in &c.inputs {
            pat_idents(p, &mut frame);
        }
        self.locals.push(frame);
        visit::visit_expr_closure(self, c);
        self.locals.pop();
    }
}

pub(crate) fn collect_sites(
    files: &[ParsedFile],
    sem: &Semantic,
    index: &NameIndex,
    spans: bool,
) -> Vec<CallSite> {
    let mut all = Vec::new();
    for f in files {
        let mut v = CallVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(spans),
            sites: Vec::new(),
            locals: Vec::new(),
            pending_params: BTreeSet::new(),
        };
        v.visit_file(&f.ast);
        // Resolve each target's head through the file's use-map (approximate).
        if let Some(uses) = sem.uses_for(&f.path) {
            for site in &mut v.sites {
                site.target_resolved = resolve_target_via_uses(&site.target, uses, index);
            }
        }
        all.extend(v.sites);
    }
    all
}

pub(crate) fn resolve_target_via_uses(
    target: &str,
    uses: &UseMap,
    index: &NameIndex,
) -> Option<String> {
    if target.starts_with('.') || target.ends_with('!') {
        return None;
    }
    let segs: Vec<&str> = target.split("::").collect();
    if segs.is_empty() {
        return None;
    }
    let head = segs[0];
    let resolved = uses.resolve(head, index)?;
    if resolved == head {
        return None;
    }
    if segs.len() == 1 {
        Some(resolved)
    } else {
        Some(format!("{}::{}", resolved, segs[1..].join("::")))
    }
}

pub(crate) fn matches_target(call_target: &str, query: &str) -> bool {
    if let Some(name) = query.strip_suffix('!') {
        let Some(target_macro) = call_target.strip_suffix('!') else {
            return false;
        };
        let target_last = crate::ast::last_segment(target_macro);
        let q_last = crate::ast::last_segment(name);
        return target_last == q_last;
    }
    if let Some(method) = query.strip_prefix('.') {
        return call_target == format!(".{}", method);
    }
    if let Some(rest) = query.strip_prefix("::") {
        if call_target.starts_with('.') || call_target.ends_with('!') {
            return false;
        }
        let last = crate::ast::last_segment(rest);
        let target_last = crate::ast::last_segment(call_target);
        return target_last == last;
    }
    if query.contains("::") {
        let trimmed = call_target.strip_suffix('!').unwrap_or(call_target);
        // Whole `::` segments. A plain `ends_with` is a substring test, and
        // `"std::fs::write"` ends with `"s::write"` — so a query for `s::write`
        // claimed both `std::fs::write` calls in a fixture written to prove it
        // did not. `show`'s help already promised this rule.
        return crate::contract_drift::ends_on_segment(trimmed, query);
    }
    let target_last = if let Some(m) = call_target.strip_prefix('.') {
        m
    } else if let Some(m) = call_target.strip_suffix('!') {
        crate::ast::last_segment(m)
    } else {
        crate::ast::last_segment(call_target)
    };
    target_last == query
}

/// Does this site call the item `query` names?
///
/// The one place that answers it, because three commands answered it three
/// ways: `callers` and `contract-drift` disagreed on nearly every qualified
/// target, and `co-call` — which never got the fix at all — reported 0/0/0 for
/// a pair that the bare form scores 1 both + 11 A-only. A call site records the
/// callee *as written*, so a qualified query sees only the sites that spell the
/// path out unless something resolves it to the item first.
pub(crate) struct QueryMatcher<'a> {
    query: &'a str,
    target: Option<&'a crate::index::Defn>,
    key: crate::contract_drift::SearchKey<'a>,
}

impl<'a> QueryMatcher<'a> {
    pub(crate) fn new(index: &'a NameIndex, query: &'a str) -> Self {
        let target = single_indexed_fn(index, query);
        let key = crate::contract_drift::search_key_for(index, query, target);
        Self { query, target, key }
    }

    /// True when the match came from widening a method by name — a lead rather
    /// than a resolution, since the index holds no `Vec::len`.
    pub(crate) fn is_method_widened(&self) -> bool {
        self.key.is_widened() && self.key.form_str().starts_with('.')
    }

    /// The site is this item by construction, not by name coincidence: the
    /// written path names it, or it is `self.name(…)`/`Self::name(…)` inside
    /// the defining impl, or a bare call inside the defining module.
    ///
    /// Kept apart from the tier so a method-name match is not demoted when it
    /// was actually *proved*. `self.push(1)` inside `impl V` is as resolved as
    /// a call gets; only `x.push(1)` on some other receiver is a lead.
    pub(crate) fn is_certain(&self, target: &str, module: &str, on_self: bool) -> bool {
        let Some(d) = self.target else { return false };
        crate::contract_drift::written_names_item(d, target)
            || crate::contract_drift::resolves_on_self(d, target, module, on_self)
            || crate::contract_drift::resolves_locally(d, target, module)
    }

    pub(crate) fn hits(
        &self,
        target: &str,
        resolved: Option<&str>,
        module: &str,
        on_self: bool,
    ) -> bool {
        let written = matches_target(target, self.query)
            || resolved.map(|t| matches_target(t, self.query)).unwrap_or(false);
        let Some(d) = self.target else { return written };
        if self.key.is_widened() {
            // `Type::name(…)` alongside `.name(…)`: an associated fn is called
            // by path, and widening to the method form alone dropped every
            // constructor in the tree.
            if crate::contract_drift::written_names_item(d, target)
                && !crate::contract_drift::out_of_visibility(d, module)
            {
                return true;
            }
            // `self.name(…)` and `Self::name(…)` inside the defining impl.
            // Consulted here as well as on the narrow path: a *unique* method
            // name takes the widened branch and would otherwise never reach it,
            // which is how three `Self::is_wildcard(…)` families stayed
            // invisible while `dead-code` knew they were called.
            if crate::contract_drift::resolves_on_self(d, target, module, on_self) {
                return true;
            }
            return matches_target(target, self.key.form_str())
                && !crate::contract_drift::names_another_item(d, target, resolved)
                && !crate::contract_drift::out_of_visibility(d, module);
        }
        if self.key.is_narrow() {
            return written
                || crate::contract_drift::resolves_locally(d, target, module)
                || crate::contract_drift::resolves_on_self(d, target, module, on_self);
        }
        written
    }
}

/// The one indexed fn a query names, when it names exactly one. `None` for an
/// ambiguous name, a macro, or a bare method form — the cases where resolving
/// to an item is not something this tool can do.
fn single_indexed_fn<'a>(index: &'a NameIndex, query: &str) -> Option<&'a crate::index::Defn> {
    let bare = crate::ast::last_segment(
        query
            .trim_start_matches('.')
            .trim_start_matches("::")
            .trim_end_matches('!'),
    );
    let mut hits = index
        .iter()
        .filter(|d| matches!(d.kind, "fn" | "impl-fn" | "trait-fn") && d.name == bare);
    let first = hits.next()?;
    // Ambiguous by bare name is still resolvable when the query qualifies it.
    if hits.next().is_some() {
        return index
            .iter()
            .find(|d| matches!(d.kind, "fn" | "impl-fn" | "trait-fn") && d.qpath.ends_with(query));
    }
    Some(first)
}

/// True if the index knows of any defined fn/method/etc. that matches the query.
pub(crate) fn query_known(index: &NameIndex, query: &str) -> bool {
    if query.ends_with('!') {
        // Macros aren't in the NameIndex (we only index struct/enum/etc.).
        // Assume known to avoid false alarms.
        return true;
    }
    let last = query
        .trim_start_matches('.')
        .trim_start_matches("::")
        .rsplit("::")
        .next()
        .unwrap_or(query);
    if last.is_empty() {
        return false;
    }
    index
        .iter()
        .any(|d| matches!(d.kind, "fn" | "impl-fn" | "trait-fn") && d.name == last)
        || index.knows_name(last)
}

fn top_module(qpath: &str) -> &str {
    qpath.split("::").next().unwrap_or(qpath)
}

/// Confidence of one call-site match against the query:
/// - resolved through the calling file's use-map → `resolved`
/// - qualified query (`Type::method`) matching a qualified call path → `resolved`
/// - bare-name query whose last segment has exactly one defn in the tree → `resolved`
/// - plain last-segment match → `heuristic`
fn site_confidence(s: &CallSite, query: &str, unique_name: bool) -> Confidence {
    confidence_of(s.target_resolved.as_deref(), s.shadowed, query, unique_name)
}

/// [`site_confidence`], demoted when the match came from widening a method by
/// name. See the note where `method_widened` is set.
fn widened_confidence(
    s: &CallSite,
    query: &str,
    unique_name: bool,
    method_widened: bool,
    matcher: &QueryMatcher,
) -> Confidence {
    // A name captured by a local `let` or closure parameter is not this item,
    // however well the module and the spelling line up — the compiler resolves
    // the binding. Certainty must not outrank shadowing.
    if !s.shadowed
        && matcher.is_certain(
            &s.target,
            crate::config_drift::module_of(&s.caller),
            s.receiver_is_self,
        )
    {
        return Confidence::Resolved;
    }
    if method_widened && s.target.starts_with('.') {
        return Confidence::Heuristic;
    }
    site_confidence(s, query, unique_name)
}

/// [`site_confidence`] over the three facts it actually reads, so a command
/// that collects its own richer call sites gets the same tiers rather than a
/// second opinion. One rule, two callers — a `contract-drift` that disagreed
/// with `callers` about which sites are trustworthy would be worse than one
/// that never reported confidence at all.
pub(crate) fn confidence_of(
    target_resolved: Option<&str>,
    shadowed: bool,
    query: &str,
    unique_name: bool,
) -> Confidence {
    // A callee shadowed by a local binding cannot be the item the query
    // names — no promotion applies, however unique or qualified the query.
    if shadowed {
        return Confidence::Heuristic;
    }
    let via_resolved = target_resolved
        .map(|t| matches_target(t, query))
        .unwrap_or(false);
    if via_resolved || query.contains("::") || unique_name {
        Confidence::Resolved
    } else {
        Confidence::Heuristic
    }
}

/// True when the query's last segment names exactly one fn/method definition
/// in the tree — a bare-name match then can't be a same-named impostor.
pub(crate) fn query_unique(index: &NameIndex, query: &str) -> bool {
    let last = query
        .trim_start_matches('.')
        .trim_start_matches("::")
        .trim_end_matches('!')
        .rsplit("::")
        .next()
        .unwrap_or(query);
    index
        .iter()
        .filter(|d| matches!(d.kind, "fn" | "impl-fn" | "trait-fn") && d.name == last)
        .count()
        == 1
}

pub fn run_callers(
    ctx: &AnalysisCtx,
    query: &str,
    transitive: bool,
    depth: Option<usize>,
    by: Option<GroupBy>,
    min_confidence: Option<Confidence>,
) -> anyhow::Result<usize> {
    let files = ctx.files;
    let index = ctx.idx;
    let sem = ctx.sem;
    let summary = ctx.summary;
    let known = query_known(index, query);
    if !known {
        ctx.warn_unknown("fn, method, or macro", query);
    }

    let sites = collect_sites(files, sem, index, ctx.spans);

    let unique_name = query_unique(index, query);
    let mut direct: Vec<&CallSite> = sites
        .iter()
        .filter(|s| {
            matches_target(&s.target, query)
                || s.target_resolved
                    .as_deref()
                    .map(|t| matches_target(t, query))
                    .unwrap_or(false)
        })
        .collect();
    if let Some(min) = min_confidence {
        direct.retain(|s| site_confidence(s, query, unique_name) >= min);
    }
    ctx.retain_changed(&mut direct, |s| &s.file);

    // Resolve the query to the *item* where that is unambiguous. Without this
    // a qualified query sees only the sites that spell the path out.
    let matcher = QueryMatcher::new(index, query);
    let method_widened = matcher.is_method_widened();
    let widened: Vec<&CallSite> = sites
        .iter()
        .filter(|s| {
            matcher.hits(
                &s.target,
                s.target_resolved.as_deref(),
                crate::config_drift::module_of(&s.caller),
                s.receiver_is_self,
            )
        })
        .collect();
    if widened.len() > direct.len() {
        direct = widened;
        if let Some(min) = min_confidence {
            direct.retain(|s| widened_confidence(s, query, unique_name, method_widened, &matcher) >= min);
        }
        ctx.retain_changed(&mut direct, |s| &s.file);
    }
    note_narrower_than_bare(ctx, index, &sites, query, direct.len());
    let local_hits = direct.iter().filter(|s| s.shadowed).count();
    if local_hits > 0 {
        ctx.out.note(&format!(
            "(note: {} of these site(s) call a *local* binding named `{}` (a closure or \
             `let`), not the item — kept at heuristic confidence; `--min-confidence \
             resolved` drops them)",
            local_hits,
            crate::ast::last_segment(query)
        ));
    }

    if !transitive {
        emit_caller_rows(ctx, &direct, by, query, unique_name, method_widened, &matcher);
        let unique = direct
            .iter()
            .map(|s| s.caller.as_str())
            .collect::<BTreeSet<_>>();
        ctx.out.summary(&format!(
            "({} call site(s) across {} caller(s))",
            direct.len(),
            unique.len()
        ));
        if !known && direct.is_empty() {
            return Err(TargetNotFound::err("fn, method, or macro matching", query));
        }
        return Ok(direct.len());
    }

    // Emit transitive callers grouped by depth.
    let mut rows = transitive_callers(&sites, query, depth.unwrap_or(usize::MAX));
    rows.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    if !summary {
        for (caller, d) in &rows {
            row!(ctx.out, "depth" => format!("d{}", d), "caller" => caller.clone());
        }
    }
    ctx.out.summary(&format!(
        "({} direct, {} transitive caller(s); max_depth={})",
        direct.len(),
        rows.len(),
        depth
            .map(|d| d.to_string())
            .unwrap_or_else(|| "∞".to_string())
    ));
    if !known && direct.is_empty() && rows.is_empty() {
        return Err(TargetNotFound::err("fn, method, or macro matching", query));
    }
    Ok(direct.len() + rows.len())
}

/// A qualified query matched fewer sites than its bare name would. Say so.
///
/// Fired only at *zero* until `contract-drift` hit the same wall with a
/// non-zero count: `svg::n` matched the 2 sites that spell the path out and
/// missed the other 162, and a short list is indistinguishable from a complete
/// one. A partial answer that looks whole is the same defect as a zero that
/// looks like an absence — so the threshold is "fewer than the bare name
/// would", not "none at all".
///
/// `Type::method` matches only where the receiver's type could be resolved,
/// which is the APPROXIMATE tier: it misses a receiver reached through a field
/// (`self.idx.similar(…)`), through a method chain, or through a generic. When
/// that happens the command reports `(0 call site(s) across 0 caller(s))` —
/// indistinguishable from a method nobody calls, and the qualified form is
/// exactly what `show` and `outline` hand a reader to paste back in.
///
/// One session ran `callers 'Region::apply'` and `callers 'report::centre'`,
/// got a confident zero from each, and went to `grep` — which found seven real
/// call sites for the first. The zero was defensible; presenting it as the
/// whole answer was not.
fn note_narrower_than_bare(
    ctx: &AnalysisCtx,
    index: &NameIndex,
    sites: &[CallSite],
    query: &str,
    matched: usize,
) {
    let bare = crate::ast::last_segment(query);
    if bare == query || ctx.summary {
        return;
    }
    let n = sites
        .iter()
        .filter(|s| matches_target(&s.target, bare))
        .count();
    if n <= matched {
        return;
    }
    // A *partial* match is only worth reporting when the bare name belongs to
    // one fn: then the sites the qualified form missed are certainly this item,
    // and losing them is losing real callers. When the name is shared —
    // `Document::new` against six `new`s — the narrow match is the correct
    // answer and the extra sites are other types', so the warning would be
    // wallpaper on every `new`, `len` and `push` in the tree.
    if matched > 0 && !query_unique(index, bare) {
        return;
    }
    if matched == 0 {
        ctx.out.answer(&format!(
            "note: no site resolves to `{}`, but {} site(s) call something named \
             `{}` — receiver types are inferred, so a receiver reached through a \
             field or a chain will not match the qualified form. Try `callers {}`.",
            query, n, bare, bare
        ));
    } else {
        ctx.out.answer(&format!(
            "note: `{}` matched {} site(s), but {} site(s) call something named `{}` — a \
             call site records the callee as written, so the qualified form sees only the \
             sites that spell the path out. `callers {}` shows all {}.",
            query,
            matched,
            n,
            bare,
            bare,
            n
        ));
    }
}

/// Breadth-first transitive callers of `query` through the last-segment call
/// graph, each at its minimum depth. BFS reaches every name at its minimum
/// depth on first visit, so each name is expanded exactly once — without the
/// `seen_names` set a cyclic call graph (any recursion) re-enqueued forever
/// and `--transitive` with unlimited depth never terminated.
fn transitive_callers(
    sites: &[CallSite],
    query: &str,
    max_depth: usize,
) -> Vec<(String, usize)> {
    // target_last_name -> set of caller qpaths.
    let mut rev: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for s in sites {
        let last = if let Some(m) = s.target.strip_prefix('.') {
            m.to_string()
        } else if let Some(m) = s.target.strip_suffix('!') {
            crate::ast::last_segment(m).to_string()
        } else {
            crate::ast::last_segment(&s.target).to_string()
        };
        rev.entry(last).or_default().insert(s.caller.clone());
    }

    let seed_last = query
        .trim_start_matches('.')
        .trim_start_matches("::")
        .trim_end_matches('!')
        .rsplit("::")
        .next()
        .unwrap_or(query)
        .to_string();

    let mut visited: BTreeMap<String, usize> = BTreeMap::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    let mut seen_names: BTreeSet<String> = BTreeSet::new();
    seen_names.insert(seed_last.clone());
    queue.push_back((seed_last, 0));

    while let Some((name, d)) = queue.pop_front() {
        if d >= max_depth {
            continue;
        }
        let Some(callers) = rev.get(&name) else {
            continue;
        };
        for caller in callers {
            let caller_last = crate::ast::last_segment(caller).to_string();
            visited.entry(caller.clone()).or_insert(d + 1);
            if d + 1 < max_depth && seen_names.insert(caller_last.clone()) {
                queue.push_back((caller_last, d + 1));
            }
        }
    }
    visited.into_iter().collect()
}

fn emit_caller_rows(
    ctx: &AnalysisCtx,
    hits: &[&CallSite],
    by: Option<GroupBy>,
    query: &str,
    unique_name: bool,
    method_widened: bool,
    matcher: &QueryMatcher,
) {
    if ctx.summary {
        return;
    }
    match by {
        Some(GroupBy::Fn) => {
            print_grouped_counts(ctx.out, hits, |h| h.caller.clone())
        }
        Some(GroupBy::File) => print_grouped_counts(ctx.out, hits, |h| h.file.clone()),
        Some(GroupBy::Module) => {
            print_grouped_counts(ctx.out, hits, |h| top_module(&h.caller).to_string())
        }
        None => {
            let mut sorted: Vec<&&CallSite> = hits.iter().collect();
            sorted.sort_by(|a, b| {
                a.caller
                    .cmp(&b.caller)
                    .then_with(|| a.file.cmp(&b.file))
                    .then_with(|| a.line.cmp(&b.line))
            });
            for s in sorted {
                row!(
                    ctx.out,
                    "caller" => s.caller.clone(),
                    "target" => s.target.clone(),
                    "confidence" => widened_confidence(s, query, unique_name, method_widened, matcher).as_str(),
                    "at" => site(&s.file, s.line),
                );
            }
        }
    }
}

/// Last-segment glob match, re-exported from [`crate::ast`].
///
/// Lived here first, because `--among` was the only glob in the tool. It moved
/// the day `inventory --name` needed the same three lines: two copies of a
/// matcher is how two commands end up disagreeing about what `*` means.
use crate::ast::glob_match;

/// The fns/methods in the tree whose last-segment name matches `pattern`.
/// Returns (display_label, qpath, file, line), de-duplicated by qpath and
/// sorted by qpath. Labels are bare names unless two cohort members share one,
/// in which case all labels fall back to their qpath to stay unambiguous.
fn cohort_members(index: &NameIndex, pattern: &str) -> Vec<(String, String, String, usize)> {
    let mut members: Vec<(String, String, usize)> = Vec::new();
    let mut seen = BTreeSet::new();
    for d in index.iter() {
        if !matches!(d.kind, "fn" | "impl-fn" | "trait-fn") {
            continue;
        }
        if !glob_match(pattern, &d.name) {
            continue;
        }
        if seen.insert(d.qpath.clone()) {
            members.push((d.qpath.clone(), d.file.clone(), d.line));
        }
    }
    members.sort_by(|a, b| a.0.cmp(&b.0));

    // Decide labels: bare last segment, or full qpath if names collide.
    let mut name_counts: BTreeMap<String, usize> = BTreeMap::new();
    for (qpath, _, _) in &members {
        let last = crate::ast::last_segment(qpath).to_string();
        *name_counts.entry(last).or_insert(0) += 1;
    }
    members
        .into_iter()
        .map(|(qpath, file, line)| {
            let last = crate::ast::last_segment(qpath.as_str());
            let label = if name_counts.get(last).copied().unwrap_or(0) > 1 {
                qpath.clone()
            } else {
                last.to_string()
            };
            (label, qpath, file, line)
        })
        .collect()
}

/// `callers <helper> --among <pattern>` — invert the callers query. For every
/// fn in the name-pattern cohort, report whether it calls the helper (✓ + the
/// call site) or not (✗). The ✗ rows are divergence candidates — but only a
/// human can say whether a given sibling *should* have called the helper.
pub fn run_callers_among(
    ctx: &AnalysisCtx,
    query: &str,
    pattern: &str,
) -> anyhow::Result<usize> {
    let files = ctx.files;
    let index = ctx.idx;
    let sem = ctx.sem;
    let summary = ctx.summary;
    let members = cohort_members(index, pattern);
    if members.is_empty() {
        warn_unknown_target("fn or method matching cohort pattern", pattern);
        ctx.out.summary(&format!(
            "(0/0 cohort member(s) call `{}`; 0 do not)",
            query
        ));
        return Err(TargetNotFound::err(
            "fn or method matching cohort pattern",
            pattern,
        ));
    }

    let sites = collect_sites(files, sem, index, ctx.spans);
    // For each cohort member qpath, the first call site of `query` inside it.
    let mut call_in: BTreeMap<&str, &CallSite> = BTreeMap::new();
    for s in &sites {
        let hits_query = matches_target(&s.target, query)
            || s.target_resolved
                .as_deref()
                .map(|t| matches_target(t, query))
                .unwrap_or(false);
        if !hits_query {
            continue;
        }
        let e = call_in.entry(s.caller.as_str());
        e.or_insert(s);
    }

    let mut calls = 0usize;
    if !summary {
        for (label, qpath, _file, _line) in &members {
            match call_in.get(qpath.as_str()) {
                Some(site) => {
                    calls += 1;
                    row!(
                        ctx.out,
                        "calls" => "✓",
                        "member" => label.clone(),
                        "at" => crate::emit::site(&site.file, site.line),
                    );
                }
                None => row!(
                    ctx.out,
                    "calls" => "✗",
                    "member" => label.clone(),
                    "at" => "(no call site)",
                ),
            }
        }
    } else {
        calls = members
            .iter()
            .filter(|(_, q, _, _)| call_in.contains_key(q.as_str()))
            .count();
    }
    ctx.out.summary(&format!(
        "({}/{} cohort member(s) call `{}`; {} do not)",
        calls,
        members.len(),
        query,
        members.len() - calls
    ));
    Ok(members.len() - calls)
}

/// `cohort-callees <pattern>` — a (callee × function) matrix for a name-pattern
/// cohort. A callee present in most columns but missing from one is a
/// divergence candidate: the sibling that forgot to call a shared helper.
pub fn run_cohort_callees(ctx: &AnalysisCtx, pattern: &str) -> anyhow::Result<usize> {
    let files = ctx.files;
    let index = ctx.idx;
    let sem = ctx.sem;
    let summary = ctx.summary;
    let members = cohort_members(index, pattern);
    if members.is_empty() {
        warn_unknown_target("fn or method matching cohort pattern", pattern);
        ctx.out
            .summary("(0 cohort member(s), 0 distinct callee(s), 0 divergence candidate(s))");
        return Err(TargetNotFound::err(
            "fn or method matching cohort pattern",
            pattern,
        ));
    }

    let sites = collect_sites(files, sem, index, ctx.spans);
    // qpath -> set of callee targets it makes.
    let mut by_member: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for (_, qpath, _, _) in &members {
        by_member.insert(qpath.as_str(), BTreeSet::new());
    }
    for s in &sites {
        if let Some(set) = by_member.get_mut(s.caller.as_str()) {
            set.insert(s.target.clone());
        }
    }

    // Union of every callee across the cohort = the matrix rows.
    let mut all_callees: BTreeSet<String> = BTreeSet::new();
    for set in by_member.values() {
        all_callees.extend(set.iter().cloned());
    }

    let cols: Vec<(&str, &str)> = members
        .iter()
        .map(|(label, qpath, _, _)| (label.as_str(), qpath.as_str()))
        .collect();
    let n = cols.len();

    let mut divergences: Vec<String> = Vec::new();
    if !summary {
        // Header row: leading "callee" column, then one column per cohort fn.
        let mut header = String::from("callee");
        for (label, _) in &cols {
            header.push('\t');
            header.push_str(label);
        }
        ctx.out.line(&header);
    }
    for callee in &all_callees {
        let present = presence_row(callee, &cols, &by_member);
        let diverges = is_divergent(&present);
        if !summary {
            let mut row = callee.clone();
            for &p in &present {
                row.push('\t');
                row.push(if p { '✓' } else { '·' });
            }
            if diverges {
                row.push_str("\t<- divergence");
            }
            ctx.out.line(&row);
        }
        if diverges {
            divergences.push(callee.clone());
        }
    }
    if let Some(n) = no_majority_note(cols.len(), divergences.len()) {
        ctx.out.note(&n);
    }
    ctx.out.summary(&format!(
        "({} cohort member(s), {} distinct callee(s), {} divergence candidate(s); explain: sibling)",
        n,
        all_callees.len(),
        divergences.len()
    ));
    Ok(divergences.len())
}

/// Per-caller tallies for one co-call pair: how often it calls A and B, and
/// the first call site of each (the `via` pointer).
#[derive(Default)]
struct Co {
    calls_a: usize,
    calls_b: usize,
    via_a: Option<(String, usize)>,
    via_b: Option<(String, usize)>,
}

type CoRow<'a> = (&'a str, usize, &'a (String, usize));

/// Partition callers into A-only / B-only rows — ranked by matched-call count
/// descending (high-traffic fns first) — plus the count of canonical
/// both-callers. (false, false) can't occur: only callers of A or B are in
/// the map.
fn partition_co<'a>(by_caller: &'a BTreeMap<&'a str, Co>) -> (Vec<CoRow<'a>>, Vec<CoRow<'a>>, usize) {
    let mut a_only: Vec<CoRow<'a>> = Vec::new();
    let mut b_only: Vec<CoRow<'a>> = Vec::new();
    let mut both = 0usize;
    for (caller, co) in by_caller {
        match (co.calls_a > 0, co.calls_b > 0) {
            (true, true) => both += 1,
            (true, false) => a_only.push((caller, co.calls_a, co.via_a.as_ref().unwrap())),
            (false, true) => b_only.push((caller, co.calls_b, co.via_b.as_ref().unwrap())),
            (false, false) => {}
        }
    }
    a_only.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(y.0)));
    b_only.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(y.0)));
    (a_only, b_only, both)
}

/// Which cohort members (columns) call `callee`.
fn presence_row(
    callee: &str,
    cols: &[(&str, &str)],
    by_member: &BTreeMap<&str, BTreeSet<String>>,
) -> Vec<bool> {
    cols.iter()
        .map(|(_, qpath)| {
            by_member
                .get(qpath)
                .map(|s| s.contains(callee))
                .unwrap_or(false)
        })
        .collect()
}

/// Divergence: a minority dissents from a present majority.
/// A callee most of the cohort calls and at least one does not.
///
/// "Most" is the whole inference: a helper 7 of 8 siblings call is a lead, a
/// helper 1 of 2 calls is just a difference. Relaxing this to "any split" was
/// tried and produced 24 candidates on a correct two-member cohort — `.count`,
/// `.filter`, and every other incidental adapter one sibling happened to use.
///
/// At N=2 no majority exists, so nothing can qualify. That is a real blind
/// spot, not a bug to paper over, and [`no_majority_note`] says so out loud
/// rather than letting `0 divergence candidate(s)` read as "they agree".
fn is_divergent(present: &[bool]) -> bool {
    let p = present.iter().filter(|&&x| x).count();
    let a = present.len() - p;
    p > a && a > 0
}

/// Warn when the cohort is too small for the majority rule to say anything.
fn no_majority_note(members: usize, divergences: usize) -> Option<String> {
    (members < 3 && divergences == 0).then(|| format!(
        "(note: a {}-member cohort has no majority, so no callee can qualify as a \
         divergence candidate — widen the pattern, or read the ✓/· matrix directly: \
         a callee in one column and not the other is visible there even when the \
         rule cannot rank it)",
        members
    ))
}

/// `co-call <A> <B>` — paired-action invariant check. A and B are a coupled
/// pair: calling one without the other leaks an invariant (e.g.
/// `refresh_world_transforms` + `recompute_derived_geometry` — both must run to
/// settle a `Document`). For every fn in the tree we test whether it calls A, B,
/// both, or neither, and emit the *asymmetric* callers:
///   `A-only`  — calls A, not B (suspect)
///   `B-only`  — calls B, not A (suspect)
/// Both-callers are the canonical pattern (counted on the summary line, not
/// listed); neither-callers are irrelevant. Each row is a candidate — some
/// asymmetries are correct (a gate that queues a mutation while a later commit
/// runs B), so a human filters. A and B accept the same target forms as
/// `callers` (bare name, `Type::method`, `.method`, `::name`, `name!`). The
/// `via` column points at the call the fn *does* make, for quick navigation.
pub fn run_co_call(ctx: &AnalysisCtx, a: &str, b: &str) -> anyhow::Result<usize> {
    let files = ctx.files;
    let index = ctx.idx;
    let sem = ctx.sem;
    let summary = ctx.summary;
    let known_a = query_known(index, a);
    let known_b = query_known(index, b);
    for (known, q) in [(known_a, a), (known_b, b)] {
        if !known {
            ctx.warn_unknown("fn, method, or macro", q);
        }
    }

    let sites = collect_sites(files, sem, index, ctx.spans);
    // Same resolution as `callers`. Before this, `co-call emit::push_str
    // emit::push_val` answered 0/0/0 while the bare pair scored 1 both and 11
    // A-only — a paired-action check that silently sees no pairs is worse than
    // one that errors.
    let (ma, mb) = (QueryMatcher::new(index, a), QueryMatcher::new(index, b));
    let hits = |s: &CallSite, m: &QueryMatcher| -> bool {
        m.hits(
            &s.target,
            s.target_resolved.as_deref(),
            crate::config_drift::module_of(&s.caller),
            s.receiver_is_self,
        )
    };

    let mut by_caller: BTreeMap<&str, Co> = BTreeMap::new();
    for s in &sites {
        let is_a = hits(s, &ma);
        let is_b = hits(s, &mb);
        if !is_a && !is_b {
            continue;
        }
        let e = by_caller.entry(s.caller.as_str()).or_default();
        if is_a {
            e.calls_a += 1;
            e.via_a.get_or_insert((s.file.clone(), s.line));
        }
        if is_b {
            e.calls_b += 1;
            e.via_b.get_or_insert((s.file.clone(), s.line));
        }
    }

    let (a_only, b_only, both) = partition_co(&by_caller);

    if !summary {
        for (caller, n, (file, line)) in &a_only {
            row!(
                ctx.out,
                "side" => "A-only",
                "count" => *n,
                "caller" => caller.to_string(),
                "via" => format!("via {}:{}", file, line),
            );
        }
        for (caller, n, (file, line)) in &b_only {
            row!(
                ctx.out,
                "side" => "B-only",
                "count" => *n,
                "caller" => caller.to_string(),
                "via" => format!("via {}:{}", file, line),
            );
        }
    }
    ctx.out.summary(&format!(
        "({} call both `{}`+`{}`; {} call A-not-B; {} call B-not-A; explain: co-call)",
        both,
        a,
        b,
        a_only.len(),
        b_only.len()
    ));
    let a_hit = by_caller.values().any(|c| c.calls_a > 0);
    let b_hit = by_caller.values().any(|c| c.calls_b > 0);
    if !known_a && !a_hit {
        return Err(TargetNotFound::err("fn, method, or macro matching", a));
    }
    if !known_b && !b_hit {
        return Err(TargetNotFound::err("fn, method, or macro matching", b));
    }
    Ok(a_only.len() + b_only.len())
}

pub fn run_callees(ctx: &AnalysisCtx, query: &str) -> anyhow::Result<usize> {
    let files = ctx.files;
    let index = ctx.idx;
    let sem = ctx.sem;
    let summary = ctx.summary;
    let sites = collect_sites(files, sem, index, ctx.spans);
    let last = crate::ast::last_segment(query);

    let in_target = |caller: &str| -> bool {
        if query.contains("::") {
            caller == query || caller.ends_with(&format!("::{}", query))
        } else {
            crate::ast::last_segment(caller) == last
        }
    };

    let hits: Vec<&CallSite> = sites.iter().filter(|s| in_target(&s.caller)).collect();
    if hits.is_empty() {
        let known = query_known(index, query);
        if !known {
            ctx.warn_unknown("fn or method", query);
        } else {
            ctx.out.note(&format!("note: `{}` makes no calls", query));
        }
        ctx.out.summary("(0 distinct callees)");
        if !known {
            return Err(TargetNotFound::err("fn or method matching", query));
        }
        return Ok(0);
    }

    let mut counts = BTreeMap::<String, (usize, String, usize)>::new();
    for h in &hits {
        let e = counts
            .entry(h.target.clone())
            .or_insert((0, h.file.clone(), h.line));
        e.0 += 1;
    }
    let mut rows: Vec<_> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| a.0.cmp(&b.0)));
    if !summary {
        for (target, (n, file, line)) in &rows {
            row!(
                ctx.out,
                "count" => *n,
                "target" => target.clone(),
                "at" => site(file, *line),
            );
        }
    }
    ctx.out
        .summary(&format!("({} distinct callees)", rows.len()));
    Ok(rows.len())
}
