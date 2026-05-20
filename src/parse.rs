use std::path::{Path, PathBuf};

pub struct ParsedFile {
    pub path: PathBuf,
    pub ast: syn::File,
    /// Implicit module path derived from the file location, e.g. `src/foo/bar.rs` -> `foo::bar`.
    /// Empty for `main.rs` / `lib.rs` / `mod.rs` (their parent dir is the module).
    pub module: String,
}

pub fn parse_dir(root: &Path) -> anyhow::Result<Vec<ParsedFile>> {
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
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warning: read failed for {}: {}", path.display(), e);
                continue;
            }
        };
        match syn::parse_file(&source) {
            Ok(ast) => {
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
