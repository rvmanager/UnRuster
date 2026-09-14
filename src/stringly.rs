use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{lit_str, print_grouped_counts, scope_visits, top_module_of, ScopeTracker};
use crate::context::{AnalysisCtx, GroupBy};
use crate::parse::display_path;
use crate::emit::{row, site};

#[derive(Debug)]
struct Hit {
    /// "match-wild-list" | "cmp-eq" | "cmp-method" | "match-lit" | "substr" |
    /// "map-lit-key"
    ///
    /// The first is per-`match` rather than per-literal, and it is the only one
    /// whose finding is a row that is *missing*: a `match` on string literals
    /// ending in `_` emits no row for the arm the wildcard swallowed. On one
    /// codebase that arm was `_ => "DELETE FROM ore_balances …"` under `for
    /// table in ["plot_machines", "plot_blocks", "ore_balances"]`, so a fourth
    /// table added to the list would compile, pass, and delete from the wrong
    /// one — on the path that wipes a player's plot. The reader found it by
    /// reading the source around two `match-lit` rows and noticing there was no
    /// third.
    class: &'static str,
    literal: String,
    context: String,
    file: String,
    line: usize,
}

struct StringlyVisitor<'a> {
    include_substring: bool,
    include_map_keys: bool,
    file: &'a str,
    scope: ScopeTracker,
    hits: Vec<Hit>,
    /// `(binding, literals)` for each enclosing `for x in ["a", "b", "c"]`.
    ///
    /// A loop over a literal list whose body matches the loop variable back
    /// into those same literals has written the enumeration twice, and only one
    /// of the two copies is checked by anything. See [`Hit::class`]
    /// `match-wild-list`.
    loop_lists: Vec<(String, usize)>,
}

impl<'a> StringlyVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    fn record(&mut self, class: &'static str, literal: String, line: usize) {
        let ctx = self.enclosing();
        self.hits.push(Hit {
            class,
            literal,
            context: ctx,
            file: self.file.to_string(),
            line,
        });
    }
}


impl StringlyVisitor<'_> {
    /// The length of the literal list a `for` loop is walking, when this match
    /// is matching that loop's variable back into literals.
    ///
    /// Peels `&x` and `x.as_str()` / `.as_ref()` / `.trim()`-shaped receivers,
    /// because the scrutinee is written both ways and the shape is the same
    /// either way.
    fn loop_list_over(&self, scrutinee: &syn::Expr) -> Option<usize> {
        let name = bare_name(scrutinee)?;
        self.loop_lists
            .iter()
            .rev()
            .find(|(b, _)| *b == name)
            .map(|(_, n)| *n)
    }
}

/// `x`, `&x`, `x.as_str()`, `(&x).as_ref()` — all name `x`.
fn bare_name(e: &syn::Expr) -> Option<String> {
    match e {
        syn::Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            Some(p.path.segments[0].ident.to_string())
        }
        syn::Expr::Reference(r) => bare_name(&r.expr),
        syn::Expr::Paren(p) => bare_name(&p.expr),
        syn::Expr::Group(g) => bare_name(&g.expr),
        syn::Expr::MethodCall(m) if m.args.is_empty() => bare_name(&m.receiver),
        syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => bare_name(&u.expr),
        _ => None,
    }
}

/// The single binding a `for` pattern introduces, if it introduces exactly one.
fn binding_of(p: &syn::Pat) -> Option<String> {
    match p {
        syn::Pat::Ident(i) if i.subpat.is_none() => Some(i.ident.to_string()),
        syn::Pat::Reference(r) => binding_of(&r.pat),
        syn::Pat::Paren(p) => binding_of(&p.pat),
        _ => None,
    }
}

/// How many string literals are in `["a", "b", "c"]` / `&["a", "b"]` /
/// `["a"].iter()`, or `None` when the iterator is not a literal list.
fn literal_array_len(e: &syn::Expr) -> Option<usize> {
    match e {
        syn::Expr::Array(a) => {
            let n = a.elems.iter().filter(|x| lit_str(x).is_some()).count();
            (n == a.elems.len() && n > 0).then_some(n)
        }
        syn::Expr::Reference(r) => literal_array_len(&r.expr),
        syn::Expr::Paren(p) => literal_array_len(&p.expr),
        syn::Expr::Group(g) => literal_array_len(&g.expr),
        // `.iter()` / `.into_iter()` / `.copied()` over one.
        syn::Expr::MethodCall(m) if m.args.is_empty() => literal_array_len(&m.receiver),
        _ => None,
    }
}

/// A trailing arm that takes everything left: `_`, or a bare binding.
///
/// `Pat::Ident` and not a path: a unit struct or a const would be a `Pat::Path`,
/// and those match one thing rather than the rest.
fn is_catch_all(p: &syn::Pat) -> bool {
    match p {
        syn::Pat::Wild(_) => true,
        syn::Pat::Ident(i) => i.subpat.is_none() && i.by_ref.is_none(),
        syn::Pat::Reference(r) => is_catch_all(&r.pat),
        syn::Pat::Paren(p) => is_catch_all(&p.pat),
        _ => false,
    }
}

/// Read-it-in-order rank: the per-`match` class first, everything else after.
/// See [`Hit::class`] for why it is different in kind.
fn rank(class: &str) -> u8 {
    u8::from(class != "match-wild-list")
}

fn collect_str_lits_in_pat(p: &syn::Pat, out: &mut Vec<(String, usize)>) {
    match p {
        syn::Pat::Lit(el) => {
            if let syn::Lit::Str(s) = &el.lit {
                out.push((s.value(), s.span().start().line));
            }
        }
        syn::Pat::Or(o) => {
            for c in &o.cases {
                collect_str_lits_in_pat(c, out);
            }
        }
        syn::Pat::Reference(r) => collect_str_lits_in_pat(&r.pat, out),
        syn::Pat::Paren(p) => collect_str_lits_in_pat(&p.pat, out),
        _ => {}
    }
}

fn truncate_lit(s: &str, max: usize) -> String {
    let escaped = s.replace('\n', "\\n").replace('\t', "\\t");
    let chars: Vec<char> = escaped.chars().collect();
    if chars.len() <= max {
        format!("\"{}\"", escaped)
    } else {
        let head: String = chars.into_iter().take(max).collect();
        format!("\"{}…\"", head)
    }
}

impl<'ast, 'a> Visit<'ast> for StringlyVisitor<'a> {
    scope_visits!(item_mod, item_impl, item_trait, item_fn, impl_item_fn, trait_item_fn);

    fn visit_expr_binary(&mut self, e: &'ast syn::ExprBinary) {
        if matches!(e.op, syn::BinOp::Eq(_) | syn::BinOp::Ne(_)) {
            if let Some(s) = lit_str(&e.left) {
                self.record("cmp-eq", truncate_lit(&s, 32), e.left.span().start().line);
            } else if let Some(s) = lit_str(&e.right) {
                self.record("cmp-eq", truncate_lit(&s, 32), e.right.span().start().line);
            }
        }
        visit::visit_expr_binary(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let m = e.method.to_string();
        let class: Option<&'static str> = match m.as_str() {
            "eq" | "ne" | "eq_ignore_ascii_case" | "eq_ignore_case" => Some("cmp-method"),
            "starts_with" | "ends_with" | "contains" if self.include_substring => Some("substr"),
            "get" | "contains_key" | "remove" | "entry" if self.include_map_keys => Some("map-lit-key"),
            _ => None,
        };
        if let Some(c) = class {
            if let Some(arg) = e.args.first() {
                if let Some(s) = lit_str(arg) {
                    self.record(c, truncate_lit(&s, 32), e.method.span().start().line);
                }
            }
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_match(&mut self, e: &'ast syn::ExprMatch) {
        let mut arms = 0usize;
        let mut first: Option<String> = None;
        for arm in &e.arms {
            let mut found = Vec::new();
            collect_str_lits_in_pat(&arm.pat, &mut found);
            if !found.is_empty() {
                arms += 1;
            }
            for (v, line) in found {
                let lit = truncate_lit(&v, 32);
                if first.is_none() {
                    first = Some(lit.clone());
                }
                self.record("match-lit", lit, line);
            }
        }
        // The arm nothing prints a row for. Two literal arms and a `_` is a
        // list with a hole in it: the compiler checks the arms, nothing checks
        // that the list of things being matched still has the same members.
        //
        // Two, not three, because the trap does not need a long list — `match
        // x { "a" => .., "b" => .., _ => .. }` already has somewhere for a
        // third case to go silently. One literal and a `_` is an if/else and is
        // left alone, the same call `enum-coverage` makes about a 1-of-2
        // `matches!`.
        if arms >= 2 {
            if let Some(last) = e.arms.last() {
                if last.guard.is_none() && is_catch_all(&last.pat) {
                    let line = last.pat.span().start().line;
                    let lit = first.unwrap_or_else(|| "_".to_string());
                    // Only the closed list. A plain wildcard was measured
                    // first and dropped: 22 of them on this crate, every one a
                    // classifier over an *open* vocabulary — `"u8" => …, _ =>`
                    // in `casts`, `"expect" => …, _ =>` in `divergence` — where
                    // the wildcard is the correct arm and there is nothing to
                    // act on. Ranked first they would have filled `audit`'s
                    // five-row window with rows nobody would ever fix, which is
                    // the concern `is_accessor_shape` records as "a correct
                    // observation and a wrong concern".
                    //
                    // `n > arms` is what makes it a finding rather than an
                    // observation: the list has members the match does not
                    // name, so the wildcard is serving them. Enumerate all
                    // three explicitly and this goes quiet, which is also the
                    // repair.
                    if let Some(n) = self.loop_list_over(&e.expr) {
                        if n > arms {
                            self.record("match-wild-list", lit, line);
                        }
                    }
                }
            }
        }
        visit::visit_expr_match(self, e);
    }

    fn visit_expr_for_loop(&mut self, e: &'ast syn::ExprForLoop) {
        let pushed = match (binding_of(&e.pat), literal_array_len(&e.expr)) {
            (Some(name), Some(n)) => {
                self.loop_lists.push((name, n));
                true
            }
            _ => false,
        };
        visit::visit_expr_for_loop(self, e);
        if pushed {
            self.loop_lists.pop();
        }
    }

    fn visit_expr_let(&mut self, e: &'ast syn::ExprLet) {
        // `if let "foo" = x.as_str()` etc.
        let mut found = Vec::new();
        collect_str_lits_in_pat(&e.pat, &mut found);
        for (v, line) in found {
            self.record("match-lit", truncate_lit(&v, 32), line);
        }
        visit::visit_expr_let(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        // Special-case assert_eq!/assert_ne!/debug_assert_eq!/debug_assert_ne! so we
        // catch `assert_eq!(role, "admin")` which is morally `role == "admin"`.
        let mac_name = m
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        let is_assert_cmp = matches!(
            mac_name.as_str(),
            "assert_eq" | "assert_ne" | "debug_assert_eq" | "debug_assert_ne"
        );
        let exprs = crate::macro_scan::macro_exprs(m);
        if is_assert_cmp {
            // First two args are the operands; either being a str literal is a hit.
            for arg in exprs.iter().take(2) {
                if let Some(s) = lit_str(arg) {
                    self.record(
                        "cmp-eq",
                        truncate_lit(&s, 32),
                        arg.span().start().line,
                    );
                }
            }
        }
        for expr in exprs {
            self.visit_expr(&expr);
        }
    }
}

pub fn run(
    ctx: &AnalysisCtx,
    include_substring: bool,
    include_map_keys: bool,
    by: Option<GroupBy>,
) -> anyhow::Result<usize> {
    let files = ctx.files;
    let summary = ctx.summary;
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = StringlyVisitor {
            include_substring,
            include_map_keys,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            hits: Vec::new(),
            loop_lists: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }

    ctx.retain_changed(&mut all, |h| &h.file);
    // Keyed on the literal, so waiving the Stripe event name on a line leaves
    // any other literal branch on it flagged. An unkeyed `ok(stringly)` above
    // a fn retires the whole cluster in one comment, which is the shape most
    // of these come in: a `match` over a wire protocol's vocabulary is one
    // judgment, not eight.
    //
    // The check had no waiver mechanism at all until now. That was load-bearing
    // in the wrong direction: `stringly` hits are frequently correct (external
    // protocol strings genuinely are strings), so its count could never reach
    // zero, so it could never gate, so the audit's advisory tier stayed
    // permanently non-empty and there was nothing a reader could do about it.
    let waived = ctx.retain_unsuppressed("stringly", &mut all, |h| {
        crate::suppress::Site::keyed(h.file.as_str(), h.line, h.literal.as_str())
    });
    // Ranked, then alphabetical within a rank. `audit` shows five rows of this
    // section and the list runs to hundreds, so the order is the product: a
    // list the `match` does not cover has to be on the first screen, and sorted
    // by class alone `match-wild-list` landed below every `cmp-eq`.
    //
    // Only two ranks. The rest of the classes are one judgment per literal and
    // rank against each other by nothing in particular; inventing an order for
    // them would be a number with no measurement behind it.
    all.sort_by(|a, b| {
        rank(a.class)
            .cmp(&rank(b.class))
            .then_with(|| a.class.cmp(b.class))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    if !summary {
        match by {
            Some(GroupBy::Fn) => print_grouped_counts(ctx.out, &all, |h| h.context.clone()),
            Some(GroupBy::File) => print_grouped_counts(ctx.out, &all, |h| h.file.clone()),
            Some(GroupBy::Module) => {
                print_grouped_counts(ctx.out, &all, |h| top_module_of(&h.context).to_string())
            }
            None => {
                let today = crate::suppress::Date::today();
                for h in &all {
                    row!(
                        ctx.out,
                        "class" => h.class,
                        "literal" => h.literal.clone(),
                        "in_fn" => h.context.clone(),
                        "at" => site(&h.file, h.line),
                    );
                    ctx.suggest("stringly", Some(&h.literal), today, (&h.file, h.line));
                }
            }
        }
    }

    use std::collections::BTreeMap;
    let mut by_class: BTreeMap<&str, usize> = BTreeMap::new();
    for h in &all {
        *by_class.entry(h.class).or_insert(0) += 1;
    }
    let break_str: Vec<String> = by_class
        .iter()
        .map(|(k, n)| format!("{}={}", k, n))
        .collect();
    ctx.out.summary(&format!(
        "({} stringly hit(s); {}; include_substring={}, include_map_keys={}{}; explain: stringly)",
        all.len(),
        break_str.join(", "),
        include_substring,
        include_map_keys,
        ctx.waived_note(waived)
    ));
    // A different kind of row, so it needs saying once: every other class is
    // one judgment about one literal, and a reader who has learned to skim this
    // list will skim past the one row that is a defect rather than a candidate.
    if let Some(n) = by_class.get("match-wild-list") {
        ctx.out.note(&format!(
            "(note: {} `match-wild-list` row(s) lead the list and are a different question: a \
             `for` loop over a literal list whose `match` names fewer of them than the list \
             holds, so the trailing `_` is serving the rest. Adding a member to the list then \
             compiles, passes, and takes the wildcard's arm. The `at` is the `_`; the repair is \
             to name every member, or to drop the list and iterate the arms.)",
            n
        ));
    }
    Ok(all.len())
}
