//! `panics` — sites that abort the process instead of reporting a failure.
//!
//! The gap this fills: nothing in the battery looked at `.unwrap()`. A
//! 200-defect evaluation of this tool against a real changelog found 18 fixes
//! whose whole content was "report X instead of panicking" — invalid credential
//! endpoints, non-UTF-8 virtualenv paths, malformed lockfile URLs — and not one
//! check in the tool could see any of them. `error-swallows` tracks Results
//! that were *discarded*; these are Results that were *asserted*.
//!
//! The two are mirror images and share their machinery: the same
//! [`Effect`](crate::error_swallows::Effect) classifier answers "what was this
//! call doing", and the same shape of score — what failed, plus how loudly it
//! fails — decides what a reader sees first.
//!
//! Where they differ is the ranking, and the difference is the point. For a
//! swallow, a `Decode` is the *safest* class: a parse returned `None` and
//! nothing outside the process moved. For a panic it is the most dangerous one,
//! because the input to a parse is by definition data the program did not
//! produce — a CLI argument, a response body, a file on disk — and the crash is
//! reachable by anyone who can supply it. Every one of those 18 fixes was this
//! shape.

use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{line_of, line_of_span, scope_visits, ScopeTracker};
use crate::context::{AnalysisCtx, Counts};
use crate::emit::{row, site};
use crate::error_swallows::{classify_effect, Effect};
use crate::parse::display_path;

#[derive(Debug)]
struct Hit {
    /// `.unwrap` | `.expect` | `panic!` | `unreachable!` | `todo!` |
    /// `unimplemented!`
    kind: &'static str,
    file: String,
    line: usize,
    context: String,
    /// Set when the site is idiomatic rather than defective — a poisoned-lock
    /// unwrap, or an assertion over a literal that cannot vary at runtime.
    /// A flag rather than a drop, so the summary can say how many were filtered
    /// and `--include-idiomatic` can restore them.
    benign: Option<&'static str>,
    /// What the asserted call was doing. `Unknown` for the bare macros, which
    /// have no receiver to classify.
    effect: Effect,
    /// Whether a `Decode` was applied to something the process produced. See
    /// [`Provenance`].
    provenance: Provenance,
}

impl Hit {
    /// How much this site deserves a reader's attention, 0.0–1.0.
    ///
    /// Two terms, added, mirroring `error-swallows`:
    ///
    /// * **What was asserted** ([`effect_weight`]) — a parse of data from
    ///   outside the process is a crash someone else can trigger; an
    ///   unrecognised in-process call is not.
    /// * **How much thought the site records** (the kind) — `.expect("…")`
    ///   names the invariant it is asserting, which is both a smaller defect
    ///   and a review someone already did. A bare `.unwrap()` records nothing.
    fn score(&self) -> f64 {
        let kind = match self.kind {
            // Ships as a crash on a path the author knows is reachable.
            "todo!" | "unimplemented!" => 0.60,
            // No message: the backtrace is the only thing the user gets.
            ".unwrap" => 0.30,
            // A deliberate abort with a reason attached.
            "panic!" => 0.25,
            // The author wrote down what must hold. Still a crash.
            ".expect" => 0.20,
            // A claim about the type system, usually true and usually local.
            "unreachable!" => 0.15,
            _ => 0.20,
        };
        // As in `error-swallows`: a site the benign classifier cleared has
        // already answered the question the effect term asks.
        let effect = if self.benign.is_some() {
            0.0
        } else if self.provenance == Provenance::InProcess {
            // A conversion of the process's own arithmetic is not a decode of
            // anything, whatever the verb is spelled. Scored as `Unknown` — no
            // claim either way — rather than as zero, because the conversion
            // can still be wrong; it just is not reachable from outside.
            effect_weight(Effect::Unknown)
        } else {
            effect_weight(self.effect)
        };
        (effect + kind).min(1.0)
    }

    /// The `effect` cell. A demoted decode says so, so a reader who disagrees
    /// with the provenance rules can see which rows they moved and re-read them
    /// with `--min-score 0`.
    fn effect_cell(&self) -> String {
        if self.provenance == Provenance::InProcess {
            format!("{}(in-process)", self.effect.as_str())
        } else {
            self.effect.as_str().to_string()
        }
    }
}

/// Effect weights for a *panic*, which are not the swallow weights.
///
/// Inverted at the top and bottom, deliberately:
///
/// | class | swallow | panic | why |
/// |:--|--:|--:|:--|
/// | `Decode` | 0.05 | 0.35 | the input came from outside; the crash is reachable |
/// | `Io` | 0.35 | 0.30 | a missing file or a failed request aborts the run |
/// | `Mutation` | 0.60 | 0.25 | the write really did fail, and it is reported |
/// | `Unknown` | 0.20 | 0.15 | no claim either way |
///
/// A dropped `DELETE` is silent data loss; a *panicking* `DELETE` is a loud,
/// diagnosable failure. The asymmetry is the whole reason this is a separate
/// table rather than a reuse of [`Effect::weight`].
fn effect_weight(e: Effect) -> f64 {
    match e {
        Effect::Decode => 0.35,
        Effect::Io => 0.30,
        Effect::Mutation => 0.25,
        Effect::Unknown => 0.15,
    }
}

/// The score at or above which a panic site is a gating audit finding.
///
/// Set so `.unwrap()` on a parse or a read (0.65, 0.60) gates and `.expect` on
/// an unrecognised in-process call (0.35) does not. `.expect` on a decode lands
/// exactly on the line at 0.55: a documented assertion about external input is
/// the weakest thing worth holding an agent loop open for.
pub const GATING_SCORE: f64 = 0.55;

/// Receiver methods whose `.unwrap()` is the documented idiom rather than a
/// defect: a poisoned lock means another thread already panicked, and
/// propagating that is what every `std` example does.
const LOCK_VERBS: &[&str] = &["lock", "read", "write", "borrow", "borrow_mut", "try_lock"];

#[derive(Clone, Copy)]
pub struct PanicOpts {
    /// Keep the idiomatic families (`Mutex::lock().unwrap()`, assertions over
    /// source literals). Off in `audit`, on for the bare command.
    pub include_idiomatic: bool,
    /// Drop rows scoring below this. 0.0 keeps everything.
    pub min_score: f64,
}

impl Default for PanicOpts {
    fn default() -> Self {
        PanicOpts {
            include_idiomatic: true,
            min_score: 0.0,
        }
    }
}

/// Where the value a fallible conversion was applied to came from.
///
/// The `decode` class treats a fallible conversion as external-input decoding
/// regardless of where the value came from, and the implementation could not
/// tell `u32::try_from(vec.len())` from `u32::try_from(parsed_header_field)`.
/// On a 4867-item codebase that produced 95 gating findings, zero fixes and 58
/// hand-written waivers: every one was a conversion of an in-process length, an
/// index, or a clamped slider value.
///
/// Demoted, not dropped — the row still prints and its `effect` cell says
/// `decode(in-process)`, so a reader who disagrees can see what happened.
/// Narrowing this too far would hide the exact crash the check exists to find,
/// which is why every rule below is structural and refuses by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provenance {
    /// The process produced the value: a collection's own length, arithmetic
    /// over one, a value the author already bounded, or a call to a local fn
    /// that cannot fail.
    InProcess,
    /// Not shown to be in-process. The default, and the class this check was
    /// built for.
    Unknown,
}

/// No-argument methods by which a container reports its own extent.
const SIZE_VERBS: &[&str] = &["len", "count", "capacity"];

/// Methods that hand back a value inside a range the caller named, so the
/// conversion that follows cannot fail whatever went in.
const BOUNDING_VERBS: &[&str] = &["clamp", "min", "max"];

/// The expressions a fallible conversion inside `e` was applied to.
///
/// `u32::try_from(x)` and `T::from_str(s)` contribute their arguments;
/// `x.try_into()` and `s.parse()` contribute their receiver. Anything else in
/// the chain is walked through, so `u8::try_from(v.len() - 1)` is asked about
/// `v.len() - 1` rather than about the `try_from`.
fn conversion_inputs<'a>(e: &'a syn::Expr, out: &mut Vec<&'a syn::Expr>) {
    match crate::ast::peel_grouping(e) {
        syn::Expr::Call(c) => {
            let converts = matches!(&*c.func, syn::Expr::Path(p)
                if p.path.segments.last().is_some_and(|s|
                    crate::error_swallows::names_a_decode_verb(&s.ident.to_string())));
            if converts {
                out.extend(c.args.iter());
                return;
            }
            for a in &c.args {
                conversion_inputs(a, out);
            }
        }
        syn::Expr::MethodCall(c) => {
            if crate::error_swallows::names_a_decode_verb(&c.method.to_string()) {
                out.push(&c.receiver);
                return;
            }
            conversion_inputs(&c.receiver, out);
        }
        _ => {}
    }
}

struct PanicVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    hits: Vec<Hit>,
    /// Return types of every fn in the tree, for [`Provenance`]'s local-call
    /// rule.
    sigs: &'a crate::semantic::FnSigIndex,
    /// One frame per lexical block: the names bound in it to a value the
    /// process computed.
    bounded: Vec<std::collections::BTreeSet<String>>,
}

impl PanicVisitor<'_> {
    fn record(
        &mut self,
        kind: &'static str,
        line: usize,
        benign: Option<&'static str>,
        effect: Effect,
        provenance: Provenance,
    ) {
        let context = self.scope.enclosing();
        self.hits.push(Hit {
            kind,
            file: self.file.to_string(),
            line,
            context,
            benign,
            effect,
            provenance,
        });
    }

    /// Is `name` bound, somewhere up the block stack, to an in-process value?
    fn bound_in_process(&self, name: &str) -> bool {
        self.bounded.iter().any(|f| f.contains(name))
    }

    /// Record `pat` as naming an in-process value in the innermost frame.
    fn note_bound(&mut self, pat: &syn::Pat) {
        if let syn::Pat::Ident(p) = pat {
            if let Some(f) = self.bounded.last_mut() {
                f.insert(p.ident.to_string());
            }
        }
    }

    /// Did the process itself produce the value this expression evaluates to?
    ///
    /// Structural on purpose: literals and arithmetic over in-process values
    /// stay in-process, and anything the rules do not recognise is `false`. An
    /// over-broad answer here hides the exact crash the check exists to find,
    /// so every rule is one a reader would accept on sight.
    fn in_process(&self, e: &syn::Expr) -> bool {
        match crate::ast::peel_grouping(e) {
            syn::Expr::Lit(_) => true,
            syn::Expr::Cast(c) => self.in_process(&c.expr),
            syn::Expr::Unary(u) => self.in_process(&u.expr),
            // `vec.len() - 1`, `n * 2`: bounded on both sides means bounded.
            // One unrecognised operand disqualifies the whole expression.
            syn::Expr::Binary(b) => self.in_process(&b.left) && self.in_process(&b.right),
            syn::Expr::MethodCall(c) => {
                let m = c.method.to_string();
                // A container reporting on itself.
                if c.args.is_empty() && SIZE_VERBS.contains(&m.as_str()) {
                    return true;
                }
                // A value the author already bounded: `x.clamp(0, 255)` fits a
                // `u8` whatever `x` was.
                if BOUNDING_VERBS.contains(&m.as_str()) && !c.args.is_empty() {
                    return true;
                }
                // A method declared in this tree that hands back a plain value
                // — the same rule as the free-fn arm below, since `self.width()`
                // and `width(self)` are the same evidence.
                self.local_fn_yields_a_plain_value(&m)
            }
            // A local bound to one of the above.
            syn::Expr::Path(p) => p
                .path
                .get_ident()
                .is_some_and(|i| self.bound_in_process(&i.to_string())),
            // A call to a fn declared in this tree that hands back a plain
            // value. `fn width(&self) -> u32` produced its answer here;
            // `fn read_header(..) -> Result<Header, E>` did not, and an
            // unknown name is not evidence of anything.
            syn::Expr::Call(c) => {
                let syn::Expr::Path(p) = crate::ast::peel_grouping(&c.func) else {
                    return false;
                };
                let Some(seg) = p.path.segments.last() else {
                    return false;
                };
                self.local_fn_yields_a_plain_value(&seg.ident.to_string())
            }
            _ => false,
        }
    }

    /// Is `name` a fn declared in this tree that returns a value rather than a
    /// fallible result, and does not name a parse or an IO?
    ///
    /// `fn width(&self) -> u32` produced its answer inside the process.
    /// `fn read_header(..) -> Result<Header, E>` did not, and a name the index
    /// does not know is not evidence of anything — `FnSigIndex` drops names two
    /// declarations share, so a hit here is a name that means one thing.
    fn local_fn_yields_a_plain_value(&self, name: &str) -> bool {
        if crate::error_swallows::names_a_decode_or_io_verb(name) {
            return false;
        }
        !matches!(
            self.sigs.return_type(name),
            None | Some("Result") | Some("Option")
        )
    }

    /// Where the value a fallible conversion was applied to came from.
    ///
    /// Only asked of `Decode`; no other class turns on provenance.
    fn provenance(&self, effect: Effect, recv: &syn::Expr) -> Provenance {
        if effect != Effect::Decode {
            return Provenance::Unknown;
        }
        let mut inputs: Vec<&syn::Expr> = Vec::new();
        conversion_inputs(recv, &mut inputs);
        if !inputs.is_empty() && inputs.iter().all(|a| self.in_process(a)) {
            Provenance::InProcess
        } else {
            Provenance::Unknown
        }
    }
}

impl<'ast> Visit<'ast> for PanicVisitor<'_> {
    scope_visits!(
        item_mod,
        item_impl,
        item_trait,
        item_fn,
        impl_item_fn,
        trait_item_fn
    );

    /// One frame per block, so `let n = v.len();` reaches the `.unwrap()` three
    /// lines down and no further. Lexical rather than whole-file: a `n` bound
    /// to a length in one fn says nothing about the `n` in the next one, and
    /// clearing it there is exactly the over-broad rule that would hide a real
    /// crash.
    fn visit_block(&mut self, b: &'ast syn::Block) {
        self.bounded.push(Default::default());
        for s in &b.stmts {
            // Bind before visiting the statement so the binding is not visible
            // to its own initialiser, and after the previous statements so a
            // shadowing rebind cannot reach backwards.
            visit::visit_stmt(self, s);
            if let syn::Stmt::Local(l) = s {
                if let Some(init) = &l.init {
                    if init.diverge.is_none() && self.in_process(&init.expr) {
                        self.note_bound(&l.pat);
                    }
                }
            }
        }
        self.bounded.pop();
    }

    /// `for i in 0..v.len()` — the loop variable cannot leave the range, so a
    /// conversion of it inside the body is arithmetic the process did.
    fn visit_expr_for_loop(&mut self, e: &'ast syn::ExprForLoop) {
        self.visit_expr(&e.expr);
        self.bounded.push(Default::default());
        let bounded_range = match crate::ast::peel_grouping(&e.expr) {
            syn::Expr::Range(r) => {
                r.start.as_ref().is_none_or(|s| self.in_process(s))
                    && r.end.as_ref().is_some_and(|s| self.in_process(s))
            }
            _ => false,
        };
        if bounded_range {
            self.note_bound(&e.pat);
        }
        self.visit_block(&e.body);
        self.bounded.pop();
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let kind = match e.method.to_string().as_str() {
            "unwrap" if e.args.is_empty() => Some(".unwrap"),
            "expect" if e.args.len() == 1 => Some(".expect"),
            _ => None,
        };
        if let Some(k) = kind {
            let benign = if receiver_is_lock(&e.receiver) {
                Some("lock-poison")
            } else if receiver_is_literal_only(&e.receiver) {
                Some("literal-infallible")
            } else {
                None
            };
            // The receiver, not the call: `.expect("…")`'s message argument is
            // the diagnostic, not the thing that can fail.
            let effect = classify_effect(&e.receiver);
            let provenance = self.provenance(effect, &e.receiver);
            self.record(k, line_of(&e.method), benign, effect, provenance);
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        let kind = match m
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .as_deref()
        {
            Some("panic") => Some("panic!"),
            Some("unreachable") => Some("unreachable!"),
            Some("todo") => Some("todo!"),
            Some("unimplemented") => Some("unimplemented!"),
            _ => None,
        };
        if let Some(k) = kind {
            // A macro has no receiver, so there is nothing to classify: the
            // kind term carries the whole score.
            self.record(
                k,
                line_of_span(m.path.span()),
                None,
                Effect::Unknown,
                Provenance::Unknown,
            );
        }
        visit::visit_macro(self, m);
    }
}

/// `self.state.lock().unwrap()` — the receiver's own last call is a lock
/// acquisition, so the `Result` is a poisoning report.
///
/// Shared with [`crate::doc_drift`], which reaches the same conclusion from the
/// other direction: a poisoned-lock unwrap backs a `# Panics` section somebody
/// wrote, and does not demand one that nobody did.
pub(crate) fn receiver_is_lock(recv: &syn::Expr) -> bool {
    match crate::ast::peel_grouping(recv) {
        syn::Expr::MethodCall(c) => {
            c.args.is_empty() && LOCK_VERBS.contains(&c.method.to_string().as_str())
        }
        _ => false,
    }
}

/// `Regex::new("^v[0-9]+$").unwrap()` and `"3".parse::<u8>().unwrap()` cannot
/// fail at runtime on any input the program did not already contain, so they
/// are assertions about the source file rather than about data.
///
/// The shared definition, so this and `error-swallows` cannot drift apart on
/// what counts as a constant. See [`crate::ast::is_literal_only`].
fn receiver_is_literal_only(recv: &syn::Expr) -> bool {
    crate::ast::is_literal_only(recv)
}

pub fn run(ctx: &AnalysisCtx, opts: PanicOpts) -> anyhow::Result<usize> {
    Ok(run_counted(ctx, opts)?.total)
}

/// As [`run`], but also reporting how many rows clear [`GATING_SCORE`] — the
/// split `audit` gates on. Every row is still printed; the tier only decides
/// which ones hold the loop open.
pub fn run_counted(ctx: &AnalysisCtx, opts: PanicOpts) -> anyhow::Result<Counts> {
    let mut counts = Counts::default();
    let mut all: Vec<Hit> = Vec::new();
    for f in ctx.files {
        let mut v = PanicVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            hits: Vec::new(),
            sigs: &ctx.sem.fn_sigs,
            bounded: vec![Default::default()],
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }
    ctx.retain_changed(&mut all, |h| &h.file);
    // Keyed by kind, like `error-swallows`: a waiver written over the
    // `.unwrap()` on a line must not also cover a `todo!()` on it.
    // The tier `audit` gates on is applied below — after this retain, because a
    // suppressed row must not be counted at all. Telling the ledger which side
    // of it each hit falls on is what makes `hits` mean "suppressed something
    // the audit battery would have gated on", which is what the column claims.
    let waived = ctx.retain_unsuppressed_tiered(
        "panics",
        &mut all,
        |h| crate::suppress::Site::keyed(h.file.as_str(), h.line, h.kind),
        |h| {
            (opts.include_idiomatic || h.benign.is_none())
                && h.score() >= opts.min_score
                && h.score() >= GATING_SCORE
        },
    );
    let before = all.len();
    if !opts.include_idiomatic {
        all.retain(|h| h.benign.is_none());
    }
    let idiomatic_hidden = before - all.len();
    let idiomatic_shown = all.iter().filter(|h| h.benign.is_some()).count();

    // Before the counts: a floor says "these are not findings", so the summary
    // must stop counting them, unlike `--top` which only bounds the listing.
    let below_floor = if opts.min_score > 0.0 {
        let n = all.len();
        all.retain(|h| h.score() >= opts.min_score);
        n - all.len()
    } else {
        0
    };

    all.sort_by(|a, b| {
        b.score()
            .partial_cmp(&a.score())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.kind.cmp(b.kind))
    });

    use std::collections::BTreeMap;
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for h in &all {
        *by_kind.entry(h.kind).or_insert(0) += 1;
    }
    let breakdown: Vec<String> = by_kind.iter().map(|(k, n)| format!("{}={}", k, n)).collect();
    // Said out loud, because it is the difference between "this codebase is
    // careful" and "the tool stopped looking". 95 gating findings on one tree
    // were all this shape.
    let in_process = all
        .iter()
        .filter(|h| h.provenance == Provenance::InProcess)
        .count();
    let top_tier = all.iter().filter(|h| h.score() >= GATING_SCORE).count();
    counts.total = all.len();
    counts.gating = top_tier;

    if !ctx.summary {
        let today = crate::suppress::Date::today();
        for h in &all {
            row!(
                ctx.out,
                "kind" => h.kind,
                "score" => format!("{:.2}", h.score()),
                "effect" => h.effect_cell(),
                "in_fn" => h.context.clone(),
                "at" => site(&h.file, h.line),
            );
            ctx.suggest("panics", Some(h.kind), today);
        }
    }
    ctx.out.summary(&format!(
        "({} panic site(s){}{}; {}{}{}{}; explain: silent-fallbacks)",
        counts.total,
        if top_tier > 0 {
            format!(
                ", {} at score >= {:.2} (asserted on data from outside the process — \
                 the tier `audit` gates on)",
                top_tier, GATING_SCORE
            )
        } else {
            String::new()
        },
        if below_floor > 0 {
            format!("; {} below --min-score {:.2}", below_floor, opts.min_score)
        } else {
            String::new()
        },
        breakdown.join(", "),
        if in_process > 0 {
            format!(
                "; {} decode(s) demoted — the converted value was a length, an index, a \
                 bounded value or a local call, so no data from outside the process \
                 reaches them (`effect` cell says `decode(in-process)`)",
                in_process
            )
        } else {
            String::new()
        },
        ctx.waived_note(waived),
        if idiomatic_hidden > 0 {
            format!(
                "; {} idiomatic site(s) hidden (poisoned-lock unwraps / assertions over \
                 source literals — `--include-idiomatic` to restore)",
                idiomatic_hidden
            )
        } else if idiomatic_shown > 0 {
            // The converse, for the same reason `error-swallows` says it: this
            // command shows every family while `audit` drops the idiomatic
            // ones, so a count that does not move after a fix reads as a fix
            // that did not work.
            format!(
                "; {} of these are idiomatic (poisoned-lock unwraps, assertions over \
                 source literals) and are hidden in `audit`",
                idiomatic_shown
            )
        } else {
            String::new()
        }
    ));
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: &'static str, effect: Effect, benign: Option<&'static str>) -> Hit {
        Hit {
            kind,
            file: "f.rs".into(),
            line: 1,
            context: "m::f".into(),
            benign,
            effect,
            provenance: Provenance::Unknown,
        }
    }

    /// Every panic site in `src`, with the provenance rules applied against
    /// `src`'s own fn signatures. Goes through the real visitor rather than
    /// hand-built `Hit`s: the provenance rules are lexical, and a test that
    /// supplied its own scope stack would be testing the test.
    fn scan(src: &str) -> Vec<Hit> {
        let file = syn::parse_file(src).expect("fixture must parse");
        let parsed = vec![crate::parse::ParsedFile {
            path: std::path::PathBuf::from("src/lib.rs"),
            module: "t".to_string(),
            ast: file,
        }];
        let sigs = crate::semantic::FnSigIndex::build(&parsed);
        let mut v = PanicVisitor {
            file: "src/lib.rs",
            scope: ScopeTracker::new("t"),
            hits: Vec::new(),
            sigs: &sigs,
            bounded: vec![Default::default()],
        };
        v.visit_file(&parsed[0].ast);
        v.hits
    }

    /// One rule per test would be six near-identical tests; this asserts the
    /// whole table at once so a rule that stops firing cannot hide behind the
    /// others.
    fn provenance_of(src: &str) -> Provenance {
        let hits = scan(src);
        assert_eq!(hits.len(), 1, "fixture must hold exactly one site: {src}");
        hits[0].provenance
    }

    fn recv(src: &str) -> syn::Expr {
        syn::parse_str(src).expect("parse")
    }

    /// The defect class this check was built for: 18 changelog entries whose
    /// fix was "report the error instead of panicking", every one of them a
    /// `.unwrap()` on a parse of something the process did not produce.
    #[test]
    fn unwrap_on_a_parse_gates() {
        assert!(
            hit(".unwrap", Effect::Decode, None).score() >= GATING_SCORE,
            "unwrapping a parse of external input must gate"
        );
        assert!(
            hit(".unwrap", Effect::Io, None).score() >= GATING_SCORE,
            "unwrapping a read must gate"
        );
    }

    /// An `.expect` on an unrecognised in-process call is the single most
    /// common shape in any Rust tree. If it gated, the gate would be one
    /// nobody could clear.
    #[test]
    fn expect_on_an_unknown_call_does_not_gate() {
        assert!(hit(".expect", Effect::Unknown, None).score() < GATING_SCORE);
        assert!(hit(".unwrap", Effect::Unknown, None).score() < GATING_SCORE);
    }

    /// A shipped `todo!()` is a crash on a path someone can reach, and the
    /// kind term alone has to carry it — there is no receiver to classify.
    #[test]
    fn todo_gates_on_its_own() {
        assert!(hit("todo!", Effect::Unknown, None).score() >= GATING_SCORE);
        assert!(hit("unimplemented!", Effect::Unknown, None).score() >= GATING_SCORE);
    }

    /// The inversion against `error-swallows`, asserted so a later edit to
    /// either table cannot quietly align them: a dropped mutation is the worst
    /// swallow and a panicking one is a reported failure.
    #[test]
    fn decode_outranks_mutation_for_panics() {
        assert!(effect_weight(Effect::Decode) > effect_weight(Effect::Mutation));
        assert!(
            crate::error_swallows::Effect::Mutation.as_str() == "mutation",
            "effect names are shared with error-swallows"
        );
    }

    /// The 95-finding class: a fallible conversion of something the process
    /// itself computed. `decode`'s intent is that "the input to a parse is by
    /// definition data the process did not produce", and the implementation
    /// could not tell `u32::try_from(vec.len())` from
    /// `u32::try_from(parsed_header_field)`.
    #[test]
    fn a_conversion_of_the_processs_own_arithmetic_is_not_a_decode() {
        let wrap = |body: &str| format!("pub fn f(v: &Vec<u8>, raw: i64) -> u8 {{ {body} }}\n");
        // A container reporting on itself, and arithmetic over one.
        assert_eq!(
            provenance_of(&wrap("u8::try_from(v.len()).unwrap()")),
            Provenance::InProcess
        );
        assert_eq!(
            provenance_of(&wrap("u8::try_from(v.len() - 1).unwrap()")),
            Provenance::InProcess
        );
        // A value the author already bounded.
        assert_eq!(
            provenance_of(&wrap("u8::try_from(raw.clamp(0, 255)).unwrap()")),
            Provenance::InProcess
        );
        // A local binding to one of the above.
        assert_eq!(
            provenance_of(&wrap("let n = v.len(); u8::try_from(n).unwrap()")),
            Provenance::InProcess
        );
        // A loop variable bounded by one.
        assert_eq!(
            provenance_of(&wrap(
                "for i in 0..v.len() { let _ = u8::try_from(i).unwrap(); } 0"
            )),
            Provenance::InProcess
        );
    }

    /// A call to a local fn that hands back a plain value produced its answer
    /// inside the process; one that hands back a `Result`, or that names a
    /// parse or an IO, did not.
    #[test]
    fn a_local_call_counts_only_when_it_cannot_have_brought_data_in() {
        let src = |call: &str| {
            format!(
                "pub fn width() -> u32 {{ 7 }}\n                 pub fn read_len() -> u32 {{ 9 }}\n                 pub fn load() -> Result<u32, ()> {{ Ok(1) }}\n                 pub fn f() -> u8 {{ u8::try_from({call}).unwrap() }}\n"
            )
        };
        assert_eq!(provenance_of(&src("width()")), Provenance::InProcess);
        // Names an IO: the value may well have come from outside.
        assert_eq!(provenance_of(&src("read_len()")), Provenance::Unknown);
        // A fallible return is not evidence of anything.
        assert_eq!(provenance_of(&src("load()")), Provenance::Unknown);
        // A name this tree does not declare is not evidence either.
        assert_eq!(provenance_of(&src("elsewhere()")), Provenance::Unknown);
    }

    /// The shape the check exists for has to survive the narrowing. Both of
    /// these are the ones a reader verified by hand on the real codebase.
    #[test]
    fn a_conversion_of_data_from_outside_is_still_a_decode_and_still_gates() {
        let outside = "pub fn parse_field(b: &[u8]) -> i64 { b.len() as i64 }\n                       pub fn f(b: &[u8]) -> u32 { u32::try_from(parse_field(b)).unwrap() }\n";
        assert_eq!(provenance_of(outside), Provenance::Unknown);
        let hits = scan("pub fn f(s: &str) -> u32 { s.parse::<u32>().unwrap() }\n");
        assert_eq!(hits[0].provenance, Provenance::Unknown);
        assert!(hits[0].score() >= GATING_SCORE, "the true positive must gate");
    }

    /// Demoted, not dropped. The row still prints and says which rows moved.
    #[test]
    fn a_demoted_decode_is_reported_and_labelled() {
        let hits = scan("pub fn f(v: &Vec<u8>) -> u8 { u8::try_from(v.len()).unwrap() }\n");
        assert_eq!(hits.len(), 1, "the row is demoted, not removed");
        assert_eq!(hits[0].effect_cell(), "decode(in-process)");
        assert!(hits[0].score() < GATING_SCORE);
    }

    /// A binding is lexical. A `n` bound to a length in one fn says nothing
    /// about the `n` in the next one, and clearing it there is exactly the
    /// over-broad rule that would hide a real crash.
    #[test]
    fn a_binding_does_not_leak_out_of_its_block() {
        let hits = scan(
            "pub fn a(v: &Vec<u8>) -> u8 { let n = v.len(); u8::try_from(n).unwrap() }\n             pub fn b(n: i64) -> u8 { u8::try_from(n).unwrap() }\n",
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].provenance, Provenance::InProcess);
        assert_eq!(hits[1].provenance, Provenance::Unknown, "`n` is a parameter here");
    }

    #[test]
    fn a_poisoned_lock_unwrap_is_idiomatic() {
        assert!(receiver_is_lock(&recv("self.state.lock()")));
        assert!(receiver_is_lock(&recv("cache.borrow_mut()")));
        assert!(!receiver_is_lock(&recv("self.state.get(k)")));
        // And it must not gate once cleared, whatever the receiver looked like.
        assert!(hit(".unwrap", Effect::Io, Some("lock-poison")).score() < GATING_SCORE);
    }

    #[test]
    fn assertions_over_source_literals_are_idiomatic() {
        assert!(receiver_is_literal_only(&recv(r#"Regex::new("^a$")"#)));
        assert!(receiver_is_literal_only(&recv(r#""3".parse::<u8>()"#)));
        // A variable anywhere in the chain means the input can vary.
        assert!(!receiver_is_literal_only(&recv("Regex::new(pattern)")));
        assert!(!receiver_is_literal_only(&recv("input.parse::<u8>()")));
    }
}
