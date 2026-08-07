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
use crate::emit::{span_site, Val};
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
    /// Append the first line of each item's doc comment.
    pub docs: bool,
    /// Flatten the nesting indent (nicer for `awk`, worse for reading).
    pub flat: bool,
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
    // Source order. An outline read out of order is a list, not an outline.
    items.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));

    if !ctx.summary {
        for d in &items {
            let indent = if opts.flat { 0 } else { d.depth };
            let name = format!("{}{}", "  ".repeat(indent), short_name(d));
            let mut cells: Vec<(&'static str, Val)> = vec![
                ("kind", Val::from(d.kind)),
                ("vis", Val::from(d.vis)),
                ("loc", Val::from(d.end.saturating_sub(d.line) + 1)),
                ("name", Val::from(name)),
                // Declaration line, not `doc_start` — the same first number
                // every other listing command reports, so `at` means one thing
                // across the tool. `show` prints the docs anyway.
                ("at", span_site(&d.file, d.line, d.end.max(d.line))),
            ];
            if opts.docs {
                cells.push(("doc", Val::from(d.doc.clone().unwrap_or_else(|| "—".into()))));
            }
            ctx.out.row(cells);
        }
    }
    ctx.out.summary(&format!(
        "({} item(s) in {}; `at` is file:decl-end — `show <name>` prints one, \
         docs included)",
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
