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
