//! `show <name>` — print one item's exact source, resolved through the AST.
//!
//! The idiom this replaces, verbatim from an agent transcript:
//!
//! ```text
//! grep -n "fn draft" src/trace.rs && sed -n "$(grep -n 'fn draft' src/trace.rs | cut -d: -f1),+70p" src/trace.rs
//! ```
//!
//! Three things are wrong with it and all three are silent. The `+70` is a
//! guess, so the output either truncates the body or drags in the next two
//! items — and nothing in the result says which. The name is a guess too: when
//! the fn is really `draft_regions` the grep matches nothing, the command
//! substitution yields an empty line number, `sed` prints nothing, and the
//! reader learns only that *something* was wrong. And a fn defined inside an
//! `impl` block is indented, so a `^fn` anchor misses it entirely.
//!
//! An AST knows where an item ends. `show` prints from the first doc-comment
//! line through the closing brace, and when the name does not resolve it says
//! what the near names are instead of printing nothing.

use crate::ast::last_segment;
use crate::context::{AnalysisCtx, TargetNotFound};
use crate::emit::{row, site, span_site, Format, Val};
use crate::index::Defn;

/// What part of the item to print.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Part {
    /// Docs, signature, and body — the whole item.
    Full,
    /// Docs and the signature, no body. For "what does this take and return"
    /// without paying for a 200-line body.
    Sig,
    /// The doc comment alone.
    Doc,
    /// No source at all: just the `file:start-end` row, so the caller's own
    /// reader can seek straight to it.
    Span,
}

pub struct ShowOpts<'a> {
    pub part: Part,
    /// Only items of this kind (`fn`, `impl-fn`, `struct`, …). The usual way
    /// to resolve `Foo` matching both a struct and its impl block.
    pub kind: Option<&'a str>,
    /// Print every match instead of listing them and stopping.
    pub all: bool,
    /// Drop the leading doc comment from a `full`/`sig` print.
    pub no_doc: bool,
    /// Prefix each source line with its number.
    pub number: bool,
}

/// The line range to print for one match, given the requested part.
///
/// `Part::Span` prints nothing but still answers with the `full` range: "which
/// lines is this item" is exactly the question it asks. `Part::Doc` is the only
/// part that can have no range — an item with no doc comment.
fn range_of(d: &Defn, o: &ShowOpts) -> Option<(usize, usize)> {
    let start = if o.no_doc { d.line } else { d.doc_start };
    match o.part {
        Part::Full | Part::Span => Some((start, d.end.max(start))),
        // `sig_end` is the declaration line for every non-fn item, so this
        // yields the `pub struct Foo {` header — the item's own first line —
        // rather than pretending a struct has a signature.
        Part::Sig => Some((start, d.sig_end.max(d.line))),
        Part::Doc => (d.doc_start < d.line).then_some((d.doc_start, d.line - 1)),
    }
}

/// Whether `part` puts source on stdout. `Span` is the one that does not.
fn prints_source(part: Part) -> bool {
    part != Part::Span
}

/// The `at` cell: always `file:start-end`, and always the range this call
/// actually printed rather than the item's full extent. `show` exists to answer
/// "which lines", so a header that named a range wider than the output would be
/// answering a question nobody asked — and `--spans` does not gate it, because
/// gating it would leave the command unable to do its one job by default.
///
/// Note this differs from the `at` of the *listing* commands, which always
/// start at the declaration line. Here the start moves with `--part` and
/// `--no-doc`, because here it is a promise about the bytes below it.
fn at(d: &Defn, range: Option<(usize, usize)>) -> Val {
    match range {
        Some((start, end)) => span_site(&d.file, start, end),
        None => site(&d.file, d.line),
    }
}

/// Print `start..=end` of `file`. Reads the file rather than reconstructing
/// from the AST on purpose: a `syn` round-trip would return the tokens, not the
/// source — no comments, no formatting, no `#[rustfmt::skip]` block as written.
fn print_source(ctx: &AnalysisCtx, file: &str, start: usize, end: usize, number: bool) {
    let Ok(src) = std::fs::read_to_string(file) else {
        ctx.out.note(&format!("note: could not read {}", file));
        return;
    };
    let lines: Vec<&str> = src.lines().collect();
    let lo = start.saturating_sub(1);
    let hi = end.min(lines.len());
    if lo >= hi {
        return;
    }
    if ctx.out.format == Format::Json {
        let body: Vec<String> = lines[lo..hi].iter().map(|l| (*l).to_string()).collect();
        ctx.out.row(vec![("source", Val::List(body))]);
        return;
    }
    for (i, l) in lines[lo..hi].iter().enumerate() {
        if number {
            ctx.out.line(&format!("{:>5}| {}", lo + i + 1, l));
        } else {
            ctx.out.line(l);
        }
    }
}

/// One header row naming the item, so a reader (or a JSON consumer) knows what
/// the source that follows actually is.
fn header(ctx: &AnalysisCtx, d: &Defn, range: Option<(usize, usize)>) {
    row!(
        ctx.out,
        "kind" => d.kind,
        "vis" => d.vis,
        "name" => d.qpath.clone(),
        "at" => at(d, range),
    );
}

/// Resolve `query` to the items it names, in source order.
///
/// Split from the printing because they fail differently: everything here ends
/// in either a set of items or a `TargetNotFound` with an explanation of *why*
/// nothing matched, and there are four distinct reasons. Interleaving that with
/// the rendering was what put this command over the tool's own complexity gate.
fn resolve<'a>(
    ctx: &'a AnalysisCtx,
    query: &str,
    opts: &ShowOpts,
) -> anyhow::Result<Vec<&'a Defn>> {
    let found = ctx.idx.lookup(query);
    let mut hits: Vec<&Defn> = match opts.kind {
        Some(k) => found.iter().copied().filter(|d| d.kind == k).collect(),
        None => found.clone(),
    };

    // The name resolved and `--kind` then emptied it. Saying "no item named X"
    // here would be a lie that sends the reader hunting for a typo they did not
    // make, so name the kinds that actually exist.
    if hits.is_empty() && !found.is_empty() {
        let mut kinds: Vec<&str> = found.iter().map(|d| d.kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        ctx.out.note(&format!(
            "note: `{}` exists but not as a `{}` — it is: {}",
            query,
            opts.kind.unwrap_or("?"),
            kinds.join(", ")
        ));
        return Err(TargetNotFound::err("item", query));
    }
    if hits.is_empty() {
        return not_found(ctx, query).map(|()| Vec::new());
    }

    // An `impl` header and its methods both answer to the type name. Listing
    // the block alongside every method it contains is not an ambiguity a reader
    // needs to resolve, so a query that names a type prefers the type.
    if hits.len() > 1 && !query.contains("::") {
        let named: Vec<&Defn> = hits
            .iter()
            .copied()
            .filter(|d| d.name == last_segment(query))
            .collect();
        if !named.is_empty() {
            hits = named;
        }
    }
    hits.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));
    hits.dedup_by(|a, b| a.file == b.file && a.line == b.line && a.kind == b.kind);
    Ok(hits)
}

/// Print one resolved item: its header row, then whatever `--part` asked for.
fn show_one(ctx: &AnalysisCtx, d: &Defn, opts: &ShowOpts) {
    let range = range_of(d, opts);
    header(ctx, d, range);
    match range {
        Some((start, end)) if prints_source(opts.part) => {
            print_source(ctx, &d.file, start, end, opts.number)
        }
        // A `--part doc` on an item with no doc. The header row already said the
        // item exists, so the only thing left to report is the absence — silence
        // here would read as "the command failed".
        None => ctx
            .out
            .note(&format!("note: `{}` has no doc comment", d.qpath)),
        _ => {}
    }
}

pub fn run(ctx: &AnalysisCtx, query: &str, opts: &ShowOpts) -> anyhow::Result<usize> {
    let hits = resolve(ctx, query, opts)?;

    // More than one match and no `--all`: list them rather than print them.
    // Concatenating four function bodies under one header is the failure mode
    // this command was built to remove — the reader cannot tell where one ends.
    // Each listed row carries the qualified path that would have selected it.
    if hits.len() > 1 && !opts.all {
        ctx.out.note(&format!(
            "note: `{}` names {} items — showing the list. Re-run with the qualified \
             name from the `name` column (or `--kind <kind>`, or `--all`) to print one.",
            query,
            hits.len()
        ));
        for d in &hits {
            header(ctx, d, range_of(d, opts));
        }
        ctx.out
            .summary(&format!("({} match(es); none printed)", hits.len()));
        return Ok(hits.len());
    }

    for (i, d) in hits.iter().enumerate() {
        // Blank line between concatenated items so `--all` output is readable.
        if i > 0 && ctx.out.format != Format::Json {
            ctx.out.line("");
        }
        show_one(ctx, d, opts);
    }
    ctx.out.summary(&format!("({} item(s) shown)", hits.len()));
    Ok(hits.len())
}

/// Nothing matched: say what is near before giving up. Exits 2 via
/// [`TargetNotFound`], the same code every other unknown target uses, so a
/// script can still tell "no such name" from "found, nothing to report".
fn not_found(ctx: &AnalysisCtx, query: &str) -> anyhow::Result<()> {
    let near = ctx.idx.similar(query, 8);
    if near.is_empty() {
        ctx.out.note(&format!(
            "note: no item named `{}` in the scanned tree, and nothing close to it \
             (try --scope all if it is test-only)",
            query
        ));
    } else {
        ctx.out
            .note(&format!("note: no item named `{}`. Did you mean:", query));
        for d in &near {
            ctx.out.note(&format!(
                "  {} {} {}\t{}:{}-{}",
                d.kind, d.vis, d.qpath, d.file, d.doc_start, d.end
            ));
        }
    }
    Err(TargetNotFound::err("item", query))
}
