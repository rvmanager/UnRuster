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

pub fn parse_dir(
    root: &Path,
    scope: Scope,
    user_cfgs: &[String],
) -> anyhow::Result<Vec<ParsedFile>> {
    let cfg_env = build_cfg_env(scope, user_cfgs);
    let mut files = Vec::new();
    let mut walk_errs = 0usize;
    let mut read_errs = 0usize;
    let mut parse_errs = 0usize;
    for entry in ignore::WalkBuilder::new(root).standard_filters(true).build() {
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
        let is_test_file = is_under_test_dir(path);
        match scope {
            Scope::Production if is_test_file => continue,
            Scope::Tests if !is_test_file && !looks_like_test_named(path) => continue,
            _ => {}
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
    p.to_string_lossy().into_owned()
}

fn build_cfg_env(scope: Scope, user_cfgs: &[String]) -> CfgEnv {
    let mut env = CfgEnv::new();
    match scope {
        Scope::Tests => {
            env.bools.insert("test".to_string());
        }
        _ => {}
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
    if parts.first().map(String::as_str) == Some("src") {
        parts.remove(0);
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

fn looks_like_test_named(p: &Path) -> bool {
    p.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.ends_with("_test") || s.ends_with("_tests"))
        .unwrap_or(false)
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
