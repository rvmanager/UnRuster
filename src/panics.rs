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
        } else {
            effect_weight(self.effect)
        };
        (effect + kind).min(1.0)
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

struct PanicVisitor<'a> {
    file: &'a str,
    scope: ScopeTracker,
    hits: Vec<Hit>,
}

impl PanicVisitor<'_> {
    fn record(
        &mut self,
        kind: &'static str,
        line: usize,
        benign: Option<&'static str>,
        effect: Effect,
    ) {
        let context = self.scope.enclosing();
        self.hits.push(Hit {
            kind,
            file: self.file.to_string(),
            line,
            context,
            benign,
            effect,
        });
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
            self.record(k, line_of(&e.method), benign, classify_effect(&e.receiver));
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
            self.record(k, line_of_span(m.path.span()), None, Effect::Unknown);
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
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }
    ctx.retain_changed(&mut all, |h| &h.file);
    // Keyed by kind, like `error-swallows`: a waiver written over the
    // `.unwrap()` on a line must not also cover a `todo!()` on it.
    let waived = ctx.retain_unsuppressed("panics", &mut all, |h| {
        crate::suppress::Site::keyed(h.file.as_str(), h.line, h.kind)
    });
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
                "effect" => h.effect.as_str(),
                "context" => h.context.clone(),
                "at" => site(&h.file, h.line),
            );
            ctx.suggest("panics", Some(h.kind), today);
        }
    }
    ctx.out.summary(&format!(
        "({} panic site(s){}{}; {}{}{}; explain: silent-fallbacks)",
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
        }
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
