//! Persisted per-crate analysis facts.
//!
//! Each crate compile writes a JSON file under
//! `~/.cache/unruster/<project-hash>/<crate>-<hash>.facts.json`.
//! The whole-program analyzer (the egui viewer) loads every facts file under
//! one project hash and computes design-level findings from the union.
//!
//! The schema here is intentionally *raw*: writer lists, edges, access kinds.
//! Thresholds and "is this bad" judgments live in the viewer so they can be
//! tuned without recompiling.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CrateFacts {
    pub crate_name: String,
    pub project_root: String,
    pub functions: Vec<FunctionFact>,
    pub calls: Vec<CallEdge>,
    pub structs: Vec<StructFact>,
    pub field_accesses: Vec<FieldAccessFact>,
    pub api_leaks: Vec<ApiLeakFact>,
    /// Public functions whose return type is `Result<_, String>` /
    /// `Result<_, &str>`. RuleBook §4.5: errors should be structured types.
    #[serde(default)]
    pub stringly_errors: Vec<StringlyErrorFact>,
    /// Public `unsafe fn` whose doc comment lacks a `# Safety` section.
    /// RuleBook §16.4: every public unsafe API must document the caller's
    /// obligations.
    #[serde(default)]
    pub unsafe_no_safety_docs: Vec<UnsafeNoSafetyDocFact>,
    /// Per-function rollup of which fields it reads/writes and which
    /// downstream methods it calls. Computed in the driver so every
    /// consumer (viewer, CI, LLM tooling) can use it without rebuilding
    /// it from raw `field_accesses` + `calls`.
    #[serde(default)]
    pub function_profiles: Vec<FunctionProfile>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FunctionFact {
    pub def_path: String,
    pub module_path: String,
    pub file: String,
    pub line: u32,
    pub is_public: bool,
    pub is_test: bool,
    pub param_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CallEdge {
    pub caller: String,
    pub callee: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StructFact {
    pub def_path: String,
    pub module_path: String,
    pub file: String,
    pub line: u32,
    pub fields: Vec<FieldDef>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    pub is_public: bool,
    /// Decomposition of the field's declared type (container shape +
    /// generic arguments). `None` for types we couldn't parse cleanly.
    #[serde(default)]
    pub type_shape: Option<TypeShape>,
}

/// A simplified view of a field's declared type — enough to spot
/// parallel-container smells like `Vec<NodeContent>` next to
/// `HashMap<NodeId, ParentEdge>` (both keyed by NodeId).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeShape {
    /// The outermost container, e.g. `"Vec"`, `"HashMap"`, `"Option"`,
    /// `"Box"`. For non-container types this is just the type's name.
    pub outer: String,
    /// Generic arguments, left to right. For `HashMap<NodeId, X>` this
    /// is `["NodeId", "X"]`. For `Vec<T>`: `["T"]`. Names are the last
    /// path segment only, so `crate::foo::Bar` becomes `"Bar"`.
    pub args: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FunctionProfile {
    pub def_path: String,
    /// Distinct `(struct_def_path, field_name)` pairs this function reads.
    pub fields_read: Vec<(String, String)>,
    /// Distinct `(struct_def_path, field_name)` pairs this function writes
    /// (includes mutable borrows).
    pub fields_written: Vec<(String, String)>,
    /// Distinct callees (def_paths) — sorted, deduplicated. Useful for
    /// "same downstream call set" clustering.
    pub callees: Vec<String>,
    /// Suffix of the function's last path segment after the final `_`,
    /// or the whole name. e.g. `aabb_drag` → `"drag"`, `process` →
    /// `"process"`. Used by the multi-implementation detector.
    pub name_suffix: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FieldAccessFact {
    pub caller: String,
    pub struct_def_path: String,
    pub field_name: String,
    pub kind: AccessKind,
    pub file: String,
    pub line: u32,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AccessKind {
    Read,
    Write,
    /// `&mut place.field` — treated as a write at design level.
    MutBorrow,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiLeakFact {
    pub function: String,
    pub container: String,
    pub is_mut: bool,
    pub file: String,
    pub line: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StringlyErrorFact {
    pub function: String,
    /// `"String"` or `"&str"` — what the error half of the `Result` resolved to.
    pub error_form: String,
    pub file: String,
    pub line: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UnsafeNoSafetyDocFact {
    pub function: String,
    /// `true` if the function had no doc comment at all (vs. a doc comment
    /// that simply omitted the `# Safety` section).
    pub undocumented: bool,
    pub file: String,
    pub line: u32,
}

// --- Cache layout -----------------------------------------------------------

/// Stable hash of a project root, hex-encoded.
pub fn project_hash(project_root: &Path) -> String {
    let canon = project_root.canonicalize().unwrap_or_else(|_| project_root.to_path_buf());
    let mut h = DefaultHasher::new();
    canon.to_string_lossy().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// `~/.cache/unruster/<project-hash>/`.
pub fn cache_dir_for(project_root: &Path) -> Option<PathBuf> {
    let base = dirs::cache_dir()?.join("unruster").join(project_hash(project_root));
    Some(base)
}

/// `~/.cache/unruster/<project-hash>/<crate>-<hash>.facts.json`.
pub fn facts_path_for(project_root: &Path, crate_name: &str) -> Option<PathBuf> {
    let dir = cache_dir_for(project_root)?;
    let mut h = DefaultHasher::new();
    crate_name.hash(&mut h);
    project_root.to_string_lossy().hash(&mut h);
    let hash = format!("{:08x}", h.finish());
    Some(dir.join(format!("{crate_name}-{hash}.facts.json")))
}

pub fn write_facts(project_root: &Path, facts: &CrateFacts) -> std::io::Result<PathBuf> {
    let path = facts_path_for(project_root, &facts.crate_name)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no cache dir"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(facts)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(&path, json)?;
    Ok(path)
}

pub fn load_all_facts(project_root: &Path) -> std::io::Result<Vec<CrateFacts>> {
    let dir = cache_dir_for(project_root)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no cache dir"))?;
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&p)?;
        match serde_json::from_slice::<CrateFacts>(&bytes) {
            Ok(f) => out.push(f),
            Err(e) => eprintln!("unruster: skipping {}: {e}", p.display()),
        }
    }
    Ok(out)
}
