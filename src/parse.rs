use std::path::{Path, PathBuf};

use crate::ast::{has_test_attr, item_attrs};
use crate::cfg_eval::{parse_cfg_attr, CfgEnv};

pub struct ParsedFile {
    pub path: PathBuf,
    pub ast: syn::File,
    /// Implicit module path derived from the file location, e.g. `src/foo/bar.rs` -> `foo::bar`.
    /// Empty for `main.rs` / `lib.rs` / `mod.rs` (their parent dir is the module).
    pub module: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Scope {
    #[value(alias = "prod")]
    Production,
    #[value(alias = "test")]
    Tests,
    All,
}

/// Directory walker honoring .gitignore plus the user's `--exclude` globs.
fn build_walker(root: &Path, excludes: &[String]) -> anyhow::Result<ignore::Walk> {
    let mut walker = ignore::WalkBuilder::new(root);
    walker.standard_filters(true);
    if !excludes.is_empty() {
        let mut ov = ignore::overrides::OverrideBuilder::new(root);
        for glob in excludes {
            // In override semantics a `!`-prefixed glob is an exclusion.
            ov.add(&format!("!{}", glob))
                .map_err(|e| anyhow::anyhow!("bad --exclude glob `{}`: {}", glob, e))?;
        }
        walker.overrides(ov.build()?);
    }
    Ok(walker.build())
}

/// The files the last non-`All` scan walked past because of `--scope`, and the
/// root they were found under.
///
/// Recorded rather than returned so `parse_dir`'s signature stays what it is:
/// three call sites, only one of which is the run's real scan. Written only
/// when `scope != Scope::All`, so the `Scope::All` pass a few commands make for
/// their call graph cannot zero it.
///
/// The paths, not just a count, because a failed lookup needs to answer "is the
/// name in there?" — and that is a question worth parsing a handful of files to
/// answer, on a path the run was going to exit 2 from anyway.
static SCOPE_SKIPPED: std::sync::Mutex<Option<(PathBuf, Vec<PathBuf>)>> =
    std::sync::Mutex::new(None);

/// How many files the run's scope excluded. See [`SCOPE_SKIPPED`].
pub fn scope_skipped() -> usize {
    SCOPE_SKIPPED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |(_, v)| v.len())
}

/// How many of those were skipped for being in a test-support *crate* rather
/// than for living in `tests/` or being named `tests.rs`.
///
/// Reported separately because it is the surprising one: nothing about
/// `crates/foo-test/src/lib.rs` looks like test code from the inside, so a
/// reader who finds it missing from a production scan deserves to be told which
/// rule removed it.
pub fn scope_skipped_test_crates() -> usize {
    SCOPE_SKIPPED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |(_, v)| v.iter().filter(|p| in_test_support_crate(p)).count())
}

/// Parse the files `--scope` excluded, so a caller can ask what is in them.
///
/// Deliberately not part of the run's own `files`: the whole point of the flag
/// is that they were not analysed. This exists for the one moment where their
/// contents change an *answer* rather than a finding — a name that did not
/// resolve, where "no such item" and "it is in the tests you did not scan" are
/// different sentences and only one of them is true.
pub fn parse_excluded() -> Vec<ParsedFile> {
    let guard = SCOPE_SKIPPED.lock().unwrap_or_else(|e| e.into_inner());
    let Some((root, paths)) = guard.as_ref() else {
        return Vec::new();
    };
    paths
        .iter()
        .filter_map(|path| {
            let source = std::fs::read_to_string(path).ok()?;
            let ast = syn::parse_file(&source).ok()?;
            Some(ParsedFile {
                path: path.clone(),
                ast,
                module: module_path_for(root, path),
            })
        })
        .collect()
}

/// Should `path` be skipped entirely under this scope? Exhaustive (no `_`)
/// so a new Scope variant forces a decision here.
fn out_of_scope(path: &Path, scope: Scope) -> bool {
    let is_test_file =
        is_under_test_dir(path) || looks_like_test_named(path) || in_test_support_crate(path);
    match scope {
        Scope::Production => is_test_file,
        Scope::Tests => !is_test_file,
        Scope::All => false,
    }
}

pub fn parse_dir(
    root: &Path,
    scope: Scope,
    user_cfgs: &[String],
    excludes: &[String],
) -> anyhow::Result<Vec<ParsedFile>> {
    let cfg_env = build_cfg_env(scope, user_cfgs);
    let mut files = Vec::new();
    let mut walk_errs = 0usize;
    let mut read_errs = 0usize;
    let mut parse_errs = 0usize;
    let mut scope_skips: Vec<PathBuf> = Vec::new();
    for entry in build_walker(root, excludes)? {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                walk_errs += 1;
                eprintln!("warning: walk error: {}", e);
                continue;
            }
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        if out_of_scope(path, scope) {
            scope_skips.push(path.to_path_buf());
            continue;
        }

        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                read_errs += 1;
                eprintln!("warning: read failed for {}: {}", path.display(), e);
                continue;
            }
        };
        match syn::parse_file(&source) {
            Ok(mut ast) => {
                if scope != Scope::All {
                    strip_dead_cfg_items(&mut ast.items, &cfg_env);
                }
                let module = module_path_for(root, path);
                files.push(ParsedFile {
                    path: path.to_path_buf(),
                    ast,
                    module,
                });
            }
            Err(e) => {
                parse_errs += 1;
                eprintln!("warning: parse failed for {}: {}", path.display(), e);
            }
        }
    }
    if scope != Scope::All {
        *SCOPE_SKIPPED.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((root.to_path_buf(), scope_skips));
    }
    let skipped = walk_errs + read_errs + parse_errs;
    if skipped > 0 {
        eprintln!(
            "(scanned {} file(s); {} skipped: {} walk, {} read, {} parse errors)",
            files.len(),
            skipped,
            walk_errs,
            read_errs,
            parse_errs
        );
    }
    Ok(files)
}

pub fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    // Strip the `./` the walker inherits from `--root .`. Without this the same
    // file renders two ways depending only on how the root was spelled —
    // `unruster --root . inventory` gave `./src/a.rs:12` and
    // `unruster --root src inventory` gave `src/a.rs:12` for the same item —
    // so two runs could not be diffed and a path grep had to allow for both.
    // Fingerprints already normalise, so this changes no baseline.
    s.strip_prefix("./").unwrap_or(&s).to_string()
}

fn build_cfg_env(scope: Scope, user_cfgs: &[String]) -> CfgEnv {
    let mut env = CfgEnv::new();
    if scope == Scope::Tests {
        env.bools.insert("test".to_string());
    }
    for raw in user_cfgs {
        env.add(raw);
    }
    env
}

fn module_path_for(root: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(root).unwrap_or(file);
    let mut parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str().map(str::to_string),
            _ => None,
        })
        .collect();
    // Drop the crate's `src/` marker wherever it sits, not only at the front.
    // Scanning one crate gives `src/foo.rs` → `foo`; scanning a *workspace*
    // gives `core/src/foo.rs`, and stripping only a leading `src` left every
    // qualified path in the tree carrying a segment Rust does not have —
    // `core::src::config::MirrorScope` for what the compiler calls
    // `core::config::MirrorScope`. Only the first is removed: `src` nested
    // below the crate root is a real module and its name is load-bearing.
    if let Some(i) = parts.iter().position(|p| p == "src") {
        parts.remove(i);
    }
    if let Some(last) = parts.last_mut() {
        if let Some(s) = last.strip_suffix(".rs") {
            *last = s.to_string();
        }
    }
    if matches!(
        parts.last().map(String::as_str),
        Some("main") | Some("lib") | Some("mod")
    ) {
        parts.pop();
    }
    parts.join("::")
}

fn is_under_test_dir(p: &Path) -> bool {
    p.components().any(|c| {
        matches!(
            c,
            std::path::Component::Normal(s) if s == "tests" || s == "benches"
        )
    })
}

/// `foo_tests.rs` / `foo_test.rs` / `tests.rs` — a whole file of test code that
/// happens to live outside a `tests/` directory.
///
/// This narrows `Production`, not just `Tests`. Previously it only widened
/// `Tests`, so `--scope production` reported swallows in
/// `model/animation/tests.rs` as production defects. Per-file cfg stripping
/// cannot catch these: a parent's `#[cfg(test)] mod foo_tests;` is stripped
/// from the parent's AST, but `parse_dir` still walks `foo_tests.rs` as its own
/// file and never sees the gate.
fn looks_like_test_named(p: &Path) -> bool {
    p.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s == "tests" || s.ends_with("_test") || s.ends_with("_tests"))
        .unwrap_or(false)
}

/// Is this file inside a workspace member whose whole job is supporting tests?
///
/// The third shape of test code, and the one the other two heuristics cannot
/// see: `crates/foo-test/src/lib.rs` is ordinary library code by every
/// syntactic measure — not under a `tests/` directory, not named `tests.rs`,
/// not behind `#[cfg(test)]`, because a crate depended on from
/// `[dev-dependencies]` is compiled normally. So `--scope production` scanned
/// it, and a battery run over a twelve-crate workspace reported swallowed
/// `env::var`s in test scaffolding as production defects, twice landing them
/// next to a real fix and reading as a near miss.
///
/// Decided from the crate's own name in the nearest ancestor `Cargo.toml`.
/// BEST-EFFORT by construction — it is a naming convention, not a fact — so it
/// is deliberately narrow: `test`, `tests`, `test-*`, `*-test`, `*-tests`,
/// `*-test-utils`, `*-test-support`, `*-testing`. A crate that supports tests
/// under some other name is missed, and `--exclude` remains the precise answer.
fn in_test_support_crate(p: &Path) -> bool {
    let Some(name) = crate_name_of(p) else {
        return false;
    };
    // Compare on `-`; Cargo lets either spelling name the same package.
    let n = name.replace('_', "-");
    n == "test"
        || n == "tests"
        || n.starts_with("test-")
        || n.ends_with("-test")
        || n.ends_with("-tests")
        || n.ends_with("-testing")
        || n.ends_with("-test-utils")
        || n.ends_with("-test-support")
}

/// `[package] name` of the nearest ancestor `Cargo.toml`, cached per directory.
///
/// Hand-scanned rather than parsed: the tool has no TOML dependency, and the
/// one field it needs is unambiguous — the first `name = "…"` after the
/// `[package]` header and before the next section.
fn crate_name_of(p: &Path) -> Option<String> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static CACHE: Mutex<Option<HashMap<PathBuf, Option<String>>>> = Mutex::new(None);

    let dir = p.parent()?;
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some(hit) = cache.get(dir) {
        return hit.clone();
    }
    let mut found = None;
    for anc in dir.ancestors() {
        let manifest = anc.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&manifest) else {
            break;
        };
        // A workspace-only root manifest has no `[package]`; keep walking up in
        // case this is a nested directory of a member.
        if let Some(name) = package_name(&src) {
            found = Some(name);
            break;
        }
    }
    cache.insert(dir.to_path_buf(), found.clone());
    found
}

/// The `name` of the `[package]` table in a manifest's text, if it has one.
fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = line.strip_prefix("name") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let v = rest.trim().trim_matches(['"', '\''].as_slice());
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

/// Strip items whose cfg attributes evaluate to definitively False, OR which carry
/// a `#[test]` / `#[bench]` attribute (those imply test scope).
fn strip_dead_cfg_items(items: &mut Vec<syn::Item>, env: &CfgEnv) {
    items.retain(|item| {
        let Some(attrs) = item_attrs(item) else { return true; };
        // Always strip #[test]/#[bench]/etc. when test is not set.
        if !env.bools.contains("test") && has_test_attr(attrs) {
            return false;
        }
        // Evaluate cfg attributes.
        let cfgs: Vec<_> = attrs.iter().filter_map(parse_cfg_attr).collect();
        !env.strip(&cfgs)
    });
    for item in items {
        if let syn::Item::Mod(m) = item {
            if let Some((_, sub)) = &mut m.content {
                strip_dead_cfg_items(sub, env);
            }
        }
    }
}
