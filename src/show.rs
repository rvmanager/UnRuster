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
    /// Stop after this many source lines, saying so.
    ///
    /// There was nothing between `--part sig` (the signature) and the whole
    /// body, so a reader wanting a bounded look at a 109-line fn reached for
    /// `show … | head -40` — which cuts mid-body and says nothing about it. A
    /// truncation the tool performs can announce itself and name the lines it
    /// dropped, which a pipe cannot.
    pub max_lines: Option<usize>,
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

/// The item's whole extent — what a *catalogue* row must report.
///
/// [`range_of`] answers "what will I print", which is right for a row with
/// source under it and wrong for a row without. An ambiguity listing prints no
/// source, so under `--part sig` every listed span used to collapse to the
/// declaration line: a 351-line `impl PathData` was catalogued as `51-51`, and
/// the reader needed a second command (`outline`) to learn it was `51-401`.
/// A silently wrong line range is the exact failure this command exists to end,
/// and arriving from an AST tool makes it more credible, not less.
fn extent_of(d: &Defn) -> Option<(usize, usize)> {
    Some((d.doc_start, d.end.max(d.doc_start)))
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
fn print_source(ctx: &AnalysisCtx, file: &str, start: usize, end: usize, opts: &ShowOpts) {
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
    // `--max-lines` cuts here rather than in a pipe, so the cut can say where
    // it happened and what is left. A silent truncation of a body is the same
    // class of wrong answer as a guessed line range.
    let cut = opts.max_lines.map_or(hi, |n| (lo + n).min(hi));
    let dropped = hi - cut;
    if ctx.out.format == Format::Json {
        let body: Vec<String> = lines[lo..cut].iter().map(|l| (*l).to_string()).collect();
        let mut cells: Vec<(&'static str, Val)> = vec![("source", Val::List(body))];
        if dropped > 0 {
            cells.push(("truncated", Val::from(dropped)));
        }
        ctx.out.row(cells);
        return;
    }
    for (i, l) in lines[lo..cut].iter().enumerate() {
        if opts.number {
            ctx.out.line(&format!("{:>5}| {}", lo + i + 1, l));
        } else {
            ctx.out.line(l);
        }
    }
    if dropped > 0 {
        ctx.out.line(&format!(
            "… {} more line(s) to {} — re-run without --max-lines for the rest",
            dropped, hi
        ));
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
    note_siblings(ctx, query, &hits);
    Ok(hits)
}

/// Say when a qualified query resolved past same-named items elsewhere.
///
/// The trap this closes: `clones` reports a duplicated fn by its *qualified*
/// path, so the obvious next command is `show <that path>` — which returns the
/// one copy and says nothing about the other seven. That reads as a complete
/// answer, and the way out was to abandon the tool and `grep -rn "fn name"`.
/// One line naming the count turns the narrow answer into a signposted one.
fn note_siblings(ctx: &AnalysisCtx, query: &str, hits: &[&Defn]) {
    let bare = last_segment(query);
    // Only for a query that narrowed: a bare-name query already listed them.
    if query == bare {
        return;
    }
    let others = ctx.idx.lookup(bare).len().saturating_sub(hits.len());
    if others == 0 {
        return;
    }
    ctx.out.note(&format!(
        "note: {} other item(s) in the tree are also named `{}` — `show {} --all` prints \
         every copy, `clones` compares them",
        others, bare, bare
    ));
}

/// Print one resolved item: its header row, then whatever `--part` asked for.
fn show_one(ctx: &AnalysisCtx, d: &Defn, opts: &ShowOpts) {
    let range = range_of(d, opts);
    header(ctx, d, range);
    match range {
        Some((start, end)) if prints_source(opts.part) => {
            print_source(ctx, &d.file, start, end, opts)
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

/// Render one query's resolved matches. Returns how many rows it emitted.
fn emit_hits(ctx: &AnalysisCtx, query: &str, hits: &[&Defn], opts: &ShowOpts) -> usize {
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
        for d in hits {
            // The item's extent, not `range_of` — see `extent_of`. Nothing is
            // printed below these rows, so the span is a fact about the item
            // rather than a promise about the output.
            header(ctx, d, extent_of(d));
        }
        return hits.len();
    }
    for (i, d) in hits.iter().enumerate() {
        // Blank line between concatenated items so `--all` output is readable.
        if i > 0 && ctx.out.format != Format::Json {
            ctx.out.line("");
        }
        show_one(ctx, d, opts);
    }
    hits.len()
}

/// Resolve and print every `query`.
///
/// Taking more than one name is what makes this usable for the bulk case. An
/// agent needing the start line of 115 named items sized the job as "115 calls,
/// slow but manageable" and went looking for another way — each call re-walks
/// and re-parses the whole tree, so the parse, not the lookup, is the cost.
/// One invocation pays it once.
pub fn run(ctx: &AnalysisCtx, queries: &[String], opts: &ShowOpts) -> anyhow::Result<usize> {
    let single = queries.len() == 1;
    let mut shown = 0usize;
    let mut missed = 0usize;
    let mut first = true;
    for q in queries {
        let hits = match resolve(ctx, q, opts) {
            Ok(h) => h,
            // A batch keeps going. One unknown name must not cost the other 114
            // lookups the single call existed to save — `resolve` has already
            // said what was wrong with this one.
            Err(e) if single => return Err(e),
            Err(_) => {
                missed += 1;
                continue;
            }
        };
        if !first && ctx.out.format != Format::Json {
            ctx.out.line("");
        }
        first = false;
        shown += emit_hits(ctx, q, &hits, opts);
    }
    // Every name missed: the run answered nothing, which is the exit-2 case
    // whether it was asked for one name or a hundred.
    if missed == queries.len() {
        return Err(TargetNotFound::err("item", &queries.join(", ")));
    }
    ctx.out.summary(&format!(
        "({} item(s) from {} name(s){})",
        shown,
        queries.len(),
        if missed > 0 {
            format!("; {} unresolved", missed)
        } else {
            String::new()
        }
    ));
    Ok(shown)
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
