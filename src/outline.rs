//! `outline <file>` — the AST table of contents for one file.
//!
//! Replaces the hand-rolled anchor that every agent transcript reaches for:
//!
//! ```text
//! grep -n "^pub fn\|^pub struct\|^pub enum\|^    pub " src/trace.rs | head -60
//! ```
//!
//! That regex is wrong in both directions at once. It misses private items,
//! multi-line signatures whose `pub fn` is not the first token on its line, and
//! anything indented by a width the author did not think of; and it matches
//! `pub` inside a struct body, so field declarations arrive mixed in with the
//! items. It also cannot report where anything *ends*, which is why the reads
//! that follow it are 150-line windows chosen by feel.
//!
//! An outline is complete by construction — it is the same walk the rest of the
//! tool indexes from — and every row carries `file:start-end`, so the follow-up
//! read is exact instead of exploratory.

use crate::context::AnalysisCtx;
use crate::emit::Val;
use crate::index::Defn;

pub struct OutlineOpts<'a> {
    /// The scan root, so "does this file exist" is asked where the user is
    /// pointing rather than at the process's working directory. Without it,
    /// `-r some/crate outline tests/x.rs` reports a file that is right there as
    /// nonexistent, and sends the reader looking for a typo.
    pub root: &'a std::path::Path,
    /// Only rows of this kind.
    pub kind: Option<&'a str>,
    /// Keep only items of this visibility. `--pub-only` is the shorthand for
    /// `--vis pub`; the long form exists so this command and `inventory` filter
    /// the same way with the same word.
    pub vis: Option<crate::inventory::VisFilter>,
    /// Keep only items whose name matches this glob — the same flag,
    /// the same matcher and the same notes as `inventory --name`.
    ///
    /// The one filter the two listings did not share, and the gap showed:
    /// asked for one type's members inside a long file, a session wrote
    /// `outline app_state.rs | grep -iE 'editopstate|take_drag|cancel'`, which
    /// dropped every member whose name did not match the filter, then retried
    /// with `sed -n '/impl EditOpState/,/^impl /p'` — whose `^impl ` never
    /// matches the tab-separated `impl` row, so the range ran on into three
    /// unrelated types. Two calls and a wrong answer for
    /// `--name 'EditOpState::*'`.
    pub name: Option<&'a str>,
    /// Row order. Shared with `inventory`; the two differ only in the default.
    pub sort: crate::inventory::ItemSort,
    /// Append the first line of each item's doc comment.
    pub docs: bool,
    /// Flatten the nesting indent (nicer for `awk`, worse for reading).
    pub flat: bool,
    /// Print each item's signature under its row: a fn's through the return
    /// type, anything else's declaration line. The question an outline is
    /// usually run for is "what is in this file and what do these take", and
    /// the second half used to cost a `sed -n` per range — 62 outlines in one
    /// project's sessions were followed by range reads of the same file.
    pub sig: bool,
}

/// Signature lines a row wants — the `--sig` column's worth, capped so a
/// forty-parameter fn does not turn an outline into a listing.
const SIG_LINES_MAX: usize = 12;

/// The lines of `d`'s signature, read from the file: a fn's declaration
/// through `sig_end`, anything else's declaration line alone.
fn signature_lines<'a>(d: &Defn, lines: &'a [String]) -> Vec<&'a str> {
    let end = match d.kind {
        "fn" | "impl-fn" | "trait-fn" => d.sig_end.max(d.line),
        _ => d.line,
    };
    let lo = d.line.saturating_sub(1);
    let hi = end.min(lines.len());
    if lo >= hi {
        return Vec::new();
    }
    lines[lo..hi].iter().map(|l| l.trim_end()).collect()
}

/// Does `d` belong to the file the user asked about?
///
/// Suffix matching on path components, so `outline window.rs`,
/// `outline geom/window.rs` and `outline src/geom/window.rs` all resolve while
/// `outline dow.rs` does not. Substring matching would accept that last one.
fn in_file(d: &Defn, want: &std::path::Path) -> bool {
    let have = std::path::Path::new(&d.file);
    let want: Vec<_> = want.components().collect();
    let have: Vec<_> = have.components().collect();
    have.len() >= want.len() && have[have.len() - want.len()..] == want[..]
}

/// Items past which an outline is long enough to route to `at`.
///
/// A file with a handful of items is one a reader is about to read whole; a
/// file with twenty is one they are navigating.
const ROUTE_TO_AT_ABOVE: usize = 20;

pub fn run(ctx: &AnalysisCtx, path: &str, opts: &OutlineOpts) -> anyhow::Result<usize> {
    let want = std::path::Path::new(path);
    let mut items: Vec<&Defn> = ctx.idx.iter().filter(|d| in_file(d, want)).collect();

    if items.is_empty() {
        return nothing_here(ctx, path, opts.root);
    }

    // Which files actually matched, so an ambiguous suffix is reported rather
    // than silently rendered as one interleaved outline.
    let mut files: Vec<&str> = items.iter().map(|d| d.file.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    if files.len() > 1 {
        ctx.out.note(&format!(
            "note: `{}` matches {} files ({}) — outlining all of them; \
             pass a longer path to pick one",
            path,
            files.len(),
            files.join(", ")
        ));
    }

    if let Some(k) = opts.kind {
        items.retain(|d| d.kind == k);
    }
    if let Some(v) = opts.vis {
        items.retain(|d| d.vis == v.as_str());
    }
    if let Some(pat) = opts.name {
        items.retain(|d| {
            crate::inventory::name_matches(pat, &crate::inventory::match_path(d))
        });
    }
    // Source order by default: an outline read out of order is a list, not an
    // outline. `--sort kind` gives `inventory`'s census ordering on one file.
    match opts.sort {
        crate::inventory::ItemSort::Source => {
            items.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)))
        }
        crate::inventory::ItemSort::Kind => items.sort_by(|a, b| {
            a.kind
                .cmp(b.kind)
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.line.cmp(&b.line))
        }),
    }

    // One read per file, for `--sig`.
    let mut sources: std::collections::HashMap<&str, Vec<String>> = std::collections::HashMap::new();
    if !ctx.summary {
        for d in &items {
            // Indented: the short name, because the indent says whose it is.
            // Flat: the qualified path, because nothing else would — `score`
            // and `make_label` on their own name no owner, and `--flat` exists
            // precisely so the rows can be fed to `awk`. Flat rows are then
            // byte-identical to `inventory --sort source` over the same file.
            let name = if opts.flat {
                d.qpath.clone()
            } else {
                format!("{}{}", "  ".repeat(d.depth), short_name(d))
            };
            let mut cells: Vec<(&'static str, Val)> = vec![
                ("kind", Val::from(d.kind)),
                ("vis", Val::from(d.vis)),
                ("loc", Val::from(d.end.saturating_sub(d.line) + 1)),
                ("name", Val::from(name)),
                // Always the span, never the bare declaration line — `at`'s
                // rule (`--spans` must not be the difference between an answer
                // and half of one), and this command has the stronger claim to
                // it: an outline is a table of contents *to read from*, and
                // "where does this item end" is the question it is opened to
                // answer. It went through `ctx.at` for a while, for consistency
                // with the other item listings, and the consistency was bought
                // at the reader's expense: with only start lines on the page
                // the follow-up read is a `sed` over the gap to the *next*
                // item's start, which overshoots by every doc comment and blank
                // line between them. Measured on one session that outlined four
                // files and then read them: four ranges, over by 37, 66, 24 and
                // 5 lines, one of them 247 lines covering four separate items.
                // `loc` carrying the extent as a number did not help — nobody
                // does the arithmetic.
                //
                // `--spans` stays accepted and is a no-op here. `inventory`
                // still honours it: a whole-tree census is scanned, not read
                // from, and there the flag is the reader asking to widen it.
                ("at", crate::emit::span_site(&d.file, d.line, d.end.max(d.line))),
            ];
            if opts.docs {
                cells.push(("doc", Val::from(d.doc.clone().unwrap_or_else(|| "—".into()))));
            }
            let sig: Vec<String> = if opts.sig {
                let src = sources.entry(d.file.as_str()).or_insert_with(|| {
                    std::fs::read_to_string(&d.file)
                        .map(|s| s.lines().map(str::to_string).collect())
                        .unwrap_or_default()
                });
                let all = signature_lines(d, src);
                let mut v: Vec<String> = all.iter().take(SIG_LINES_MAX).map(|l| l.to_string()).collect();
                if all.len() > SIG_LINES_MAX {
                    v.push(format!("… {} more signature line(s)", all.len() - SIG_LINES_MAX));
                }
                v
            } else {
                Vec::new()
            };
            // JSON carries the signature on the row; TSV prints it beneath,
            // indented past any column, so the rows themselves keep their shape.
            if opts.sig && ctx.out.format == crate::emit::Format::Json {
                cells.push(("sig", Val::List(sig.clone())));
            }
            ctx.out.row(cells);
            if ctx.out.format != crate::emit::Format::Json {
                for l in &sig {
                    ctx.out.line(&format!("        {}", l.trim_start()));
                }
            }
        }
    }
    // The reverse lookup, named where the forward one just happened. A file
    // long enough to need an outline is a file whose line numbers a reader
    // already has — from a compiler error, a stack trace, a `grep -n` — and
    // `at <file>:<line>` turns one into an item. It went unused across a whole
    // session in which fourteen `sed -n 'N,Mp'` range reads were written by
    // hand, because nothing points at it when the question arises.
    if items.len() >= ROUTE_TO_AT_ABOVE {
        ctx.out.note(&format!(
            "(note: `at {}:<line>` is the reverse lookup — it names the item a line number \
             falls in, and prints its extent so a read needs no guessed range)",
            files[0]
        ));
    }
    // The pointer sits here because this is where a reader *has* a list of
    // names and is about to read several of them. Told only in `--help`, the
    // batch form goes unused: one session made 34 `show` calls of which 23 sat
    // in groups of two to four on a single shell line, each re-parsing the tree.
    crate::inventory::note_name_filter(ctx, opts.name, items.len(), true);
    ctx.out.summary(&format!(
        "({} item(s) in {}; `at` is file:decl-end — `show <name>` prints one \
         with its docs, `show <a> <b> <c>` several in one pass)",
        items.len(),
        files.join(", ")
    ));
    Ok(items.len())
}

/// The name to render. Nested items already sit under their parent's row, so
/// the bare name plus the indent says what a repeated `module::Type::method`
/// path would. The exception is an `impl` header, whose whole point is the
/// rendered `impl Display for Foo` text rather than the type name alone.
fn short_name(d: &Defn) -> String {
    match d.kind {
        "impl" => d.qpath.clone(),
        _ => d.name.clone(),
    }
}

/// No items matched the path. Distinguish the three reasons, because the fix
/// differs for each and an empty listing suggests none of them.
fn nothing_here(
    ctx: &AnalysisCtx,
    path: &str,
    root: &std::path::Path,
) -> anyhow::Result<usize> {
    let exists = std::path::Path::new(path).exists() || root.join(path).exists();
    let msg = if !exists {
        format!(
            "note: no file matching `{}` was scanned, and no such path exists here — \
             check the path and --root",
            path
        )
    } else {
        format!(
            "note: `{}` exists but was not scanned (it is out of --scope, excluded, \
             or gitignored) — try --scope all",
            path
        )
    };
    ctx.out.note(&msg);
    Err(crate::context::TargetNotFound::err("file", path))
}

/// `at <file>:<line>` — which item owns a line.
///
/// The reverse of every other lookup here, and the one an agent needs when a
/// line number arrives from somewhere else: a compiler error, a stack trace, a
/// `grep -n` hit, a review comment. The observed escape, verbatim:
///
/// ```text
/// awk 'NR<=5728 && /^(pub )?fn |^    pub fn /{l=NR": "$0} END{}' src/cmd/mod.rs >/dev/null; \
///   grep -n '^pub fn \|^fn ' src/cmd/mod.rs | awk -F: '$1<5728' | tail -3
/// ```
///
/// That is a reverse span lookup, hand-rolled over data `outline` already
/// computes, with the same `^fn` anchor this module's header explains is wrong
/// in both directions — and no way to see the `impl` block the answer sits in.
///
/// Answers with the whole containing chain rather than the innermost item
/// alone. `impl-fn` rows carry a bare method name, so "you are in `lookup`" is
/// only half an answer when four types have one; the `impl` and `mod` rows
/// above it are what make the innermost row mean something.
pub fn run_at(ctx: &AnalysisCtx, target: &str, root: &std::path::Path) -> anyhow::Result<usize> {
    let (path, line) = match split_file_line(target) {
        Some(v) => v,
        None => {
            // `answer`, not `note`: this is the whole reply to the query, and a
            // reader who wrote `2>/dev/null` must still receive it. Same rule
            // `say_unknown` follows for every other failed lookup.
            ctx.out.answer(&format!(
                "note: `{}` is not a `<file>:<line>` target. Write the line number after a \
                 colon, as `grep -n`, `outline` and rustc all print it: `at src/trace.rs:2111`.",
                target
            ));
            return Err(crate::context::TargetNotFound::err("file:line", target));
        }
    };

    let want = std::path::Path::new(path);
    let in_this_file: Vec<&Defn> = ctx.idx.iter().filter(|d| in_file(d, want)).collect();
    if in_this_file.is_empty() {
        return nothing_here(ctx, path, root);
    }

    let mut files: Vec<&str> = in_this_file.iter().map(|d| d.file.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    if files.len() > 1 {
        ctx.out.note(&format!(
            "note: `{}` matches {} files ({}) — a line number means a different place in \
             each, so pass a longer path to pick one",
            path,
            files.len(),
            files.join(", ")
        ));
    }

    // Doc comments and attributes count as inside the item: a reader pointed at
    // a `#[arg(long)]` line is asking about the field it decorates, and an
    // extent that started at `decl` would answer "nothing owns this".
    let mut chain: Vec<&Defn> = in_this_file
        .iter()
        .copied()
        .filter(|d| d.doc_start <= line && line <= d.end)
        .collect();
    // Outermost first: widest span, then earliest start. Read top-down it is a
    // breadcrumb — module, impl, method.
    chain.sort_by(|a, b| {
        (b.end - b.doc_start)
            .cmp(&(a.end - a.doc_start))
            .then_with(|| a.doc_start.cmp(&b.doc_start))
    });

    if chain.is_empty() {
        return between_items(ctx, path, line, &in_this_file);
    }

    if !ctx.summary {
        for d in &chain {
            ctx.out.row(vec![
                ("kind", Val::from(d.kind)),
                ("vis", Val::from(d.vis)),
                ("name", Val::from(d.qpath.clone())),
                ("loc", Val::from(d.end.saturating_sub(d.line) + 1)),
                // Always the span, never the bare declaration line: the span is
                // the answer to "what owns this", and `--spans` must not be the
                // difference between an answer and half of one.
                ("at", crate::emit::span_site(&d.file, d.doc_start, d.end)),
            ]);
        }
    }
    // The innermost row is the answer; the summary names it so a reader does
    // not have to work out which end of the chain they wanted.
    let inner = chain[chain.len() - 1];
    ctx.out.summary(&format!(
        "({}:{} is in `{}` ({} {}-{}); {} enclosing item(s) listed outermost first — \
         `show {}` prints it)",
        path,
        line,
        inner.qpath,
        inner.kind,
        inner.doc_start,
        inner.end,
        chain.len().saturating_sub(1),
        inner.qpath
    ));
    Ok(chain.len())
}

/// Split `src/trace.rs:2111` into its path and line.
///
/// On the *last* colon, so a Windows-style `C:\…:12` and a path containing one
/// still split where the number is. A target with no line number is a mistake
/// worth naming rather than defaulting to line 1, which would answer confidently
/// about the top of the file.
fn split_file_line(target: &str) -> Option<(&str, usize)> {
    let (path, num) = target.rsplit_once(':')?;
    let line: usize = num.trim().parse().ok()?;
    (!path.is_empty() && line > 0).then_some((path, line))
}

/// The line is in the file but inside no item — a header comment, a `use`
/// block, a blank run between two items.
///
/// Naming the neighbours is the whole value: "nothing owns line 12" is true and
/// useless, where "it is after the `use` block and before `fn build` at 67"
/// tells the reader where they actually are. Exits 0, because this is a real
/// answer about a real line rather than a lookup that failed.
fn between_items(
    ctx: &AnalysisCtx,
    path: &str,
    line: usize,
    items: &[&Defn],
) -> anyhow::Result<usize> {
    let before = items
        .iter()
        .filter(|d| d.end < line)
        .max_by_key(|d| d.end);
    let after = items
        .iter()
        .filter(|d| d.doc_start > line)
        .min_by_key(|d| d.doc_start);
    let describe = |d: Option<&Defn>, rel: &str| match d {
        Some(d) => format!("{} `{}` ({} {}-{})", rel, d.qpath, d.kind, d.doc_start, d.end),
        None => format!("{} nothing", rel),
    };
    ctx.out.summary(&format!(
        "({}:{} is inside no item — it is {}, {}. Between-item lines are `use` blocks, \
         module headers and blank runs; `outline {}` lists the items.)",
        path,
        line,
        describe(before.copied(), "after"),
        describe(after.copied(), "before"),
        path
    ));
    Ok(0)
}
