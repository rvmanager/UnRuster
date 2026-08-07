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
        let existing = self.idx.lookup(name);
        if existing.is_empty() {
            let near = self.idx.similar(name, 6);
            if near.is_empty() {
                self.out.note(&format!(
                    "note: no {} `{}` in the scanned tree, and nothing close to it \
                     (try --scope all if it is test-only)",
                    what, name
                ));
            } else {
                self.out
                    .note(&format!("note: no {} `{}`. Did you mean:", what, name));
                for d in &near {
                    self.out
                        .note(&format!("  {} {}\t{}:{}", d.kind, d.qpath, d.file, d.line));
                }
            }
        } else {
            let mut kinds: Vec<&str> = existing.iter().map(|d| d.kind).collect();
            kinds.sort_unstable();
            kinds.dedup();
            self.out.note(&format!(
                "note: `{}` is in the scanned tree but not as {} {} — it is: {}",
                name,
                article(what),
                what,
                kinds.join(", ")
            ));
        }
        TargetNotFound::err_owned(what, name)
    }

    /// With `--changed-since`, keep only hits whose file is in the changed
    /// set (no-op otherwise). `file_of` extracts the hit's display path.
    pub fn retain_changed<T>(&self, items: &mut Vec<T>, file_of: impl Fn(&T) -> &str) {
        if let Some(set) = &self.changed {
            items.retain(|it| {
                std::fs::canonicalize(file_of(it))
                    .map(|p| set.contains(&p))
                    .unwrap_or(false)
            });
        }
    }
}

/// Canonical paths of files changed vs `git_ref`: `git diff --name-only
/// <ref>` (tracked changes, staged or not) plus untracked files. Paths are
/// resolved against the repo top-level, so this works from any CWD. Git is
/// the only state consulted — there is no tracking file.
// unruster: ok(error-swallows/if-let-ok) 2026-08-06 — a path that will not
// canonicalize is not in the working tree, which is precisely the reason to
// leave it out of the changed set.
pub fn changed_set(
    git_ref: &str,
) -> anyhow::Result<std::collections::HashSet<std::path::PathBuf>> {
    use std::process::Command;
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let out = Command::new("git").args(args).output()?;
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
