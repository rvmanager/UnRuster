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
