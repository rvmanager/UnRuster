//! UnRuster egui viewer.
//!
//! Loads per-crate facts files from `~/.cache/unruster/<project-hash>/` and
//! presents an interactive view of design-level findings derived from them.
//! Filters are pure post-processing (no recompile) so thresholds can be
//! tuned freely. The "Export report" button writes a Markdown file aimed at
//! being fed to an LLM for follow-up action.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use eframe::egui;
use unruster_facts::{
    AccessKind, ApiLeakFact, CrateFacts, cache_dir_for, load_all_facts,
};

fn main() -> eframe::Result<()> {
    let project_root = parse_project_root();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_title("UnRuster"),
        ..Default::default()
    };
    eframe::run_native(
        "UnRuster",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(project_root)))),
    )
}

fn parse_project_root() -> PathBuf {
    let mut args = std::env::args().skip(1);
    match args.next() {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

// --- App state --------------------------------------------------------------

struct App {
    project_root: PathBuf,
    facts: Vec<CrateFacts>,
    load_error: Option<String>,

    // Filters (live-tuned).
    min_writers: usize,
    min_modules: usize,
    include_test: bool,
    count_mut_borrow_as_write: bool,
    name_filter: String,
    exclude_filter: String,

    // Selection.
    selected_field: Option<(String, String)>, // (struct_def_path, field_name)
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
            min_writers: 3,
            min_modules: 1,
            include_test: false,
            count_mut_borrow_as_write: true,
            name_filter: String::new(),
            exclude_filter: String::new(),
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
}

// --- Aggregation (recomputed each frame; tiny dataset so this is fine) ------

#[derive(Default)]
struct FieldAggregate {
    struct_def_path: String,
    field_name: String,
    writers: BTreeSet<String>,
    readers: BTreeSet<String>,
    writer_sites: Vec<(String, String, u32)>, // (caller, file, line)
}

impl FieldAggregate {
    fn modules(&self) -> BTreeSet<String> {
        self.writers.iter().map(|w| module_of(w)).collect()
    }
    fn severity(&self) -> usize {
        // Crude: 1 star per 3 writers, 1 extra per crossed module.
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
    out.retain(|a| matches_filters(a, app));
    out.sort_by(|a, b| {
        b.writers
            .len()
            .cmp(&a.writers.len())
            .then_with(|| b.modules().len().cmp(&a.modules().len()))
    });
    out
}

fn matches_filters(a: &FieldAggregate, app: &App) -> bool {
    if a.writers.len() < app.min_writers {
        return false;
    }
    if a.modules().len() < app.min_modules {
        return false;
    }
    let path = format!("{}::{}", a.struct_def_path, a.field_name);
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
            if let Some(err) = &self.load_error {
                ui.colored_label(egui::Color32::RED, format!("Load error: {err}"));
            }
            if let Some(path) = &self.last_export {
                ui.colored_label(egui::Color32::LIGHT_GREEN, format!("Exported: {}", path.display()));
            }
        });

        let aggregates = aggregate_fields(self);

        egui::SidePanel::left("filters").resizable(true).default_width(280.0).show(ctx, |ui| {
            ui.heading("Filters");
            ui.add(egui::Slider::new(&mut self.min_writers, 1..=30).text("min writers"));
            ui.add(egui::Slider::new(&mut self.min_modules, 1..=10).text("min modules"));
            ui.checkbox(&mut self.include_test, "include #[test] callers");
            ui.checkbox(&mut self.count_mut_borrow_as_write, "count &mut as write");
            ui.separator();
            ui.label("Field path contains:");
            ui.text_edit_singleline(&mut self.name_filter);
            ui.label("…and does not contain:");
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

        let selected = self.selected_field.clone();
        let selected_agg = selected
            .as_ref()
            .and_then(|s| aggregates.iter().find(|a| (&a.struct_def_path, &a.field_name) == (&&s.0, &&s.1)));

        if let Some(agg) = selected_agg {
            egui::SidePanel::right("detail").resizable(true).default_width(380.0).show(ctx, |ui| {
                ui.heading(format!("{}::{}", agg.struct_def_path, agg.field_name));
                ui.label(format!(
                    "{} writers across {} modules",
                    agg.writers.len(),
                    agg.modules().len()
                ));
                ui.separator();
                ui.label("Writers:");
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (caller, file, line) in &agg.writer_sites {
                        ui.label(format!("• {caller}  ({file}:{line})"));
                    }
                    ui.separator();
                    ui.label("Modules touched:");
                    for m in agg.modules() {
                        ui.label(format!("  {m}"));
                    }
                });
            });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Sprawling fields (ranked)");
            ui.label("Fields with many writers across many modules — invariants likely live nowhere.");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                if aggregates.is_empty() {
                    ui.weak("No findings match current filters. Try lowering min writers/modules.");
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
                    let is_selected = self
                        .selected_field
                        .as_ref()
                        .map(|s| s.0 == agg.struct_def_path && s.1 == agg.field_name)
                        .unwrap_or(false);
                    if ui.selectable_label(is_selected, label).clicked() {
                        self.selected_field = Some((agg.struct_def_path.clone(), agg.field_name.clone()));
                    }
                }
                ui.separator();
                ui.heading("API leaks");
                let leaks: Vec<&ApiLeakFact> = self.facts.iter().flat_map(|f| f.api_leaks.iter()).collect();
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
        });
    }
}

// --- Markdown report export -------------------------------------------------

impl App {
    fn export_report(&mut self) {
        let aggregates = aggregate_fields(self);
        let path = self.project_root.join("unruster-report.md");
        let body = render_report(self, &aggregates);
        match std::fs::write(&path, body) {
            Ok(()) => self.last_export = Some(path),
            Err(e) => self.load_error = Some(format!("export failed: {e}")),
        }
    }
}

fn render_report(app: &App, aggregates: &[FieldAggregate]) -> String {
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

    out.push_str("## Filter settings\n\n");
    out.push_str(&format!("- min writers: {}\n", app.min_writers));
    out.push_str(&format!("- min modules: {}\n", app.min_modules));
    out.push_str(&format!("- include #[test] callers: {}\n", app.include_test));
    out.push_str(&format!("- count &mut as write: {}\n", app.count_mut_borrow_as_write));
    if !app.name_filter.is_empty() {
        out.push_str(&format!("- path includes: `{}`\n", app.name_filter));
    }
    if !app.exclude_filter.is_empty() {
        out.push_str(&format!("- path excludes: `{}`\n", app.exclude_filter));
    }
    out.push('\n');

    out.push_str("## Sprawling fields\n\n");
    out.push_str("Fields written from many functions across many modules. Each such field's");
    out.push_str(" invariants are not encapsulated by its owning struct.\n\n");
    if aggregates.is_empty() {
        out.push_str("_None match current filters._\n\n");
    }
    for agg in aggregates {
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
        out.push_str("- **Modules touched**: ");
        let mods: Vec<String> = agg.modules().into_iter().collect();
        out.push_str(&mods.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", "));
        out.push_str("\n\n");
    }

    out.push_str("## API leaks\n\n");
    out.push_str("Public functions returning concrete container types — leaks internal storage choice into the API surface.\n\n");
    let leaks: Vec<&ApiLeakFact> = app.facts.iter().flat_map(|f| f.api_leaks.iter()).collect();
    if leaks.is_empty() {
        out.push_str("_No findings._\n\n");
    }
    for leak in &leaks {
        let kind = if leak.is_mut { "&mut" } else { "&" };
        out.push_str(&format!(
            "- `{}` returns `{} {}<…>` — `{}:{}`\n",
            leak.function, kind, leak.container, leak.file, leak.line
        ));
    }
    out.push('\n');

    out.push_str("## Notes for follow-up\n\n");
    out.push_str("- Each \"sprawling field\" is a candidate for refactoring: gather writes behind a struct method.\n");
    out.push_str("- Each api_leak is a public-API stability risk: return slices / iterators / accessor methods instead.\n");
    out.push_str("- Findings are derived from raw HIR/MIR facts; tune thresholds in the viewer to surface different cuts.\n");

    out
}

