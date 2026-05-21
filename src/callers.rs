use std::collections::{BTreeMap, BTreeSet, VecDeque};

use syn::visit::{self, Visit};

use crate::ast::{line_of, path_to_string, type_short};
use crate::index::NameIndex;
use crate::parse::{display_path, ParsedFile};

#[derive(Debug, Clone)]
struct CallSite {
    file: String,
    line: usize,
    caller: String,
    target: String,
}

struct CallVisitor<'a> {
    file: &'a str,
    module: &'a str,
    fn_stack: Vec<String>,
    impl_stack: Vec<String>,
    mod_stack: Vec<String>,
    sites: Vec<CallSite>,
}

impl<'a> CallVisitor<'a> {
    fn enclosing(&self) -> String {
        let mut path: Vec<String> = Vec::new();
        if !self.module.is_empty() {
            path.push(self.module.to_string());
        }
        path.extend(self.mod_stack.iter().cloned());
        if let Some(t) = self.impl_stack.last() {
            path.push(t.clone());
        }
        if let Some(f) = self.fn_stack.last() {
            path.push(f.clone());
        } else if path.is_empty() {
            return "<top-level>".to_string();
        } else {
            path.push("<top-level>".to_string());
        }
        path.join("::")
    }

    fn record(&mut self, target: String, line: usize) {
        self.sites.push(CallSite {
            file: self.file.to_string(),
            line,
            caller: self.enclosing(),
            target,
        });
    }
}

impl<'ast, 'a> Visit<'ast> for CallVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.mod_stack.push(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.mod_stack.pop();
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        visit::visit_item_fn(self, i);
        self.fn_stack.pop();
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.impl_stack.push(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.impl_stack.pop();
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        visit::visit_impl_item_fn(self, i);
        self.fn_stack.pop();
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        visit::visit_trait_item_fn(self, i);
        self.fn_stack.pop();
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

fn collect_sites(files: &[ParsedFile]) -> Vec<CallSite> {
    let mut all = Vec::new();
    for f in files {
        let mut v = CallVisitor {
            file: &display_path(&f.path),
            module: &f.module,
            fn_stack: Vec::new(),
            impl_stack: Vec::new(),
            mod_stack: Vec::new(),
            sites: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.sites);
    }
    all
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
    !index
        .iter()
        .filter(|d| matches!(d.kind, "fn" | "impl-fn" | "trait-fn"))
        .filter(|d| d.name == last)
        .next()
        .is_none()
        || index.knows_name(last)
}

fn top_module(qpath: &str) -> &str {
    qpath.split("::").next().unwrap_or(qpath)
}

pub fn run_callers(
    files: &[ParsedFile],
    index: &NameIndex,
    query: &str,
    transitive: bool,
    depth: Option<usize>,
    by: Option<&str>,
    summary: bool,
) -> anyhow::Result<()> {
    if !query_known(index, query) {
        eprintln!(
            "note: no fn/method matching `{}` is defined in the scanned tree \
             (zero callers could mean the symbol doesn't exist; use --scope all if testing).",
            query
        );
    }

    let sites = collect_sites(files);

    let direct: Vec<&CallSite> = sites
        .iter()
        .filter(|s| matches_target(&s.target, query))
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
        Some("file") => {
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for h in hits {
                *counts.entry(h.file.as_str()).or_insert(0) += 1;
            }
            let mut rows: Vec<_> = counts.into_iter().collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            for (file, n) in rows {
                println!("{}\t{}", n, file);
            }
        }
        Some("module") => {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for h in hits {
                *counts
                    .entry(top_module(&h.caller).to_string())
                    .or_insert(0) += 1;
            }
            let mut rows: Vec<_> = counts.into_iter().collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            for (m, n) in rows {
                println!("{}\t{}", n, m);
            }
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

pub fn run_callees(
    files: &[ParsedFile],
    _index: &NameIndex,
    query: &str,
    summary: bool,
) -> anyhow::Result<()> {
    let sites = collect_sites(files);
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
