//! Output layer: one funnel for every row, section header, and summary line
//! the tool prints, so `--json` is a rendering choice rather than a per-command
//! reimplementation.
//!
//! Why it exists: agents consume this tool's output. The TSV form is compact
//! and pipe-friendly but forces cross-row analysis into `awk`; the JSON form
//! keeps `file`/`line` and numeric columns as real fields so a consumer can
//! filter and rank without re-parsing. Both render from the same [`Row`], so
//! the two can't drift.
//!
//! TSV output is byte-identical to what each command printed before this
//! module existed — the column shapes are asserted in `tests/cli.rs`.

use std::cell::RefCell;

/// How rows are rendered. `Tsv` writes as it goes; `Json` buffers the whole
/// run and emits one document at the end (a valid document can't be streamed
/// section-by-section).
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Format {
    Tsv,
    Json,
}

/// One typed cell. `Site` is the reason this is an enum rather than a string:
/// TSV wants `path/to/file.rs:42` as a single column, JSON wants `file` and
/// `line` as separate, sortable fields.
#[derive(Clone, Debug)]
pub enum Val {
    Str(String),
    Num(i64),
    Float(f64),
    Bool(bool),
    List(Vec<String>),
    Site { file: String, line: usize },
}

impl Val {
    /// The TSV rendering of this cell.
    fn tsv(&self) -> String {
        match self {
            Val::Str(s) => s.clone(),
            Val::Num(n) => n.to_string(),
            Val::Float(f) => format!("{:.2}", f),
            Val::Bool(b) => b.to_string(),
            Val::List(v) => v.join(","),
            Val::Site { file, line } => format!("{}:{}", file, line),
        }
    }
}

impl From<&str> for Val {
    fn from(s: &str) -> Self {
        Val::Str(s.to_string())
    }
}
impl From<String> for Val {
    fn from(s: String) -> Self {
        Val::Str(s)
    }
}
impl From<&String> for Val {
    fn from(s: &String) -> Self {
        Val::Str(s.clone())
    }
}
impl From<usize> for Val {
    fn from(n: usize) -> Self {
        Val::Num(n as i64)
    }
}
impl From<u64> for Val {
    fn from(n: u64) -> Self {
        Val::Num(n as i64)
    }
}
impl From<f64> for Val {
    fn from(f: f64) -> Self {
        Val::Float(f)
    }
}
impl From<bool> for Val {
    fn from(b: bool) -> Self {
        Val::Bool(b)
    }
}
impl From<Vec<String>> for Val {
    fn from(v: Vec<String>) -> Self {
        Val::List(v)
    }
}

/// A `file:line` cell — the one column shape shared by nearly every finding.
pub fn site(file: &str, line: usize) -> Val {
    Val::Site {
        file: file.to_string(),
        line,
    }
}

/// Build and emit one row: `row!(ctx, "kind" => h.kind, "at" => site(&h.file, h.line))`.
/// Field order is the TSV column order; the keys become JSON object keys.
macro_rules! row {
    ($out:expr, $($k:literal => $v:expr),+ $(,)?) => {
        $out.row(vec![ $(($k, $crate::emit::Val::from($v))),+ ])
    };
}
pub(crate) use row;

/// One emitted finding: ordered `(key, value)` cells plus any `--context`
/// snippet lines gathered for its site.
#[derive(Debug)]
struct Row {
    cells: Vec<(&'static str, Val)>,
    context: Vec<String>,
}

/// A named block of rows. Single-command runs have exactly one (untitled);
/// `audit` opens one per check.
#[derive(Debug)]
struct Section {
    title: Option<String>,
    rows: Vec<Row>,
    /// Raw text lines that aren't tabular (tree renderings, matrix headers).
    summary: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    sections: Vec<Section>,
    notes: Vec<String>,
}

impl State {
    /// The section rows land in, opening an implicit untitled one if needed.
    fn current(&mut self) -> &mut Section {
        if self.sections.is_empty() {
            self.sections.push(Section {
                title: None,
                rows: Vec::new(),
                summary: None,
            });
        }
        self.sections.last_mut().expect("just ensured non-empty")
    }
}

/// The output sink. Shared by `&` (it lives in [`crate::context::AnalysisCtx`]),
/// so the buffer is behind a `RefCell`; every borrow is short and non-reentrant.
pub struct Out {
    pub format: Format,
    /// `--summary`: suppress per-row output, keep the summary line.
    pub summary_only: bool,
    /// `--all-stdout`: route summary/note lines to stdout instead of stderr,
    /// so one redirect captures the whole run.
    pub all_stdout: bool,
    /// `--context N`: source lines to gather around each row's site.
    /// A `Cell` because `audit` raises it for the sections whose output is
    /// small enough that the snippet saves a follow-up file read outright.
    context_lines: std::cell::Cell<Option<usize>>,
    /// While set, [`Out::summary`] writes to stdout inside the current section
    /// instead of to stderr. `audit` turns this on for its battery so each
    /// check's `(N finding(s); …)` line sits with the rows it counts, rather
    /// than being separated onto another stream and re-associated by hand.
    summary_inline: std::cell::Cell<bool>,
    /// Swallow every kind of output. `waivers` re-runs the check battery purely
    /// to populate per-waiver hit counts; the battery's own rows would drown
    /// the listing it is gathering data for.
    silent: bool,
    state: RefCell<State>,
}

impl Out {
    pub fn new(format: Format, summary_only: bool, all_stdout: bool, context_lines: Option<usize>) -> Self {
        Out {
            format,
            summary_only,
            all_stdout,
            context_lines: std::cell::Cell::new(context_lines),
            summary_inline: std::cell::Cell::new(false),
            silent: false,
            state: RefCell::new(State::default()),
        }
    }

    /// An `Out` that emits nothing at all — see [`Out::silent`].
    pub fn silent() -> Self {
        Out {
            format: Format::Tsv,
            summary_only: true,
            all_stdout: false,
            context_lines: std::cell::Cell::new(None),
            summary_inline: std::cell::Cell::new(false),
            silent: true,
            state: RefCell::new(State::default()),
        }
    }

    /// Route summary lines into the section body (stdout) rather than stderr.
    /// Returns the previous setting so a caller can restore it.
    pub fn set_summary_inline(&self, on: bool) -> bool {
        self.summary_inline.replace(on)
    }

    /// Source-context width for the rows emitted next. Returns the previous
    /// value so a caller can restore it.
    pub fn set_context_lines(&self, n: Option<usize>) -> Option<usize> {
        self.context_lines.replace(n)
    }

    pub fn context_lines(&self) -> Option<usize> {
        self.context_lines.get()
    }

    fn json(&self) -> bool {
        self.format == Format::Json
    }

    /// Open a named section. In TSV this prints the `## title` header — and it
    /// must be called *before* the section's rows, which is exactly the bug
    /// `audit` had when it passed an eagerly-evaluated count as an argument.
    pub fn section(&self, title: &str) {
        if self.silent {
            return;
        }
        if self.json() {
            self.state.borrow_mut().sections.push(Section {
                title: Some(title.to_string()),
                rows: Vec::new(),
                summary: None,
            });
            return;
        }
        if !self.summary_only {
            println!("## {}", title);
        }
    }

    /// Blank separator between TSV sections (no-op in JSON).
    pub fn section_end(&self) {
        if self.silent {
            return;
        }
        if !self.json() && !self.summary_only {
            println!();
        }
    }

    /// Emit one finding. In TSV the cells are tab-joined in order; in JSON they
    /// become an object (a `Site` cell expands to `file` + `line`).
    pub fn row(&self, cells: Vec<(&'static str, Val)>) {
        if self.summary_only || self.silent {
            return;
        }
        let context = self.context_for(&cells);
        if self.json() {
            self.state
                .borrow_mut()
                .current()
                .rows
                .push(Row { cells, context });
            return;
        }
        let line: Vec<String> = cells.iter().map(|(_, v)| v.tsv()).collect();
        println!("{}", line.join("\t"));
        for l in context {
            println!("{}", l);
        }
    }

    /// Emit a non-tabular line (tree renderings, cohort matrices, playbook
    /// text). JSON keeps it as a `{"text": …}` row so nothing is silently lost.
    pub fn line(&self, text: &str) {
        if self.summary_only || self.silent {
            return;
        }
        if self.json() {
            self.state.borrow_mut().current().rows.push(Row {
                cells: vec![("text", Val::Str(text.to_string()))],
                context: Vec::new(),
            });
            return;
        }
        println!("{}", text);
    }

    /// An advisory line attached to the row just emitted — currently the
    /// `--suggest-waivers` comment. Rides the same channel as `--context`
    /// snippets so JSON keeps it with its row instead of stranding it.
    pub fn hint(&self, text: &str) {
        if self.summary_only || self.silent {
            return;
        }
        if self.json() {
            let mut st = self.state.borrow_mut();
            if let Some(r) = st.current().rows.last_mut() {
                r.context.push(text.to_string());
            }
            return;
        }
        println!("{}", text);
    }

    /// The trailing `(N finding(s); …)` line. Goes to stderr by default so
    /// stdout stays pipe-clean; `--all-stdout` moves it, and `audit` also
    /// echoes each section's line into the section body.
    pub fn summary(&self, text: &str) {
        if self.silent {
            return;
        }
        if self.json() {
            let mut st = self.state.borrow_mut();
            st.current().summary = Some(text.to_string());
            return;
        }
        if self.summary_inline.get() {
            if !self.summary_only {
                println!("{}", text);
            }
            return;
        }
        if self.all_stdout {
            println!("{}", text);
        } else {
            eprintln!("{}", text);
        }
    }

    /// A warning / note not tied to a row (unknown target, macro blind spots,
    /// "showing 20 of 87"). Follows `summary_inline`: inside an `audit`
    /// section a truncation note is only useful next to the rows it qualifies.
    pub fn note(&self, text: &str) {
        if self.silent {
            return;
        }
        if self.json() {
            self.state.borrow_mut().notes.push(text.to_string());
            return;
        }
        if self.summary_inline.get() {
            if !self.summary_only {
                println!("{}", text);
            }
            return;
        }
        if self.all_stdout {
            println!("{}", text);
        } else {
            eprintln!("{}", text);
        }
    }

    /// `--context N` lines around a row's site, `>`-marking the site line.
    /// Returns empty when the flag is off or the row has no site.
    fn context_for(&self, cells: &[(&'static str, Val)]) -> Vec<String> {
        let Some(n) = self.context_lines.get() else {
            return Vec::new();
        };
        let Some((file, line)) = cells.iter().find_map(|(_, v)| match v {
            Val::Site { file, line } => Some((file, *line)),
            _ => None,
        }) else {
            return Vec::new();
        };
        snippet(file, line, n)
    }

    /// Context lines for a site printed outside `row()` (grouped listings that
    /// render their own indented site lines).
    pub fn context_at(&self, file: &str, line: usize) {
        let Some(n) = self.context_lines.get() else {
            return;
        };
        if self.summary_only || self.silent {
            return;
        }
        let lines = snippet(file, line, n);
        if self.json() {
            let mut st = self.state.borrow_mut();
            if let Some(r) = st.current().rows.last_mut() {
                r.context = lines;
            }
            return;
        }
        for l in lines {
            println!("{}", l);
        }
    }

    /// In JSON mode, serialize everything buffered so far as one document.
    /// No-op for TSV, which has already streamed.
    pub fn finish(&self, command: &str) {
        if !self.json() || self.silent {
            return;
        }
        let st = self.state.borrow();
        let mut s = String::new();
        s.push_str("{\n  \"command\": ");
        push_str(&mut s, command);
        s.push_str(",\n  \"sections\": ");
        if st.sections.is_empty() {
            s.push_str("[]");
        }
        if !st.sections.is_empty() {
            s.push('[');
        }
        for (i, sec) in st.sections.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str("\n    {");
            if let Some(t) = &sec.title {
                s.push_str("\n      \"title\": ");
                push_str(&mut s, t);
                s.push(',');
            }
            s.push_str("\n      \"rows\": ");
            if sec.rows.is_empty() {
                // `[]`, not `[\n]` — an empty result is the common case and
                // consumers grep for it.
                s.push_str("[]");
            } else {
                s.push('[');
                for (j, r) in sec.rows.iter().enumerate() {
                    if j > 0 {
                        s.push(',');
                    }
                    s.push_str("\n        ");
                    push_row(&mut s, r);
                }
                s.push_str("\n      ]");
            }
            if let Some(sum) = &sec.summary {
                s.push_str(",\n      \"summary\": ");
                push_str(&mut s, sum);
            }
            s.push_str("\n    }");
        }
        if !st.sections.is_empty() {
            s.push_str("\n  ]");
        }
        s.push_str(",\n  \"notes\": ");
        if st.notes.is_empty() {
            s.push_str("[]");
        } else {
            s.push('[');
            for (i, n) in st.notes.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str("\n    ");
                push_str(&mut s, n);
            }
            s.push_str("\n  ]");
        }
        s.push_str("\n}");
        println!("{}", s);
    }
}

/// ±`n` source lines around `line`, `>`-marking the site. Silently yields
/// nothing when the file can't be read — a missing snippet must never abort a
/// scan that already found the row.
fn snippet(file: &str, line: usize, n: usize) -> Vec<String> {
    let Ok(src) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let lines: Vec<&str> = src.lines().collect();
    let start = line.saturating_sub(n + 1);
    let end = (line + n).min(lines.len());
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let ln = start + i + 1;
            let marker = if ln == line { '>' } else { ' ' };
            format!("  {}{:>4}| {}", marker, ln, l)
        })
        .collect()
}

fn push_row(s: &mut String, r: &Row) {
    s.push('{');
    let mut first = true;
    let sep = |s: &mut String, first: &mut bool| {
        if *first {
            *first = false;
        } else {
            s.push_str(", ");
        }
    };
    for (k, v) in &r.cells {
        match v {
            Val::Site { file, line } => {
                sep(s, &mut first);
                s.push_str("\"file\": ");
                push_str(s, file);
                s.push_str(", \"line\": ");
                s.push_str(&line.to_string());
            }
            other => {
                sep(s, &mut first);
                push_str(s, k);
                s.push_str(": ");
                push_val(s, other);
            }
        }
    }
    if !r.context.is_empty() {
        sep(s, &mut first);
        s.push_str("\"context\": [");
        for (i, l) in r.context.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            push_str(s, l);
        }
        s.push(']');
    }
    s.push('}');
}

fn push_val(s: &mut String, v: &Val) {
    match v {
        Val::Str(x) => push_str(s, x),
        Val::Num(n) => s.push_str(&n.to_string()),
        // Finite check: JSON has no NaN/Infinity literal, and a gap score is
        // computed from a division that could in principle see a zero total.
        Val::Float(f) if f.is_finite() => s.push_str(&format!("{:.4}", f)),
        Val::Float(_) => s.push_str("null"),
        Val::Bool(b) => s.push_str(if *b { "true" } else { "false" }),
        Val::List(items) => {
            s.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                push_str(s, it);
            }
            s.push(']');
        }
        Val::Site { file, line } => {
            // Only reached for a Site nested where a scalar was expected.
            push_str(s, &format!("{}:{}", file, line));
        }
    }
}

/// Write `v` as a JSON string literal, escaping per RFC 8259.
fn push_str(s: &mut String, v: &str) {
    s.push('"');
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04x}", c as u32)),
            c => s.push(c),
        }
    }
    s.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_string_escaping_survives_quotes_tabs_and_controls() {
        let mut s = String::new();
        push_str(&mut s, "a\"b\\c\td\ne\u{1}");
        let mut expect = String::from("\"a");
        expect.push_str("\\\"b");   // escaped quote
        expect.push_str("\\\\c");  // escaped backslash
        expect.push_str("\\t");
        expect.push_str("d\\ne");
        expect.push_str("\\u0001\"");
        assert_eq!(s, expect);
    }

    #[test]
    fn site_cells_render_as_file_colon_line_in_tsv() {
        assert_eq!(site("src/a.rs", 12).tsv(), "src/a.rs:12");
    }

    #[test]
    fn non_finite_floats_serialize_as_null_not_invalid_json() {
        let mut s = String::new();
        push_val(&mut s, &Val::Float(f64::NAN));
        assert_eq!(s, "null");
    }
}
