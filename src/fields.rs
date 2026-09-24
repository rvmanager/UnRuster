use syn::visit::Visit;

use crate::ast::{line_of, type_to_string, vis_str};
use crate::context::AnalysisCtx;
use crate::parse::display_path;
use crate::emit::site;

#[derive(Debug)]
struct FieldDef {
    name: String,
    ty: String,
    vis: &'static str,
    file: String,
    line: usize,
    /// The field's attributes other than its doc comment, rendered as written
    /// — `#[serde(skip_serializing_if = "Vec::is_empty", default)]`.
    attrs: Vec<String>,
}

/// One attribute as source text, for the `attrs` column.
///
/// Rendered from tokens rather than sliced from the file, with Rust's own
/// spacing (`a::b`, `f(x, y)`, `k = v`) so the column reads like the source.
/// Token-aware rather than a string tidy-up over `to_string()`, which would
/// also re-space the inside of a string literal.
fn render_attr(a: &syn::Attribute) -> String {
    let inner = if matches!(a.style, syn::AttrStyle::Inner(_)) { "!" } else { "" };
    format!("#{}[{}]", inner, render_tokens(quote::ToTokens::to_token_stream(&a.meta)))
}

fn render_tokens(ts: proc_macro2::TokenStream) -> String {
    use proc_macro2::{Delimiter, Spacing, TokenTree};
    let mut out = String::new();
    // Whether the last token was a word (ident or literal): two words in a
    // row need a space between them, nothing else does.
    let mut after_word = false;
    for tt in ts {
        match tt {
            TokenTree::Ident(i) => {
                if after_word {
                    out.push(' ');
                }
                out.push_str(&i.to_string());
                after_word = true;
            }
            TokenTree::Literal(l) => {
                if after_word {
                    out.push(' ');
                }
                out.push_str(&l.to_string());
                after_word = true;
            }
            TokenTree::Punct(p) => {
                match p.as_char() {
                    '=' if p.spacing() == Spacing::Alone => out.push_str(" = "),
                    ',' => out.push_str(", "),
                    c => out.push(c),
                }
                after_word = false;
            }
            TokenTree::Group(g) => {
                let (open, close) = match g.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::Brace => ("{", "}"),
                    Delimiter::None => ("", ""),
                };
                out.push_str(open);
                out.push_str(render_tokens(g.stream()).trim_end_matches(", "));
                out.push_str(close);
                after_word = false;
            }
        }
    }
    out
}

/// Locate field definitions for a given struct (or struct-like enum variant container).
struct FieldDefVisitor<'a> {
    target_type: &'a str,
    file: &'a str,
    out: Vec<FieldDef>,
}

impl<'ast, 'a> Visit<'ast> for FieldDefVisitor<'a> {
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        if i.ident == self.target_type {
            if let syn::Fields::Named(fs) = &i.fields {
                for f in &fs.named {
                    if let Some(id) = &f.ident {
                        self.out.push(FieldDef {
                            name: id.to_string(),
                            ty: type_to_string(&f.ty),
                            vis: vis_str(&f.vis),
                            file: self.file.to_string(),
                            line: line_of(id),
                            attrs: f
                                .attrs
                                .iter()
                                .filter(|a| !a.path().is_ident("doc"))
                                .map(render_attr)
                                .collect(),
                        });
                    }
                }
            }
        }
    }
}

/// `show_attrs` is `--attrs`: an `attrs` column after the site. JSON always
/// carries it — a named key costs a document consumer nothing, where a new TSV
/// column would shift every reader counting tabs.
pub fn run(ctx: &AnalysisCtx, ty: &str, show_attrs: bool) -> anyhow::Result<usize> {
    // Targets resolve by last `::` segment throughout this tool — the playbook
    // says so, `impls --of` and `callers` already do it, and `show` prints the
    // *qualified* path in its header row. These three commands compared the raw
    // string against a bare `ident`, so the qualified name a reader had just
    // read off `show` silently matched nothing: `fields index::Defn` answered
    // "(0 field(s))" plus a note contradicting itself ("not as a struct with
    // named fields — it is: struct"). A command that says "none" for a copied
    // name teaches the reader it does not work.
    let ty = crate::ast::last_segment(ty);
    let files = ctx.files;
    let summary = ctx.summary;
    // 1. Collect field definitions for the target type from all files.
    let mut defs: Vec<FieldDef> = Vec::new();
    for f in files {
        let mut v = FieldDefVisitor {
            target_type: ty,
            file: &display_path(&f.path),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        defs.extend(v.out);
    }

    // No `(0 field(s) on …)` line first. It printed above the explanation it
    // contradicted — "0 fields on `RoutingReport`", then "no struct
    // `RoutingReport`" — and a count asserts the thing it counts exists. The
    // miss is explained by `unknown_target`, and `main` says what exit 2 means.
    if defs.is_empty() {
        return Err(ctx.unknown_target("struct with named fields", ty));
    }

    // 2. Count read/write/init sites per field, via the same strict collector
    //    `field-uses` uses — so these counts equal the sum of its rows.
    let mut write_only: Vec<&str> = Vec::new();
    for fd in &defs {
        let (reads, writes, inits) =
            crate::field_uses::count_kinds(files, ty, &fd.name, &ctx.sem.fn_sigs);
        // Built and never read. `r:0` was already in the row; naming it is the
        // difference between a count and a finding — see `note_write_only`.
        if reads == 0 && inits > 0 {
            write_only.push(&fd.name);
        }
        if !summary {
            let mut cells: Vec<(&'static str, crate::emit::Val)> = vec![
                ("vis", fd.vis.into()),
                ("name", fd.name.clone().into()),
                ("type", fd.ty.clone().into()),
                ("reads", format!("r:{}", reads).into()),
                ("writes", format!("w:{}", writes).into()),
                ("inits", format!("i:{}", inits).into()),
                ("at", site(&fd.file, fd.line)),
            ];
            if show_attrs || ctx.out.format == crate::emit::Format::Json {
                let a = if fd.attrs.is_empty() { "—".to_string() } else { fd.attrs.join(" ") };
                cells.push(("attrs", a.into()));
            }
            ctx.out.row(cells);
        }
    }
    ctx.out.summary(&format!(
        "({} field(s) on `{}`; use `unruster field-uses {} <field>` for site details)",
        defs.len(),
        ty,
        ty
    ));
    // The column is opt-in, so say when it would have had something in it.
    // What a session wanted from `fields` was whether `unrouted` carried
    // `skip_serializing_if` — the one fact the rows did not hold — and it went
    // to `grep -B1` for it.
    let with_attrs = defs.iter().filter(|d| !d.attrs.is_empty()).count();
    if !show_attrs && with_attrs > 0 && ctx.out.format != crate::emit::Format::Json {
        ctx.out.advice(&format!(
            "(note: {} of these field(s) carry attributes (serde, cfg, …) — `--attrs` prints them)",
            with_attrs
        ));
    }
    if !write_only.is_empty() {
        ctx.out.note(&format!(
            "note: written and never read: {} — no site reads {}. Confirm with `--scope all` \
             before removing: a field read only from tests looks exactly like this.",
            write_only.join(", "),
            if write_only.len() == 1 { "it" } else { "them" }
        ));
    }
    Ok(defs.len())
}
