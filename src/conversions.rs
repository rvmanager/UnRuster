use syn::visit::{self, Visit};

use crate::ast::{line_of, print_grouped_counts, scope_visits, ScopeTracker, top_module_of};
use crate::context::{AnalysisCtx, GroupBy};
use crate::parse::display_path;
use crate::emit::{row, site};

#[derive(Debug)]
struct Hit {
    kind: &'static str,
    target: String, // dst type when visible (e.g. `::from` target, `::<T>` turbofish)
    context: String,
    file: String,
    line: usize,
}

struct ConvVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    hits: Vec<Hit>,
}

impl<'a> ConvVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    fn record(&mut self, kind: &'static str, target: String, line: usize) {
        let ctx = self.enclosing();
        self.hits.push(Hit {
            kind,
            target,
            context: ctx,
            file: self.file.to_string(),
            line,
        });
    }
}

/// Recognized zero-arg conversion methods and their row labels — one data
/// table instead of a 13-arm match (its own `metrics --sort cyclo` flagged
/// the match form).
const METHOD_LABELS: &[(&str, &str)] = &[
    ("into", ".into"),
    ("try_into", ".try_into"),
    ("to_string", ".to_string"),
    ("to_owned", ".to_owned"),
    ("to_vec", ".to_vec"),
    ("as_str", ".as_str"),
    ("as_bytes", ".as_bytes"),
    ("as_ref", ".as_ref"),
    ("as_mut", ".as_mut"),
    ("parse", ".parse"),
    ("cloned", ".cloned"),
    ("copied", ".copied"),
    ("collect", ".collect"),
];

fn first_is_uppercase(s: &str) -> bool {
    s.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
}

/// `--kind` filter values. Value names keep the sigil the row labels use
/// (`.into`, `::from`) so filters read the same as the output.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ConvKind {
    #[value(name = ".into")]
    Into,
    #[value(name = ".try_into")]
    TryInto,
    #[value(name = ".to_string")]
    ToString,
    #[value(name = ".to_owned")]
    ToOwned,
    #[value(name = ".to_vec")]
    ToVec,
    #[value(name = ".as_str")]
    AsStr,
    #[value(name = ".as_bytes")]
    AsBytes,
    #[value(name = ".as_ref")]
    AsRef,
    #[value(name = ".as_mut")]
    AsMut,
    #[value(name = ".parse")]
    Parse,
    #[value(name = ".cloned")]
    Cloned,
    #[value(name = ".copied")]
    Copied,
    #[value(name = ".collect")]
    Collect,
    #[value(name = "::from")]
    From,
    #[value(name = "::try_from")]
    TryFrom,
}

impl ConvKind {
    fn as_str(self) -> &'static str {
        match self {
            ConvKind::Into => ".into",
            ConvKind::TryInto => ".try_into",
            ConvKind::ToString => ".to_string",
            ConvKind::ToOwned => ".to_owned",
            ConvKind::ToVec => ".to_vec",
            ConvKind::AsStr => ".as_str",
            ConvKind::AsBytes => ".as_bytes",
            ConvKind::AsRef => ".as_ref",
            ConvKind::AsMut => ".as_mut",
            ConvKind::Parse => ".parse",
            ConvKind::Cloned => ".cloned",
            ConvKind::Copied => ".copied",
            ConvKind::Collect => ".collect",
            ConvKind::From => "::from",
            ConvKind::TryFrom => "::try_from",
        }
    }
}

impl<'ast, 'a> Visit<'ast> for ConvVisitor<'a> {
    scope_visits!(item_mod, item_impl, item_trait, item_fn, impl_item_fn, trait_item_fn);

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let m = e.method.to_string();
        // Every recognized conversion method is zero-arg; guard once, then
        // look the label up in the shared table.
        let kind: Option<&'static str> = if e.args.is_empty() {
            METHOD_LABELS
                .iter()
                .find(|(name, _)| *name == m)
                .map(|(_, label)| *label)
        } else {
            None
        };
        if let Some(k) = kind {
            // Extract turbofish target if present.
            let target = e
                .turbofish
                .as_ref()
                .and_then(|t| t.args.first())
                .and_then(|arg| match arg {
                    syn::GenericArgument::Type(t) => Some(crate::ast::type_to_string(t)),
                    _ => None,
                })
                .unwrap_or_else(|| "_".into());
            self.record(k, target, line_of(&e.method));
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            let segs: Vec<&syn::PathSegment> = p.path.segments.iter().collect();
            if segs.len() >= 2 {
                let last = segs[segs.len() - 1].ident.to_string();
                let pen = segs[segs.len() - 2].ident.to_string();
                if first_is_uppercase(&pen) {
                    let kind = match last.as_str() {
                        "from" => Some("::from"),
                        "try_from" => Some("::try_from"),
                        _ => None,
                    };
                    if let Some(k) = kind {
                        self.record(k, pen, line_of(&segs[segs.len() - 1].ident));
                    }
                }
            }
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

pub fn run(
    ctx: &AnalysisCtx,
    kind_filter: &[ConvKind],
    by: Option<GroupBy>,
    top: Option<usize>,
) -> anyhow::Result<usize> {
    let files = ctx.files;
    let summary = ctx.summary;
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = ConvVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }

    ctx.retain_changed(&mut all, |h| &h.file);
    if !kind_filter.is_empty() {
        let wanted: Vec<&str> = kind_filter.iter().map(|k| k.as_str()).collect();
        all.retain(|h| wanted.contains(&h.kind));
    }

    if !summary {
        match by {
            Some(GroupBy::Fn) => print_grouped_counts(&all, top, |h| h.context.clone()),
            Some(GroupBy::File) => print_grouped_counts(&all, top, |h| h.file.clone()),
            Some(GroupBy::Module) => {
                print_grouped_counts(&all, top, |h| top_module_of(&h.context).to_string())
            }
            None => {
                all.sort_by(|a, b| {
                    a.kind
                        .cmp(b.kind)
                        .then_with(|| a.file.cmp(&b.file))
                        .then_with(|| a.line.cmp(&b.line))
                });
                let rows: &[Hit] = if let Some(n) = top { &all[..all.len().min(n)] } else { &all };
                let shown = rows.len();
                if shown < all.len() {
                    // Never truncate silently: a capped list reads as the whole
                    // result set, and "20 rows" then gets treated as "20 hits".
                    ctx.out.note(&format!(
                        "(note: showing {} of {} row(s) — raise or drop --top for the rest)",
                        shown,
                        all.len()
                    ));
                }
                for h in rows {
                    row!(
                        ctx.out,
                        "kind" => h.kind,
                        "target" => h.target.clone(),
                        "context" => h.context.clone(),
                        "at" => site(&h.file, h.line),
                    );
                }
            }
        }
    }

    use std::collections::BTreeMap;
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for h in &all {
        *by_kind.entry(h.kind).or_insert(0) += 1;
    }
    let break_str: Vec<String> = by_kind.iter().map(|(k, n)| format!("{}={}", k, n)).collect();
    ctx.out.summary(&format!(
        "({} conversion call(s); {})",
        all.len(),
        break_str.join(", ")
    ));
    Ok(all.len())
}
