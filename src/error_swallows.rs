use std::collections::HashSet;

use syn::visit::{self, Visit};

use crate::ast::{line_of, line_of_span, pat_is_ok, peel_grouping, scope_visits, ScopeTracker};
use crate::context::{AnalysisCtx, Counts};
use crate::parse::display_path;
use crate::emit::{row, site};

#[derive(Debug)]
struct Hit {
    /// Method-call swallows:
    ///   ".ok" | ".err" | ".unwrap_or_default" | ".unwrap_or_else" |
    ///   ".unwrap_or" | ".map_err(|_|...)"
    /// Syntactic swallows:
    ///   "match-err-wild" | "if-let-ok" | "while-let-ok" | "let-_"
    kind: &'static str,
    file: String,
    line: usize,
    context: String,
    /// True when the site is one of the two families that are idiomatic rather
    /// than defective: an infallible in-memory write, or a fallback that logs.
    /// Kept as a flag rather than dropped at scan time so the summary can say
    /// how many were filtered and `--include-*` can restore them.
    benign: Option<&'static str>,
    /// What the discarded `Result` was reporting on. See [`Effect`].
    effect: Effect,
}

impl Hit {
    /// How much this site deserves a reader's attention, 0.0–1.0.
    ///
    /// Two independent questions, added:
    ///
    /// * **What failed** ([`Effect`]) — an external mutation that nobody
    ///   checked is a different animal from a base64 decode that returned
    ///   `None`, even though both are `.ok()`.
    /// * **How completely the failure vanished** (the swallow kind) — `let _ =`
    ///   drops the error *and* continues; `.map_err(|_|)` replaces the cause but
    ///   still propagates the failure, so the caller can act.
    ///
    /// The second term is why the crypto-sanitization family sorts to the
    /// bottom on its own: those sites collapse causes deliberately and the
    /// failure still travels. That family was 13 of the 89 rows on the codebase
    /// this ranking was built against, all correct, all previously
    /// indistinguishable from the money bug.
    fn score(&self) -> f64 {
        let kind = match self.kind {
            // Error and value both gone, control continues on the happy path.
            "let-_" | "match-err-wild" => 0.30,
            // The failure becomes a `None` the caller may or may not check.
            ".ok" | ".err" | "if-let-ok" | "while-let-ok" => 0.20,
            // A substituted value: execution continues as if it had succeeded.
            ".unwrap_or_default" | ".unwrap_or_else" | ".unwrap_or" => 0.15,
            // Cause replaced, failure still propagates — the sanitization shape.
            ".map_err(|_|)" => 0.05,
            _ => 0.15,
        };
        (self.effect.weight() + kind).min(1.0)
    }
}

/// What the discarded `Result` was reporting on — the single feature that
/// separated the real defects from the correct-by-design sites on the codebase
/// this was calibrated against.
///
/// Classified from the swallowed expression's call chain, so it is a
/// BEST-EFFORT signal: a project that wraps its database in `fn persist()`
/// reads as `Unknown`, not `Mutation`. It ranks, it does not adjudicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect {
    /// External state was changed — a row written, a message sent, a file
    /// replaced. If this `Result` is dropped, the only record that the effect
    /// did or did not happen is gone with it. `let _ = sqlx::query("DELETE
    /// FROM stripe_events …").execute(&db).await` is this class, and it was a
    /// permanent loss of Stripe payment confirmations.
    Mutation,
    /// An external interaction that only reads — a fetch, a query, a file read.
    /// Dropping it degrades behaviour but leaves the world consistent.
    Io,
    /// A pure transformation of data already in hand: parse, decode, convert.
    /// Nothing outside the process was touched, and on a validation path
    /// "it didn't parse" is frequently the whole answer.
    Decode,
    /// The chain named nothing recognizable.
    Unknown,
}

impl Effect {
    fn weight(self) -> f64 {
        match self {
            Effect::Mutation => 0.60,
            Effect::Io => 0.35,
            Effect::Unknown => 0.20,
            Effect::Decode => 0.05,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Effect::Mutation => "mutation",
            Effect::Io => "io",
            Effect::Decode => "decode",
            Effect::Unknown => "unknown",
        }
    }
}

/// Verbs that change state outside this process. Matched on the method or
/// call-path segment, by whole name or as a leading word (`send_batch`,
/// `write_all`), so a project's own spellings mostly land without an
/// allow-list entry.
const MUTATION_VERBS: &[&str] = &[
    "execute", "commit", "rollback", "send", "publish", "emit", "dispatch", "write", "write_all",
    "flush", "insert", "remove", "delete", "update", "upsert", "persist", "save", "store",
    "create", "create_dir", "create_dir_all", "remove_file", "remove_dir", "remove_dir_all",
    "rename", "copy", "set_permissions", "set_len", "truncate", "sync_all", "sync_data",
    "spawn", "kill", "wait", "notify", "ack", "commit_async", "bind_execute",
];

/// Verbs that reach outside the process without changing it.
const IO_VERBS: &[&str] = &[
    "fetch", "fetch_one", "fetch_all", "fetch_optional", "query", "query_as", "query_scalar",
    "get", "post", "put", "patch", "head", "request", "call", "connect", "read", "read_to_string",
    "read_to_end", "read_dir", "recv", "receive", "poll", "load", "open", "metadata", "canonicalize",
    "lock", "acquire", "begin",
];

/// Verbs that only reshape data the process already holds.
const DECODE_VERBS: &[&str] = &[
    "parse", "parse_str", "from_str", "from_slice", "from_bytes", "from_utf8", "decode",
    "deserialize", "try_into", "try_from", "into", "to_str", "as_str", "encode", "serialize",
    "to_string", "strip_prefix", "strip_suffix", "split_once", "from_hex", "to_vec",
];

/// Does `name` name one of `verbs`, either exactly or as its leading word?
fn verb_matches(name: &str, verbs: &[&str]) -> bool {
    verbs.iter().any(|v| {
        name == *v
            || name
                .strip_prefix(v)
                .is_some_and(|rest| rest.starts_with('_'))
    })
}

/// Classify the swallowed expression by the strongest effect anywhere in its
/// call chain.
///
/// The whole subtree is walked rather than just the outermost call: the effect
/// in `sqlx::query(…).bind(id).execute(&mut *tx).await` sits three links down,
/// and `query` alone would read as a plain read. Mutation wins over IO wins
/// over decode, because a chain that both queries and executes did mutate.
fn classify_effect(expr: &syn::Expr) -> Effect {
    struct V {
        mutation: bool,
        io: bool,
        decode: bool,
    }
    impl V {
        fn note(&mut self, name: &str) {
            if verb_matches(name, MUTATION_VERBS) {
                self.mutation = true;
            } else if verb_matches(name, IO_VERBS) {
                self.io = true;
            } else if verb_matches(name, DECODE_VERBS) {
                self.decode = true;
            }
        }
    }
    impl<'ast> Visit<'ast> for V {
        fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
            self.note(&c.method.to_string());
            visit::visit_expr_method_call(self, c);
        }
        fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
            if let syn::Expr::Path(p) = &*c.func {
                if let Some(seg) = p.path.segments.last() {
                    self.note(&seg.ident.to_string());
                }
            }
            visit::visit_expr_call(self, c);
        }
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            // `let _ = writeln!(file, …)` is a write; the benign filter already
            // spares the in-memory buffers.
            if let Some(seg) = m.path.segments.last() {
                self.note(&seg.ident.to_string());
            }
            for e in crate::macro_scan::macro_exprs(m) {
                self.visit_expr(&e);
            }
        }
        // A closure inside the chain is the *handler*, not the effect — it is
        // what runs when the thing failed. Walking into it would let a
        // `.unwrap_or_else(|| String::new())` read as whatever the fallback does.
        fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
    }
    let mut v = V {
        mutation: false,
        io: false,
        decode: false,
    };
    v.visit_expr(expr);
    match (v.mutation, v.io, v.decode) {
        (true, _, _) => Effect::Mutation,
        (_, true, _) => Effect::Io,
        (_, _, true) => Effect::Decode,
        _ => Effect::Unknown,
    }
}

struct SwallowVisitor<'a> {
    include_unwrap_or: bool,
    file: &'a str,
    scope: ScopeTracker,
    /// Whether the expression being visited sits in a closure's tail position.
    /// `filter_map(|t| t.parse().ok())` turns a Result into an Option *so the
    /// iterator can filter on it* — the error is the predicate, not something
    /// dropped. On one real codebase this idiom was most of the `.ok` bucket.
    closure_tail: Vec<bool>,
    /// Spans of `.ok()` / `.err()` calls that sit directly under a `?`.
    /// `parse().ok()?` discards the error *value* but propagates the failure —
    /// control never continues past it, so nothing is silently swallowed. On
    /// this codebase six of the seven `.ok` rows were this idiom.
    propagated: HashSet<(usize, usize)>,
    hits: Vec<Hit>,
}

/// Macros whose `Result` is infallible when the target is an in-memory
/// `String`/`Vec` — `write!`/`writeln!` into a `fmt::Write` buffer cannot fail,
/// so `let _ = write!(s, …)` is the idiomatic spelling, not a swallowed error.
/// These dominated the `let-_` bucket on a real codebase (a large majority of
/// 116 rows) while producing no defects.
const INFALLIBLE_WRITE_MACROS: &[&str] = &["write", "writeln"];

/// Does this `let _ = …;` discard an infallible in-memory write?
fn is_infallible_write(init: &syn::Expr) -> bool {
    let syn::Expr::Macro(m) = init else {
        return false;
    };
    let Some(name) = m.mac.path.segments.last() else {
        return false;
    };
    INFALLIBLE_WRITE_MACROS.contains(&name.ident.to_string().as_str())
}

/// Does a fallback closure body make the failure observable — a log, a warn, a
/// debug macro, an `eprintln!`? `\u{2e}unwrap_or_else(|| { log!(…); default })`
/// is a *handled* fallback: the error was noticed and a policy applied. Rows
/// like these were ~half the `.unwrap_or_else` bucket and none were defects.
fn fallback_is_logged(e: &syn::ExprMethodCall) -> bool {
    e.args.iter().any(crate::ast::mentions_logging)
}


/// Methods that yield `Option` and have no `Result` counterpart. Reaching one
/// while walking back up a call chain proves the chain's value is an `Option`.
const OPTION_SOURCES: &[&str] = &[
    "last", "first", "get", "get_mut", "next", "next_back", "find", "find_map", "pop", "peek",
    "position", "rposition", "strip_prefix", "strip_suffix", "file_name", "file_stem",
    "extension", "parent", "to_str", "checked_add", "checked_sub", "checked_mul", "checked_div",
    "chars_next", "front", "back", "iter_next",
];

/// Combinators that pass the Option/Result shape through unchanged, so the walk
/// can look past them for the source.
const SHAPE_PRESERVING: &[&str] = &[
    "map", "filter", "cloned", "copied", "as_ref", "as_deref", "as_mut", "take", "or", "or_else",
    "and_then", "flatten", "inspect",
];

/// Is this call chain's value definitively an `Option`?
///
/// `.unwrap_or_default()` on an `Option` is not error swallowing — there is no
/// error. The check cannot infer types, but it does not need to: a chain that
/// bottoms out in `.last()` / `.get()` / `.find()` has no `Result` anywhere in
/// it. Nine of twenty-two rows on this codebase were
/// `path.segments.last().map(…).unwrap_or_default()`.
fn receiver_is_option(mut e: &syn::Expr) -> bool {
    for _ in 0..8 {
        let syn::Expr::MethodCall(mc) = peel_grouping(e) else {
            return false;
        };
        let name = mc.method.to_string();
        if OPTION_SOURCES.contains(&name.as_str()) {
            return true;
        }
        if !SHAPE_PRESERVING.contains(&name.as_str()) {
            return false;
        }
        e = &mc.receiver;
    }
    false
}


/// Does every path out of this block leave the enclosing function or loop?
/// Only the last statement is inspected: an early `return` buried mid-block
/// still leaves a joining tail, which is the case that genuinely drops the
/// failure.
fn block_diverges(b: &syn::Block) -> bool {
    let Some(last) = b.stmts.last() else {
        return false;
    };
    let e = match last {
        syn::Stmt::Expr(e, _) => e,
        _ => return false,
    };
    matches!(
        peel_grouping(e),
        syn::Expr::Return(_) | syn::Expr::Break(_) | syn::Expr::Continue(_)
    ) || matches!(peel_grouping(e), syn::Expr::Macro(m)
        if m.mac.path.segments.last().is_some_and(|s| {
            let n = s.ident.to_string();
            n == "panic" || n == "unreachable" || n == "todo"
        }))
}

/// `Option::unwrap_or_else` takes `||`; `Result::unwrap_or_else` receives the
/// error as `|e|`. The arity alone settles which one this is — the same free
/// discriminator that distinguishes `.ok()?` from a bare `.ok()`.
fn fallback_closure_is_nullary(e: &syn::ExprMethodCall) -> bool {
    matches!(e.args.first(), Some(syn::Expr::Closure(c)) if c.inputs.is_empty())
}

impl<'a> SwallowVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    /// Is this `.ok()` / `.err()` the operand of a `?`, i.e. propagation
    /// rather than a silent drop?
    fn is_propagated(&self, method: &syn::Ident) -> bool {
        let s = method.span().start();
        self.propagated.contains(&(s.line, s.column))
    }

    fn record(&mut self, kind: &'static str, line: usize, swallowed: &syn::Expr) {
        self.record_tagged(kind, line, None, swallowed);
    }

    /// `swallowed` is the expression whose `Result` is being dropped — the
    /// method receiver, the `let` initialiser, the match scrutinee. It is the
    /// only thing that distinguishes a discarded DELETE from a discarded
    /// base64 decode, so every record path has to supply it.
    fn record_tagged(
        &mut self,
        kind: &'static str,
        line: usize,
        benign: Option<&'static str>,
        swallowed: &syn::Expr,
    ) {
        let ctx = self.enclosing();
        self.hits.push(Hit {
            kind,
            file: self.file.to_string(),
            line,
            context: ctx,
            benign,
            effect: classify_effect(swallowed),
        });
    }
}

/// True for `_` and underscore-prefixed bindings (`_`, `_e`, `_err`) — the
/// convention for "intentionally discarded." A bare `e` returns false because
/// it may be referenced in the body.
fn pat_is_discarded(p: &syn::Pat) -> bool {
    match p {
        syn::Pat::Wild(_) => true,
        syn::Pat::Ident(i) => {
            i.subpat.is_none() && i.ident.to_string().starts_with('_')
        }
        syn::Pat::Reference(r) => pat_is_discarded(&r.pat),
        syn::Pat::Paren(p) => pat_is_discarded(&p.pat),
        _ => false,
    }
}

/// `.map_err(|_| …)` / `.map_err(|_e| …)` — the closure's first arg is a
/// discard binding, so the error contents are intentionally dropped.
fn map_err_discards(e: &syn::ExprMethodCall) -> bool {
    let Some(syn::Expr::Closure(c)) = e.args.first() else {
        return false;
    };
    c.inputs.first().map(pat_is_discarded).unwrap_or(false)
}

/// `Err(_)` / `Err(_e)` — the error contents are discarded by the pattern.
/// `Err(e)` is NOT flagged because the body may reference `e`.
fn pat_is_err_swallow(p: &syn::Pat) -> bool {
    match p {
        syn::Pat::TupleStruct(ts) => {
            let last = ts
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default();
            last == "Err" && ts.elems.iter().all(pat_is_discarded)
        }
        syn::Pat::Or(o) => o.cases.iter().any(pat_is_err_swallow),
        syn::Pat::Reference(r) => pat_is_err_swallow(&r.pat),
        syn::Pat::Paren(p) => pat_is_err_swallow(&p.pat),
        _ => false,
    }
}


impl<'ast, 'a> Visit<'ast> for SwallowVisitor<'a> {
    scope_visits!(item_mod, item_impl, item_trait, item_fn, impl_item_fn, trait_item_fn, expr_closure_tail);

    fn visit_expr_try(&mut self, e: &'ast syn::ExprTry) {
        // Runs before the child method call is visited, so the mark is in
        // place by the time the `.ok` arm asks about it.
        if let syn::Expr::MethodCall(mc) = &*e.expr {
            let s = mc.method.span().start();
            self.propagated.insert((s.line, s.column));
        }
        visit::visit_expr_try(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let m = e.method.to_string();
        let kind: Option<&'static str> = match m.as_str() {
            "ok" if e.args.is_empty() => Some(".ok"),
            "err" if e.args.is_empty() => Some(".err"),
            "unwrap_or_default" if e.args.is_empty() => Some(".unwrap_or_default"),
            "unwrap_or_else" => Some(".unwrap_or_else"),
            "unwrap_or" if self.include_unwrap_or => Some(".unwrap_or"),
            "map_err" if map_err_discards(e) => Some(".map_err(|_|)"),
            _ => None,
        };
        if let Some(k) = kind {
            let benign = if matches!(k, ".unwrap_or_else" | ".unwrap_or_default")
                && (fallback_closure_is_nullary(e) || receiver_is_option(&e.receiver))
            {
                // An Option has no error to swallow.
                Some("option-default")
            } else if k == ".unwrap_or_else" && fallback_is_logged(e) {
                Some("logged-fallback")
            } else if matches!(k, ".ok" | ".err") && self.is_propagated(&e.method) {
                Some("propagated")
            } else if k == ".ok" && self.closure_tail.last().copied().unwrap_or(false) {
                Some("combinator-ok")
            } else {
                None
            };
            // The receiver, not the whole call: the closure argument of
            // `.map_err(|_| …)` / `.unwrap_or_else(|| …)` is the handler that
            // runs on failure, not the thing that failed.
            self.record_tagged(k, line_of(&e.method), benign, &e.receiver);
        }
        // A closure passed as an argument is not in *this* call's tail slot.
        self.closure_tail.push(false);
        visit::visit_expr_method_call(self, e);
        self.closure_tail.pop();
    }


    fn visit_expr_match(&mut self, e: &'ast syn::ExprMatch) {
        for arm in &e.arms {
            if pat_is_err_swallow(&arm.pat) {
                let line = line_of_span(arm.fat_arrow_token.spans[0]);
                self.record("match-err-wild", line, &e.expr);
                break; // one report per match site
            }
        }
        visit::visit_expr_match(self, e);
    }

    fn visit_expr_if(&mut self, e: &'ast syn::ExprIf) {
        if e.else_branch.is_none() {
            if let syn::Expr::Let(le) = &*e.cond {
                if pat_is_ok(&le.pat) {
                    // `if let Ok(v) = … { return v }` is a strategy in a
                    // cascade: control diverges on success, so *falling
                    // through is the error handler*, not a silent drop. Only a
                    // body that runs on and joins the normal path discards the
                    // failure. Four of eight surviving rows on this codebase
                    // were the diverging shape, all in one parse-fallback chain.
                    let benign = if block_diverges(&e.then_branch) {
                        Some("fallthrough-is-handler")
                    } else {
                        None
                    };
                    self.record_tagged("if-let-ok", line_of(&e.if_token), benign, &le.expr);
                }
            }
        }
        visit::visit_expr_if(self, e);
    }

    fn visit_expr_while(&mut self, e: &'ast syn::ExprWhile) {
        if let syn::Expr::Let(le) = &*e.cond {
            if pat_is_ok(&le.pat) {
                self.record("while-let-ok", line_of(&e.while_token), &le.expr);
            }
        }
        visit::visit_expr_while(self, e);
    }

    // Every sibling site-scanner walks macro bodies; without this, swallows
    // inside macro args (e.g. `.ok()` in a `writeln!`) were invisible —
    // flagged by `cohort-callees visit_macro`.
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }

    fn visit_local(&mut self, l: &'ast syn::Local) {
        // `let _ = expr;` with init — explicit discard.
        let is_wild = match &l.pat {
            syn::Pat::Wild(_) => true,
            syn::Pat::Type(pt) => matches!(*pt.pat, syn::Pat::Wild(_)),
            _ => false,
        };
        if is_wild {
            if let Some(init) = &l.init {
                let benign = if is_infallible_write(&init.expr) {
                    Some("infallible-write")
                } else {
                    None
                };
                self.record_tagged("let-_", line_of(&l.let_token), benign, &init.expr);
            }
        }
        visit::visit_local(self, l);
    }
}

/// The score at or above which a swallow is a gating audit finding.
///
/// Placed so that the class it admits is "an external effect happened and the
/// only report of whether it worked was discarded" — mutation at any kind,
/// plus IO that vanished completely (`let _`, `match … Err(_) =>`). On the
/// workspace this was calibrated against that is ~8 rows out of 89, and the two
/// highest were both real production defects: a dropped `DELETE FROM
/// stripe_events` that permanently lost payment confirmations, and a dropped
/// dead-APNs-token delete whose sibling arm logged.
///
/// Deliberately above `Unknown + let-_` (0.50). An unrecognised call chain is
/// the common case in a codebase with its own wrappers, and gating on it would
/// reproduce the unranked list this score exists to replace.
pub const GATING_SCORE: f64 = 0.55;

/// Which families of swallow site to report.
#[derive(Clone, Copy)]
pub struct SwallowOpts {
    /// `.unwrap_or(…)` with any argument. Noisy; off by default.
    pub include_unwrap_or: bool,
    /// `let _ = write!(buf, …)` into an in-memory buffer.
    pub include_infallible: bool,
    /// `.unwrap_or_else(|| { log!(…); default })` — failure already observable.
    pub include_logged: bool,
}

impl Default for SwallowOpts {
    /// The bare `error-swallows` command keeps every family: the dedicated
    /// command is where someone goes to see everything. `audit` opts out of
    /// the benign families, since it is read for defects.
    fn default() -> Self {
        SwallowOpts {
            include_unwrap_or: false,
            include_infallible: true,
            include_logged: true,
        }
    }
}

pub fn run(ctx: &AnalysisCtx, opts: SwallowOpts) -> anyhow::Result<usize> {
    Ok(run_counted(ctx, opts)?.total)
}

/// As [`run`], but also reporting how many rows clear [`GATING_SCORE`] — the
/// split `audit` gates on. Every row is still printed; the tier only decides
/// which ones hold the loop open.
pub fn run_counted(ctx: &AnalysisCtx, opts: SwallowOpts) -> anyhow::Result<Counts> {
    let mut counts = Counts::default();
    let include_unwrap_or = opts.include_unwrap_or;
    let files = ctx.files;
    let summary = ctx.summary;
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = SwallowVisitor {
            include_unwrap_or,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(ctx.spans),
            closure_tail: Vec::new(),
            propagated: HashSet::new(),
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }
    ctx.retain_changed(&mut all, |h| &h.file);
    // Keyed by swallow kind (`let-_`, `.ok`, …) so a waiver written for the
    // `let _ =` on a line doesn't also cover a `.unwrap_or_default()` on it.
    let waived = ctx.retain_unsuppressed("error-swallows", &mut all, |h| {
        crate::suppress::Site::keyed(h.file.as_str(), h.line, h.kind)
    });
    let before = all.len();
    all.retain(|h| match h.benign {
        Some("infallible-write") => opts.include_infallible,
        Some("logged-fallback") | Some("combinator-ok") | Some("propagated")
        | Some("option-default") | Some("fallthrough-is-handler") => opts.include_logged,
        _ => true,
    });
    let benign_hidden = before - all.len();
    let benign_shown = all.iter().filter(|h| h.benign.is_some()).count();
    // Ranked, not alphabetical. This list runs to ~90 rows on a mid-size
    // workspace and converts at a few percent; sorted by kind, the one row that
    // was losing money sat at position 62, wedged between `db_clean` cleanup
    // noise, and the only way to find it was to read all 89 sites and their
    // surrounding source. Score first, then file/line so a given score is
    // stable to read and to diff.
    all.sort_by(|a, b| {
        b.score()
            .partial_cmp(&a.score())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.kind.cmp(b.kind))
    });
    if !summary {
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
            ctx.suggest("error-swallows", Some(h.kind), today);
        }
    }
    use std::collections::BTreeMap;
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for h in &all {
        *by_kind.entry(h.kind).or_insert(0) += 1;
    }
    let breakdown: Vec<String> = by_kind
        .iter()
        .map(|(k, n)| format!("{}={}", k, n))
        .collect();
    let top_tier = all.iter().filter(|h| h.score() >= GATING_SCORE).count();
    counts.total = all.len();
    counts.gating = top_tier;
    ctx.out.summary(&format!(
        "({} swallow site(s){}; {}; include_unwrap_or={}{}{}; explain: silent-fallbacks)",
        all.len(),
        if top_tier > 0 {
            format!(
                ", {} at score >= {:.2} (discarded external effects — the tier \
                 `audit` gates on)",
                top_tier, GATING_SCORE
            )
        } else {
            String::new()
        },
        breakdown.join(", "),
        include_unwrap_or,
        ctx.waived_note(waived),
        if benign_hidden > 0 {
            format!(
                "; {} benign site(s) hidden (infallible writes / logged fallbacks — \
                 `--include-infallible` / `--include-logged` to restore)",
                benign_hidden
            )
        } else if benign_shown > 0 {
            // The converse matters just as much. This command shows every
            // family by default while `audit` drops the benign ones, so after
            // fixing a site the count here does not move and the fix reads as
            // ineffective. Say which rows are already accounted for.
            format!(
                "; {} of these are benign (Option defaults, propagated `?`, logged \
                 fallbacks, infallible writes) and are hidden in `audit`",
                benign_shown
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

    fn effect_of(expr_src: &str) -> Effect {
        let e: syn::Expr = syn::parse_str(expr_src).expect("parse");
        classify_effect(&e)
    }

    /// The distinction the ranking exists to draw. Both of these are a
    /// discarded `Result`; only one of them can lose a payment.
    #[test]
    fn effect_separates_external_mutation_from_local_decode() {
        assert_eq!(
            effect_of(r#"sqlx::query("DELETE FROM stripe_events WHERE id = $1").bind(id).execute(&mut *tx).await"#),
            Effect::Mutation
        );
        assert_eq!(effect_of("std::fs::remove_dir_all(&self.dir)"), Effect::Mutation);
        assert_eq!(effect_of("client.send(&payload).await"), Effect::Mutation);

        assert_eq!(
            effect_of(r#"sqlx::query_scalar("SELECT 1").fetch_one(&db).await"#),
            Effect::Io
        );
        assert_eq!(effect_of("std::fs::read_to_string(path)"), Effect::Io);

        assert_eq!(effect_of("Uuid::from_slice(&scanned.snagpin_id)"), Effect::Decode);
        assert_eq!(effect_of("s.parse::<u32>()"), Effect::Decode);
        assert_eq!(effect_of("base64::decode(token)"), Effect::Decode);

        assert_eq!(effect_of("self.require_business(sponsor).await"), Effect::Unknown);
    }

    /// A chain that both reads and writes has written. `query(...).execute()`
    /// must not read as a plain query because `query` came first.
    #[test]
    fn mutation_outranks_io_within_one_chain() {
        assert_eq!(
            effect_of(r#"sqlx::query("UPDATE t SET a = 1").execute(&db).await"#),
            Effect::Mutation
        );
    }

    /// The closure is what runs *because* the thing failed. Classifying by it
    /// would let the fallback's verbs stand in for the effect's.
    #[test]
    fn handler_closures_do_not_contribute_effect() {
        // `.unwrap_or_else(|| fs::remove_dir_all(p))` — the receiver decodes,
        // the handler mutates. Only the receiver is passed in, but guard the
        // visitor directly too.
        let e: syn::Expr =
            syn::parse_str("foo.map(|x| std::fs::remove_dir_all(x))").expect("parse");
        assert_eq!(classify_effect(&e), Effect::Unknown);
    }

    fn hit(kind: &'static str, effect: Effect) -> Hit {
        Hit {
            kind,
            file: "f.rs".into(),
            line: 1,
            context: "f".into(),
            benign: None,
            effect,
        }
    }

    /// The row that was losing money must outrank the rows that were correct
    /// by design. This is the whole point of the score, stated as an ordering.
    #[test]
    fn discarded_mutation_outranks_deliberate_sanitization() {
        // `let _ = sqlx::query("DELETE …").execute(&db).await;`
        let webhook = hit("let-_", Effect::Mutation);
        // `Uuid::from_slice(b).map_err(|_| QrError::Malformed)?`
        let sanitize = hit(".map_err(|_|)", Effect::Decode);
        assert!(webhook.score() > sanitize.score());
        assert!(webhook.score() >= GATING_SCORE, "the defect must gate");
        assert!(
            sanitize.score() < GATING_SCORE,
            "collapsing crypto causes must not gate — it is correct and there \
             were 13 of them"
        );
    }

    /// `.map_err(|_|)` still propagates the failure, so it is the mildest
    /// swallow at equal effect. That is what put the sanitization family at the
    /// bottom without needing a special case for it.
    #[test]
    fn propagating_kinds_rank_below_vanishing_ones() {
        for e in [Effect::Mutation, Effect::Io, Effect::Decode, Effect::Unknown] {
            assert!(hit(".map_err(|_|)", e).score() < hit("let-_", e).score());
        }
    }

    /// An unrecognised call chain is the common case in a codebase with its own
    /// wrappers. Gating on it would rebuild the flat list.
    #[test]
    fn unknown_chains_do_not_gate() {
        assert!(hit("let-_", Effect::Unknown).score() < GATING_SCORE);
        assert!(hit(".unwrap_or_default", Effect::Io).score() < GATING_SCORE);
    }
}
