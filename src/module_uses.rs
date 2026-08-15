//! `module-uses <module>` — what the rest of the tree takes from one module.
//!
//! The removal-planning question, and the one this tool had no command for.
//! From a session deciding whether a 3,772-line `trace` module could go, in
//! order, all five of these:
//!
//! ```text
//! grep -rhoE '\btrace::[a-z_A-Z]+' src/ --include=*.rs | sort | uniq -c | sort -rn
//! grep -rlE '\btrace::' src --include='*.rs'
//! for f in report edit json measure; do grep -noE 'trace::[a-zA-Z_]+' src/$f.rs; done
//! grep -rnE 'trace::[a-zA-Z_]+' src --include='*.rs' | grep -v '^src/trace.rs'
//! grep -rn 'refine::' src --include='*.rs' | grep -v '^src/refine.rs'
//! ```
//!
//! `type-refs` answers this for one type and `callers` for one fn; neither
//! answers it for a module, which is the unit a removal is scoped by.
//!
//! Two things those greps could not do, and both cost that session a round
//! trip. The first: four of the eight hits outside `trace.rs` were *doc
//! comment* references — ``[`crate::trace::labelled`]`` inside a `///` — and
//! separating them took a second pass with `-n` and an eyeball. Doc comments
//! have been parsed here from the start, so the `via` column simply says which.
//! The second: grep reports the spelling, not the item. `trace::round` and a
//! bare `round` brought in by a `use` are one coupling written two ways, and
//! `grep -rhoE '\btrace::'` never sees the second.
//!
//! What this does not do is decide. A row is a site; whether the coupling is
//! load-bearing is the reader's call, which is why the summary counts rather
//! than concludes.

use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{fn_span, last_segment, line_of, scope_visits, ScopeTracker};
use crate::context::{AnalysisCtx, Confidence, GroupBy};
use crate::emit::{site, Val};
use crate::index::Defn;
use crate::parse::display_path;
use crate::semantic::UseMap;

/// One reference from outside the module to something inside it.
#[derive(Debug)]
struct ModuleRef {
    /// How the site was matched — see [`conf_of_reach`].
    via: &'static str,
    /// The item's kind as the index records it (`fn`, `struct`, `const`, …).
    kind: &'static str,
    /// The referenced item's qualified path — the *definition*, not the
    /// spelling at the site, so two spellings of one item group together.
    item: String,
    /// The consuming module, so a `--by module` row and a site row agree.
    from: String,
    /// The enclosing fn (or `<top-level>`): what a reader needs in order to
    /// judge whether the coupling is load-bearing.
    context: String,
    file: String,
    line: usize,
}

impl ModuleRef {
    /// Is this linkage the compiler enforces, rather than prose about it?
    ///
    /// One predicate because the question is asked three times — the
    /// `--no-docs` filter, the summary's code/doc split, and the per-group
    /// tally — and `unruster stringly` reported all three as separate literal
    /// comparisons on this module's own `via` vocabulary. Three spellings of
    /// one question is three chances for them to stop agreeing.
    fn is_code(&self) -> bool {
        self.via != "doc"
    }
}

/// `path` and `use` are code linkage; `doc` is prose.
///
/// A qualified `trace::labelled` is the item and nothing else. A bare
/// `labelled` under `use crate::trace::labelled` is equally certain, but only
/// because the use-map said so — reported separately, because a reader checking
/// a row wants to know which fact carried it.
///
/// There is deliberately no bare-name tier, and the reason is Rust's: reaching
/// an item in another module takes a qualified path or a `use`, and there is no
/// third way. A tier that matched a bare spelling could therefore only ever add
/// false positives, and did — a first cut of this command reported 5,869 uses
/// of `index`, of which 5,794 were local bindings called `doc` colliding with a
/// private `index::Spot::doc`. `type-refs` can match by last segment because a
/// *type* name is distinctive in a way a method name is not.
///
/// `doc` is `Inferred` rather than `Resolved` deliberately: prose naming an
/// item is evidence about intent, not about linkage, and a rename breaks it
/// silently where the compiler catches the other two.
///
/// Deliberately *not* [`crate::field_uses`]'s `conf_of_via`, which the
/// pre-write gate reports as the same signature: that one grades
/// `self`/`init`/`ti`/`?`, a disjoint vocabulary about receiver knowledge. One
/// fn over both would be a `match` on seven labels from two commands that share
/// no meaning — the shape is the same and the concept is not.
fn conf_of_reach(via: &str) -> Confidence {
    match via {
        "path" | "use" => Confidence::Resolved,
        _ => Confidence::Inferred,
    }
}

/// The module paths that count as *inside* the target.
///
/// Descendants are inside: a removal takes `geom::build::helpers` along with
/// `geom::build`, so traffic between them is internal and reporting it would
/// drown the rows that matter. The target itself resolves by suffix, the same
/// way every other name here does — `build` finds `geom::build`.
fn inside_modules(ctx: &AnalysisCtx, target: &str) -> Vec<String> {
    let target = target.trim_end_matches("::");
    let suffix = format!("::{}", target);
    let mut roots: Vec<String> = ctx
        .idx
        .iter()
        .map(|d| d.module.clone())
        .filter(|m| m == target || m.ends_with(&suffix))
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

fn is_inside(roots: &[String], module: &str) -> bool {
    roots
        .iter()
        .any(|r| module == r || module.starts_with(&format!("{}::", r)))
}

/// Every item declared inside the module.
///
/// `mod` headers name the module itself and an `impl` header's name is its
/// self-type — already reachable through the type's own defn. Neither is
/// something an outside site can name.
fn declared_inside<'a>(ctx: &'a AnalysisCtx, roots: &[String]) -> Vec<&'a Defn> {
    ctx.idx
        .iter()
        .filter(|d| !matches!(d.kind, "mod" | "impl"))
        .filter(|d| is_inside(roots, &d.module))
        .collect()
}

struct ReachVisitor<'a> {
    roots: &'a [String],
    /// Bare name → a definition inside the module carrying it.
    items: &'a std::collections::BTreeMap<String, &'a Defn>,
    /// The module's own last segment, as a site would spell it.
    tail: &'a str,
    /// Use-maps from the file's top level inward, one per enclosing inline
    /// `mod`. Resolved innermost-first — `mod tests { use super::*; }` is the
    /// commonest nested scope in Rust, and a file-level map cannot see it.
    ums: Vec<UseMap>,
    idx: &'a crate::index::NameIndex,
    file: &'a str,
    scope: ScopeTracker,
    out: Vec<ModuleRef>,
}

impl ReachVisitor<'_> {
    /// The module this site sits in — the file's own path plus any inline
    /// `mod` blocks around it.
    fn here(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if !self.scope.module.is_empty() {
            parts.push(self.scope.module.as_str());
        }
        parts.extend(self.scope.mod_stack.iter().map(String::as_str));
        parts.join("::")
    }

    /// Am I inside the module under examination? Asked per site rather than per
    /// file, so an inline `mod trace { … }` elsewhere is treated the same way
    /// `trace.rs` would be.
    fn in_target(&self) -> bool {
        is_inside(self.roots, &self.here())
    }

    /// Does a `use` in scope bring this bare name in from the target module?
    ///
    /// Innermost scope first: an inline `mod` can shadow the file's import, and
    /// the nearer one is the one that binds.
    fn imported_from_target(&self, name: &str) -> bool {
        self.ums.iter().rev().any(|um| {
            um.resolve(name, self.idx)
                .is_some_and(|q| is_inside(self.roots, crate::ast::module_of_path(&q)))
        })
    }

    fn record(&mut self, via: &'static str, d: &Defn, line: usize) {
        let from = self.here();
        self.out.push(ModuleRef {
            via,
            kind: d.kind,
            item: d.qpath.clone(),
            from: if from.is_empty() {
                "<crate root>".to_string()
            } else {
                from
            },
            context: self.scope.enclosing(),
            file: self.file.to_string(),
            line,
        });
    }

    /// Classify one written path against the module.
    ///
    /// Scans *every* segment rather than the last: `Kind::Circle` names the
    /// inside enum `Kind` in its second-to-last position, and a variant match
    /// site is exactly the coupling a removal has to find. First match wins, so
    /// one path yields one row.
    fn reach_of_path(&mut self, p: &syn::Path) {
        if self.in_target() {
            return;
        }
        let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
        for (i, s) in segs.iter().enumerate() {
            let Some(d) = self.items.get(s.as_str()).copied() else {
                continue;
            };
            let via = if i > 0 && segs[i - 1] == self.tail {
                // Written through the module: `trace::labelled`, `crate::trace::labelled`.
                "path"
            } else if self.imported_from_target(s) {
                // Imported here — including through a `use mod::*;`, which the
                // use-map resolves against the index rather than guessing.
                "use"
            } else {
                // A bare spelling with nothing importing it. In Rust that is a
                // local, a parameter or someone else's item — never this
                // module's. See `conf_of_reach` for what assuming otherwise
                // cost.
                continue;
            };
            let line = line_of(&p.segments[i].ident);
            self.record(via, d, line);
            return;
        }
    }

    /// `use crate::trace::labelled;` — a coupling with no expression to visit,
    /// and the first line a removal has to delete.
    fn reach_of_use(&mut self, u: &syn::ItemUse) {
        if self.in_target() {
            return;
        }
        let mut names: Vec<(String, usize)> = Vec::new();
        use_leaves(&u.tree, &mut names);
        for (n, line) in names {
            if let Some(d) = self.items.get(n.as_str()).copied() {
                self.record("use", d, line);
            }
        }
    }

    /// Doc comments naming `module::Item`.
    ///
    /// Only the qualified spelling: an intra-doc link is written
    /// ``[`crate::trace::labelled`]`` or ``[`trace::labelled`]``, where a bare
    /// word in prose is just a word. This is the class the greps could not
    /// separate — four of the eight `trace::` hits outside `trace.rs` in the
    /// session that motivated this command, every one of them read as a call
    /// site until a second pass said otherwise.
    fn reach_of_docs(&mut self, attrs: &[syn::Attribute], line: usize) {
        if self.in_target() {
            return;
        }
        let needle = format!("{}::", self.tail);
        for doc in crate::ast::doc_lines(attrs) {
            let mut from = 0usize;
            while let Some(i) = doc[from..].find(&needle) {
                from += i + needle.len();
                let rest = &doc[from..];
                let end = rest
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len());
                if let Some(d) = self.items.get(&rest[..end]).copied() {
                    self.record("doc", d, line);
                }
            }
        }
    }
}

/// Every leaf name a `use` tree brings into scope, with its line.
fn use_leaves(t: &syn::UseTree, out: &mut Vec<(String, usize)>) {
    match t {
        syn::UseTree::Path(p) => use_leaves(&p.tree, out),
        syn::UseTree::Name(n) => out.push((n.ident.to_string(), line_of(&n.ident))),
        // `use trace::labelled as l;` — the item is still `labelled`.
        syn::UseTree::Rename(r) => out.push((r.ident.to_string(), line_of(&r.ident))),
        syn::UseTree::Group(g) => {
            for t in &g.items {
                use_leaves(t, out);
            }
        }
        // A glob names nothing in particular; the bare references it enables
        // are caught at their own sites through the use-map's `globs`.
        syn::UseTree::Glob(_) => {}
    }
}

impl<'ast> Visit<'ast> for ReachVisitor<'_> {
    scope_visits!(item_impl, item_fn);

    // Hand-rolled rather than a `scope_visits!` arm: entering an inline module
    // enters a new *import* scope as well as a new name scope, and the macro
    // only knows about the second.
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        self.scope.enter_mod(m.ident.to_string());
        let pushed = match &m.content {
            Some((_, items)) => {
                let here = self.here();
                self.ums.push(UseMap::build_in_items(items, &here));
                true
            }
            None => false,
        };
        visit::visit_item_mod(self, m);
        if pushed {
            self.ums.pop();
        }
        self.scope.leave_mod();
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        self.reach_of_path(p);
        visit::visit_path(self, p);
    }

    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        self.reach_of_use(u);
    }

    // Doc comments hang off items, so they are read where the item is rather
    // than in a second pass over the file's text.
    fn visit_item(&mut self, i: &'ast syn::Item) {
        if let Some(attrs) = crate::ast::item_attrs(i) {
            self.reach_of_docs(attrs, i.span().start().line);
        }
        visit::visit_item(self, i);
    }

    // Hand-rolled rather than a `scope_visits!` arm because an impl method's
    // own doc comment is a reference site too, and the macro's arm cannot see
    // it.
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.reach_of_docs(&i.attrs, line_of(&i.sig.ident));
        self.scope
            .enter_fn(i.sig.ident.to_string(), fn_span(&i.sig, &i.block));
        visit::visit_impl_item_fn(self, i);
        self.scope.leave_fn();
    }

    // `assert_eq!(trace::round(x), 1.0)` is a call site, and a plain `Visit`
    // walk stops at the macro. Bodies that will not parse are reported by
    // `blind-spots`, as everywhere else.
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for e in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&e);
        }
    }
}

pub struct ModuleUsesOpts {
    /// Aggregate instead of listing sites. `Fn` — the default — lists them.
    pub by: GroupBy,
    /// Drop rows below this tier. `--min-confidence resolved` hides the
    /// bare-name guesses and the doc references in one flag.
    pub min_confidence: Option<Confidence>,
    /// Keep `doc` rows. On by default: separating prose from linkage is half
    /// the point of the command.
    pub include_docs: bool,
}

/// Scan the tree, filter, and put the sites in reading order.
///
/// Separated from [`run`], which otherwise reads as resolve-scan-filter-sort-
/// dedup-render-summarise in one body and hits this tool's own god-function
/// threshold doing it.
fn gather(
    ctx: &AnalysisCtx,
    roots: &[String],
    items: &std::collections::BTreeMap<String, &Defn>,
    tail: &str,
    opts: &ModuleUsesOpts,
) -> Vec<ModuleRef> {
    let mut all: Vec<ModuleRef> = Vec::new();
    for f in ctx.files {
        let mut v = ReachVisitor {
            roots,
            items,
            tail,
            ums: vec![UseMap::build_in(&f.ast, f.module.as_str())],
            idx: ctx.idx,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.out);
    }

    if !opts.include_docs {
        all.retain(ModuleRef::is_code);
    }
    if let Some(min) = opts.min_confidence {
        all.retain(|u| conf_of_reach(u.via) >= min);
    }
    ctx.retain_changed(&mut all, |u| &u.file);
    all.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.item.cmp(&b.item))
    });
    // One path is visited as itself and again as part of its parent, so the
    // same site can arrive twice. One site is one row.
    all.dedup_by(|a, b| a.file == b.file && a.line == b.line && a.item == b.item);
    all
}

pub fn run(ctx: &AnalysisCtx, target: &str, opts: &ModuleUsesOpts) -> anyhow::Result<usize> {
    use std::collections::{BTreeMap, BTreeSet};

    let roots = inside_modules(ctx, target);
    if roots.is_empty() {
        return Err(no_such_module(ctx, target));
    }
    if roots.len() > 1 {
        ctx.out.note(&format!(
            "note: `{}` matches {} modules ({}) — reporting all of them; pass a longer \
             path to pick one",
            target,
            roots.len(),
            roots.join(", ")
        ));
    }
    // The summary names the target once and must stay one readable line.
    // Spelling out every match put 26 module paths — `arith_drift::tests`
    // through `workspace::tests` — inside a sentence about counts, twice, on a
    // bare `module-uses tests`. The note above already lists them, which is
    // where a list belongs.
    let label = match roots.len() {
        1 => roots[0].clone(),
        n => format!("{} ({} modules)", target, n),
    };
    let tail = last_segment(roots[0].as_str()).to_string();

    let defns = declared_inside(ctx, &roots);
    if defns.is_empty() {
        ctx.out.summary(&format!(
            "(0 use(s) of `{}` from outside it; the module declares no items)",
            label
        ));
        return Ok(0);
    }
    let items: BTreeMap<String, &Defn> = defns.iter().map(|d| (d.name.clone(), *d)).collect();

    let all = gather(ctx, &roots, &items, &tail, opts);

    let shown = match opts.by {
        GroupBy::Fn => list_sites(ctx, &all),
        GroupBy::File => collapse(ctx, &all, "item", |u| u.item.clone()),
        GroupBy::Module => collapse(ctx, &all, "from", |u| u.from.clone()),
    };

    let code = all.iter().filter(|u| u.is_code()).count();
    let consumers: BTreeSet<&str> = all.iter().map(|u| u.from.as_str()).collect();
    let reached: BTreeSet<&str> = all.iter().map(|u| u.item.as_str()).collect();
    ctx.out.summary(&format!(
        "({} site(s) outside `{}` across {} module(s): {} in code, {} in doc comments; \
         {} of the module's {} item(s) are reached{})",
        all.len(),
        label,
        consumers.len(),
        code,
        all.len() - code,
        reached.len(),
        items.len(),
        if opts.by == GroupBy::Fn {
            ""
        } else {
            "; `--by fn` for the sites"
        }
    ));
    // The judgment stays the reader's, but this shape is worth naming: prose is
    // the only thing outside the module, and a removal deletes it for free.
    if code == 0 && !all.is_empty() {
        ctx.out.note(
            "note: every site is a doc comment — no code outside this module references it. \
             A rename breaks these silently; the compiler will not.",
        );
    }
    Ok(shown)
}

/// One row per site — the default, and the shape `--context N` annotates.
fn list_sites(ctx: &AnalysisCtx, all: &[ModuleRef]) -> usize {
    if !ctx.summary {
        for u in all {
            ctx.out.row(vec![
                ("via", Val::from(u.via)),
                ("conf", Val::from(conf_of_reach(u.via).as_str())),
                ("kind", Val::from(u.kind)),
                ("item", Val::from(u.item.clone())),
                ("from", Val::from(u.from.clone())),
                ("context", Val::from(u.context.clone())),
                ("at", site(&u.file, u.line)),
            ]);
        }
    }
    all.len()
}

/// Collapse the sites onto one axis — which items are reached, or which modules
/// reach them. Both spellings of `sort | uniq -c | sort -rn` over the grep.
fn collapse(
    ctx: &AnalysisCtx,
    all: &[ModuleRef],
    key_name: &'static str,
    key: impl Fn(&ModuleRef) -> String,
) -> usize {
    use std::collections::BTreeMap;
    // (total, code sites, first site) per key.
    let mut acc: BTreeMap<String, (usize, usize, &ModuleRef)> = BTreeMap::new();
    for u in all {
        let e = acc.entry(key(u)).or_insert((0, 0, u));
        e.0 += 1;
        if u.is_code() {
            e.1 += 1;
        }
    }
    let mut rows: Vec<(String, (usize, usize, &ModuleRef))> = acc.into_iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| a.0.cmp(&b.0)));
    if !ctx.summary {
        for (k, (n, code, first)) in &rows {
            ctx.out.row(vec![
                ("count", Val::from(*n)),
                ("code", Val::from(format!("code:{}", code))),
                ("doc", Val::from(format!("doc:{}", n - code))),
                (key_name, Val::from(k.clone())),
                ("first", site(&first.file, first.line)),
            ]);
        }
    }
    rows.len()
}

/// The module did not resolve.
///
/// The usual near-name list cannot help here: a module derived from a file path
/// is not in the name index, so there is nothing for `similar` to rank. The
/// module paths themselves are the candidate set.
fn no_such_module(ctx: &AnalysisCtx, target: &str) -> anyhow::Error {
    let mut mods: Vec<&str> = ctx
        .idx
        .iter()
        .map(|d| d.module.as_str())
        .filter(|m| !m.is_empty())
        .collect();
    mods.sort_unstable();
    mods.dedup();
    let want = last_segment(target);
    let near: Vec<&str> = mods
        .iter()
        .copied()
        .filter(|m| {
            let t = last_segment(m);
            t.starts_with(want) || want.starts_with(t)
        })
        .take(6)
        .collect();
    ctx.out.answer(&format!(
        "note: no module `{}` in the scanned tree. {}",
        target,
        if near.is_empty() {
            format!(
                "{} module(s) were scanned; `inventory` lists their items with qualified \
                 paths, and the module is everything before the item's last `::`.",
                mods.len()
            )
        } else {
            format!("Did you mean: {}", near.join(", "))
        }
    ));
    crate::context::TargetNotFound::err_owned("module", target)
}
