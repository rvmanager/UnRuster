use std::path::{Path, PathBuf};

use crate::ast::{has_test_attr, item_attrs};

pub struct ParsedFile {
    pub path: PathBuf,
    pub ast: syn::File,
    /// Implicit module path derived from the file location, e.g. `src/foo/bar.rs` -> `foo::bar`.
    /// Empty for `main.rs` / `lib.rs` / `mod.rs` (their parent dir is the module).
    pub module: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Production,
    Tests,
    All,
}

impl Scope {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "production" | "prod" => Ok(Scope::Production),
            "tests" | "test" => Ok(Scope::Tests),
            "all" => Ok(Scope::All),
            _ => anyhow::bail!("invalid --scope {:?}: expected production|tests|all", s),
        }
    }
}

pub fn parse_dir(root: &Path, scope: Scope) -> anyhow::Result<Vec<ParsedFile>> {
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(root).standard_filters(true).build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
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
                eprintln!("warning: read failed for {}: {}", path.display(), e);
                continue;
            }
        };
        match syn::parse_file(&source) {
            Ok(mut ast) => {
                if scope == Scope::Production {
                    strip_test_items(&mut ast.items);
                }
                let module = module_path_for(root, path);
                files.push(ParsedFile {
                    path: path.to_path_buf(),
                    ast,
                    module,
                });
            }
            Err(e) => eprintln!("warning: parse failed for {}: {}", path.display(), e),
        }
    }
    Ok(files)
}

pub fn display_path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
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

/// Recursively remove items annotated with `#[cfg(test)]`, `#[test]`, etc.
/// Also recurses into inline `mod foo { ... }` blocks.
fn strip_test_items(items: &mut Vec<syn::Item>) {
    items.retain(|item| {
        item_attrs(item)
            .map(|a| !has_test_attr(a))
            .unwrap_or(true)
    });
    for item in items {
        if let syn::Item::Mod(m) = item {
            if let Some((_, sub)) = &mut m.content {
                strip_test_items(sub);
            }
        }
    }
}
