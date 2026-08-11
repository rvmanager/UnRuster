use crate::emit::Out;
use crate::index::NameIndex;
use crate::parse::ParsedFile;
use crate::semantic::Semantic;

/// The shared, read-only inputs every analysis command works from: the parsed
/// production files, the name index, semantic info (use-maps, fn signatures,
/// type aliases), and the global `--summary` flag. Built once in `main` and
/// passed by reference to each `run`, replacing the `(files, idx, sem, …,
/// summary)` tuple that was threaded through every command signature.
///
/// All fields are cheap to copy out (`&T` / `bool`), so a command that needs
/// only a subset binds what it uses at the top, e.g. `let files = ctx.files;`.
pub struct AnalysisCtx<'a> {
    pub files: &'a [ParsedFile],
    pub idx: &'a NameIndex,
    pub sem: &'a Semantic,
    pub summary: bool,
    /// Render enclosing-fn labels as `name@start-end` (the `--spans` flag).
    pub spans: bool,
    /// With `--changed-since <ref>`: canonical paths of files changed vs that
    /// git ref. Site-listing commands drop rows outside this set, so an agent
    /// can verify exactly its own edit. `None` = no filter.
    pub changed: Option<std::collections::HashSet<std::path::PathBuf>>,
    /// Where rows, section headers, and summary lines go. Every command emits
    /// through this so `--json` needs no per-command support.
    pub out: &'a Out,
    /// Sites waived by an in-source `// unruster: ok(…)` comment. Borrowed
    /// rather than owned so `waivers` can re-run the check battery against the
    /// same set and then read back each waiver's hit count.
    pub suppressions: &'a crate::suppress::Suppressions,
    /// With `--suggest-waivers`, print the exact waiver comment under each row.
    pub suggest_waivers: bool,
}

/// A check's findings, split by whether they clear that check's gating
/// threshold.
///
/// Only the ranked checks distinguish the two. Everything else reports the same
/// number twice (all-gating) or reports zero gating (all-advisory), because for
/// an unranked check "which rows matter" is not a question the tool can answer
/// — which is exactly why the ranked ones were built.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Rows reported, after waivers and filters.
    pub total: usize,
    /// Of those, the ones above the check's gating threshold.
    pub gating: usize,
}

impl Counts {
    /// A check with no tiers: every row counts the same.
    pub fn flat(total: usize) -> Self {
        Counts {
            total,
            gating: total,
        }
    }
}

impl AnalysisCtx<'_> {
    /// `min_score=0.05 (audit gates at 0.40)` — the check's own threshold, plus
    /// the one `audit` uses when they differ.
    ///
    /// The dedicated commands deliberately run looser than the battery so they
    /// can show the long tail, but the gap was invisible and uneven: 1.8x on
    /// `divergence`, 2.4x on `config-drift` and 8x on `builder-drift`. A reader
    /// comparing a command's output against an audit section had no way to know
    /// the two were asking different questions, and the 8x one looked like a
    /// bug in the audit rather than a wider net here.
    ///
    /// Prints nothing when the two agree — which is the case when `audit`
    /// itself is the caller, so its own sections stay uncluttered.
    pub fn threshold_note(&self, used: f64, audit_gate: f64) -> String {
        if (used - audit_gate).abs() < f64::EPSILON {
            String::new()
        } else {
            format!(" (audit gates at {:.2})", audit_gate)
        }
    }

    /// The `at` cell for a row that names a whole *item* rather than a point in
    /// one. Plain `file:line` by default; `file:start-end` under `--spans`.
    ///
    /// The column count does not change, which is the reason this is an upgrade
    /// of the existing cell rather than a new one — every caller's `awk` and
    /// every column-shape assertion keeps working, and a JSON consumer written
    /// against `line` still reads the start.
    ///
    /// Site-listing commands do not use this: their `--spans` support comes
    /// from [`crate::ast::ScopeTracker`], which appends `@start-end` to the
    /// *enclosing fn*, since the thing worth reading around a call site is the
    /// fn that contains it.
    /// `line` stays the item's declaration line under both renderings. A flag
    /// that only says where a row *ends* must not also move where it starts.
    ///
    /// A one-line item renders as `file:7-7`, not `file:7`: under one flag every
    /// row has one shape, or a consumer has to handle both and the ones that
    /// forget break on whichever item happens to be a one-liner.
    pub fn at(&self, file: &str, line: usize, end: usize) -> crate::emit::Val {
        if self.spans {
            crate::emit::span_site(file, line, end.max(line))
        } else {
            crate::emit::site(file, line)
        }
    }

    /// With `--context N`, print the ±N source lines around `line` beneath a
    /// finding row (`>` marks the site line). No-op otherwise. Rows emitted
    /// through `out.row(…)` already carry their own context — this is only for
    /// grouped listings that render their site lines by hand.
    pub fn print_context(&self, file: &str, line: usize) {
        self.out.context_at(file, line);
    }

    /// Drop waived sites from `items`, returning how many were dropped so the
    /// summary line can say so — a silent drop would read as "clean".
    ///
    /// `check` is the waiver check name (`"casts"`, `"divergence"`, …): an
    /// `ok(casts)` waiver must not silence an error-swallow that happens to
    /// share the line. `site_of` supplies the optional check-specific key.
    pub fn retain_unsuppressed<T>(
        &self,
        check: &str,
        items: &mut Vec<T>,
        site_of: impl Fn(&T) -> crate::suppress::Site<'_>,
    ) -> usize {
        if self.suppressions.is_empty() {
            return 0;
        }
        let before = items.len();
        items.retain(|it| !self.suppressions.matches(check, site_of(it)));
        before - items.len()
    }

    /// `; N waived` for a summary line, or empty when nothing was waived. Every
    /// check that filters appends this — a suppression that hides its own
    /// volume reads as a clean codebase.
    pub fn waived_note(&self, n: usize) -> String {
        if n == 0 {
            String::new()
        } else {
            format!("; {} waived", n)
        }
    }

    /// With `--suggest-waivers`, print the exact comment that would retire the
    /// row just emitted — correct check, correct key, today's date filled in.
    /// This is the only place the waiver grammar is spelled out at the point of
    /// use, so nobody has to go find it in the help.
    pub fn suggest(&self, check: &str, key: Option<&str>, today: crate::suppress::Date) {
        if !self.suggest_waivers {
            return;
        }
        let spec = match key {
            Some(k) => format!("{}/{}", check, k),
            None => check.to_string(),
        };
        self.out
            .hint(&format!("  // unruster: ok({}) {} — WHY?", spec, today));
    }

    /// The target did not resolve: say which of the two reasons, and return the
    /// error that exits 2.
    ///
    /// The two are not the same question and the old single message answered
    /// only one of them. `unruster variants Defn` — a struct handed to an
    /// enum-only command — reported "no enum `Defn` found in the scanned tree",
    /// which is false: `Defn` is right there, as a struct. A reader who
    /// believes it goes looking for a typo, or for a `--scope` problem, and
    /// finds neither. Naming the kinds that *do* exist answers the question
    /// they actually have, and near-name suggestions cover the real typo case.
    ///
    /// Every kind-requiring command returns this, so the exit code is the same
    /// across all of them: an unanswerable query is 2, not a clean 0. A command
    /// where any name could plausibly match (`callers`, `type-refs`) must NOT
    /// use this for a zero-hit result — there, zero is a real answer.
    pub fn unknown_target(&self, what: &str, name: &str) -> anyhow::Error {
        self.say_unknown(what, name);
        TargetNotFound::err_owned(what, name)
    }

    /// The same explanation, without ending the run.
    ///
    /// Some commands warn and keep scanning on purpose: a name absent from the
    /// index can still be reached through a macro or an external crate, so the
    /// scan may yet find hits, and only a zero-hit result is an error. They get
    /// the near-name list too — a typo is the likeliest reason the index has
    /// never heard of the name, and finding that out at the end of the scan
    /// rather than the start is what made `callers` a dead end while `show`
    /// answered the same question.
    pub fn warn_unknown(&self, what: &str, name: &str) {
        self.say_unknown(what, name);
    }

    /// Which of the three things went wrong, said once — on stdout, because
    /// this is the answer rather than a remark about it (see [`Out::answer`]).
    fn say_unknown(&self, what: &str, name: &str) {
        let existing = self.idx.lookup(name);
        if !existing.is_empty() {
            let mut kinds: Vec<&str> = existing.iter().map(|d| d.kind).collect();
            kinds.sort_unstable();
            kinds.dedup();
            self.out.answer(&format!(
                "note: `{}` is in the scanned tree but not as {} {} — it is: {}",
                name,
                article(what),
                what,
                kinds.join(", ")
            ));
            return;
        }
        // Not an item — but it may be a `let` binding or a closure, which the
        // index does not hold and never will: those are not items and have no
        // callers, fields or variants to report. Saying *that* ends the search;
        // saying "no such name" sends the reader off to grep for a definition
        // that is right there. One session hunted a `focus_regions` that turned
        // out to be `let push = |…|` a few lines away.
        if let Some(b) = crate::semantic::find_binding(self.files, name) {
            self.out.answer(&format!(
                "note: `{}` is not an item — it is {} at {}:{}. \
                 This tool indexes items (fn, struct, enum, impl, …); locals and \
                 closures have no callers or fields to report, so a text search \
                 is the right tool for them.",
                name, b.kind, b.file, b.line
            ));
            return;
        }
        // Before offering guesses: is it simply outside this run's `--scope`?
        //
        // The old order asked that only when there was nothing close, so the
        // *better* the fuzzy match the more confidently wrong the answer got.
        // `show rows_of` on this tree offered `Row`, `Out::row`, `Out::row_note`
        // and `group_of` — four production near-misses — while `rows_of` sat in
        // `tests/cli.rs`, unmentioned, because the run had not scanned it. A
        // reader concludes their name is wrong when their scope is. Parsing the
        // excluded files costs a handful of files on a path that was going to
        // exit 2 regardless.
        if let Some(d) = self.in_excluded_scope(name) {
            self.out.answer(&format!(
                "note: no {} `{}` in the scanned tree — but `{}` is there in the code \
                 `--scope` excluded: {} {} at {}:{}. Re-run with `--scope all`.",
                what, name, name, d.kind, d.qpath, d.file, d.line
            ));
            return;
        }
        // `mod::name` where `mod` glob-imports the module that defines `name`.
        // This is the name a reader writes from a call site: inside
        // `geom/boolean.rs`, `dist(a, b)` is spelled bare and the enclosing
        // module is `geom::boolean`, so `geom::boolean::dist` is the honest
        // guess — and it was answered with six near-misses in other modules,
        // the right item not among them. Asked before the fuzzy list, because
        // an exact resolution beats every guess.
        if let Some(d) = self.resolve_through_globs(name) {
            self.out.answer(&format!(
                "note: no {} `{}` — `{}` is defined as `{}` and reaches `{}` \
                 through a glob import (`use …::*`), so the two name one item. \
                 Use `{}`.",
                what,
                name,
                crate::ast::last_segment(name),
                d.qpath,
                crate::ast::module_of_path(name),
                d.qpath
            ));
            return;
        }
        let near = self.idx.similar_to_query(name, 6);
        if near.is_empty() {
            self.out.answer(&format!(
                "note: no {} `{}` in the scanned tree, and nothing close to it \
                 (try --scope all if it is test-only)",
                what, name
            ));
            return;
        }
        self.out
            .answer(&format!("note: no {} `{}`. Did you mean:", what, name));
        for d in &near {
            self.out
                .answer(&format!("  {} {}\t{}:{}", d.kind, d.qpath, d.file, d.line));
        }
        // The list holds one entry per distinct name, so a method defined in
        // four impls cannot crowd out the other candidates. That is right for
        // four impls and wrong in silence: with `geom::dist` and `trace::dist`
        // both in the tree, a query for `geom::boolean::dist` was answered with
        // `trace::dist` alone and the reader had no way to know a second `dist`
        // existed. Ranking now prefers the shared module prefix; this says when
        // the choice was made at all.
        let hidden: usize = near
            .iter()
            .map(|d| self.idx.lookup(&d.name).len().saturating_sub(1))
            .sum();
        if hidden > 0 {
            self.out.answer(&format!(
                "  (note: {} further item(s) share a name listed above and are not shown — \
                 one row per distinct name. `show <name> --all` prints every copy)",
                hidden
            ));
        }
    }

    /// The item `mod::name` names when `mod` reaches `name` through a glob.
    ///
    /// Only the unambiguous case answers: if two globs in that module both
    /// supply the name, the query really is ambiguous and a confident answer
    /// would be a guess wearing the shape of a resolution.
    fn resolve_through_globs(&self, query: &str) -> Option<&crate::index::Defn> {
        if !query.contains("::") {
            return None;
        }
        let module = crate::ast::module_of_path(query);
        let name = crate::ast::last_segment(query);
        let mut found: Vec<&crate::index::Defn> = Vec::new();
        for f in self.files {
            if f.module != module {
                continue;
            }
            let Some(uses) = self.sem.uses_for(&f.path) else {
                continue;
            };
            for g in &uses.globs {
                let candidate = if g.is_empty() {
                    name.to_string()
                } else {
                    format!("{}::{}", g, name)
                };
                for d in self.idx.lookup(&candidate) {
                    if d.qpath == candidate && !found.iter().any(|x| x.qpath == d.qpath) {
                        found.push(d);
                    }
                }
            }
        }
        match found.as_slice() {
            [one] => Some(one),
            _ => None,
        }
    }

    /// Does `name` resolve in the files `--scope` left out of this run?
    ///
    /// Parses them on the spot. That is affordable *here and nowhere else*:
    /// every caller is already on its way to exit 2, and the set is whatever
    /// the scope excluded rather than the whole tree. Returns the first match
    /// in source order — one concrete location ends the search, where a list
    /// would just be a second set of guesses.
    fn in_excluded_scope(&self, name: &str) -> Option<crate::index::Defn> {
        let files = crate::parse::parse_excluded();
        if files.is_empty() {
            return None;
        }
        let idx = crate::index::NameIndex::build(&files);
        idx.lookup(name).first().map(|d| (*d).clone())
    }

    /// Is this file inside the run's `--changed-since` scope? Always true when
    /// there is no filter.
    ///
    /// Split out of [`retain_changed`](Self::retain_changed) because not
    /// everything scoping applies to arrives as a `Vec` of rows. The waiver
    /// ledger is the case that forced it: `audit`'s closing line tallies
    /// waivers that suppressed nothing, and under a scoped run every waiver in
    /// an unchanged file has zero hits by construction — the check dropped its
    /// rows there before the waiver could ever be consulted. Asking this per
    /// waiver is what keeps that tally from calling a live ledger dead.
    pub fn in_scope(&self, file: &str) -> bool {
        match &self.changed {
            None => true,
            Some(set) => std::fs::canonicalize(file)
                .map(|p| set.contains(&p))
                .unwrap_or(false),
        }
    }

    /// With `--changed-since`, keep only hits whose file is in the changed
    /// set (no-op otherwise). `file_of` extracts the hit's display path.
    pub fn retain_changed<T>(&self, items: &mut Vec<T>, file_of: impl Fn(&T) -> &str) {
        if self.changed.is_some() {
            items.retain(|it| self.in_scope(file_of(it)));
        }
    }
}

/// Canonical paths of files changed vs `git_ref`: `git diff --name-only
/// <ref>` (tracked changes, staged or not) plus untracked files. Paths are
/// resolved against the repo top-level, so this works from any CWD. Git is
/// the only state consulted — there is no tracking file.
///
/// Asked of `root`'s repository, not the process's. Reading the CWD's repo
/// instead is the same failure the empty-`files` guard in `main` exists to
/// catch, one layer down and quieter: `unruster -r ../other/src
/// --changed-since HEAD audit` diffed *this* checkout, none of whose paths are
/// under `../other`, so every check dropped every row and the run reported
/// "0 gating + 0 advisory; clean; exit 0" over a tree it had not looked at.
// unruster: ok(error-swallows/if-let-ok) 2026-08-06 — a path that will not
// canonicalize is not in the working tree, which is precisely the reason to
// leave it out of the changed set.
pub fn changed_set(
    git_ref: &str,
    root: &std::path::Path,
) -> anyhow::Result<std::collections::HashSet<std::path::PathBuf>> {
    use std::process::Command;
    // `--root` may name a file; git wants a directory.
    let at = match root.is_dir() {
        true => root,
        false => root.parent().unwrap_or(std::path::Path::new(".")),
    };
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let out = Command::new("git").arg("-C").arg(at).args(args).output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let top = git(&["rev-parse", "--show-toplevel"])?;
    let top = std::path::Path::new(top.trim());
    let mut set = std::collections::HashSet::new();
    let listings = [
        git(&["diff", "--name-only", git_ref])?,
        git(&["ls-files", "--others", "--exclude-standard"])?,
    ];
    for listing in &listings {
        for line in listing.lines() {
            if line.is_empty() {
                continue;
            }
            if let Ok(p) = std::fs::canonicalize(top.join(line)) {
                set.insert(p);
            }
        }
    }
    Ok(set)
}

/// How strongly a row's match is grounded. Ordered weakest-first so
/// `--min-confidence <tier>` filters with a simple `>=`:
/// - `heuristic` — last-segment name match only; same-named items elsewhere
///   would also match.
/// - `inferred`  — matched through local type inference or an alias chain.
/// - `resolved`  — matched through a `use`-map resolution, a qualified path,
///   or a name with exactly one definition in the tree.
/// - `exact`     — structurally certain (e.g. `self.field` inside `impl Type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Confidence {
    Heuristic,
    Inferred,
    Resolved,
    Exact,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Heuristic => "heuristic",
            Confidence::Inferred => "inferred",
            Confidence::Resolved => "resolved",
            Confidence::Exact => "exact",
        }
    }
}

/// Grouping dimension for commands that support `--by`. Parsed by clap
/// (value_enum), so an invalid value is rejected uniformly at the CLI boundary
/// instead of each command improvising its own fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum GroupBy {
    Fn,
    File,
    Module,
}

/// Typed error for "the queried target doesn't exist in the scanned tree".
/// `main` maps it to exit code 2 so scripts can distinguish "no findings"
/// (exit 0, empty output) from "the queried name isn't there". The warning
/// text is printed by [`warn_unknown_target`] before the scan runs; this error
/// itself is not printed again.
#[derive(Debug)]
pub struct TargetNotFound {
    pub what: String,
    pub name: String,
}

impl std::fmt::Display for TargetNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no {} `{}` found in the scanned tree", self.what, self.name)
    }
}

impl std::error::Error for TargetNotFound {}

impl TargetNotFound {
    pub fn err(what: &'static str, name: &str) -> anyhow::Error {
        Self::err_owned(what, name)
    }

    /// Same, for a `what` computed at run time.
    pub fn err_owned(what: &str, name: &str) -> anyhow::Error {
        anyhow::Error::new(TargetNotFound {
            what: what.to_string(),
            name: name.to_string(),
        })
    }
}

/// `a` or `an` for a target kind. The kinds are a fixed, tiny vocabulary
/// (`enum`, `impl`, `struct with named fields`, …), so the vowel test is exact
/// here rather than the usual approximation.
fn article(what: &str) -> &'static str {
    match what.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u' | 'A' | 'E' | 'I' | 'O' | 'U') => "an",
        _ => "a",
    }
}

/// Uniform up-front warning for a target the index doesn't know. The scan
/// still runs (macros and external names aren't indexed, so hits are possible);
/// commands that then find zero hits return [`TargetNotFound::err`] so main
/// exits with code 2.
///
/// Prefer [`AnalysisCtx::unknown_target`], which can tell "no such name" from
/// "that name is something else" and routes through `out` so `--json` keeps it.
/// This plain form remains for the few callers whose target is not an indexed
/// item at all (a cohort glob, a constructor path).
pub fn warn_unknown_target(what: &str, name: &str) {
    eprintln!(
        "warning: no {} `{}` found in the scanned tree; \
         a zero-hit result likely means the name doesn't exist here \
         (try --scope all if it's test-only)",
        what, name
    );
}
