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
    /// With `--context N`: print ±N source lines under each finding row so
    /// small findings need zero follow-up reads. `None` = off.
    pub context_lines: Option<usize>,
}

impl AnalysisCtx<'_> {
    /// With `--context N`, print the ±N source lines around `line` beneath a
    /// finding row (`>` marks the site line). No-op otherwise.
    pub fn print_context(&self, file: &str, line: usize) {
        let Some(n) = self.context_lines else { return };
        let Ok(src) = std::fs::read_to_string(file) else {
            return;
        };
        let lines: Vec<&str> = src.lines().collect();
        let start = line.saturating_sub(n + 1);
        let end = (line + n).min(lines.len());
        for (i, l) in lines[start..end].iter().enumerate() {
            let ln = start + i + 1;
            let marker = if ln == line { '>' } else { ' ' };
            println!("  {}{:>4}| {}", marker, ln, l);
        }
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
    pub what: &'static str,
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
        anyhow::Error::new(TargetNotFound {
            what,
            name: name.to_string(),
        })
    }
}

/// Uniform up-front warning for a target the index doesn't know. The scan
/// still runs (macros and external names aren't indexed, so hits are possible);
/// commands that then find zero hits return [`TargetNotFound::err`] so main
/// exits with code 2.
pub fn warn_unknown_target(what: &str, name: &str) {
    eprintln!(
        "warning: no {} `{}` found in the scanned tree; \
         a zero-hit result likely means the name doesn't exist here \
         (try --scope all if it's test-only)",
        what, name
    );
}
