//! UnRuster egui viewer.
//!
//! Loads per-crate facts files from `~/Library/Caches/unruster/<project-hash>/`
//! (macOS) and shows design-level findings across five tabs:
//!
//! - **Sprawling fields** — fields written from too many functions / modules.
//! - **Co-access clusters** — groups of functions that all read the same field
//!   set (a candidate "cached projection" / single-source-of-truth refactor).
//! - **Parallel state** — pairs of fields on one struct that look like
//!   duplicate storage of the same logical data.
//! - **Multi-implementation** — functions sharing a naming/signature shape
//!   across modules; candidates for unification.
//! - **API leaks** — public functions returning concrete container types.
//!
//! Filters are pure post-processing so thresholds can be tuned without
//! recompiling. "Export report" writes a Markdown file aimed at being fed to
//! an LLM for follow-up.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use eframe::egui;
use unruster_facts::{
    AccessKind, ApiLeakFact, CrateFacts, FunctionProfile, TypeShape, cache_dir_for,
    load_all_facts,
};

fn main() -> eframe::Result<()> {
    let (project_root, export_only) = parse_args();

    if export_only {
        let mut app = App::new(project_root);
        app.export_report();
        match (&app.last_export, &app.load_error) {
            (Some(p), _) => println!("wrote {}", p.display()),
            (None, Some(e)) => {
                eprintln!("export failed: {e}");
                std::process::exit(1);
            }
            _ => {
                eprintln!("export failed: unknown error");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 760.0])
            .with_title("UnRuster"),
        ..Default::default()
    };
    eframe::run_native(
        "UnRuster",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(project_root)))),
    )
}

/// CLI: positional path = project root. `--export` runs headless and writes
/// the Markdown report to `<project>/unruster-report.md`.
fn parse_args() -> (PathBuf, bool) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let export_only = args.iter().any(|a| a == "--export");
    let path_arg = args.into_iter().find(|a| !a.starts_with('-'));
    let project_root = match path_arg {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    (project_root, export_only)
}

// --- App state --------------------------------------------------------------

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Sprawling,
    CoAccess,
    Parallel,
    MultiImpl,
    Leaks,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Sprawling => "Sprawling fields",
            Tab::CoAccess => "Co-access clusters",
            Tab::Parallel => "Parallel state",
            Tab::MultiImpl => "Multi-implementation",
            Tab::Leaks => "API leaks",
        }
    }
}

struct App {
    project_root: PathBuf,
    facts: Vec<CrateFacts>,
    load_error: Option<String>,
    tab: Tab,

    // Sprawling-fields filters.
    min_writers: usize,
    min_modules: usize,
    include_test: bool,
    count_mut_borrow_as_write: bool,
    name_filter: String,
    exclude_filter: String,
    hide_std: bool,

    // Co-access filters.
    coaccess_min_fields: usize,
    coaccess_min_fns: usize,
    coaccess_jaccard_pct: usize,

    // Multi-impl filters.
    multiimpl_min_fns: usize,
    multiimpl_min_modules: usize,
    multiimpl_jaccard_pct: usize,

    // Parallel-state filters.
    parallel_min_writer_jaccard_pct: usize,
    parallel_hide_encapsulated: bool,
    parallel_hide_wire_structs: bool,
    parallel_apply_idioms: bool,
    parallel_max_findings: usize,

    // Selection (sprawling tab only).
    selected_field: Option<(String, String)>,
    last_export: Option<PathBuf>,
}

impl App {
    fn new(project_root: PathBuf) -> Self {
        let (facts, err) = match load_all_facts(&project_root) {
            Ok(f) => (f, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        Self {
            project_root,
            facts,
            load_error: err,
            tab: Tab::Sprawling,
            min_writers: 3,
            min_modules: 1,
            include_test: false,
            count_mut_borrow_as_write: true,
            name_filter: String::new(),
            exclude_filter: String::new(),
            hide_std: true,
            coaccess_min_fields: 3,
            coaccess_min_fns: 3,
            coaccess_jaccard_pct: 60,
            multiimpl_min_fns: 2,
            multiimpl_min_modules: 2,
            multiimpl_jaccard_pct: 15,
            parallel_min_writer_jaccard_pct: 80,
            parallel_hide_encapsulated: true,
            parallel_hide_wire_structs: true,
            parallel_apply_idioms: true,
            parallel_max_findings: 25,
            selected_field: None,
            last_export: None,
        }
    }

    fn reload(&mut self) {
        match load_all_facts(&self.project_root) {
            Ok(f) => {
                self.facts = f;
                self.load_error = None;
            }
            Err(e) => {
                self.facts.clear();
                self.load_error = Some(e.to_string());
            }
        }
    }

    /// Iterate every FunctionProfile from every crate.
    fn all_profiles(&self) -> impl Iterator<Item = &FunctionProfile> {
        self.facts.iter().flat_map(|c| c.function_profiles.iter())
    }
}

// --- Sprawling-field aggregation -------------------------------------------

#[derive(Default)]
struct FieldAggregate {
    struct_def_path: String,
    field_name: String,
    writers: BTreeSet<String>,
    readers: BTreeSet<String>,
    writer_sites: Vec<(String, String, u32)>,
}

impl FieldAggregate {
    fn modules(&self) -> BTreeSet<String> {
        self.writers.iter().map(|w| module_of(w)).collect()
    }
    fn severity(&self) -> usize {
        (self.writers.len() / 3).saturating_add(self.modules().len().saturating_sub(1))
    }
}

fn aggregate_fields(app: &App) -> Vec<FieldAggregate> {
    let is_test_fn: BTreeMap<&str, bool> = app
        .facts
        .iter()
        .flat_map(|f| f.functions.iter().map(|fn_| (fn_.def_path.as_str(), fn_.is_test)))
        .collect();

    let mut by_field: BTreeMap<(String, String), FieldAggregate> = BTreeMap::new();
    for facts in &app.facts {
        for acc in &facts.field_accesses {
            if !app.include_test && *is_test_fn.get(acc.caller.as_str()).unwrap_or(&false) {
                continue;
            }
            if app.hide_std && acc.struct_def_path.starts_with("std::") {
                continue;
            }
            let key = (acc.struct_def_path.clone(), acc.field_name.clone());
            let entry = by_field.entry(key.clone()).or_insert_with(|| FieldAggregate {
                struct_def_path: key.0.clone(),
                field_name: key.1.clone(),
                ..Default::default()
            });
            let is_write = matches!(acc.kind, AccessKind::Write)
                || (app.count_mut_borrow_as_write && matches!(acc.kind, AccessKind::MutBorrow));
            if is_write {
                entry.writers.insert(acc.caller.clone());
                entry.writer_sites.push((acc.caller.clone(), acc.file.clone(), acc.line));
            } else {
                entry.readers.insert(acc.caller.clone());
            }
        }
    }

    let mut out: Vec<_> = by_field.into_values().collect();
    out.retain(|a| {
        a.writers.len() >= app.min_writers
            && a.modules().len() >= app.min_modules
            && matches_text(&format!("{}::{}", a.struct_def_path, a.field_name), app)
    });
    out.sort_by(|a, b| {
        b.writers
            .len()
            .cmp(&a.writers.len())
            .then_with(|| b.modules().len().cmp(&a.modules().len()))
    });
    out
}

fn matches_text(path: &str, app: &App) -> bool {
    if !app.name_filter.is_empty() && !path.contains(&app.name_filter) {
        return false;
    }
    if !app.exclude_filter.is_empty() && path.contains(&app.exclude_filter) {
        return false;
    }
    true
}

fn module_of(def_path: &str) -> String {
    match def_path.rsplit_once("::") {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    }
}

/// Take a few prefix segments of the def_path so "modules touched" is
/// meaningful instead of just "Type".
fn module_root_of(def_path: &str) -> String {
    let segs: Vec<&str> = def_path.split("::").collect();
    let take = segs.len().saturating_sub(1).min(2);
    segs.iter().take(take).copied().collect::<Vec<_>>().join("::")
}

// --- Co-access clusters -----------------------------------------------------

#[derive(Default)]
struct CoAccessCluster {
    fields: BTreeSet<(String, String)>,
    members: Vec<String>,
}

fn compute_coaccess_clusters(app: &App) -> Vec<CoAccessCluster> {
    let threshold = app.coaccess_jaccard_pct as f32 / 100.0;
    let min_fields = app.coaccess_min_fields;

    // Filter usable profiles: enough field reads, not test fns (unless requested).
    let is_test_fn: BTreeMap<&str, bool> = app
        .facts
        .iter()
        .flat_map(|f| f.functions.iter().map(|fn_| (fn_.def_path.as_str(), fn_.is_test)))
        .collect();

    let profiles: Vec<(&str, BTreeSet<(String, String)>)> = app
        .all_profiles()
        .filter(|p| p.fields_read.len() >= min_fields)
        .filter(|p| app.include_test || !*is_test_fn.get(p.def_path.as_str()).unwrap_or(&false))
        .filter(|p| !is_macro_generated(&p.def_path))
        .map(|p| {
            let s: BTreeSet<_> = p
                .fields_read
                .iter()
                // Filter out reads of fields on the function's own owning
                // type — a method reading its own struct's fields is trivial
                // and would dominate every cluster otherwise.
                .filter(|(struct_path, _)| !is_self_field_read(&p.def_path, struct_path))
                .filter(|(s, _)| !app.hide_std || !s.starts_with("std::"))
                .cloned()
                .collect();
            (p.def_path.as_str(), s)
        })
        .filter(|(_, s)| s.len() >= min_fields)
        .collect();

    // Greedy single-link clustering: walk in order, add to existing cluster
    // if Jaccard against any member ≥ threshold; else start a new cluster.
    // Adequate for the sizes we see (a few thousand profiles).
    let mut clusters: Vec<CoAccessCluster> = Vec::new();
    'next: for (name, fields) in &profiles {
        if fields.is_empty() {
            continue;
        }
        for c in clusters.iter_mut() {
            // Compare against the cluster's *intersection* (its current
            // signature) so the cluster's identity tightens, not loosens.
            let inter: BTreeSet<_> = c.fields.intersection(fields).cloned().collect();
            let union_sz = c.fields.union(fields).count();
            if union_sz == 0 {
                continue;
            }
            let jac = inter.len() as f32 / union_sz as f32;
            if jac >= threshold {
                c.fields = inter;
                c.members.push(name.to_string());
                continue 'next;
            }
        }
        clusters.push(CoAccessCluster {
            fields: fields.clone(),
            members: vec![name.to_string()],
        });
    }

    clusters.retain(|c| {
        let distinct_structs = c.fields.iter().map(|(s, _)| s.as_str()).collect::<BTreeSet<_>>().len();
        c.members.len() >= app.coaccess_min_fns
            && c.fields.len() >= min_fields
            && distinct_modules(&c.members) >= 2
            // Clusters where every shared field lives on a single struct
            // mostly surface "people use this small data type" (Point, etc.).
            // Cross-cutting clusters span ≥ 2 distinct owning structs.
            && distinct_structs >= 2
    });
    clusters.sort_by(|a, b| b.members.len().cmp(&a.members.len()));
    clusters
}

fn distinct_modules(members: &[String]) -> usize {
    members.iter().map(|m| module_root_of(m)).collect::<BTreeSet<_>>().len()
}

// --- Parallel state ---------------------------------------------------------

struct ParallelStateFinding {
    struct_def_path: String,
    file: String,
    line: u32,
    field_a: String,
    field_b: String,
    shape_a: TypeShape,
    shape_b: TypeShape,
    reason: String,
    tier: ShapeTier,
    writers_a: BTreeSet<String>,
    writers_b: BTreeSet<String>,
    writer_jaccard: f32,
    severity: f32,
    encapsulated: bool,
}

fn compute_parallel_state(app: &App) -> Vec<ParallelStateFinding> {
    let writer_index = build_writer_index(app);
    let base_jac = app.parallel_min_writer_jaccard_pct as f32 / 100.0;

    let mut out: Vec<ParallelStateFinding> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for facts in &app.facts {
        for s in &facts.structs {
            if app.hide_std && s.def_path.starts_with("std::") {
                continue;
            }
            if is_macro_generated(&s.def_path) {
                continue;
            }
            // Same struct can appear multiple times when a derive macro
            // expands inside the crate; only process each def_path once.
            if !seen.insert(s.def_path.as_str()) {
                continue;
            }

            let triplet_idiom_fields: BTreeSet<String> = if app.parallel_apply_idioms {
                let names: BTreeSet<&str> = s.fields.iter().map(|f| f.name.as_str()).collect();
                collect_triplet_idiom_fields(&names)
            } else {
                BTreeSet::new()
            };

            for i in 0..s.fields.len() {
                for j in (i + 1)..s.fields.len() {
                    let (Some(sa), Some(sb)) = (
                        s.fields[i].type_shape.as_ref(),
                        s.fields[j].type_shape.as_ref(),
                    ) else {
                        continue;
                    };
                    let Some((reason, tier)) = parallel_reason(sa, sb) else {
                        continue;
                    };
                    let name_a = &s.fields[i].name;
                    let name_b = &s.fields[j].name;

                    // Phase 4: idiom blocklist (pairs + triplets).
                    if app.parallel_apply_idioms {
                        if triplet_idiom_fields.contains(name_a.as_str())
                            && triplet_idiom_fields.contains(name_b.as_str())
                        {
                            continue;
                        }
                        if is_known_idiom_pair(name_a, name_b) {
                            continue;
                        }
                    }

                    let key_a = (s.def_path.clone(), name_a.clone());
                    let key_b = (s.def_path.clone(), name_b.clone());
                    let writers_a = writer_index.get(&key_a).cloned().unwrap_or_default();
                    let writers_b = writer_index.get(&key_b).cloned().unwrap_or_default();

                    let inter = writers_a.intersection(&writers_b).count();
                    let union = writers_a.union(&writers_b).count();
                    let writer_jaccard = if union == 0 { 0.0 } else { inter as f32 / union as f32 };

                    // Phase 2: co-mutation filter. If both fields have at
                    // least one writer and they don't share enough mutators,
                    // the fields are mutated by independent sites — not
                    // parallel storage. Weak tier requires a tighter bar
                    // (shape alone is the noisiest signal).
                    let both_have_writers = !writers_a.is_empty() && !writers_b.is_empty();
                    let effective_threshold = match tier {
                        ShapeTier::Weak => base_jac.max(0.9),
                        _ => base_jac,
                    };
                    if both_have_writers && writer_jaccard < effective_threshold {
                        continue;
                    }

                    let all_writers: BTreeSet<String> = writers_a.union(&writers_b).cloned().collect();

                    // Phase 10: wire-struct filter — all writers are serde
                    // trait-impl methods. Wire structs are Jaccard 1.0 by
                    // construction (one serializer touches every field) and
                    // their layout is a serialization constraint, not a
                    // design smell.
                    let all_serde = !all_writers.is_empty()
                        && all_writers.iter().all(|w| is_serde_method(w));
                    if app.parallel_hide_wire_structs && all_serde {
                        continue;
                    }

                    // Phase 3: encapsulation filter — all writers are methods
                    // on the owning struct AND the mutator set is small.
                    let encapsulated = !all_writers.is_empty()
                        && all_writers
                            .iter()
                            .all(|w| is_self_field_read(w, &s.def_path));
                    if app.parallel_hide_encapsulated && encapsulated && all_writers.len() <= 4 {
                        continue;
                    }

                    // Phase 6: severity = Jaccard × min-writers × tier weight.
                    let min_writers = writers_a.len().min(writers_b.len()).max(1) as f32;
                    let jac_for_score = if writer_jaccard <= 0.0 { 0.1 } else { writer_jaccard };
                    let severity = jac_for_score * min_writers * tier.weight();

                    out.push(ParallelStateFinding {
                        struct_def_path: s.def_path.clone(),
                        file: s.file.clone(),
                        line: s.line,
                        field_a: name_a.clone(),
                        field_b: name_b.clone(),
                        shape_a: sa.clone(),
                        shape_b: sb.clone(),
                        reason,
                        tier,
                        writers_a,
                        writers_b,
                        writer_jaccard,
                        severity,
                        encapsulated,
                    });
                }
            }
        }
    }
    out.retain(|f| matches_text(&f.struct_def_path, app));
    out.sort_by(|a, b| {
        b.severity
            .partial_cmp(&a.severity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(app.parallel_max_findings);
    out
}

/// Why two fields might be parallel state. None means "not flagging."
/// Returns the reason text and a tier indicating how strong the shape signal is.
fn parallel_reason(a: &TypeShape, b: &TypeShape) -> Option<(String, ShapeTier)> {
    if !is_container(&a.outer) || !is_container(&b.outer) {
        return None;
    }
    // Strong: both unordered keyed containers (Map/Set), same key type. This
    // is the genuine "two indexes of the same set" shape.
    if is_unordered_keyed(&a.outer)
        && is_unordered_keyed(&b.outer)
        && !a.args.is_empty()
        && a.args.first() == b.args.first()
    {
        return Some((
            format!(
                "both unordered, keyed on `{}` ({} + {})",
                a.args[0], a.outer, b.outer
            ),
            ShapeTier::Strong,
        ));
    }
    // Medium: same container kind, same first generic. E.g. two `Vec<NodeId>`
    // or two `HashMap<NodeId, _>` (the latter is already Strong above).
    if a.outer == b.outer && !a.args.is_empty() && a.args.first() == b.args.first() {
        return Some((
            format!("both `{}<{}…>`", a.outer, a.args[0]),
            ShapeTier::Medium,
        ));
    }
    // Weak: different containers, same first generic (Vec<T> + Map<T, _>).
    // Noisiest rule — must clear a tighter Jaccard bar in compute_parallel_state.
    if !a.args.is_empty() && !b.args.is_empty() && a.args.first() == b.args.first() {
        return Some((
            format!(
                "both index over `{}` ({}<{}> vs {}<{}>)",
                a.args[0], a.outer, a.args[0], b.outer, b.args[0]
            ),
            ShapeTier::Weak,
        ));
    }
    None
}

/// Heuristic: items inside derive-macro-expanded scopes (serde, etc.) carry
/// a `::_::` marker in their def_path that authored code never produces.
fn is_macro_generated(def_path: &str) -> bool {
    def_path.contains("::_::")
        || def_path.contains("::__Visitor")
        || def_path.contains("::__FieldVisitor")
}

/// True when a function's def_path looks like a serde trait-impl method:
/// `<T as serde::ser::Serialize>::serialize`, `<T as Deserialize<'_>>::deserialize`,
/// or a derive-generated visitor shim. Used to recognise wire structs whose
/// fields are only mutated by serialization code (Jaccard 1.0 by definition).
fn is_serde_method(def_path: &str) -> bool {
    if def_path.contains("::__Visitor") || def_path.contains("::__FieldVisitor") {
        return true;
    }
    if let Some(idx) = def_path.find(" as ") {
        let trait_part = &def_path[idx + 4..];
        return trait_part.starts_with("serde::")
            || trait_part.starts_with("_serde::")
            || trait_part.contains("Serialize")
            || trait_part.contains("Deserialize");
    }
    false
}

/// Common trait-method or derive-method names that produce noise in
/// multi-implementation grouping.
fn is_noise_suffix(s: &str) -> bool {
    matches!(
        s,
        "new" | "default" | "drop" | "fmt" | "clone" | "from" | "into"
        | "eq" | "ne" | "hash" | "deserialize" | "serialize"
        | "visit" | "expecting" | "next" | "cmp" | "partial_cmp"
        | "bytes" | "str" | "u64" | "u32" | "i64" | "i32" | "seq"
        | "key" | "value" | "size_hint" | "len" | "is_empty"
    )
}

/// True when a function defined as a method on type T is reading a field of T.
/// E.g. `model::math::Point::is_finite` reading `model::math::Point::x`.
/// Used to filter out trivial intra-method-self reads from co-access clusters.
/// Returns the last `_`-tail of the module-name segment that owns the
/// function. Walks the def-path right-to-left skipping the method name,
/// any `<impl …>` shim segments, and any uppercase-typed segments —
/// leaving the lowercase Rust module the function lives in.
///
/// `app::anim_aabb_drag::<impl VectorianApp>::start_anim_scale_drag`
///   → owner module is `anim_aabb_drag`, returns `Some("drag")`.
fn module_tail_word(fn_def_path: &str) -> Option<String> {
    let segs: Vec<&str> = fn_def_path.split("::").collect();
    let module = segs.iter().rev().find(|s| {
        let s = s.trim();
        !s.is_empty()
            && !s.starts_with('<')         // skip `<impl ...>`
            && !starts_uppercase(s)        // skip type segments
    })?;
    // Strip the trailing method name (the last segment we found that
    // matched the filter might be the method itself if it's snake_case).
    // To handle that, take the *second* such match — the module above
    // the method.
    let mut lowercase_iter = segs.iter().rev().filter(|s| {
        !s.starts_with('<') && !starts_uppercase(s)
    });
    let _method = lowercase_iter.next();
    let module = lowercase_iter.next().unwrap_or(module);
    Some(module.rsplit('_').next().unwrap_or(module).to_string())
}

fn starts_uppercase(s: &str) -> bool {
    s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
}

fn is_self_field_read(fn_def_path: &str, struct_def_path: &str) -> bool {
    // Strip trailing `::method_name` from the fn def path.
    let owner = fn_def_path.rsplit_once("::").map(|(o, _)| o).unwrap_or("");
    // For methods inside `impl Trait for Type`, the def_path looks like
    // `<Type as Trait>::method`. Pull the inner type if present.
    let owner = owner.trim_start_matches('<');
    let owner = owner.split_once(" as ").map(|(t, _)| t).unwrap_or(owner);
    let owner = owner.trim_end_matches('>');
    owner == struct_def_path
}

fn is_container(name: &str) -> bool {
    matches!(
        name,
        "Vec" | "VecDeque" | "LinkedList" | "HashMap" | "BTreeMap" | "HashSet" | "BTreeSet" | "SmallVec" | "IndexMap"
    )
}

fn is_unordered_keyed(name: &str) -> bool {
    matches!(name, "HashMap" | "BTreeMap" | "HashSet" | "BTreeSet" | "IndexMap")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShapeTier {
    Strong,
    Medium,
    Weak,
}

impl ShapeTier {
    fn weight(self) -> f32 {
        match self {
            ShapeTier::Strong => 1.0,
            ShapeTier::Medium => 0.7,
            ShapeTier::Weak => 0.4,
        }
    }
    fn label(self) -> &'static str {
        match self {
            ShapeTier::Strong => "strong",
            ShapeTier::Medium => "medium",
            ShapeTier::Weak => "weak",
        }
    }
}

const IDIOM_PAIRS: &[(&str, &str)] = &[
    ("undo", "redo"),
    ("undo_stack", "redo_stack"),
    ("undos", "redos"),
    ("x", "y"),
    ("width", "height"),
    ("w", "h"),
    ("min", "max"),
    ("start", "end"),
    ("first", "last"),
    ("from", "to"),
    ("src", "dst"),
    ("source", "target"),
    ("input", "output"),
    ("i", "o"),
    ("in_tangent", "out_tangent"),
];

const IDIOM_TRIPLETS: &[(&str, &str, &str)] = &[
    ("i", "o", "v"),
    ("in_tangent", "out_tangent", "vertex"),
    ("x", "y", "z"),
    ("r", "g", "b"),
];

fn is_known_idiom_pair(a: &str, b: &str) -> bool {
    IDIOM_PAIRS
        .iter()
        .any(|(p, q)| (a == *p && b == *q) || (a == *q && b == *p))
}

fn collect_triplet_idiom_fields(field_names: &BTreeSet<&str>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (a, b, c) in IDIOM_TRIPLETS {
        if field_names.contains(*a) && field_names.contains(*b) && field_names.contains(*c) {
            out.insert((*a).to_string());
            out.insert((*b).to_string());
            out.insert((*c).to_string());
        }
    }
    out
}

fn build_writer_index(app: &App) -> BTreeMap<(String, String), BTreeSet<String>> {
    let is_test_fn: BTreeMap<&str, bool> = app
        .facts
        .iter()
        .flat_map(|f| f.functions.iter().map(|fn_| (fn_.def_path.as_str(), fn_.is_test)))
        .collect();
    let mut idx: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for facts in &app.facts {
        for acc in &facts.field_accesses {
            if !app.include_test && *is_test_fn.get(acc.caller.as_str()).unwrap_or(&false) {
                continue;
            }
            if !matches!(acc.kind, AccessKind::Write | AccessKind::MutBorrow) {
                continue;
            }
            idx.entry((acc.struct_def_path.clone(), acc.field_name.clone()))
                .or_default()
                .insert(acc.caller.clone());
        }
    }
    idx
}

// --- Multi-implementation ---------------------------------------------------

struct MultiImplGroup {
    suffix: String,
    members: Vec<String>,    // def_paths
    modules: BTreeSet<String>,
    shared_callees: BTreeSet<String>,
}

fn compute_multi_impl(app: &App) -> Vec<MultiImplGroup> {
    let threshold = app.multiimpl_jaccard_pct as f32 / 100.0;

    let is_test_fn: BTreeMap<&str, bool> = app
        .facts
        .iter()
        .flat_map(|f| f.functions.iter().map(|fn_| (fn_.def_path.as_str(), fn_.is_test)))
        .collect();

    // Two grouping passes, unioned together: by function-name suffix AND by
    // *module-name* suffix. The latter is what catches the `aabb_drag` /
    // `cp_drag` / `anim_aabb_drag` pattern where the unification opportunity
    // lives in the module names rather than the function names themselves.
    let mut groups_by_key: BTreeMap<String, Vec<&FunctionProfile>> = BTreeMap::new();
    for p in app.all_profiles() {
        if !app.include_test && *is_test_fn.get(p.def_path.as_str()).unwrap_or(&false) {
            continue;
        }
        if is_macro_generated(&p.def_path) {
            continue;
        }

        // Key 1: name_suffix (existing behavior, with the noise filter).
        if !is_noise_suffix(&p.name_suffix) && p.name_suffix.len() >= 3 {
            groups_by_key.entry(format!("name:{}", p.name_suffix)).or_default().push(p);
        }

        // Key 2: last word of the *module path* (after final `::` and final `_`).
        if let Some(mod_suffix) = module_tail_word(&p.def_path) {
            if mod_suffix.len() >= 3 && !is_noise_suffix(&mod_suffix) {
                groups_by_key.entry(format!("mod:{}", mod_suffix)).or_default().push(p);
            }
        }
    }
    // Dedupe entries within each group (a profile can match both keys).
    let by_suffix: BTreeMap<String, Vec<&FunctionProfile>> = groups_by_key
        .into_iter()
        .map(|(k, v)| {
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            let v: Vec<_> = v.into_iter().filter(|p| seen.insert(p.def_path.as_str())).collect();
            (k, v)
        })
        .collect();

    let mut out = Vec::new();
    for (key, members) in by_suffix {
        if members.len() < app.multiimpl_min_fns {
            continue;
        }
        let modules: BTreeSet<String> = members.iter().map(|p| module_root_of(&p.def_path)).collect();
        if modules.len() < app.multiimpl_min_modules {
            continue;
        }
        // Strip the `name:` / `mod:` prefix for display.
        let suffix = key.splitn(2, ':').nth(1).unwrap_or(&key).to_string();

        // Compute shared callee set (intersection across members).
        let mut shared: BTreeSet<String> = members[0].callees.iter().cloned().collect();
        for m in &members[1..] {
            let mc: BTreeSet<String> = m.callees.iter().cloned().collect();
            shared = shared.intersection(&mc).cloned().collect();
        }
        // Jaccard of any pair — use first pair as a proxy. Cheap heuristic.
        let union: BTreeSet<String> = members
            .iter()
            .flat_map(|m| m.callees.iter().cloned())
            .collect();
        let jac = if union.is_empty() {
            0.0
        } else {
            shared.len() as f32 / union.len() as f32
        };
        // Escape hatch: groups with many members across many modules are
        // signal on their own — don't require strong callee overlap.
        let large_group = members.len() >= 5 && modules.len() >= 3;
        if jac < threshold && !large_group {
            continue;
        }
        out.push(MultiImplGroup {
            suffix,
            members: members.iter().map(|p| p.def_path.clone()).collect(),
            modules,
            shared_callees: shared,
        });
    }
    // After both grouping passes (name + module suffix), dedupe groups whose
    // member sets are identical or one is a subset of the other.
    out.sort_by(|a, b| b.members.len().cmp(&a.members.len()));
    let mut kept: Vec<MultiImplGroup> = Vec::new();
    for g in out {
        let g_set: BTreeSet<&str> = g.members.iter().map(String::as_str).collect();
        let subsumed = kept.iter().any(|k| {
            let k_set: BTreeSet<&str> = k.members.iter().map(String::as_str).collect();
            g_set.is_subset(&k_set)
        });
        if !subsumed {
            kept.push(g);
        }
    }
    let mut out = kept;
    out.sort_by(|a, b| b.members.len().cmp(&a.members.len()));
    out
}

// --- UI ---------------------------------------------------------------------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("UnRuster");
                ui.separator();
                ui.label(format!("Project: {}", self.project_root.display()));
                if let Some(dir) = cache_dir_for(&self.project_root) {
                    ui.label(format!("Cache: {}", dir.display()));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Export report…").clicked() {
                        self.export_report();
                    }
                    if ui.button("Reload").clicked() {
                        self.reload();
                    }
                });
            });
            ui.horizontal(|ui| {
                for tab in [Tab::Sprawling, Tab::CoAccess, Tab::Parallel, Tab::MultiImpl, Tab::Leaks] {
                    if ui.selectable_label(self.tab == tab, tab.label()).clicked() {
                        self.tab = tab;
                    }
                }
            });
            if let Some(err) = &self.load_error {
                ui.colored_label(egui::Color32::RED, format!("Load error: {err}"));
            }
            if let Some(path) = &self.last_export {
                ui.colored_label(egui::Color32::LIGHT_GREEN, format!("Exported: {}", path.display()));
            }
        });

        // Left filter panel — contents depend on active tab.
        egui::SidePanel::left("filters").resizable(true).default_width(280.0).show(ctx, |ui| {
            ui.heading("Filters");
            ui.checkbox(&mut self.include_test, "include #[test] callers");
            ui.checkbox(&mut self.hide_std, "hide std::*");
            ui.separator();
            match self.tab {
                Tab::Sprawling => {
                    ui.add(egui::Slider::new(&mut self.min_writers, 1..=30).text("min writers"));
                    ui.add(egui::Slider::new(&mut self.min_modules, 1..=10).text("min modules"));
                    ui.checkbox(&mut self.count_mut_borrow_as_write, "count &mut as write");
                }
                Tab::CoAccess => {
                    ui.add(egui::Slider::new(&mut self.coaccess_min_fields, 2..=15).text("min field-set size"));
                    ui.add(egui::Slider::new(&mut self.coaccess_min_fns, 2..=20).text("min fns in cluster"));
                    ui.add(egui::Slider::new(&mut self.coaccess_jaccard_pct, 30..=95).text("Jaccard ≥ %"));
                }
                Tab::Parallel => {
                    ui.label(
                        "Pairs of fields on one struct whose types share a key/element, \
                         filtered by writer-set co-mutation and ranked by shape strength.",
                    );
                    ui.add(
                        egui::Slider::new(&mut self.parallel_min_writer_jaccard_pct, 0..=100)
                            .text("min writer Jaccard %"),
                    );
                    ui.checkbox(
                        &mut self.parallel_hide_encapsulated,
                        "hide encapsulated pairs",
                    );
                    ui.checkbox(
                        &mut self.parallel_hide_wire_structs,
                        "hide serde wire structs",
                    );
                    ui.checkbox(&mut self.parallel_apply_idioms, "apply idiom blocklist");
                    ui.add(
                        egui::Slider::new(&mut self.parallel_max_findings, 5..=200)
                            .text("max findings"),
                    );
                }
                Tab::MultiImpl => {
                    ui.add(egui::Slider::new(&mut self.multiimpl_min_fns, 2..=10).text("min fns in group"));
                    ui.add(egui::Slider::new(&mut self.multiimpl_min_modules, 2..=10).text("min modules"));
                    ui.add(egui::Slider::new(&mut self.multiimpl_jaccard_pct, 20..=90).text("shared callee ≥ %"));
                }
                Tab::Leaks => {
                    ui.label("Pure-syntactic check: public fns returning concrete container types.");
                }
            }
            ui.separator();
            ui.label("Path contains:");
            ui.text_edit_singleline(&mut self.name_filter);
            ui.label("…and not:");
            ui.text_edit_singleline(&mut self.exclude_filter);
            ui.separator();
            ui.label(format!("Crates loaded: {}", self.facts.len()));
            let n_fn: usize = self.facts.iter().map(|f| f.functions.len()).sum();
            let n_struct: usize = self.facts.iter().map(|f| f.structs.len()).sum();
            let n_access: usize = self.facts.iter().map(|f| f.field_accesses.len()).sum();
            let n_calls: usize = self.facts.iter().map(|f| f.calls.len()).sum();
            ui.label(format!("Functions: {n_fn}"));
            ui.label(format!("Structs:   {n_struct}"));
            ui.label(format!("Accesses:  {n_access}"));
            ui.label(format!("Calls:     {n_calls}"));
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            match self.tab {
                Tab::Sprawling => self.render_sprawling(ui),
                Tab::CoAccess => self.render_coaccess(ui),
                Tab::Parallel => self.render_parallel(ui),
                Tab::MultiImpl => self.render_multi_impl(ui),
                Tab::Leaks => self.render_leaks(ui),
            }
        });
    }
}

impl App {
    fn render_sprawling(&mut self, ui: &mut egui::Ui) {
        ui.heading("Sprawling fields");
        ui.label("Fields with many writers across many modules — invariants likely live nowhere.");
        ui.separator();
        let aggregates = aggregate_fields(self);
        let selected = self.selected_field.clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            if aggregates.is_empty() {
                ui.weak("No findings match current filters.");
            }
            for agg in &aggregates {
                let label = format!(
                    "{}::{}  —  {} writers / {} modules  {}",
                    agg.struct_def_path,
                    agg.field_name,
                    agg.writers.len(),
                    agg.modules().len(),
                    "★".repeat(agg.severity().min(5)),
                );
                let is_selected = selected
                    .as_ref()
                    .map(|s| s.0 == agg.struct_def_path && s.1 == agg.field_name)
                    .unwrap_or(false);
                if ui.selectable_label(is_selected, label).clicked() {
                    self.selected_field = Some((agg.struct_def_path.clone(), agg.field_name.clone()));
                }
                if is_selected {
                    ui.indent("writers", |ui| {
                        for (caller, file, line) in &agg.writer_sites {
                            ui.label(format!("  • {caller}  ({file}:{line})"));
                        }
                    });
                }
            }
        });
    }

    fn render_coaccess(&mut self, ui: &mut egui::Ui) {
        ui.heading("Co-access clusters");
        ui.label(
            "Groups of functions that read the same field set. Each cluster is a candidate \
             for a cached projection (single-source-of-truth refactor).",
        );
        ui.separator();
        let clusters = compute_coaccess_clusters(self);
        egui::ScrollArea::vertical().show(ui, |ui| {
            if clusters.is_empty() {
                ui.weak("No co-access clusters match current filters.");
            }
            for c in &clusters {
                ui.label(format!(
                    "▶ {} functions across {} modules read the same {} fields",
                    c.members.len(),
                    distinct_modules(&c.members),
                    c.fields.len()
                ));
                ui.indent("fs", |ui| {
                    ui.label("Fields:");
                    for (s, f) in &c.fields {
                        ui.label(format!("  • {s}::{f}"));
                    }
                    ui.label("Functions:");
                    for m in &c.members {
                        ui.label(format!("  • {m}"));
                    }
                });
                ui.separator();
            }
        });
    }

    fn render_parallel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Parallel state");
        ui.label(
            "Pairs of fields on the same struct that look like duplicate storage of the same \
             logical data. Ranked by writer-set co-mutation × shape strength.",
        );
        ui.separator();
        let findings = compute_parallel_state(self);
        egui::ScrollArea::vertical().show(ui, |ui| {
            if findings.is_empty() {
                ui.weak("No parallel-state pairs found.");
            }
            for f in &findings {
                let stars = "★".repeat((f.severity.round() as usize).clamp(0, 5));
                let shared = f.writers_a.intersection(&f.writers_b).count();
                ui.label(format!(
                    "▶ {}  ({}:{})  {}  [{}]",
                    f.struct_def_path,
                    f.file,
                    f.line,
                    stars,
                    f.tier.label(),
                ));
                ui.indent("p", |ui| {
                    ui.label(format!(
                        "  • `{}: {}<{}>`",
                        f.field_a,
                        f.shape_a.outer,
                        f.shape_a.args.join(", ")
                    ));
                    ui.label(format!(
                        "  • `{}: {}<{}>`",
                        f.field_b,
                        f.shape_b.outer,
                        f.shape_b.args.join(", ")
                    ));
                    ui.label(format!("  → {}", f.reason));
                    ui.label(format!(
                        "  writers: {} / {} (shared {}), Jaccard {:.0}%{}",
                        f.writers_a.len(),
                        f.writers_b.len(),
                        shared,
                        f.writer_jaccard * 100.0,
                        if f.encapsulated { ", encapsulated" } else { "" },
                    ));
                });
                ui.separator();
            }
        });
    }

    fn render_multi_impl(&mut self, ui: &mut egui::Ui) {
        ui.heading("Multi-implementation candidates");
        ui.label(
            "Groups of functions sharing a name suffix, signature shape, and downstream callees — \
             candidates for unification under one dispatch point.",
        );
        ui.separator();
        let groups = compute_multi_impl(self);
        egui::ScrollArea::vertical().show(ui, |ui| {
            if groups.is_empty() {
                ui.weak("No multi-implementation groups match current filters.");
            }
            for g in &groups {
                ui.label(format!(
                    "▶ {} functions sharing suffix `_{}` across {} modules",
                    g.members.len(),
                    g.suffix,
                    g.modules.len()
                ));
                ui.indent("m", |ui| {
                    ui.label("Members:");
                    for m in &g.members {
                        ui.label(format!("  • {m}"));
                    }
                    if !g.shared_callees.is_empty() {
                        ui.label(format!("Shared callees ({}):", g.shared_callees.len()));
                        for c in g.shared_callees.iter().take(8) {
                            ui.label(format!("  · {c}"));
                        }
                        if g.shared_callees.len() > 8 {
                            ui.label(format!("  · … and {} more", g.shared_callees.len() - 8));
                        }
                    }
                });
                ui.separator();
            }
        });
    }

    fn render_leaks(&mut self, ui: &mut egui::Ui) {
        ui.heading("API leaks");
        ui.label("Public functions returning concrete container types.");
        ui.separator();
        let leaks: Vec<&ApiLeakFact> = self.facts.iter().flat_map(|f| f.api_leaks.iter()).collect();
        egui::ScrollArea::vertical().show(ui, |ui| {
            if leaks.is_empty() {
                ui.weak("No api_leak findings.");
            }
            for leak in &leaks {
                let kind = if leak.is_mut { "&mut" } else { "&" };
                ui.label(format!(
                    "• {}  →  returns {} {}<…>  ({}:{})",
                    leak.function, kind, leak.container, leak.file, leak.line
                ));
            }
        });
    }
}

// --- Markdown export --------------------------------------------------------

impl App {
    fn export_report(&mut self) {
        let path = self.project_root.join("unruster-report.md");
        let body = render_report(self);
        match std::fs::write(&path, body) {
            Ok(()) => self.last_export = Some(path),
            Err(e) => self.load_error = Some(format!("export failed: {e}")),
        }
    }
}

fn render_report(app: &App) -> String {
    let mut out = String::new();
    out.push_str("# UnRuster Design Report\n\n");
    out.push_str(&format!("**Project**: `{}`\n", app.project_root.display()));
    out.push_str(&format!("**Crates analyzed**: {}\n", app.facts.len()));

    let n_fn: usize = app.facts.iter().map(|f| f.functions.len()).sum();
    let n_struct: usize = app.facts.iter().map(|f| f.structs.len()).sum();
    let n_access: usize = app.facts.iter().map(|f| f.field_accesses.len()).sum();
    let n_calls: usize = app.facts.iter().map(|f| f.calls.len()).sum();
    out.push_str(&format!(
        "**Inventory**: {n_fn} functions, {n_struct} structs, {n_calls} call edges, {n_access} field accesses\n\n"
    ));

    // --- Sprawling -------------------------------------------------------
    out.push_str("## Sprawling fields\n\n");
    out.push_str(
        "Fields written from many functions across many modules; invariants aren't encapsulated by the owning struct.\n\n",
    );
    let aggregates = aggregate_fields(app);
    if aggregates.is_empty() {
        out.push_str("_None match current filters._\n\n");
    }
    for agg in &aggregates {
        out.push_str(&format!("### `{}::{}`\n\n", agg.struct_def_path, agg.field_name));
        out.push_str(&format!(
            "- **Writers**: {} functions across {} modules\n",
            agg.writers.len(),
            agg.modules().len()
        ));
        out.push_str("- **Writer sites**:\n");
        for (caller, file, line) in &agg.writer_sites {
            out.push_str(&format!("  - `{caller}` — `{file}:{line}`\n"));
        }
        out.push_str("- **Refactor hint**: gather writes behind a struct method (`set_X` / `with_X`) or hold the field in a sub-struct that owns the invariant.\n\n");
    }

    // --- Co-access -------------------------------------------------------
    out.push_str("## Co-access clusters (candidate cached projections)\n\n");
    out.push_str(
        "Groups of functions that read the same field set. Each is a candidate for a single \
         derivation site (a cache or helper) so consumers stop re-deriving the same value.\n\n",
    );
    let clusters = compute_coaccess_clusters(app);
    if clusters.is_empty() {
        out.push_str("_None match current filters._\n\n");
    }
    for c in &clusters {
        out.push_str(&format!(
            "### Cluster: {} fns reading {} fields\n\n",
            c.members.len(),
            c.fields.len()
        ));
        out.push_str("- **Shared fields**:\n");
        for (s, f) in &c.fields {
            out.push_str(&format!("  - `{s}::{f}`\n"));
        }
        out.push_str("- **Members**:\n");
        for m in &c.members {
            out.push_str(&format!("  - `{m}`\n"));
        }
        out.push_str("- **Refactor hint**: introduce a cached projection (e.g. precomputed map or helper method) on the owning type; have these callers read from it instead of re-deriving.\n\n");
    }

    // --- Parallel state --------------------------------------------------
    out.push_str("## Parallel state\n\n");
    out.push_str(
        "Pairs of fields on the same struct whose types suggest duplicate storage of the \
         same logical data. Ranked by writer-set co-mutation × shape strength.\n\n",
    );
    let parallel = compute_parallel_state(app);
    if parallel.is_empty() {
        out.push_str("_No findings._\n\n");
    }
    for p in &parallel {
        let stars = "★".repeat((p.severity.round() as usize).clamp(0, 5));
        let shared = p.writers_a.intersection(&p.writers_b).count();
        out.push_str(&format!(
            "- `{}` — `{}: {}<{}>` and `{}: {}<{}>` ({}) — {} {}, Jaccard {:.0}%, writers {}/{} (shared {}){} — `{}:{}`\n",
            p.struct_def_path,
            p.field_a, p.shape_a.outer, p.shape_a.args.join(", "),
            p.field_b, p.shape_b.outer, p.shape_b.args.join(", "),
            p.reason,
            p.tier.label(), stars,
            p.writer_jaccard * 100.0,
            p.writers_a.len(), p.writers_b.len(), shared,
            if p.encapsulated { ", encapsulated" } else { "" },
            p.file, p.line,
        ));
    }
    out.push('\n');

    // --- Multi-implementation -------------------------------------------
    out.push_str("## Multi-implementation candidates\n\n");
    out.push_str(
        "Functions sharing a name suffix, parameter shape, and downstream call set across modules. \
         Candidates for unification under one dispatch point.\n\n",
    );
    let groups = compute_multi_impl(app);
    if groups.is_empty() {
        out.push_str("_None match current filters._\n\n");
    }
    for g in &groups {
        out.push_str(&format!(
            "### Suffix `_{}` — {} fns across {} modules\n\n",
            g.suffix,
            g.members.len(),
            g.modules.len()
        ));
        out.push_str("- **Members**:\n");
        for m in &g.members {
            out.push_str(&format!("  - `{m}`\n"));
        }
        if !g.shared_callees.is_empty() {
            out.push_str(&format!("- **Shared callees** ({}):\n", g.shared_callees.len()));
            for c in g.shared_callees.iter().take(12) {
                out.push_str(&format!("  - `{c}`\n"));
            }
            if g.shared_callees.len() > 12 {
                out.push_str(&format!("  - …and {} more\n", g.shared_callees.len() - 12));
            }
        }
        out.push('\n');
    }

    // --- API leaks ------------------------------------------------------
    out.push_str("## API leaks\n\n");
    out.push_str(
        "Public functions returning concrete container types — leaks internal storage into the API surface.\n\n",
    );
    let leaks: Vec<&ApiLeakFact> = app.facts.iter().flat_map(|f| f.api_leaks.iter()).collect();
    if leaks.is_empty() {
        out.push_str("_None._\n\n");
    }
    for leak in &leaks {
        let kind = if leak.is_mut { "&mut" } else { "&" };
        out.push_str(&format!(
            "- `{}` returns `{} {}<…>` — `{}:{}`\n",
            leak.function, kind, leak.container, leak.file, leak.line
        ));
    }
    out.push('\n');

    out.push_str("## Filter settings used\n\n");
    out.push_str(&format!("- include #[test]: {}\n", app.include_test));
    out.push_str(&format!("- hide std::*: {}\n", app.hide_std));
    out.push_str(&format!("- sprawling: min_writers={}, min_modules={}, count_mut_borrow_as_write={}\n", app.min_writers, app.min_modules, app.count_mut_borrow_as_write));
    out.push_str(&format!("- co-access: min_fields={}, min_fns={}, Jaccard≥{}%\n", app.coaccess_min_fields, app.coaccess_min_fns, app.coaccess_jaccard_pct));
    out.push_str(&format!("- multi-impl: min_fns={}, min_modules={}, shared callees≥{}%\n", app.multiimpl_min_fns, app.multiimpl_min_modules, app.multiimpl_jaccard_pct));
    out.push_str(&format!(
        "- parallel: min_writer_jaccard={}%, hide_encapsulated={}, hide_wire_structs={}, apply_idioms={}, max_findings={}\n",
        app.parallel_min_writer_jaccard_pct,
        app.parallel_hide_encapsulated,
        app.parallel_hide_wire_structs,
        app.parallel_apply_idioms,
        app.parallel_max_findings,
    ));
    if !app.name_filter.is_empty() {
        out.push_str(&format!("- path includes: `{}`\n", app.name_filter));
    }
    if !app.exclude_filter.is_empty() {
        out.push_str(&format!("- path excludes: `{}`\n", app.exclude_filter));
    }

    out
}
