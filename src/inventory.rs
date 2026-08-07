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

pub fn run(
    ctx: &AnalysisCtx,
    kind_filter: Option<ItemKind>,
    vis_filter: Option<VisFilter>,
    tree: bool,
) -> anyhow::Result<usize> {
    let summary = ctx.summary;
    let mut all: Vec<&Defn> = ctx.idx.iter().collect();

    if let Some(k) = kind_filter {
        all.retain(|d| d.kind == k.as_str());
    }
    if let Some(v) = vis_filter {
        all.retain(|d| d.vis == v.as_str());
    }

    if tree {
        print_tree(&all, summary);
    } else {
        all.sort_by(|a, b| {
            a.kind
                .cmp(b.kind)
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.line.cmp(&b.line))
        });
        if !summary {
            for d in &all {
                row!(
                    ctx.out,
                    "kind" => d.kind,
                    "vis" => d.vis,
                    "name" => d.qpath.clone(),
                    "at" => ctx.at(&d.file, d.line, d.end),
                );
            }
        }
    }
    ctx.out.summary(&format!("({} items)", all.len()));
    Ok(all.len())
}

fn print_tree(items: &[&Defn], summary: bool) {
    if summary {
        return;
    }
    use std::collections::BTreeMap;
    // Group by leading module path. Items with empty module path go under "<crate>".
    let mut by_mod: BTreeMap<String, Vec<&Defn>> = BTreeMap::new();
    for it in items {
        by_mod.entry(module_path_of(it)).or_default().push(it);
    }

    for (m, items) in &by_mod {
        print_module(m, items);
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
fn print_module(module: &str, items: &[&Defn]) {
    use std::collections::BTreeMap;
    println!("{}\t({} items)", module, items.len());
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for it in items {
        *by_kind.entry(it.kind).or_insert(0) += 1;
    }
    for (kind, n) in &by_kind {
        println!("  {}\t{}", n, kind);
    }
    // List items by kind, sorted within each group.
    let mut grouped: BTreeMap<&str, Vec<&Defn>> = BTreeMap::new();
    for it in items {
        grouped.entry(it.kind).or_default().push(it);
    }
    for (kind, mut its) in grouped {
        its.sort_by_key(|i| &i.qpath);
        for it in its {
            println!("    {}\t{}\t{}\t{}:{}", kind, it.vis, it.qpath, it.file, it.line);
        }
    }
}
