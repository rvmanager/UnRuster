use std::collections::{BTreeMap, BTreeSet, VecDeque};

use syn::visit::{self, Visit};

use crate::ast::{line_of, path_to_string, print_grouped_counts, type_short, ScopeTracker};
use crate::context::AnalysisCtx;
use crate::index::NameIndex;
use crate::parse::{display_path, ParsedFile};
use crate::semantic::{Semantic, UseMap};

#[derive(Debug, Clone)]
struct CallSite {
    file: String,
    line: usize,
    caller: String,
    target: String,
    /// Target as resolved through the calling file's `use` map, if different
    /// from `target`. Used as a secondary key for `matches_target`. Approximate.
    target_resolved: Option<String>,
}

struct CallVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    sites: Vec<CallSite>,
}

impl<'a> CallVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing_with_toplevel()
    }

    fn record(&mut self, target: String, line: usize) {
        self.sites.push(CallSite {
            file: self.file.to_string(),
            line,
            caller: self.enclosing(),
            target,
            target_resolved: None,
        });
    }
}

impl<'ast, 'a> Visit<'ast> for CallVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.scope.enter_mod(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.scope.leave_mod();
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.scope.enter_fn(i.sig.ident.to_string());
        visit::visit_item_fn(self, i);
        self.scope.leave_fn();
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.scope.enter_impl(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.scope.leave_impl();
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.scope.enter_fn(i.sig.ident.to_string());
        visit::visit_impl_item_fn(self, i);
        self.scope.leave_fn();
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.scope.enter_fn(i.sig.ident.to_string());
        visit::visit_trait_item_fn(self, i);
        self.scope.leave_fn();
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            let target = path_to_string(&p.path);
            self.record(target, line_of(&e.func));
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let target = format!(".{}", e.method);
        self.record(target, line_of(&e.method));
        visit::visit_expr_method_call(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if let Some(last) = m.path.segments.last() {
            let target = format!("{}!", path_to_string(&m.path));
            self.record(target, line_of(&last.ident));
        }
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

fn collect_sites(files: &[ParsedFile], sem: &Semantic, index: &NameIndex) -> Vec<CallSite> {
    let mut all = Vec::new();
    for f in files {
        let mut v = CallVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()),
            sites: Vec::new(),
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

fn resolve_target_via_uses(target: &str, uses: &UseMap, index: &NameIndex) -> Option<String> {
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

fn matches_target(call_target: &str, query: &str) -> bool {
    if let Some(name) = query.strip_suffix('!') {
        let Some(target_macro) = call_target.strip_suffix('!') else {
            return false;
        };
        let target_last = target_macro.rsplit("::").next().unwrap_or(target_macro);
        let q_last = name.rsplit("::").next().unwrap_or(name);
        return target_last == q_last;
    }
    if let Some(method) = query.strip_prefix('.') {
        return call_target == format!(".{}", method);
    }
    if let Some(rest) = query.strip_prefix("::") {
        if call_target.starts_with('.') || call_target.ends_with('!') {
            return false;
        }
        let last = rest.rsplit("::").next().unwrap_or(rest);
        let target_last = call_target.rsplit("::").next().unwrap_or(call_target);
        return target_last == last;
    }
    if query.contains("::") {
        let trimmed = call_target.strip_suffix('!').unwrap_or(call_target);
        return trimmed.ends_with(query);
    }
    let target_last = if let Some(m) = call_target.strip_prefix('.') {
        m
    } else if let Some(m) = call_target.strip_suffix('!') {
        m.rsplit("::").next().unwrap_or(m)
    } else {
        call_target.rsplit("::").next().unwrap_or(call_target)
    };
    target_last == query
}

/// True if the index knows of any defined fn/method/etc. that matches the query.
fn query_known(index: &NameIndex, query: &str) -> bool {
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

pub fn run_callers(
    ctx: &AnalysisCtx,
    query: &str,
    transitive: bool,
    depth: Option<usize>,
    by: Option<&str>,
) -> anyhow::Result<()> {
    let files = ctx.files;
    let index = ctx.idx;
    let sem = ctx.sem;
    let summary = ctx.summary;
    if !query_known(index, query) {
        eprintln!(
            "note: no fn/method matching `{}` is defined in the scanned tree \
             (zero callers could mean the symbol doesn't exist; use --scope all if testing).",
            query
        );
    }

    let sites = collect_sites(files, sem, index);

    let direct: Vec<&CallSite> = sites
        .iter()
        .filter(|s| {
            matches_target(&s.target, query)
                || s.target_resolved
                    .as_deref()
                    .map(|t| matches_target(t, query))
                    .unwrap_or(false)
        })
        .collect();

    if !transitive {
        emit_caller_rows(&direct, by, summary);
        let unique = direct
            .iter()
            .map(|s| s.caller.as_str())
            .collect::<BTreeSet<_>>();
        eprintln!(
            "({} call site(s) across {} caller(s))",
            direct.len(),
            unique.len()
        );
        return Ok(());
    }

    // Transitive: BFS from query outwards through the call graph.
    // Build: target_last_name -> set of caller qpaths.
    let mut rev: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for s in &sites {
        let last = if let Some(m) = s.target.strip_prefix('.') {
            m.to_string()
        } else if let Some(m) = s.target.strip_suffix('!') {
            m.rsplit("::").next().unwrap_or(m).to_string()
        } else {
            s.target.rsplit("::").next().unwrap_or(&s.target).to_string()
        };
        rev.entry(last).or_default().insert(s.caller.clone());
    }

    let max_depth = depth.unwrap_or(usize::MAX);
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
    queue.push_back((seed_last, 0));

    while let Some((name, d)) = queue.pop_front() {
        if d >= max_depth {
            continue;
        }
        let Some(callers) = rev.get(&name) else {
            continue;
        };
        for caller in callers {
            let caller_last = caller
                .rsplit("::")
                .next()
                .unwrap_or(caller)
                .to_string();
            let entry = visited.entry(caller.clone()).or_insert(d + 1);
            if *entry > d + 1 {
                *entry = d + 1;
            }
            // Only enqueue if depth not yet reached.
            if d + 1 < max_depth {
                queue.push_back((caller_last, d + 1));
            }
        }
    }

    // Emit transitive callers grouped by depth.
    let mut rows: Vec<(String, usize)> = visited.into_iter().collect();
    rows.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    if !summary {
        for (caller, d) in &rows {
            println!("d{}\t{}", d, caller);
        }
    }
    eprintln!(
        "({} direct, {} transitive caller(s); max_depth={})",
        direct.len(),
        rows.len(),
        depth
            .map(|d| d.to_string())
            .unwrap_or_else(|| "∞".to_string())
    );
    Ok(())
}

fn emit_caller_rows(hits: &[&CallSite], by: Option<&str>, summary: bool) {
    if summary {
        return;
    }
    match by {
        Some("file") => print_grouped_counts(hits, None, |h| h.file.clone()),
        Some("module") => {
            print_grouped_counts(hits, None, |h| top_module(&h.caller).to_string())
        }
        _ => {
            let mut sorted: Vec<&&CallSite> = hits.iter().collect();
            sorted.sort_by(|a, b| {
                a.caller
                    .cmp(&b.caller)
                    .then_with(|| a.file.cmp(&b.file))
                    .then_with(|| a.line.cmp(&b.line))
            });
            for s in sorted {
                println!("{}\t{}\t{}:{}", s.caller, s.target, s.file, s.line);
            }
        }
    }
}

/// Last-segment glob match. `*` matches any (possibly empty) run of chars.
/// No other metacharacters. `name` is the bare last segment of an item.
fn glob_match(pattern: &str, name: &str) -> bool {
    // Fast path: no wildcard means exact match.
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    // Anchored prefix (text before the first `*`).
    let mut rest = name;
    if let Some(first) = parts.first() {
        if !rest.starts_with(first) {
            return false;
        }
        rest = &rest[first.len()..];
    }
    // Anchored suffix (text after the last `*`).
    if let Some(last) = parts.last() {
        if !rest.ends_with(last) {
            return false;
        }
        rest = &rest[..rest.len() - last.len()];
    }
    // Interior literals must appear in order.
    for mid in &parts[1..parts.len().saturating_sub(1)] {
        if mid.is_empty() {
            continue;
        }
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

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
        let last = qpath.rsplit("::").next().unwrap_or(qpath).to_string();
        *name_counts.entry(last).or_insert(0) += 1;
    }
    members
        .into_iter()
        .map(|(qpath, file, line)| {
            let last = qpath.rsplit("::").next().unwrap_or(&qpath);
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
) -> anyhow::Result<()> {
    let files = ctx.files;
    let index = ctx.idx;
    let sem = ctx.sem;
    let summary = ctx.summary;
    let members = cohort_members(index, pattern);
    if members.is_empty() {
        eprintln!("no fn/method matching cohort pattern `{}` found", pattern);
        return Ok(());
    }

    let sites = collect_sites(files, sem, index);
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
                    println!("✓\t{}\t{}:{}", label, site.file, site.line);
                }
                None => println!("✗\t{}\t(no call site)", label),
            }
        }
    } else {
        calls = members
            .iter()
            .filter(|(_, q, _, _)| call_in.contains_key(q.as_str()))
            .count();
    }
    eprintln!(
        "({}/{} cohort member(s) call `{}`; {} do not)",
        calls,
        members.len(),
        query,
        members.len() - calls
    );
    Ok(())
}

/// `cohort-callees <pattern>` — a (callee × function) matrix for a name-pattern
/// cohort. A callee present in most columns but missing from one is a
/// divergence candidate: the sibling that forgot to call a shared helper.
pub fn run_cohort_callees(ctx: &AnalysisCtx, pattern: &str) -> anyhow::Result<()> {
    let files = ctx.files;
    let index = ctx.idx;
    let sem = ctx.sem;
    let summary = ctx.summary;
    let members = cohort_members(index, pattern);
    if members.is_empty() {
        eprintln!("no fn/method matching cohort pattern `{}` found", pattern);
        return Ok(());
    }

    let sites = collect_sites(files, sem, index);
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
        println!("{}", header);

        for callee in &all_callees {
            let present: Vec<bool> = cols
                .iter()
                .map(|(_, qpath)| by_member.get(qpath).map(|s| s.contains(callee)).unwrap_or(false))
                .collect();
            let present_count = present.iter().filter(|&&p| p).count();
            let absent_count = n - present_count;
            // Divergence: a minority dissents from a present majority.
            let diverges = present_count > absent_count && absent_count > 0;
            let mut row = callee.clone();
            for &p in &present {
                row.push('\t');
                row.push(if p { '✓' } else { '·' });
            }
            if diverges {
                row.push_str("\t<- divergence");
                divergences.push(callee.clone());
            }
            println!("{}", row);
        }
    } else {
        for callee in &all_callees {
            let present_count = cols
                .iter()
                .filter(|(_, qpath)| {
                    by_member.get(qpath).map(|s| s.contains(callee)).unwrap_or(false)
                })
                .count();
            let absent_count = n - present_count;
            if present_count > absent_count && absent_count > 0 {
                divergences.push(callee.clone());
            }
        }
    }
    eprintln!(
        "({} cohort member(s), {} distinct callee(s), {} divergence candidate(s))",
        n,
        all_callees.len(),
        divergences.len()
    );
    Ok(())
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
pub fn run_co_call(ctx: &AnalysisCtx, a: &str, b: &str) -> anyhow::Result<()> {
    let files = ctx.files;
    let index = ctx.idx;
    let sem = ctx.sem;
    let summary = ctx.summary;
    for q in [a, b] {
        if !query_known(index, q) {
            eprintln!(
                "note: no fn/method matching `{}` is defined in the scanned tree \
                 (zero callers could mean the symbol doesn't exist; use --scope all if testing).",
                q
            );
        }
    }

    let sites = collect_sites(files, sem, index);
    let hits = |s: &CallSite, query: &str| -> bool {
        matches_target(&s.target, query)
            || s.target_resolved
                .as_deref()
                .map(|t| matches_target(t, query))
                .unwrap_or(false)
    };

    #[derive(Default)]
    struct Co {
        calls_a: usize,
        calls_b: usize,
        via_a: Option<(String, usize)>,
        via_b: Option<(String, usize)>,
    }
    let mut by_caller: BTreeMap<&str, Co> = BTreeMap::new();
    for s in &sites {
        let is_a = hits(s, a);
        let is_b = hits(s, b);
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

    // Partition. (true, true) = canonical, just counted. (false, false) can't
    // occur here — only callers of A or B made it into the map.
    let mut a_only: Vec<(&str, usize, &(String, usize))> = Vec::new();
    let mut b_only: Vec<(&str, usize, &(String, usize))> = Vec::new();
    let mut both = 0usize;
    for (caller, co) in &by_caller {
        match (co.calls_a > 0, co.calls_b > 0) {
            (true, true) => both += 1,
            (true, false) => a_only.push((caller, co.calls_a, co.via_a.as_ref().unwrap())),
            (false, true) => b_only.push((caller, co.calls_b, co.via_b.as_ref().unwrap())),
            (false, false) => {}
        }
    }
    // High-traffic fns first: rank by matched-call count desc, then name.
    a_only.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(y.0)));
    b_only.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(y.0)));

    if !summary {
        for (caller, n, (file, line)) in &a_only {
            println!("A-only\t{}\t{}\tvia {}:{}", n, caller, file, line);
        }
        for (caller, n, (file, line)) in &b_only {
            println!("B-only\t{}\t{}\tvia {}:{}", n, caller, file, line);
        }
    }
    eprintln!(
        "({} call both `{}`+`{}`; {} call A-not-B; {} call B-not-A)",
        both,
        a,
        b,
        a_only.len(),
        b_only.len()
    );
    Ok(())
}

pub fn run_callees(ctx: &AnalysisCtx, query: &str) -> anyhow::Result<()> {
    let files = ctx.files;
    let index = ctx.idx;
    let sem = ctx.sem;
    let summary = ctx.summary;
    let sites = collect_sites(files, sem, index);
    let last = query.rsplit("::").next().unwrap_or(query);

    let in_target = |caller: &str| -> bool {
        if query.contains("::") {
            caller == query || caller.ends_with(&format!("::{}", query))
        } else {
            caller.rsplit("::").next().unwrap_or(caller) == last
        }
    };

    let hits: Vec<&CallSite> = sites.iter().filter(|s| in_target(&s.caller)).collect();
    if hits.is_empty() {
        eprintln!("no fn matching `{}` found (or it makes no calls)", query);
        return Ok(());
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
            println!("{}\t{}\t{}:{}", n, target, file, line);
        }
    }
    eprintln!("({} distinct callees)", rows.len());
    Ok(())
}
