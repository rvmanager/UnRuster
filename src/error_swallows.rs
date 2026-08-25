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
    /// The fallback puts a value from *somewhere else* in place of the one that
    /// failed, rather than a type default. See [`fallback_substitutes`].
    substitutes: bool,
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
        // A site the benign classifier already cleared contributes no effect
        // risk: it has answered the question the effect term is asking.
        //
        // Without this, `let _ = write!(buf, "{c}")` into an in-memory `String`
        // scored 0.90 — `write` reads as a mutation — and led the standalone
        // list, above every real finding, while `audit` hid it as benign. The
        // two views disagreed about the single most important row, and the
        // standalone one is what `--suggest-waivers` is run against.
        let effect = if self.benign.is_some() {
            0.0
        } else {
            self.effect.weight()
        };
        // Third term: the fallback substituted a value from elsewhere.
        //
        // Added because the two swallows a 200-defect evaluation confirmed as
        // real bugs both scored *below* the gate — 0.40 and 0.35 — so the
        // ranking buried its own true positives. Both were the same shape:
        // `.unwrap_or_else(|_| dist.install_path.clone())`, where the fallback
        // is not a default but a *different value of the same type*. The
        // failure vanishes and the program carries on holding data that looks
        // valid, which is why the defect (an absolute path silently becoming a
        // relative one) survived review.
        //
        // Effect alone could not see it: the receiver was a project-specific
        // call, so it classified `Unknown` (0.20) and no combination of
        // `Unknown` with a fallback kind reaches 0.55. Raising `Unknown`
        // instead would have promoted every unrecognised chain in the tree.
        let substitution = if self.substitutes && self.benign.is_none() {
            SUBSTITUTION_WEIGHT
        } else {
            0.0
        };
        (effect + kind + substitution).min(1.0)
    }
}

/// Weight of the value-substitution term. Tuned to the boundary it exists to
/// cross: `.unwrap_or_else` (0.15) on an `Unknown` effect (0.20) is 0.35, and
/// 0.55 is the gate — so a substituting fallback on an unrecognised call gates,
/// and a defaulting one still does not.
const SUBSTITUTION_WEIGHT: f64 = 0.20;

/// What the discarded `Result` was reporting on — the single feature that
/// separated the real defects from the correct-by-design sites on the codebase
/// this was calibrated against.
///
/// Classified from the swallowed expression's call chain, so it is a
/// BEST-EFFORT signal: a project that wraps its database in `fn persist()`
/// reads as `Unknown`, not `Mutation`. It ranks, it does not adjudicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
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

    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Mutation => "mutation",
            Effect::Io => "io",
            Effect::Decode => "decode",
            Effect::Unknown => "unknown",
        }
    }
}

/// Verbs that are external state changes on sight — no other Rust API spells
/// them, so seeing one anywhere in the chain settles the question.
const STRONG_MUTATION_VERBS: &[&str] = &[
    "execute", "commit", "rollback", "publish", "upsert", "persist", "write_all", "flush",
    "sync_all", "sync_data", "set_permissions", "set_len", "create_dir", "create_dir_all",
    "remove_file", "remove_dir", "remove_dir_all", "hard_link", "symlink", "spawn", "kill",
    "commit_async", "bind_execute",
];

/// Verbs that name an external mutation *in an external context* and an
/// ordinary in-memory operation everywhere else.
///
/// This distinction is the difference between a check that gates on real
/// defects and one that gates on `HashMap::insert`. Treating these as external
/// on sight scored four idiomatic lines —
///
/// ```ignore
/// let _ = self.map.insert(k, v);      // returns the PREVIOUS value, an Option
/// let _ = self.seen.insert(k);        // returns bool
/// let _ = self.order.remove(0);       // returns T
/// self.rename(from, to).unwrap_or_else(…)   // an in-memory model edit
/// ```
///
/// — at 0.75–0.90, all four above the gate, on code with nothing wrong with it.
/// A weak verb needs [`IO_ROOTS`] corroboration in the same chain to count.
const WEAK_MUTATION_VERBS: &[&str] = &[
    "write", "send", "insert", "remove", "delete", "update", "create", "rename", "copy", "save",
    "store", "emit", "dispatch", "notify", "ack", "truncate", "wait",
];

/// Path segments and receiver names that mark a chain as reaching outside the
/// process. Presence of one promotes a [`WEAK_MUTATION_VERBS`] hit to
/// [`Effect::Mutation`]; absence leaves it `Unknown`.
///
/// Matched against every path segment and method receiver in the chain, so
/// `std::fs::write(…)` and `self.db.insert(…)` both qualify while
/// `self.map.insert(…)` does not.
const IO_ROOTS: &[&str] = &[
    "fs", "File", "OpenOptions", "Path", "PathBuf", "io", "net", "process", "Command", "sqlx",
    "diesel", "reqwest", "hyper", "client", "conn", "connection", "db", "pool", "socket", "stream",
    "writer", "sink", "producer", "publisher", "transaction", "session", "storage", "bucket",
    "s3", "redis", "cache_dir", "tokio",
    // `tx` and `channel` are deliberately absent. `tx` reads as "transaction"
    // but in Rust it is overwhelmingly an `mpsc::Sender`, and
    // `let _ = tx.send(v)` — whose failure means only that the receiver was
    // dropped — scored 0.90 and gated on a GUI codebase where the very next
    // line matched on `rx.recv()`. An in-process channel is not the outside.
];

/// Verbs that reach outside the process without changing it.
const IO_VERBS: &[&str] = &[
    "fetch", "fetch_one", "fetch_all", "fetch_optional", "query", "query_as", "query_scalar",
    "get", "post", "put", "patch", "head", "request", "call", "connect", "read", "read_to_string",
    "read_to_end", "read_dir", "load", "open", "metadata", "canonicalize", "lock", "acquire",
    "begin",
    // `recv` and `poll` are deliberately absent: `mpsc::Receiver::recv` and
    // `Future::poll` are in-process, and `wgpu::Device::poll` is a synchronous
    // wait. A socket read spells itself `read`/`read_to_end`, which stay.
];

/// Verbs that only reshape data the process already holds.
const DECODE_VERBS: &[&str] = &[
    "parse", "parse_str", "from_str", "from_slice", "from_bytes", "from_utf8", "decode",
    "deserialize", "try_into", "try_from", "into", "to_str", "as_str", "encode", "serialize",
    "to_string", "strip_prefix", "strip_suffix", "split_once", "from_hex", "to_vec",
];

/// Is `name` one of the conversion verbs, by the same rule
/// [`classify_effect`] uses?
///
/// Exposed for [`crate::panics`], which has to find *which* call in a chain
/// was the fallible conversion before it can ask where that conversion's input
/// came from. Sharing the predicate is the point: a provenance rule keyed off a
/// second, hand-copied verb list would go quiet the moment either list moved.
pub fn names_a_decode_verb(name: &str) -> bool {
    verb_matches(name, DECODE_VERBS)
}

/// As [`names_a_decode_verb`], plus the IO verbs.
///
/// The question it answers is "could this call have brought data in from
/// outside the process" — asked of a local fn before its return type is trusted
/// as in-process.
pub fn names_a_decode_or_io_verb(name: &str) -> bool {
    verb_matches(name, DECODE_VERBS) || verb_matches(name, IO_VERBS)
}

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
pub fn classify_effect(expr: &syn::Expr) -> Effect {
    struct V {
        strong: bool,
        weak: bool,
        io_root: bool,
        io: bool,
        decode: bool,
    }
    impl V {
        fn note(&mut self, name: &str) {
            if verb_matches(name, STRONG_MUTATION_VERBS) {
                self.strong = true;
            } else if verb_matches(name, WEAK_MUTATION_VERBS) {
                self.weak = true;
            } else if verb_matches(name, IO_VERBS) {
                self.io = true;
            } else if verb_matches(name, DECODE_VERBS) {
                self.decode = true;
            }
        }
        /// Any path segment or receiver name that says "this leaves the
        /// process". Checked separately from the verb so `std::fs::write` and
        /// `map.write` are told apart.
        fn note_root(&mut self, name: &str) {
            if IO_ROOTS.contains(&name) {
                self.io_root = true;
            }
        }
    }
    /// `.create(true)` / `.append(true)` / `.write(true)` — a builder flag, not
    /// an action.
    ///
    /// `OpenOptions::new().create(true).append(true).open(p)` names two weak
    /// mutation verbs and an IO root, so it classified as an external mutation
    /// and gated — on a fn whose whole job is to *try* to open a log. The
    /// action in that chain is `open`; `create` and `append` are arguments to
    /// it that happen to be spelled as methods. A single boolean literal is the
    /// tell: a real `write` takes bytes, a configuring `write` takes `true`.
    fn is_builder_flag(c: &syn::ExprMethodCall) -> bool {
        c.args.len() == 1
            && matches!(
                c.args.first(),
                Some(syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Bool(_),
                    ..
                }))
            )
    }

    impl<'ast> Visit<'ast> for V {
        fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
            if !is_builder_flag(c) {
                self.note(&c.method.to_string());
            }
            // `self.db.insert(…)` — the receiver names the destination.
            if let syn::Expr::Field(f) = &*c.receiver {
                if let syn::Member::Named(n) = &f.member {
                    self.note_root(&n.to_string());
                }
            }
            if let syn::Expr::Path(p) = &*c.receiver {
                for seg in &p.path.segments {
                    self.note_root(&seg.ident.to_string());
                }
            }
            visit::visit_expr_method_call(self, c);
        }
        fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
            if let syn::Expr::Path(p) = &*c.func {
                if let Some(seg) = p.path.segments.last() {
                    self.note(&seg.ident.to_string());
                }
                // `std::fs::write(…)` — every segment but the verb itself.
                for seg in p.path.segments.iter().rev().skip(1) {
                    self.note_root(&seg.ident.to_string());
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
        strong: false,
        weak: false,
        io_root: false,
        io: false,
        decode: false,
    };
    v.visit_expr(expr);
    // A weak verb with no external context is left `Unknown` rather than
    // promoted: at 0.20 it lands below the gate, so it is still reported and
    // still ranked above a plain decode, but it does not hold the loop open.
    if v.strong || (v.weak && v.io_root) {
        Effect::Mutation
    } else if v.io {
        Effect::Io
    } else if v.weak {
        // A mutation verb we could not place. Not `Decode` — something was
        // being changed — but not evidence of an external effect either.
        Effect::Unknown
    } else if v.decode {
        Effect::Decode
    } else {
        Effect::Unknown
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
        self.record_tagged(kind, line, None, swallowed, false);
    }

    /// `swallowed` is the expression whose `Result` is being dropped — the
    /// method receiver, the `let` initialiser, the match scrutinee. It is the
    /// only thing that distinguishes a discarded DELETE from a discarded
    /// base64 decode, so every record path has to supply it.
    ///
    /// `substitutes` is the fallback's verdict from [`fallback_substitutes`];
    /// only the two `.unwrap_or*` paths can answer it, and every other record
    /// path passes false because it produces no replacement value.
    fn record_tagged(
        &mut self,
        kind: &'static str,
        line: usize,
        benign: Option<&'static str>,
        swallowed: &syn::Expr,
        substitutes: bool,
    ) {
        let ctx = self.enclosing();
        self.hits.push(Hit {
            kind,
            file: self.file.to_string(),
            line,
            context: ctx,
            benign,
            effect: classify_effect(swallowed),
            substitutes,
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

/// Does this fallback substitute a value from elsewhere, rather than fall back
/// to a default?
///
/// The distinction the [`Hit::score`] substitution term turns on:
///
/// ```ignore
/// .unwrap_or_default()                                  // default — no
/// .unwrap_or_else(|_| String::new())                     // default — no
/// .unwrap_or(0)                                          // default — no
/// .unwrap_or_else(|_| dist.install_path.clone())         // substitution — yes
/// .unwrap_or_else(|_| self.fallback_index())             // substitution — yes
/// ```
///
/// A default says "there was nothing"; a substitution says "there was
/// *this* instead", and downstream code cannot tell the difference. Answers
/// BEST-EFFORT: it reads the fallback's shape, not its meaning, so a `fn
/// empty_path()` helper reads as a substitution.
fn fallback_substitutes(kind: &str, e: &syn::ExprMethodCall) -> bool {
    // `.unwrap_or_default()` is the default by construction; `.ok`/`.err`/
    // `.map_err` produce no replacement value at all.
    let expr = match kind {
        ".unwrap_or" => e.args.first(),
        ".unwrap_or_else" => match e.args.first() {
            Some(syn::Expr::Closure(c)) => {
                // A fallback built *out of the error* is not a substitution:
                // it is the "inspects" tier of the `divergence --handling`
                // care scale, one step more careful than a bare default.
                // `.unwrap_or_else(|e| e.into_inner())` — the poisoned-lock
                // recovery idiom — is this shape, and without the exemption
                // the substitution term promoted every one of them.
                if closure_uses_its_error(c) {
                    return false;
                }
                Some(&*c.body)
            }
            other => other,
        },
        _ => None,
    };
    let Some(expr) = expr else { return false };
    expr_is_substitute(expr)
}

/// Does this fallback closure read the error it was handed?
///
/// The parameter has to be a real binding (`|e|`, not `|_|` or `||`) and the
/// body has to mention it. A closure that names its error and never uses it is
/// discarding it just as completely as `|_|`.
fn closure_uses_its_error(c: &syn::ExprClosure) -> bool {
    let Some(syn::Pat::Ident(p)) = c.inputs.first() else {
        return false;
    };
    let name = p.ident.to_string();
    if name.starts_with('_') {
        return false;
    }
    struct Uses<'a> {
        name: &'a str,
        found: bool,
    }
    impl<'ast> Visit<'ast> for Uses<'_> {
        fn visit_ident(&mut self, i: &'ast syn::Ident) {
            if i == self.name {
                self.found = true;
            }
        }
    }
    let mut v = Uses {
        name: &name,
        found: false,
    };
    v.visit_expr(&c.body);
    v.found
}

/// True when `e` names existing program state rather than an empty value.
fn expr_is_substitute(e: &syn::Expr) -> bool {
    match peel_grouping(e) {
        // A block's answer is its tail expression; a block with no tail
        // (`{ log!(…); }`) yields `()` and replaces nothing.
        syn::Expr::Block(b) => match b.block.stmts.last() {
            Some(syn::Stmt::Expr(tail, None)) => expr_is_substitute(tail),
            _ => false,
        },
        // Literals, `None`, empty collections and `Default::default()` are the
        // "there was nothing" family — and so is anything built entirely out of
        // literals, however many calls it takes to spell: `"/tmp".to_string()`
        // is a constant, not a value from elsewhere.
        _ if crate::ast::is_literal_only(e) => false,
        syn::Expr::Lit(_) => false,
        syn::Expr::Array(a) => !a.elems.is_empty(),
        syn::Expr::Tuple(t) => !t.elems.is_empty(),
        syn::Expr::Unary(u) => expr_is_substitute(&u.expr),
        syn::Expr::Reference(r) => expr_is_substitute(&r.expr),
        syn::Expr::Path(p) => {
            let last = p.path.segments.last().map(|s| s.ident.to_string());
            !matches!(last.as_deref(), Some("None"))
        }
        syn::Expr::Call(c) => {
            // `String::new()`, `Vec::new()`, `Default::default()`,
            // `HashMap::with_capacity(0)` — constructors of the empty value.
            // With arguments, a call is building something out of live data.
            if !c.args.is_empty() {
                return true;
            }
            let syn::Expr::Path(p) = peel_grouping(&c.func) else {
                return true;
            };
            let last = p.path.segments.last().map(|s| s.ident.to_string());
            !matches!(last.as_deref(), Some("new") | Some("default"))
        }
        // A method call, a field read, an index: all of them reach for a value
        // the program already has.
        syn::Expr::MethodCall(_)
        | syn::Expr::Field(_)
        | syn::Expr::Index(_)
        | syn::Expr::Binary(_)
        | syn::Expr::Try(_)
        | syn::Expr::Await(_) => true,
        // Anything unrecognised: no claim. The term only ever adds score, so
        // the conservative answer is the one that adds none.
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
            self.record_tagged(
                k,
                line_of(&e.method),
                benign,
                &e.receiver,
                fallback_substitutes(k, e),
            );
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
                    self.record_tagged("if-let-ok", line_of(&e.if_token), benign, &le.expr, false);
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
                self.record_tagged("let-_", line_of(&l.let_token), benign, &init.expr, false);
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
    /// Drop rows scoring below this. 0.0 keeps everything.
    ///
    /// This check ranks its own rows and gates on the top tier, and was the
    /// only ranked check in the tool with no way to ask for that tier: the
    /// sibling drift checks all take `--min-score`, and the answer here was
    /// "read all 665 rows, or pipe them through `awk`".
    pub min_score: f64,
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
            min_score: 0.0,
        }
    }
}

pub fn run(ctx: &AnalysisCtx, opts: SwallowOpts) -> anyhow::Result<usize> {
    Ok(run_counted(ctx, opts)?.total)
}

/// As [`run`], but also reporting how many rows clear [`GATING_SCORE`] — the
/// split `audit` gates on. Every row is still printed; the tier only decides
/// which ones hold the loop open.
pub fn run_counted(
    ctx: &AnalysisCtx,
    opts: SwallowOpts,
) -> anyhow::Result<Counts> {
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
    // The tier `audit` gates on is applied below — after this retain, because a
    // suppressed row must not be counted at all. Telling the ledger which side
    // of it each hit falls on is what makes `hits` mean "suppressed something
    // the audit battery would have gated on", which is what the column claims.
    let waived = ctx.retain_unsuppressed_tiered(
        "error-swallows",
        &mut all,
        |h| crate::suppress::Site::keyed(h.file.as_str(), h.line, h.kind),
        |h| {
            let kept = match h.benign {
                Some("infallible-write") => opts.include_infallible,
                Some("logged-fallback") | Some("combinator-ok") | Some("propagated")
                | Some("option-default") | Some("fallthrough-is-handler") => opts.include_logged,
                _ => true,
            };
            kept && h.score() >= opts.min_score && h.score() >= GATING_SCORE
        },
    );
    let before = all.len();
    all.retain(|h| match h.benign {
        Some("infallible-write") => opts.include_infallible,
        Some("logged-fallback") | Some("combinator-ok") | Some("propagated")
        | Some("option-default") | Some("fallthrough-is-handler") => opts.include_logged,
        _ => true,
    });
    let benign_hidden = before - all.len();
    // Before the counts are taken: `--min-score` is a different question from
    // `--top`. A cap says "show me fewer of these"; a floor says "these are
    // not findings", so the summary must not go on counting them.
    let below_floor = if opts.min_score > 0.0 {
        let n = all.len();
        all.retain(|h| h.score() >= opts.min_score);
        n - all.len()
    } else {
        0
    };
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
    // Counts describe the whole result set; the cap only bounds what is
    // listed. Taken before truncating so `--top 5` still reports "42 sites".
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
    let total = all.len();
    if !summary {
        let today = crate::suppress::Date::today();
        for h in &all {
            row!(
                ctx.out,
                "kind" => h.kind,
                "score" => format!("{:.2}", h.score()),
                "effect" => h.effect.as_str(),
                "in_fn" => h.context.clone(),
                "at" => site(&h.file, h.line),
            );
            ctx.suggest("error-swallows", Some(h.kind), today, (&h.file, h.line));
        }
    }
    let substituting = all.iter().filter(|h| h.substitutes && h.benign.is_none()).count();
    ctx.out.summary(&format!(
        "({} swallow site(s){}{}{}; {}; include_unwrap_or={}{}{}; explain: silent-fallbacks)",
        total,
        if top_tier > 0 {
            format!(
                ", {} at score >= {:.2} (discarded external effects — the tier \
                 `audit` gates on)",
                top_tier, GATING_SCORE
            )
        } else {
            String::new()
        },
        // Named because it is the term that moved a row above the gate, and a
        // reader comparing two `unknown`-effect rows has no other way to see
        // why one outranks the other.
        if substituting > 0 {
            format!(
                ", {} whose fallback substitutes another value rather than a default",
                substituting
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

    /// The regression this whole strong/weak split exists to prevent.
    ///
    /// Every one of these is idiomatic Rust over in-memory state, none is even
    /// fallible — `HashMap::insert` returns the previous value, `HashSet::insert`
    /// a bool, `Vec::remove` a `T` — and all four were scored as external
    /// mutations at 0.75–0.90 and *gated* `audit`, holding an agent loop open
    /// on correct code.
    #[test]
    fn in_memory_mutations_are_not_external_effects() {
        for src in [
            "self.map.insert(k, v)",
            "self.seen.insert(k)",
            "self.order.remove(0)",
            "self.rename(from, to)",
            "buf.write(bytes)",
            "list.update(i, v)",
        ] {
            assert_eq!(effect_of(src), Effect::Unknown, "{src}");
            assert!(
                hit("let-_", effect_of(src)).score() < GATING_SCORE,
                "{src} must not gate"
            );
        }
    }

    /// The other direction: an external mutation still has to be unmistakable,
    /// whether it is named by a strong verb or by a weak one in an `fs`/db
    /// context.
    #[test]
    fn external_mutations_still_classify_and_gate() {
        for src in [
            "std::fs::write(p, b)",
            "std::fs::rename(a, b)",
            "std::fs::remove_dir_all(&dir)",
            "f.write_all(bytes)",
            "f.flush()",
            "sqlx::query(\"DELETE FROM t\").bind(id).execute(&mut *tx).await",
            "self.db.insert(id, row)",
        ] {
            assert_eq!(effect_of(src), Effect::Mutation, "{src}");
            assert!(
                hit("let-_", effect_of(src)).score() >= GATING_SCORE,
                "{src} must gate"
            );
        }
    }

    /// A site the benign classifier already cleared must not lead the list.
    /// `let _ = write!(buf, …)` into a `String` scored 0.90 and outranked every
    /// real finding in the standalone view, while `audit` hid it entirely.
    #[test]
    fn benign_sites_sort_below_real_ones() {
        let benign = Hit {
            benign: Some("infallible-write"),
            ..hit("let-_", Effect::Mutation)
        };
        let real = hit(".unwrap_or_default", Effect::Io);
        assert!(benign.score() < real.score());
        assert!(benign.score() < GATING_SCORE, "a benign site must not gate");
    }

    /// In-process plumbing is not the outside world.
    ///
    /// All five gating rows on one real GUI codebase were this shape, and all
    /// five were false positives: a channel send whose result the next line
    /// matches on, a `Device::poll` that is a synchronous wait, and an
    /// `OpenOptions` builder whose `create(true)` flag read as an action.
    #[test]
    fn in_process_plumbing_is_not_external() {
        for src in [
            "tx.send(value)",
            "rx.recv()",
            "device.poll(Maintain::Wait)",
            "future.poll(cx)",
        ] {
            assert_eq!(effect_of(src), Effect::Unknown, "{src}");
            assert!(
                hit("let-_", effect_of(src)).score() < GATING_SCORE,
                "{src} must not gate"
            );
        }
    }

    /// A builder flag is an argument, not an action. `.create(true)` and
    /// `.append(true)` are how you *describe* an open, and reading them as two
    /// mutation verbs gated a fn whose whole job is to try to open a log file.
    #[test]
    fn builder_flags_are_not_actions() {
        assert_eq!(
            effect_of(r#"std::fs::OpenOptions::new().create(true).append(true).open(p)"#),
            Effect::Io,
            "the action in that chain is `open`, not `create`"
        );
        // A real write still takes bytes, and still counts.
        assert_eq!(effect_of("f.write(buf)"), Effect::Unknown);
        assert_eq!(effect_of("f.write_all(buf)"), Effect::Mutation);
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
            substitutes: false,
        }
    }

    /// A fallback that hands downstream code a *different value* rather than a
    /// default. Both swallows a 200-defect evaluation confirmed as real bugs
    /// were this shape and both scored below the gate.
    fn substituting_hit(kind: &'static str, effect: Effect) -> Hit {
        Hit {
            substitutes: true,
            ..hit(kind, effect)
        }
    }

    fn call(src: &str) -> syn::ExprMethodCall {
        match syn::parse_str::<syn::Expr>(src).expect("parse") {
            syn::Expr::MethodCall(c) => c,
            other => panic!("not a method call: {:?}", other),
        }
    }

    /// The regression the substitution term exists for: uv PR #18176 replaced
    /// `.unwrap_or_else(|_| dist.install_path.clone())`, which quietly turned an
    /// absolute lockfile path into a relative one. It scored 0.35 — below the
    /// gate — so `audit` ranked its own true positive into the tail.
    #[test]
    fn a_substituting_fallback_on_an_unknown_call_gates() {
        let defect = substituting_hit(".unwrap_or_else", Effect::Unknown);
        assert!(
            defect.score() >= GATING_SCORE,
            "score {:.2} must gate",
            defect.score()
        );
        // …and the same site falling back to a default still must not, or the
        // term has promoted the whole family rather than one shape.
        assert!(hit(".unwrap_or_else", Effect::Unknown).score() < GATING_SCORE);
    }

    #[test]
    fn defaults_are_not_substitutions() {
        for src in [
            "x.unwrap_or_else(|_| String::new())",
            "x.unwrap_or_else(|_| Vec::new())",
            "x.unwrap_or_else(|_| Default::default())",
            "x.unwrap_or(0)",
            r#"x.unwrap_or("")"#,
            "x.unwrap_or_else(|_| None)",
            // A block whose tail is not an expression yields `()`.
            "x.unwrap_or_else(|_| { log(); })",
            // Built out of the error: the `divergence --handling` scale calls
            // this "inspects", one step more careful than a default. The
            // poisoned-lock recovery idiom is this shape.
            "x.unwrap_or_else(|e| e.into_inner())",
            "x.unwrap_or_else(|e| Recovered::from(e))",
        ] {
            assert!(
                !fallback_substitutes(".unwrap_or_else", &call(src))
                    && !fallback_substitutes(".unwrap_or", &call(src)),
                "{} is a default, not a substitution",
                src
            );
        }
    }

    #[test]
    fn values_from_elsewhere_are_substitutions() {
        for src in [
            "x.unwrap_or_else(|_| dist.install_path.clone())",
            "x.unwrap_or_else(|_| self.fallback_index())",
            "x.unwrap_or_else(|_| other[0])",
            "x.unwrap_or_else(|_| { warn(); previous.clone() })",
            // A closure that names its error and ignores it anyway.
            "x.unwrap_or_else(|e| cached.clone())",
        ] {
            assert!(
                fallback_substitutes(".unwrap_or_else", &call(src)),
                "{} substitutes a value",
                src
            );
        }
        assert!(fallback_substitutes(".unwrap_or", &call("x.unwrap_or(cached)")));
        // The kinds that produce no replacement value cannot substitute.
        assert!(!fallback_substitutes(".ok", &call("x.ok()")));
        assert!(!fallback_substitutes(
            ".unwrap_or_default",
            &call("x.unwrap_or_default()")
        ));
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
