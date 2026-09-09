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

use std::cell::{Cell, RefCell};

/// Write one line to stderr.
///
/// Deliberately *not* flushing stdout first. It looks like it should have to:
/// under the `2>&1 | head -N` agents write, a note that arrives before the row
/// it qualifies reads like a buffering race, and one was observed
/// (`show cmd::isolate 2>&1 | head -160` printed its "also named `isolate`"
/// note above the header row). It is not one — Rust backs `Stdout` with a
/// `LineWriter` whether or not it is a terminal, unlike C's stdio, so the two
/// streams already interleave in program order through a pipe. A note landing
/// early means it was *emitted* early, and the fix belongs at the emit site.
fn to_stderr(text: &str) {
    eprintln!("{}", text);
}

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
    /// A site that knows where its item *ends*: `file:start-end`. The `--spans`
    /// upgrade of [`Val::Site`], and the reason it is a separate variant rather
    /// than an `Option<usize>` on `Site` is that the two render differently and
    /// every consumer of `Site` already parses `file:line`.
    Span {
        file: String,
        start: usize,
        end: usize,
    },
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
            Val::Span { file, start, end } => format!("{}:{}-{}", file, start, end),
        }
    }

    /// The `(file, line)` this cell points at, for the machinery that keys off
    /// a row's location — `--context` snippets and fingerprinting. A `Span`
    /// answers with its start, so turning on `--spans` cannot silently switch
    /// either of them off.
    pub fn as_site(&self) -> Option<(&str, usize)> {
        match self {
            Val::Site { file, line } => Some((file.as_str(), *line)),
            Val::Span { file, start, .. } => Some((file.as_str(), *start)),
            _ => None,
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
        // Saturate rather than wrap. Every `u64` this tool emits is a count, so
        // the branch is unreachable in practice — but `as` would turn an
        // impossible count into a *negative* one, and a nonsense row that reads
        // as plausible data is worse than one that reads as a clamp.
        Val::Num(i64::try_from(n).unwrap_or(i64::MAX))
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

/// A `file:start-end` cell: the same column, upgraded to say where the item
/// ends. Emitted in place of [`site`] under `--spans` so a reader can fetch
/// exactly the body a row names instead of guessing a line budget.
pub fn span_site(file: &str, start: usize, end: usize) -> Val {
    Val::Span {
        file: file.to_string(),
        start,
        end,
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
    /// The check that produced these rows, recorded when the first one lands.
    ///
    /// The title is prose (`"[medium] metrics — fns with cyclo >= 15 (explain:
    /// god-function)"`) and a consumer had to regex it to find out which check
    /// it was reading. That is how a `metrics` row — a whole 1200-line function
    /// — ends up scored as if it pointed at a line.
    check: Option<String>,
}

/// What a row's location *means*, so a consumer can tell a pointer from an
/// extent without knowing the battery by heart.
///
/// Emitted per section alongside [`Section::check`]. The distinction is the one
/// that made a proximity-scored evaluation of this tool report matches it had
/// not made: an `item` row names a whole function, so every defect inside that
/// function lands "on" it.
pub fn kind_of_check(check: &str) -> &'static str {
    match check {
        // Two locations, and the finding is the disagreement between them.
        "divergence" | "divergence-handling" | "conversion-pairs" | "config-drift"
        | "builder-drift" | "clones" | "near-clones" | "arith-drift" | "co-call" => "pair",
        // A whole item: the row's line is where it starts, not where a defect is.
        // `concepts` and `gate` name declarations, so their line is where an
        // item begins rather than where anything is wrong on it.
        "metrics" | "dead-code" | "pass-through" | "inventory" | "impls" | "tests" | "outline"
        | "show" | "contract-drift" | "concepts" | "gate" | "vocabulary" | "doc-drift"
        | "validation-drift" => "item",
        // Everything else points at the line the reader should open.
        _ => "site",
    }
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
                check: None,
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
    /// `--all-stdout`: route the summary line to stdout too, so one redirect
    /// captures the whole run. Notes already live there.
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
    /// `--fingerprints`: add the `fp` column to TSV. Off by default because
    /// adding a column unconditionally breaks every caller's `awk` and every
    /// column-count assertion. JSON always carries it.
    pub show_fingerprints: bool,
    /// `--top N`: how many more rows this section may *display*.
    ///
    /// Lives here rather than in each check because every row in the tool goes
    /// through [`Out::row`], and 23 per-command copies of the same flag had
    /// already drifted into three behaviours (uncapped, capped-at-20, and
    /// absent on the highest-volume check of all). Reset by [`Out::section`],
    /// so `audit --top N` means N rows per section and a single command means
    /// N rows for the run.
    ///
    /// Applied *after* fingerprint recording: a cap bounds what is listed, not
    /// what was found, so `--since` baselines and summary counts are unaffected.
    row_budget: Cell<Option<usize>>,
    /// Score at or above which a row is exempt from [`Self::row_budget`].
    ///
    /// `audit --findings-only` promises a *complete* digest of what gates, and
    /// a cap that can hide a gating row breaks that promise silently. Rows
    /// arrive score-sorted, so in practice this only fires on a section whose
    /// gating tier is longer than the section's default cap — but "in practice"
    /// is what a truncation note is for, and this is what makes the guarantee
    /// hold. `None` caps every row alike.
    row_budget_floor: Cell<Option<f64>>,
    /// Rows kept past the budget by [`Self::row_budget_floor`], so the cap note
    /// reports what it actually did.
    kept_over_budget: Cell<usize>,
    /// Rows the budget suppressed in the current section, so the cap can
    /// announce itself. A silent truncation reads as "that is all there is".
    dropped: Cell<usize>,
    /// Rows the budget let through, so the note can say "showing N of M"
    /// rather than making the reader add two numbers together.
    emitted: Cell<usize>,
    /// Whether the most recent [`Out::row`] actually reached the output, so
    /// [`Out::hint`] can decline to speak about a row nobody saw.
    ///
    /// A hint is a remark *about the row above it* and has no location of its
    /// own, so one printed after a dropped row belongs to nothing. Without this
    /// flag `panics --top 3 --suggest-waivers` printed three rows and thirty
    /// waiver comments, and `stringly --top 2` printed two rows and two
    /// hundred; in JSON they were worse than noise, because `hint` appends to
    /// `rows.last_mut()` and every dropped row's suggestion piled onto the last
    /// surviving one — a `casts` row of class `other` carrying waiver keys for
    /// `unknown` and `usize-cross`, which waive nothing if pasted.
    last_row_emitted: Cell<bool>,
    /// Note texts already emitted this run, so a note is said once.
    ///
    /// `show a::f b::f` fired the "N other items are also named `f`" note once
    /// per argument — the same sentence, twice, above one listing. Notes are
    /// run-level commentary; a repeat carries no information the first did not.
    said: RefCell<std::collections::HashSet<String>>,
    /// Which check is producing rows right now. `audit` sets it per section; a
    /// single-command run sets it once. Part of every fingerprint, so two
    /// checks reporting the same line stay distinguishable.
    current_check: RefCell<String>,
    /// The standalone command that reproduces the current section, when the
    /// check's name alone does not spell it.
    ///
    /// `cap_note` used to build "for the rest, run …" out of `current_check`,
    /// which is a *fingerprint identity*, not a command line. Two ways that
    /// lied: `audit`'s params ranking is registered as `metrics-params`, which
    /// is not a subcommand at all, and every section that runs its check with
    /// a non-default threshold (`divergence` at 0.45, `metrics` at
    /// `--sort cyclo`) named a command that answers a different question. A
    /// hint a reader pastes has to be the command, so the section states it.
    current_rerun: RefCell<Option<String>>,
    /// Every row emitted this run, for `--since` / `--baseline` comparison.
    /// `None` unless someone asked.
    recorded: RefCell<Option<Vec<Finding>>>,
    /// `file -> lines`, so fingerprinting N rows in a file reads it once.
    line_cache: RefCell<std::collections::HashMap<String, Vec<String>>>,
    /// A `## title` opened but not yet printed. See [`Out::section`].
    pending_section: RefCell<Option<String>>,
    /// While set, `summary` stores rather than prints — `audit --findings-only`
    /// cannot know a section is empty until after the check that fills it has
    /// already emitted its own `(0 …)` line.
    hold_summary: Cell<bool>,
    held_summary: RefCell<Option<String>>,
    state: RefCell<State>,
    /// While `Some`, every TSV stdout line lands here instead of being
    /// printed, so `audit` can put its `## gating` digest *first* — ahead of
    /// rows that were emitted before the last gating row was known. Taken back
    /// with [`Out::take_buffered`]. `None` streams, which is every other run.
    buffer: RefCell<Option<Vec<String>>>,
    /// `audit` only: prefix every TSV row with a gate column (`!` when the row
    /// holds the exit code open, empty otherwise) and carry `"gating"` in JSON.
    /// A reader grepping for the rows that matter had no handle on them: the
    /// gating tier was a threshold in a section header, and one session ran
    /// the battery eight times to find the single row that gated.
    mark_gating: Cell<bool>,
    /// Every gating row this run emitted, for the `## gating` digest and for
    /// the summary line that names which checks hold the gate.
    gating_rows: RefCell<Vec<GatingRow>>,
    /// Print `hint` lines beside their row. `audit` turns this off unless
    /// `--suggest-waivers` was named, and prints the hints it collected under
    /// the gating rows alone — the rows a reader is about to decide about.
    hints_inline: Cell<bool>,
    /// Whether the most recent rendered row gated, so a following `hint`
    /// knows whether it belongs in the digest.
    last_row_gating: Cell<bool>,
    /// Under `--changed-since`, says whether a row's line sits in a changed
    /// hunk (`changed`) or merely in a changed file (`pre-existing`). Takes the
    /// check name because an `item` row's line is where the item *starts*, so
    /// its verdict is about the whole extent. Set by `audit`.
    change_of: RefCell<Option<ChangeOf>>,
}

/// `(check, file, line) -> "changed" | "pre-existing" | ""`. See [`Out::change_of`].
pub type ChangeOf = Box<dyn Fn(&str, &str, usize) -> &'static str>;

/// One row that gates, kept for the digest that opens `audit`'s report.
#[derive(Debug, Clone)]
pub struct GatingRow {
    pub check: String,
    /// The TSV cells, tab-joined, without the gate column.
    pub tsv: String,
    pub file: Option<String>,
    pub line: usize,
    /// `--suggest-waivers` lines attached to this row.
    pub hints: Vec<String>,
    /// `changed` / `pre-existing` under `--changed-since`, else empty.
    pub change: &'static str,
}

/// One emitted finding, reduced to what a cross-run comparison needs.
#[derive(Clone, Debug)]
pub struct Finding {
    pub check: String,
    pub fp: String,
    pub file: String,
    pub line: usize,
    /// Human-readable identity, for the diff listing.
    pub label: String,
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
            show_fingerprints: false,
            row_budget: Cell::new(None),
            row_budget_floor: Cell::new(None),
            kept_over_budget: Cell::new(0),
            dropped: Cell::new(0),
            emitted: Cell::new(0),
            last_row_emitted: Cell::new(false),
            said: RefCell::new(std::collections::HashSet::new()),
            current_check: RefCell::new(String::new()),
            current_rerun: RefCell::new(None),
            recorded: RefCell::new(None),
            line_cache: RefCell::new(std::collections::HashMap::new()),
            pending_section: RefCell::new(None),
            hold_summary: Cell::new(false),
            held_summary: RefCell::new(None),
            state: RefCell::new(State::default()),
            buffer: RefCell::new(None),
            mark_gating: Cell::new(false),
            gating_rows: RefCell::new(Vec::new()),
            hints_inline: Cell::new(true),
            last_row_gating: Cell::new(false),
            change_of: RefCell::new(None),
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
            show_fingerprints: false,
            row_budget: Cell::new(None),
            row_budget_floor: Cell::new(None),
            kept_over_budget: Cell::new(0),
            dropped: Cell::new(0),
            emitted: Cell::new(0),
            last_row_emitted: Cell::new(false),
            said: RefCell::new(std::collections::HashSet::new()),
            current_check: RefCell::new(String::new()),
            current_rerun: RefCell::new(None),
            recorded: RefCell::new(None),
            line_cache: RefCell::new(std::collections::HashMap::new()),
            pending_section: RefCell::new(None),
            hold_summary: Cell::new(false),
            held_summary: RefCell::new(None),
            state: RefCell::new(State::default()),
            buffer: RefCell::new(None),
            mark_gating: Cell::new(false),
            gating_rows: RefCell::new(Vec::new()),
            hints_inline: Cell::new(true),
            last_row_gating: Cell::new(false),
            change_of: RefCell::new(None),
        }
    }

    /// Name the check producing subsequent rows. Returns the previous name.
    pub fn set_check(&self, name: &str) -> String {
        self.current_check.replace(name.to_string())
    }

    /// State the command that reproduces the rows about to be emitted — the
    /// arguments included, not just the subcommand. Returns the previous one.
    ///
    /// `None` means "`unruster <check>` is exact", which is true of a
    /// single-command run and of every `audit` section that runs its check on
    /// the defaults. See [`Out::current_rerun`].
    pub fn set_rerun(&self, cmd: Option<String>) -> Option<String> {
        self.current_rerun.replace(cmd)
    }

    /// Start recording every row's fingerprint for a cross-run comparison.
    pub fn start_recording(&self) {
        *self.recorded.borrow_mut() = Some(Vec::new());
    }

    /// Take what was recorded, leaving recording off.
    pub fn take_recording(&self) -> Vec<Finding> {
        self.recorded.borrow_mut().take().unwrap_or_default()
    }

    /// One source line, cached per file. Fingerprinting touches every row, and
    /// re-reading a 2000-line file per row would dominate the run.
    fn source_line(&self, file: &str, line: usize) -> Option<String> {
        let mut cache = self.line_cache.borrow_mut();
        let lines = cache.entry(file.to_string()).or_insert_with(|| {
            std::fs::read_to_string(file)
                .map(|s| s.lines().map(str::to_string).collect())
                .unwrap_or_default()
        });
        lines.get(line.checked_sub(1)?).cloned()
    }

    /// Fingerprint plus the bits a diff listing needs.
    fn finding_of(&self, cells: &[(&'static str, Val)]) -> Finding {
        let check = self.current_check.borrow().clone();
        let site = cells
            .iter()
            .find_map(|(_, v)| v.as_site().map(|(f, l)| (f.to_string(), l)));
        let text = site
            .as_ref()
            .and_then(|(f, l)| self.source_line(f, *l));
        let fp = crate::fingerprint::of(&check, cells, text.as_deref());
        let label = cells
            .iter()
            .filter_map(|(_, v)| match v {
                Val::Str(s) => Some(crate::fingerprint::normalize(s)),
                Val::List(i) => Some(i.join(",")),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" | ");
        let (file, line) = site.unwrap_or_default();
        Finding {
            check,
            fp,
            file,
            line,
            label,
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

    /// One stdout line: printed, or held while [`Out::start_buffering`] is on.
    fn put(&self, text: &str) {
        if let Some(buf) = self.buffer.borrow_mut().as_mut() {
            buf.push(text.to_string());
            return;
        }
        println!("{}", text);
    }

    /// Hold every stdout line from here on. See [`Out::buffer`].
    pub fn start_buffering(&self) {
        *self.buffer.borrow_mut() = Some(Vec::new());
    }

    /// Stop holding and hand back what was held, in order.
    pub fn take_buffered(&self) -> Vec<String> {
        self.buffer.borrow_mut().take().unwrap_or_default()
    }

    /// Whether this sink swallows everything (the waiver-probe battery).
    pub fn is_silent(&self) -> bool {
        self.silent
    }

    /// Print a line now, bypassing the buffer — for the digest that has to
    /// precede everything the buffer holds.
    pub fn print_now(&self, text: &str) {
        println!("{}", text);
    }

    /// Turn the gate column on (see [`Out::mark_gating`]).
    pub fn set_mark_gating(&self, on: bool) {
        self.mark_gating.set(on);
    }

    /// Whether `hint` prints beside its row. Returns the previous setting.
    pub fn set_hints_inline(&self, on: bool) -> bool {
        self.hints_inline.replace(on)
    }

    /// Install the `--changed-since` classifier. See [`Out::change_of`].
    pub fn set_change_of(&self, f: Option<ChangeOf>) {
        *self.change_of.borrow_mut() = f;
    }

    /// The gating rows emitted so far, oldest first.
    pub fn gating_rows(&self) -> Vec<GatingRow> {
        self.gating_rows.borrow().clone()
    }

    /// Does this row hold the exit code open, under the section's gate?
    ///
    /// `None` floor: an advisory section, or a single-command run — nothing
    /// gates. A `-inf` floor: every row gates, `score` cell or not. Any other
    /// floor: the row's `score` has to clear it. The old test required a
    /// `score` cell even under `-inf`, so a `divergence` row past the section's
    /// cap — a check with no score column — was dropped while the note above
    /// it promised the cap never hides a gating row.
    fn gates(&self, cells: &[(&'static str, Val)]) -> bool {
        let Some(floor) = self.row_budget_floor.get() else {
            return false;
        };
        if floor == f64::NEG_INFINITY {
            return true;
        }
        cells
            .iter()
            .find(|(k, _)| *k == "score")
            .and_then(|(_, v)| v.tsv().parse::<f64>().ok())
            .is_some_and(|s| s >= floor)
    }

    /// Open a named section. In TSV this prints the `## title` header — and it
    /// must be called *before* the section's rows, which is exactly the bug
    /// `audit` had when it passed an eagerly-evaluated count as an argument.
    /// Set the display cap for the rows that follow. `None` lifts it.
    pub fn set_row_budget(&self, n: Option<usize>) {
        self.row_budget.set(n);
        self.row_budget_floor.set(None);
        self.kept_over_budget.set(0);
        self.dropped.set(0);
        self.emitted.set(0);
    }

    /// As [`Self::set_row_budget`], but exempting rows whose `score` cell is at
    /// or above `floor` — the tier `audit` gates on. See [`Self::row_budget_floor`].
    pub fn set_row_budget_keeping(&self, n: Option<usize>, floor: Option<f64>) {
        self.set_row_budget(n);
        self.row_budget_floor.set(floor);
    }

    /// The `--top` note for the rows since the budget was last set, or `None`
    /// when nothing was dropped. Both numbers, so the reader does no arithmetic.
    pub fn cap_note(&self) -> Option<String> {
        let dropped = self.dropped.get();
        if dropped == 0 {
            return None;
        }
        let kept = self.kept_over_budget.get();
        // The section's own command when it stated one, else the check name:
        // see [`Out::current_rerun`] for why the name alone is not a command.
        let check = self
            .current_rerun
            .borrow()
            .clone()
            .unwrap_or_else(|| self.current_check.borrow().clone());
        Some(format!(
            "(note: showing {} of {} row(s){} — {} for the rest)",
            self.emitted.get(),
            self.emitted.get() + dropped,
            if kept > 0 {
                format!(
                    ", including all {} above the gating tier — the cap never hides one",
                    kept
                )
            } else {
                String::new()
            },
            if check.is_empty() {
                "raise or drop --top".to_string()
            } else {
                // Name the command, not the flag: a reader who wants the tail of
                // one section wants that section, and `--top 0` on the battery
                // gives them twenty others as well.
                format!("`unruster {} --top 0`", check)
            }
        ))
    }

    /// Open a section. In TSV the `## title` header is *deferred* until the
    /// section emits something, so a caller that decides after the fact to drop
    /// an empty section can do so without having already printed its heading.
    /// Nothing about the ordering changes: every emitter flushes the pending
    /// header before its own line, so the header still precedes its rows.
    pub fn section(&self, title: &str) {
        if self.silent {
            return;
        }
        if self.json() {
            // Stamped at creation, not at the first row: a section that found
            // nothing still has to say which check found nothing, or a
            // consumer cannot tell an empty `metrics` from an empty
            // `dead-code`. Callers set the check before opening the section;
            // `row` fills it in for the ones that cannot.
            let check = self.current_check.borrow().clone();
            self.state.borrow_mut().sections.push(Section {
                title: Some(title.to_string()),
                rows: Vec::new(),
                summary: None,
                check: (!check.is_empty()).then_some(check),
            });
            return;
        }
        if !self.summary_only {
            *self.pending_section.borrow_mut() = Some(title.to_string());
        }
    }

    /// Print the deferred `## title`, if one is waiting.
    fn flush_section(&self) {
        if let Some(t) = self.pending_section.borrow_mut().take() {
            self.put(&format!("## {}", t));
        }
    }

    /// Drop the deferred header. Returns false if it had already been printed,
    /// in which case the caller is mid-section and must finish it normally.
    pub fn drop_pending_section(&self) -> bool {
        self.pending_section.borrow_mut().take().is_some()
    }

    /// While set, [`summary`](Self::summary) stores its line instead of
    /// printing it, so a caller can decide whether the section it belongs to is
    /// worth showing at all. Returns the previous setting.
    pub fn hold_summary(&self, on: bool) -> bool {
        self.hold_summary.replace(on)
    }

    /// Take the held summary line, if any.
    pub fn take_held_summary(&self) -> Option<String> {
        self.held_summary.borrow_mut().take()
    }

    /// Blank separator between TSV sections (no-op in JSON).
    pub fn section_end(&self) {
        if self.silent {
            return;
        }
        if !self.json() && !self.summary_only {
            self.flush_section();
            self.put("");
        }
    }

    /// Emit one finding. In TSV the cells are tab-joined in order; in JSON they
    /// become an object (a `Site` cell expands to `file` + `line`).
    pub fn row(&self, cells: Vec<(&'static str, Val)>) {
        // Recording happens before every suppression check, including `silent`.
        // The baseline half of `--since` runs the battery through a silent sink
        // *specifically* to collect fingerprints; bailing out first made it
        // record nothing and report the entire codebase as new.
        let recording = self.recorded.borrow().is_some();
        let want_fp = recording || (!self.silent && (self.json() || self.show_fingerprints));
        let finding = want_fp.then(|| self.finding_of(&cells));
        if let (Some(f), Some(list)) = (&finding, self.recorded.borrow_mut().as_mut()) {
            list.push(f.clone());
        }
        if self.silent {
            self.last_row_emitted.set(false);
            return;
        }
        // Decided before the cap and before `--summary`'s early return: a
        // gating row is recorded whether or not it is rendered, so the digest
        // and the summary line name every one of them.
        let gating = self.gates(&cells);
        let change = self.change_of.borrow().as_ref().map(|f| {
            let check = self.current_check.borrow().clone();
            cells
                .iter()
                .find_map(|(_, v)| v.as_site())
                .map_or("", |(file, line)| f(&check, file, line))
        });
        if gating {
            let site = cells.iter().find_map(|(_, v)| v.as_site());
            self.gating_rows.borrow_mut().push(GatingRow {
                check: self.current_check.borrow().clone(),
                tsv: cells.iter().map(|(_, v)| v.tsv()).collect::<Vec<_>>().join("\t"),
                file: site.map(|(f, _)| f.to_string()),
                line: site.map_or(0, |(_, l)| l),
                hints: Vec::new(),
                change: change.unwrap_or(""),
            });
        }
        if self.summary_only {
            self.last_row_emitted.set(false);
            return;
        }
        // The cap. After recording (above) so fingerprints and `--since`
        // baselines still see every finding, and before rendering so the cap
        // only bounds the listing.
        match self.row_budget.get() {
            Some(0) if gating => {
                self.kept_over_budget.set(self.kept_over_budget.get() + 1);
            }
            Some(0) => {
                self.dropped.set(self.dropped.get() + 1);
                self.last_row_emitted.set(false);
                return;
            }
            Some(n) => self.row_budget.set(Some(n - 1)),
            None => {}
        }
        self.emitted.set(self.emitted.get() + 1);
        self.last_row_emitted.set(true);
        self.last_row_gating.set(gating);
        let context = self.context_for(&cells);
        if self.json() {
            let mut cells = cells;
            if let Some(f) = &finding {
                cells.push(("fp", Val::Str(f.fp.clone())));
            }
            if self.mark_gating.get() {
                cells.push(("gating", Val::Bool(gating)));
            }
            if let Some(c) = change.filter(|c| !c.is_empty()) {
                cells.push(("change", Val::Str(c.to_string())));
            }
            let check = self.current_check.borrow().clone();
            let mut st = self.state.borrow_mut();
            let sec = st.current();
            if sec.check.is_none() && !check.is_empty() {
                sec.check = Some(check);
            }
            sec.rows.push(Row { cells, context });
            return;
        }
        let mut line: Vec<String> = cells.iter().map(|(_, v)| v.tsv()).collect();
        if self.show_fingerprints {
            if let Some(f) = &finding {
                line.push(f.fp.clone());
            }
        }
        // The gate column leads, so `grep '^!'` is the whole filter.
        if self.mark_gating.get() {
            line.insert(0, if gating { "!" } else { "" }.to_string());
        }
        self.flush_section();
        self.put(&line.join("\t"));
        for l in context {
            self.put(&l);
        }
    }

    /// Attach extra fields to the row just emitted, in JSON only.
    ///
    /// TSV's contract is a fixed column count per section, so a field that
    /// exists for some rows and not others cannot go there — but JSON is an
    /// object per row and can carry it without moving anything. Used by
    /// `--suggest-waivers` to put the check and key next to the `file`/`line`
    /// they belong to, which is what makes `<check> --json | jq | waivers
    /// --apply -` a one-liner.
    pub fn tag_last_row(&self, fields: &[(&'static str, String)]) {
        if !self.json() || self.silent || self.summary_only || !self.last_row_emitted.get() {
            return;
        }
        let mut st = self.state.borrow_mut();
        if let Some(r) = st.current().rows.last_mut() {
            for (k, v) in fields {
                r.cells.push((k, Val::Str(v.clone())));
            }
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
        self.flush_section();
        self.put(text);
    }

    /// An advisory line attached to the row just emitted — currently the
    /// `--suggest-waivers` comment. Rides the same channel as `--context`
    /// snippets so JSON keeps it with its row instead of stranding it.
    pub fn hint(&self, text: &str) {
        if self.summary_only || self.silent {
            return;
        }
        // A hint carries no location of its own — it is a remark about the row
        // above it. When `--top` dropped that row there is nothing for it to be
        // about, and in JSON it would attach to whichever row survived last.
        // See [`Out::last_row_emitted`] for what that cost.
        if !self.last_row_emitted.get() {
            return;
        }
        // A hint about a gating row travels with it into the digest, where the
        // reader is deciding what to do about exactly that row.
        if self.last_row_gating.get() {
            if let Some(g) = self.gating_rows.borrow_mut().last_mut() {
                g.hints.push(text.to_string());
            }
        }
        if self.json() {
            let mut st = self.state.borrow_mut();
            if let Some(r) = st.current().rows.last_mut() {
                r.context.push(text.to_string());
            }
            return;
        }
        if self.hints_inline.get() {
            self.put(text);
        }
    }

    /// The trailing `(N finding(s); …)` line. Goes to stderr by default so
    /// stdout stays pipe-clean — it is a count, and a reader who dropped it
    /// still holds a correct answer; `--all-stdout` moves it, and `audit` also
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
        // Held rather than printed: the caller will decide whether the section
        // this line belongs to is worth showing. Deliberately does *not* flush
        // the pending header, since holding it is the whole point.
        if self.hold_summary.get() {
            *self.held_summary.borrow_mut() = Some(text.to_string());
            return;
        }
        if self.summary_inline.get() {
            if !self.summary_only {
                self.flush_section();
                self.put(text);
            }
            return;
        }
        if self.all_stdout {
            self.flush_section();
            self.put(text);
        } else {
            to_stderr(text);
        }
    }

    /// A note that *is* the result, not commentary on it — the near-name list
    /// for a lookup that found nothing, and anything else a caller would be
    /// wrong to discard.
    ///
    /// Always stdout, even under `--summary`. This was the first line to move
    /// off stderr: agents suppress stderr routinely, and one session ran
    /// `show <name> 2>/dev/null | head -30 || <fallback>` four times. Each got
    /// total silence — the suggestion erased by the redirect, the `||` never
    /// firing because a pipeline exits with `head`'s status — and each time the
    /// reader concluded the tool had nothing and went back to `grep`. A failed
    /// lookup's explanation is the answer to the question that was asked.
    /// [`note`](Self::note) has since followed it for the same reason.
    ///
    /// JSON keeps it in `notes` alongside [`Out::note`], so a document consumer
    /// reads both from one place.
    pub fn answer(&self, text: &str) {
        if self.silent {
            return;
        }
        if self.json() {
            self.state.borrow_mut().notes.push(text.to_string());
            return;
        }
        self.put(text);
    }

    /// A note about the **rows themselves** — that they were cut short. Goes to
    /// stdout, beside the rows it qualifies.
    ///
    /// The rule that decides the channel: everything on stderr is commentary a
    /// caller can discard and still hold a correct answer. A truncation is not
    /// that. `2>/dev/null` is what a caller writes to silence the blind-spot
    /// paragraph, and in one real session it was paired with `| head -N` on
    /// five of seven invocations — so the `(showing 3 of 36 row(s))` line, the
    /// only thing saying the answer was partial, was the first casualty, and
    /// three rows read as "that is all there is". A cut this tool performed
    /// has to survive the redirect the caller reaches for. Under `--summary`
    /// there are no rows on stdout to sit beside, so it rejoins the summary on
    /// stderr. Said once: a section that cuts twice reports it once.
    pub fn row_note(&self, text: &str) {
        if self.silent {
            return;
        }
        if !self.said.borrow_mut().insert(text.to_string()) {
            return;
        }
        if self.json() {
            self.state.borrow_mut().notes.push(text.to_string());
            return;
        }
        if self.summary_only {
            to_stderr(text);
        } else {
            self.flush_section();
            self.put(text);
        }
    }

    /// A note not tied to a row: what the answer leaves out, or what to run
    /// next — "`dir` names 3 items, qualify it or `--all`", "2 of these sites
    /// are a local binding", "0 files changed, so this is an empty scope",
    /// the macro blind spots. Follows `summary_inline`: inside an `audit`
    /// section a note is only useful next to the rows it qualifies.
    ///
    /// **Stdout**, with the rows. It went to stderr for a long time so a piped
    /// run stayed clean, and every reader who mattered redirected stderr away:
    /// in one seven-hour session 61 of 76 invocations carried `2>/dev/null`,
    /// so four ambiguous `show`s answered with a bare header list and the
    /// reader fell back to `sed`, a `callers` on a common name showed fourteen
    /// heuristic rows with the "these are locals" note erased, and a `show`
    /// cut at its line budget lost the line saying so. A note is written
    /// because the rows alone mislead; a channel the reader has closed does not
    /// deliver it. Under `--summary` there are no rows on stdout, so it rejoins
    /// the summary on stderr, and JSON keeps it in `notes` either way.
    pub fn note(&self, text: &str) {
        if self.silent {
            return;
        }
        if !self.said.borrow_mut().insert(text.to_string()) {
            return;
        }
        if self.json() {
            self.state.borrow_mut().notes.push(text.to_string());
            return;
        }
        if self.summary_only {
            if !self.summary_inline.get() {
                to_stderr(text);
            }
            return;
        }
        self.flush_section();
        self.put(text);
    }

    /// `--context N` lines around a row's site, `>`-marking the site line.
    /// Returns empty when the flag is off or the row has no site.
    fn context_for(&self, cells: &[(&'static str, Val)]) -> Vec<String> {
        let Some(n) = self.context_lines.get() else {
            return Vec::new();
        };
        let Some((file, line)) = cells.iter().find_map(|(_, v)| v.as_site()) else {
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
            self.put(&l);
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
            // The two fields that make a section machine-readable: which check
            // wrote it, and whether its rows point at a line or span an item.
            // Without them the title — prose, with a severity tag and an
            // `explain:` topic in it — was the only handle a consumer had.
            if let Some(c) = &sec.check {
                s.push_str("\n      \"check\": ");
                push_str(&mut s, c);
                s.push_str(",\n      \"kind\": ");
                push_str(&mut s, kind_of_check(c));
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

/// JSON field prefix for the *second* and later site cells in one row.
///
/// The first site keeps the bare `file` / `line` names every consumer is
/// written against. Subsequent ones are namespaced by their own cell key, which
/// is what the emitter used to throw away: `push_row` hardcoded `file`/`line`
/// for every [`Val::Site`], so a row naming two locations — every row the five
/// comparison checks emit — produced the same key twice. `json.loads` keeps the
/// last, so the *primary* (lean) location was silently replaced by the `vs` one
/// and findings were attributed to the wrong file and line.
///
/// The trailing `_at` of a column name is dropped because those keys read as
/// locations already (`vs_at` → `vs_file`, `vs_line`); anything else keeps its
/// name whole (`vs_richest` → `vs_richest_file`).
fn site_prefix(key: &str) -> &str {
    key.strip_suffix("_at").unwrap_or(key)
}

fn push_row(s: &mut String, r: &Row) {
    s.push('{');
    let mut first = true;
    // Whether the bare `file`/`line` pair has been spent on this row.
    let mut primary_site_emitted = false;
    let sep = |s: &mut String, first: &mut bool| {
        if *first {
            *first = false;
        } else {
            s.push_str(", ");
        }
    };
    // `"file"` for the first site cell, `"vs_file"` for the ones after it.
    let key_of = |primary: bool, cell_key: &str, field: &str| -> String {
        if primary {
            format!("\"{}\"", field)
        } else {
            format!("\"{}_{}\"", site_prefix(cell_key), field)
        }
    };
    for (k, v) in &r.cells {
        match v {
            Val::Site { file, line } => {
                sep(s, &mut first);
                let primary = !primary_site_emitted;
                primary_site_emitted = true;
                s.push_str(&key_of(primary, k, "file"));
                s.push_str(": ");
                push_str(s, file);
                s.push_str(", ");
                s.push_str(&key_of(primary, k, "line"));
                s.push_str(": ");
                s.push_str(&line.to_string());
            }
            // `line` stays the start, so a consumer written against `Site`
            // keeps working and only gains `end_line`.
            Val::Span { file, start, end } => {
                sep(s, &mut first);
                let primary = !primary_site_emitted;
                primary_site_emitted = true;
                s.push_str(&key_of(primary, k, "file"));
                s.push_str(": ");
                push_str(s, file);
                s.push_str(", ");
                s.push_str(&key_of(primary, k, "line"));
                s.push_str(": ");
                s.push_str(&start.to_string());
                s.push_str(", ");
                s.push_str(&key_of(primary, k, "end_line"));
                s.push_str(": ");
                s.push_str(&end.to_string());
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
        // `context_lines`, not `context`: nine checks emit a column *called*
        // `context` (the enclosing item), and under `--context N` — which
        // `audit` turns on by itself for two of its sections — both landed in
        // one object under one key. Worse than the site collision above,
        // because the two values are different types: a consumer that reads
        // the column as a string got an array of source lines instead.
        s.push_str("\"context_lines\": [");
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
        Val::Span { file, start, end } => {
            push_str(s, &format!("{}:{}-{}", file, start, end));
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
