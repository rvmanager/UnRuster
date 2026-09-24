//! `inventory` — every top-level item in the scanned tree.
//!
//! Reads [`crate::index::NameIndex`] rather than walking the AST itself. It
//! used to have its own visitor, which was the same twelve `visit_item_*`
//! bodies with the same qualification rules — and the two had already begun to
//! answer differently, because only one of them knew where an item *ends*.
//! Under `--spans` this command must report `file:start-end`, and a second
//! opinion about where a `fn` stops is exactly the kind of drift that shows up
//! as an off-by-a-few source range rather than as a failing test.

use crate::context::AnalysisCtx;
use crate::index::Defn;
use crate::emit::row;

/// `--kind` filter values. Kebab-cased by clap (TraitFn → `trait-fn`).
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ItemKind {
    Struct,
    Enum,
    Trait,
    Fn,
    Impl,
    Mod,
    Const,
    Static,
    Type,
    TraitFn,
    ImplFn,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Struct => "struct",
            ItemKind::Enum => "enum",
            ItemKind::Trait => "trait",
            ItemKind::Fn => "fn",
            ItemKind::Impl => "impl",
            ItemKind::Mod => "mod",
            ItemKind::Const => "const",
            ItemKind::Static => "static",
            ItemKind::Type => "type",
            ItemKind::TraitFn => "trait-fn",
            ItemKind::ImplFn => "impl-fn",
        }
    }
}

/// `--vis` filter values.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum VisFilter {
    Pub,
    Crate,
    Priv,
}

impl VisFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            VisFilter::Pub => "pub",
            VisFilter::Crate => "pub(crate)",
            VisFilter::Priv => "priv",
        }
    }
}

/// How a listing is ordered. `outline` defaults to `Source` because an outline
/// read out of order is a list; `inventory` defaults to `Kind` because a
/// whole-tree listing is read as a census.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ItemSort {
    /// Kind, then file, then line.
    Kind,
    /// File, then line — the order the code is written in.
    Source,
}

/// Does `--name <pat>` select this item?
///
/// A bare pattern matches the **last segment**, so `--name Options` is not
/// defeated by every item's module prefix. A pattern containing `::` matches
/// any whole-segment suffix of the qualified path — the same rule `show`
/// resolves a name by, so `--name 'Document::*'` means here what
/// `show Document::new` means there. Without the qualified form the only way to
/// list one type's methods was `inventory --kind impl-fn | grep 'Document::'`,
/// which this command's own playbook was recommending.
///
/// The whole path is one of the suffixes tried, so a *leading* glob narrows by
/// crate or module: `--name 'crates::fab-vcs::*'`. That has always worked and
/// was never written down, so on a 23-crate workspace 13 of 14 `inventory`
/// calls were `inventory | grep <crate-name>` — which matches the file path and
/// the doc column as readily as the name, and discards the item count and the
/// `--top` cut along with them.
pub(crate) fn name_matches(pat: &str, qpath: &str) -> bool {
    use crate::ast::{glob_match_smart, last_segment};
    if !pat.contains("::") {
        return glob_match_smart(pat, last_segment(qpath));
    }
    std::iter::once(qpath)
        .chain(qpath.match_indices("::").map(|(i, _)| &qpath[i + 2..]))
        .any(|suffix| glob_match_smart(pat, suffix))
}

/// Keep the items any of `pats` selects, and return the patterns that selected
/// none — the `--name` filter, for every listing that has one.
///
/// Several patterns because the question is often several names: one session
/// wanted eight (`material|vocab|look|rise|decal|shape|stair|grain`) out of
/// an outline, could give `--name` one, and piped to `grep -iE` — which the
/// tool's own note tells the reader not to do, and which also matched a note
/// line on "lookup". A pattern that matched nothing is returned rather than
/// lost in the union: one typo among eight is invisible in a non-empty listing.
pub(crate) fn retain_by_name(items: &mut Vec<&Defn>, pats: &[String]) -> Vec<String> {
    if pats.is_empty() {
        return Vec::new();
    }
    let mut hit = vec![false; pats.len()];
    items.retain(|d| {
        let path = match_path(d);
        let mut any = false;
        for (i, pat) in pats.iter().enumerate() {
            if name_matches(pat, &path) {
                hit[i] = true;
                any = true;
            }
        }
        any
    });
    pats.iter()
        .zip(hit)
        .filter(|(_, h)| !h)
        .map(|(p, _)| p.clone())
        .collect()
}

/// `--name` as the summary line spells it back: the patterns, `|`-joined —
/// the same spelling the flag accepts.
pub(crate) fn name_filter_label(pats: &[String]) -> String {
    if pats.is_empty() {
        String::new()
    } else {
        format!("; --name {}", pats.join("|"))
    }
}

/// The path-shaped spelling of an item, for [`name_matches`].
///
/// An `impl` header's `qpath` is its rendered `impl Trait for Type` line, not a
/// path — so a module- or crate-prefixed `--name` matched none of them and
/// silently dropped every impl block in the crate it selected. On a 23-crate
/// workspace `--name 'crates::fab-features::*'` returned 49 rows where
/// `inventory | grep fab-features` returned 59, and all 10 of the difference
/// were impl headers. A reader comparing the two concludes the flag is broken
/// and goes back to the grep — which is the habit the flag exists to replace.
///
/// Its `module` is a real path, so that is what the pattern is offered.
pub(crate) fn match_path(d: &Defn) -> String {
    if d.kind == "impl" && !d.module.is_empty() {
        format!("{}::{}", d.module, d.name)
    } else {
        d.qpath.clone()
    }
}

pub fn run(
    ctx: &AnalysisCtx,
    kind_filter: Option<ItemKind>,
    vis_filter: Option<VisFilter>,
    name_filter: &[String],
    tree: bool,
    sort: ItemSort,
    docs: bool,
) -> anyhow::Result<usize> {
    let summary = ctx.summary;
    let mut all: Vec<&Defn> = ctx.idx.iter().collect();

    if let Some(k) = kind_filter {
        all.retain(|d| d.kind == k.as_str());
    }
    if let Some(v) = vis_filter {
        all.retain(|d| d.vis == v.as_str());
    }
    // The filter that was missing. `--kind`/`--vis` narrow by category, and a
    // reader looking for one *name* had nothing — so the shape that showed up
    // in practice was `unruster inventory | grep -iE "profile|span"`, which
    // costs the row count and the `--top` cut along with the stderr it drops,
    // and matches the file path and the doc summary as happily as the name.
    // Matching on the last segment keeps `--name Options` from being defeated
    // by every item's module prefix.
    let unmatched = retain_by_name(&mut all, name_filter);

    if tree {
        print_tree(ctx, &all);
    } else {
        match sort {
            ItemSort::Kind => all.sort_by(|a, b| {
                a.kind
                    .cmp(b.kind)
                    .then_with(|| a.file.cmp(&b.file))
                    .then_with(|| a.line.cmp(&b.line))
            }),
            ItemSort::Source => {
                all.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)))
            }
        }
        if !summary {
            for d in &all {
                // The same five cells `outline` emits, in the same order.
                // These two commands list the same items from the same index —
                // `inventory --root x.rs` and `outline x.rs` differed only in
                // that one carried `loc` and a line range and the other did
                // not, so a consumer could parse one and not the other.
                let mut cells: Vec<(&'static str, crate::emit::Val)> = vec![
                    ("kind", crate::emit::Val::from(d.kind)),
                    ("vis", crate::emit::Val::from(d.vis)),
                    ("loc", crate::emit::Val::from(d.end.saturating_sub(d.line) + 1)),
                    ("name", crate::emit::Val::from(d.qpath.clone())),
                    ("at", ctx.at(&d.file, d.line, d.end)),
                ];
                if docs {
                    cells.push((
                        "doc",
                        crate::emit::Val::from(d.doc.clone().unwrap_or_else(|| "—".into())),
                    ));
                }
                ctx.out.row(cells);
            }
        }
    }
    ctx.out.summary(&format!(
        "({} items{})",
        all.len(),
        name_filter_label(name_filter)
    ));
    // A glob that matches nothing is a typo far more often than a fact about
    // the tree, and an empty listing plus `(0 items)` says neither. `show`
    // answers an unresolvable name with the near names; this is the same
    // courtesy for the pattern that is one character off.
    // The listing people reach for `| grep` on. Named here rather than only in
    // `--help`, because the observed shape was `inventory | grep -i mask`
    // written by a reader who never opened the help — and a grep matches the
    // file path and the doc column as readily as the name, takes this count
    // down with the stderr it redirects, and hides the `--top` cut.
    note_name_filter(ctx, name_filter, &unmatched, all.len(), !tree);
    Ok(all.len())
}

/// What `--name` has to say about this listing: that it exists, or that it
/// matched nothing.
///
/// Shared with `outline`, which grew the flag for the same reason and would
/// otherwise have grown a second, drifting copy of these sentences — the
/// failure `--vis` and `--pub-only` already had across the three commands that
/// filter by visibility.
///
/// `offer` is false where the pointer would be wrong: `--tree` is already a
/// narrowing view, so suggesting a narrower one is noise.
pub(crate) fn note_name_filter(
    ctx: &AnalysisCtx,
    pats: &[String],
    unmatched: &[String],
    shown: usize,
    offer: bool,
) {
    // A listing long enough to scroll is where the reader reaches for `grep`
    // — which also matches the path and the doc column, and discards the
    // count and the `--top` cut along with the stderr it redirects.
    if pats.is_empty() && offer && shown > 40 {
        // `advice`, not `note`: this sentence is aimed at the reader who is
        // about to pipe the listing into `grep`, and on stdout their own grep
        // is what deletes it. See `Out::advice`.
        ctx.out.advice(
            "note: `--name <glob>` narrows by name — `*` the only metacharacter, smartcase, \
             `Type::*` for one type's members, a leading glob for a crate or module \
             (`--name 'crates::fab-vcs::*'`), and `|` for several at once \
             (`--name '*stair*|*decal*'`). Prefer it to `| grep`, which also matches \
             the path and the doc column and discards the count above.",
        );
    }
    if unmatched.is_empty() {
        return;
    }
    // Some patterns matched and some did not: the listing is not empty, so
    // nothing else would say that part of the question went unanswered.
    if shown > 0 {
        ctx.out.note(&format!(
            "note: {} of {} `--name` pattern(s) matched nothing: {} — the rows above are \
             the other patterns'. `*` is the only metacharacter, so a bare word matches a \
             whole name; `*{}*` matches names containing it.",
            unmatched.len(),
            pats.len(),
            unmatched.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", "),
            unmatched[0].trim_matches('*'),
        ));
        return;
    }
    let pat = pats.join("|");
    {
        ctx.out.note(&format!(
            "note: nothing matches `{}` — `*` is the only metacharacter, the match is on the \
             last `::` segment (a pattern with `::` matches any qualified suffix, the whole \
             path included, so `'mod::*'` narrows by module), and an all-lowercase pattern \
             already matches case-insensitively. `show {}` answers with the near names if \
             it is a typo.",
            pat,
            // `show` takes several names, so every pattern is offered at once.
            pats.iter().map(|p| p.trim_matches('*')).collect::<Vec<_>>().join(" ")
        ));
    }
}

fn print_tree(ctx: &AnalysisCtx, items: &[&Defn]) {
    if ctx.summary {
        return;
    }
    use std::collections::BTreeMap;
    // Group by leading module path. Items with empty module path go under "<crate>".
    let mut by_mod: BTreeMap<String, Vec<&Defn>> = BTreeMap::new();
    for it in items {
        by_mod.entry(module_path_of(it)).or_default().push(it);
    }

    for (m, items) in &by_mod {
        print_module(ctx, m, items);
    }
}

/// Leading module path of an item's qualified name — the prefix before the
/// first uppercase (type) segment. `inventory::Visitor::push` → `inventory`;
/// a bare `main` → `<crate>`; a `mod` item is its own path.
fn module_path_of(it: &Defn) -> String {
    if it.kind == "mod" {
        return it.qpath.clone();
    }
    let segs: Vec<&str> = it.qpath.split("::").collect();
    let keep: Vec<&str> = segs[..segs.len().saturating_sub(1)]
        .iter()
        .take_while(|s| !s.chars().next().unwrap_or('A').is_ascii_uppercase())
        .copied()
        .collect();
    if keep.is_empty() {
        "<crate>".to_string()
    } else {
        keep.join("::")
    }
}

/// Print one module's header, per-kind counts, and kind-grouped item rows.
/// Through `ctx.out`, not `println!`. Writing straight to stdout meant
/// `inventory --tree --json` emitted raw TSV instead of JSON, and dropped
/// `--fingerprints` — the same defect the grouped-count helper had.
fn print_module(ctx: &AnalysisCtx, module: &str, items: &[&Defn]) {
    use std::collections::BTreeMap;
    ctx.out.line(&format!("{}\t({} items)", module, items.len()));
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for it in items {
        *by_kind.entry(it.kind).or_insert(0) += 1;
    }
    for (kind, n) in &by_kind {
        ctx.out.line(&format!("  {}\t{}", n, kind));
    }
    // List items by kind, sorted within each group.
    let mut grouped: BTreeMap<&str, Vec<&Defn>> = BTreeMap::new();
    for it in items {
        grouped.entry(it.kind).or_default().push(it);
    }
    for (kind, mut its) in grouped {
        its.sort_by_key(|i| &i.qpath);
        for it in its {
            row!(
                ctx.out,
                "kind" => kind,
                "vis" => it.vis,
                "loc" => it.end.saturating_sub(it.line) + 1,
                "name" => it.qpath.clone(),
                "at" => ctx.at(&it.file, it.line, it.end),
            );
        }
    }
}
