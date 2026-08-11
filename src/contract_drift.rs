//! `contract-drift` — compare an implementation against what its callers assume.
//!
//! Every other check in this tool is *horizontal*: it compares siblings to each
//! other. This one is vertical — one function's implementation against the
//! aggregate expectation of everything that calls it — and it is the one
//! command where the tool is not the analyst. It assembles the material for a
//! reader (human or agent) in two phases and refuses to hand over the second
//! during the first:
//!
//! 1. the callers, with the signature but **not** the body or the doc comment;
//! 2. `--reveal`: the doc comment, the body, and the callee list.
//!
//! The withholding is the product. Contamination is the failure mode and it is
//! silent — an expectation written after reading the implementation is not
//! evidence of anything, and nothing in the output would show it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::ast::{last_segment, path_to_string, scope_visits, ScopeTracker};
use crate::callers::pat_idents;
use crate::context::{AnalysisCtx, Confidence, TargetNotFound};
use crate::emit::{site, span_site, Format, Val};
use crate::index::Defn;
use crate::parse::display_path;

/// Per-caller source lines. Lower than `show`'s 240 because this command prints
/// one body *per caller*: ten callers at `show`'s budget is 2400 lines, which is
/// not a dossier, it is a file dump.
const DEFAULT_MAX_LINES: usize = 80;

/// Callers listed before `--top` has to be passed explicitly. Ten whole fn
/// bodies is already a long read; past that the reader is skimming, and a
/// skimmed caller set produces a confidently wrong contract.
const DEFAULT_TOP: usize = 10;

pub struct ContractOpts {
    pub reveal: bool,
    pub candidates: bool,
    pub no_bodies: bool,
    pub max_lines: Option<usize>,
    pub min_callers: usize,
    pub min_confidence: Option<Confidence>,
    pub top: Option<usize>,
}

// ---------------------------------------------------------------------------
// Evidence vocabulary
// ---------------------------------------------------------------------------

/// What the caller does with the return value — the single highest-signal fact
/// about an expectation, because it says what the caller believes *can* go
/// wrong. A site that discards the result asserts the call is infallible; the
/// implementation may disagree.
///
/// One per site, and the outermost construct wins: in `f()?` the `?` is the
/// disposition, not the `let` that binds it.
#[derive(Debug, Clone)]
struct Disp {
    kind: &'static str,
    detail: Option<String>,
}

impl Disp {
    fn bare(kind: &'static str) -> Self {
        Self { kind, detail: None }
    }
    fn with(kind: &'static str, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: Some(detail.into()),
        }
    }
    /// `kind` or `kind:detail` — the `ret` cell, and the key the usage table
    /// aggregates on.
    fn label(&self) -> String {
        match &self.detail {
            Some(d) => format!("{}:{}", self.kind, d),
            None => self.kind.to_string(),
        }
    }
}

/// The shape of one argument at one call site. A parameter that is `literal` or
/// `default` at *every* site is a parameter the implementation may treat as
/// more variable than it is.
#[derive(Debug, Clone)]
struct Arg {
    shape: &'static str,
    text: String,
}

/// One call site of the target, with everything a reader needs to infer the
/// contract without opening the implementation.
#[derive(Debug, Clone)]
struct Site {
    file: String,
    line: usize,
    /// Position of the *name* being called. A method call's expression starts
    /// at its receiver, which every call in a chain shares, so the receiver's
    /// span cannot key one call in `a.f().g()`.
    key: (usize, usize),
    caller: String,
    caller_span: Option<(usize, usize)>,
    module: String,
    target: String,
    target_resolved: Option<String>,
    shadowed: bool,
    /// The receiver was literally `self` — enough to attribute `self.push(…)`
    /// inside the defining impl without inferring a type.
    receiver_is_self: bool,
    ret: Disp,
    args: Vec<Arg>,
    env: Vec<String>,
}

impl Site {
    fn args_cell(&self) -> String {
        if self.args.is_empty() {
            return "—".to_string();
        }
        self.args
            .iter()
            .map(|a| a.shape)
            .collect::<Vec<_>>()
            .join(", ")
    }
    fn env_cell(&self) -> String {
        if self.env.is_empty() {
            "—".to_string()
        } else {
            self.env.join(",")
        }
    }
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

struct ContractVisitor<'a> {
    file: &'a str,
    query: &'a str,
    scope: ScopeTracker,
    sites: Vec<Site>,
    /// Disposition per call key, written by whichever parent construct claims
    /// the call. Outermost writer wins (`or_insert`): `visit_expr_while` runs
    /// before the `visit_expr_let` inside its condition.
    disp: HashMap<(usize, usize), Disp>,
    /// Call keys a preceding `if`/`assert!` in the same block guards, with the
    /// guard's rendered condition.
    guards: HashMap<(usize, usize), String>,
    locals: Vec<BTreeSet<String>>,
    pending_params: BTreeSet<String>,
    loop_depth: usize,
    cond_depth: usize,
}

impl ContractVisitor<'_> {
    /// The key identifying `e` if it is a call to the target, `None` otherwise.
    fn target_key(&self, e: &syn::Expr) -> Option<(usize, usize)> {
        let (target, key) = call_of(e)?;
        crate::callers::matches_target(&target, self.query).then_some(key)
    }

    fn claim(&mut self, e: &syn::Expr, d: Disp) {
        if let Some(k) = self.target_key(e) {
            self.disp.entry(k).or_insert(d);
        }
    }

    fn record(&mut self, target: String, key: (usize, usize), args: Vec<Arg>, shadowed: bool) {
        self.record_full(target, key, args, shadowed, false)
    }

    fn record_full(
        &mut self,
        target: String,
        key: (usize, usize),
        args: Vec<Arg>,
        shadowed: bool,
        on_self: bool,
    ) {
        let mut env = Vec::new();
        if self.loop_depth > 0 {
            env.push("loop".to_string());
        }
        if self.cond_depth > 0 {
            env.push("cond".to_string());
        }
        self.sites.push(Site {
            file: self.file.to_string(),
            line: key.0,
            key,
            caller: self.scope.enclosing_with_toplevel(),
            caller_span: self.scope.fn_span(),
            module: crate::config_drift::module_of(&self.scope.enclosing_with_toplevel()).to_string(),
            target,
            target_resolved: None,
            shadowed,
            receiver_is_self: on_self,
            ret: Disp::bare("bare"),
            args,
            env,
        });
    }

    fn local_shadows(&self, name: &str) -> bool {
        self.locals.iter().any(|frame| frame.contains(name))
    }

    /// The target handed to another call *as a value* rather than invoked:
    /// `.map(arg_shape)`. There is no call expression, so nothing recorded it
    /// and the command answered "0 caller(s)" for a fn with two real uses.
    ///
    /// It is a caller in the sense that matters here — the combinator names
    /// exactly what it expects of the fn, which is contract evidence a plain
    /// call site does not carry.
    fn record_fn_ref(&mut self, a: &syn::Expr, consumer: &str) {
        let Some(path) = crate::callers::fn_ref_path(a) else {
            return;
        };
        if !crate::callers::matches_target(&path, self.query)
            && !crate::callers::matches_target(&format!("::{}", path), self.query)
        {
            return;
        }
        let key = pos(a);
        self.record(path, key, Vec::new(), false);
        self.disp.entry(key).or_insert(Disp::with("fn-ref", consumer));
    }
}

/// `(target, key)` for a call-shaped expression: a free call, a method call, or
/// a macro invocation. The key is the position of the callee's *name*, which is
/// what makes it unique within a chain.
fn call_of(e: &syn::Expr) -> Option<(String, (usize, usize))> {
    match peel(e) {
        syn::Expr::Call(c) => match &*c.func {
            syn::Expr::Path(p) => Some((path_to_string(&p.path), pos(&p.path))),
            _ => None,
        },
        syn::Expr::MethodCall(m) => Some((format!(".{}", m.method), pos(&m.method))),
        syn::Expr::Macro(m) => {
            let last = m.mac.path.segments.last()?;
            Some((format!("{}!", path_to_string(&m.mac.path)), pos(&last.ident)))
        }
        _ => None,
    }
}

/// Strip the wrappers that do not change *which* call an expression is:
/// parentheses, grouping, and a leading `&`/`&mut`.
fn peel(e: &syn::Expr) -> &syn::Expr {
    match e {
        syn::Expr::Paren(p) => peel(&p.expr),
        syn::Expr::Group(g) => peel(&g.expr),
        syn::Expr::Reference(r) => peel(&r.expr),
        other => other,
    }
}

fn pos<T: Spanned>(t: &T) -> (usize, usize) {
    let s = t.span().start();
    (s.line, s.column)
}

/// Classify one argument. `render_const` decides what counts as constant-shaped
/// — reused rather than re-decided, so `config-drift` and this command agree on
/// what a literal is.
fn arg_shape(e: &syn::Expr) -> Arg {
    let inner = peel(e);
    if let Some(text) = crate::config_drift::render_const(inner) {
        let shape = if is_defaultish(&text) {
            "default"
        } else if matches!(inner, syn::Expr::Lit(_)) {
            "literal"
        } else {
            "const"
        };
        return Arg { shape, text };
    }
    match inner {
        syn::Expr::MethodCall(m) if m.method == "default" || m.method == "new" => Arg {
            shape: "default",
            text: expr_label(inner),
        },
        syn::Expr::Call(_) | syn::Expr::MethodCall(_) | syn::Expr::Macro(_) => Arg {
            shape: "call",
            text: call_of(inner).map(|(t, _)| t).unwrap_or_default(),
        },
        syn::Expr::Field(_) => Arg {
            shape: "field",
            text: expr_label(inner),
        },
        _ => Arg {
            shape: "var",
            text: expr_label(inner),
        },
    }
}

/// Constant spellings that mean "I have nothing to say about this parameter".
fn is_defaultish(text: &str) -> bool {
    matches!(
        text,
        "None" | "\"\"" | "0" | "0.0" | "false" | "[]" | "()" | "Default::default()"
    ) || text.ends_with("::default()")
        || text.ends_with("::new()")
}

/// A short, readable spelling of an expression for the `detail` column. Not a
/// round-trip: paths, fields and calls render exactly, everything else renders
/// as its kind, because a token-stream dump of an arbitrary expression is
/// noise in a table cell.
fn expr_label(e: &syn::Expr) -> String {
    match peel(e) {
        syn::Expr::Path(p) => path_to_string(&p.path),
        syn::Expr::Field(f) => match &f.member {
            syn::Member::Named(n) => format!("{}.{}", expr_label(&f.base), n),
            syn::Member::Unnamed(i) => format!("{}.{}", expr_label(&f.base), i.index),
        },
        syn::Expr::MethodCall(m) => format!("{}.{}(…)", expr_label(&m.receiver), m.method),
        syn::Expr::Call(c) => format!("{}(…)", expr_label(&c.func)),
        syn::Expr::Macro(m) => format!("{}!(…)", path_to_string(&m.mac.path)),
        syn::Expr::Index(i) => format!("{}[…]", expr_label(&i.expr)),
        syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Deref(_)) => {
            format!("*{}", expr_label(&u.expr))
        }
        syn::Expr::Try(t) => format!("{}?", expr_label(&t.expr)),
        syn::Expr::Lit(l) => crate::config_drift::render_const(e)
            .unwrap_or_else(|| format!("{:?}", l.lit.span().start().line)),
        syn::Expr::Binary(_) => "<expr>".to_string(),
        _ => "<expr>".to_string(),
    }
}

/// Every identifier an expression mentions — the guard test's vocabulary,
/// matched against the argument names at a later call in the same block.
fn idents_of(e: &syn::Expr, out: &mut BTreeSet<String>) {
    struct V<'a>(&'a mut BTreeSet<String>);
    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_ident(&mut self, i: &'ast proc_macro2::Ident) {
            self.0.insert(i.to_string());
        }
    }
    V(out).visit_expr(e);
}

/// Method names that say what the caller believes about a `Result`/`Option`.
fn method_disp(name: &str) -> Option<Disp> {
    Some(match name {
        "unwrap" => Disp::bare("unwrap"),
        "expect" => Disp::bare("expect"),
        "ok" => Disp::bare("ok"),
        "err" => Disp::bare("err"),
        "unwrap_or" => Disp::bare("unwrap_or"),
        "unwrap_or_default" => Disp::bare("unwrap_or_default"),
        "unwrap_or_else" => Disp::bare("unwrap_or_else"),
        "map_err" => Disp::bare("map_err"),
        "is_ok" | "is_err" | "is_some" | "is_none" => Disp::bare("tested"),
        other => Disp::with("chained", other),
    })
}

impl<'ast> Visit<'ast> for ContractVisitor<'_> {
    scope_visits!(item_mod, item_impl, item_fn, impl_item_fn, trait_item_fn);

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func {
            let target = path_to_string(&p.path);
            if crate::callers::matches_target(&target, self.query) {
                let shadowed = p.path.segments.len() == 1 && self.local_shadows(&target);
                let args = e.args.iter().map(arg_shape).collect();
                self.record(target, pos(&p.path), args, shadowed);
            }
        }
        // The target passed *into* another call: the caller trusts the result
        // enough to hand it straight on, without inspecting it.
        let callee = call_of(&syn::Expr::Call(e.clone())).map(|(t, _)| t);
        for a in &e.args {
            self.record_fn_ref(a, callee.as_deref().unwrap_or("?"));
            if let Some(name) = &callee {
                self.claim(a, Disp::with("arg", name.clone()));
            }
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        let target = format!(".{}", e.method);
        if crate::callers::matches_target(&target, self.query) {
            let args = e.args.iter().map(arg_shape).collect();
            let on_self = matches!(crate::ast::peel_grouping(&e.receiver), syn::Expr::Path(p) if p.path.is_ident("self"));
            self.record_full(target, pos(&e.method), args, false, on_self);
        }
        // `f(…).unwrap()` — the method applied to the target *is* the
        // disposition, and the one that says most about the expectation.
        if let Some(d) = method_disp(&e.method.to_string()) {
            self.claim(&e.receiver, d);
        }
        let name = format!(".{}", e.method);
        for a in &e.args {
            // `xs.and_then(|e| f(e))` — the target is the closure's value, and
            // *which* combinator consumes it is the expectation: `and_then`
            // says the caller propagates `None`, `map_or` says it substitutes.
            // Without this the commonest chaining shape in the language landed
            // on `bare`.
            if let syn::Expr::Closure(c) = peel(a) {
                self.claim(&c.body, Disp::with("closure", e.method.to_string()));
            }
            self.record_fn_ref(a, &name);
            self.claim(a, Disp::with("arg", name.clone()));
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_try(&mut self, e: &'ast syn::ExprTry) {
        self.claim(&e.expr, Disp::bare("?"));
        visit::visit_expr_try(self, e);
    }

    fn visit_expr_match(&mut self, e: &'ast syn::ExprMatch) {
        self.claim(&e.expr, Disp::with("match", e.arms.len().to_string()));
        // An arm body is the match's value. Without this the commonest
        // dispatch shape in Rust — `Expr::Paren(p) => f(&p.expr)` — fell
        // through to `bare`, which says nothing at all.
        for a in &e.arms {
            self.claim(&a.body, Disp::bare("arm-value"));
        }
        visit::visit_expr_match(self, e);
    }

    fn visit_arm(&mut self, a: &'ast syn::Arm) {
        self.cond_depth += 1;
        visit::visit_arm(self, a);
        self.cond_depth -= 1;
    }

    fn visit_expr_let(&mut self, e: &'ast syn::ExprLet) {
        self.claim(&e.expr, Disp::bare("if-let"));
        visit::visit_expr_let(self, e);
    }

    fn visit_expr_while(&mut self, e: &'ast syn::ExprWhile) {
        if let syn::Expr::Let(l) = &*e.cond {
            self.claim(&l.expr, Disp::bare("while-let"));
        }
        self.loop_depth += 1;
        visit::visit_expr_while(self, e);
        self.loop_depth -= 1;
    }

    fn visit_expr_for_loop(&mut self, e: &'ast syn::ExprForLoop) {
        self.loop_depth += 1;
        visit::visit_expr_for_loop(self, e);
        self.loop_depth -= 1;
    }

    fn visit_expr_loop(&mut self, e: &'ast syn::ExprLoop) {
        self.loop_depth += 1;
        visit::visit_expr_loop(self, e);
        self.loop_depth -= 1;
    }

    fn visit_expr_if(&mut self, e: &'ast syn::ExprIf) {
        if !matches!(&*e.cond, syn::Expr::Let(_)) {
            self.claim(&e.cond, Disp::bare("cond-test"));
        }
        self.cond_depth += 1;
        visit::visit_expr_if(self, e);
        self.cond_depth -= 1;
    }

    fn visit_expr_return(&mut self, e: &'ast syn::ExprReturn) {
        if let Some(v) = &e.expr {
            self.claim(v, Disp::bare("returned"));
        }
        visit::visit_expr_return(self, e);
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        let ty = last_segment(&path_to_string(&e.path)).to_string();
        for f in &e.fields {
            let name = match &f.member {
                syn::Member::Named(n) => n.to_string(),
                syn::Member::Unnamed(i) => i.index.to_string(),
            };
            self.claim(&f.expr, Disp::with("field", format!("{}.{}", ty, name)));
        }
        visit::visit_expr_struct(self, e);
    }

    fn visit_expr_assign(&mut self, e: &'ast syn::ExprAssign) {
        self.claim(&e.right, Disp::with("assign", expr_label(&e.left)));
        visit::visit_expr_assign(self, e);
    }

    fn visit_expr_binary(&mut self, e: &'ast syn::ExprBinary) {
        if matches!(
            e.op,
            syn::BinOp::Eq(_) | syn::BinOp::Ne(_) | syn::BinOp::Lt(_) | syn::BinOp::Gt(_)
        ) {
            self.claim(&e.left, Disp::bare("compared"));
            self.claim(&e.right, Disp::bare("compared"));
        }
        visit::visit_expr_binary(self, e);
    }

    fn visit_signature(&mut self, s: &'ast syn::Signature) {
        self.pending_params = crate::callers::params_of(s);
        visit::visit_signature(self, s);
    }

    fn visit_local(&mut self, l: &'ast syn::Local) {
        if let Some(init) = &l.init {
            let d = if init.diverge.is_some() {
                Disp::bare("let-else")
            } else if matches!(l.pat, syn::Pat::Wild(_)) {
                // `let _ = f();` — the caller has decided the result cannot
                // matter. That is a claim about the implementation.
                Disp::bare("discarded")
            } else {
                Disp::bare("bound")
            };
            self.claim(&init.expr, d);
        }
        visit::visit_local(self, l);
        let mut names = BTreeSet::new();
        pat_idents(&l.pat, &mut names);
        if let Some(frame) = self.locals.last_mut() {
            frame.extend(names);
        }
    }

    fn visit_block(&mut self, b: &'ast syn::Block) {
        let frame = std::mem::take(&mut self.pending_params);
        self.locals.push(frame);

        // A statement-position call whose value is dropped, and the block's
        // tail expression, are both dispositions no expression-level visit can
        // see — they are facts about the statement, not about the expression.
        for (i, s) in b.stmts.iter().enumerate() {
            match s {
                syn::Stmt::Expr(e, Some(_)) => self.claim(e, Disp::bare("discarded")),
                syn::Stmt::Expr(e, None) if i + 1 == b.stmts.len() => {
                    self.claim(e, Disp::bare("returned"))
                }
                _ => {}
            }
        }
        self.scan_guards(b);

        visit::visit_block(self, b);
        self.locals.pop();
    }

    fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
        // A closure not passed to a combinator: its body is still a value
        // position, so it beats `bare`. The method-call visit claims the
        // combinator case first, and `or_insert` keeps the more specific one.
        self.claim(&c.body, Disp::bare("closure-tail"));
        let mut frame = BTreeSet::new();
        for p in &c.inputs {
            pat_idents(p, &mut frame);
        }
        self.locals.push(frame);
        visit::visit_expr_closure(self, c);
        self.locals.pop();
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

impl ContractVisitor<'_> {
    /// Mark target calls that a preceding `if`/`assert!` in the same block
    /// guards on one of their own argument names.
    ///
    /// This is the highest-value fact the tool can compute here: a guard is a
    /// precondition the caller believes it must establish before calling, and
    /// it is written down nowhere else — not in the signature, and (if it were
    /// in the doc comment) not in a form anything checks.
    fn scan_guards(&mut self, b: &syn::Block) {
        let mut guarded: BTreeSet<String> = BTreeSet::new();
        let mut text = String::new();
        for s in &b.stmts {
            // First: does this statement contain a target call whose arguments
            // the guards so far mention?
            if !guarded.is_empty() {
                let mut found = Vec::new();
                collect_target_calls(self.query, s, &mut found);
                for (key, names) in found {
                    if names.iter().any(|n| guarded.contains(n)) {
                        self.guards.insert(key, text.clone());
                    }
                }
            }
            // Then: does it establish a new guard for the statements below?
            if let Some((names, label)) = guard_of(s) {
                guarded.extend(names);
                text = label;
            }
        }
    }
}

/// The names a statement guards on, if it is a guard: a bare `if` with no
/// `else`, an `if …  { return … }` early exit, or an `assert!`-family macro. An
/// `if/else` is a branch rather than a precondition — both sides run something.
fn guard_of(s: &syn::Stmt) -> Option<(BTreeSet<String>, String)> {
    let mut names = BTreeSet::new();
    match s {
        syn::Stmt::Expr(syn::Expr::If(i), _) if i.else_branch.is_none() => {
            idents_of(&i.cond, &mut names);
            Some((names, format!("if {}", expr_label(&i.cond))))
        }
        // `let Some(x) = … else { return };` — a guard in the shape of a
        // binding, and the idiom that replaced most early-return `if`s.
        syn::Stmt::Local(l) if l.init.as_ref().is_some_and(|i| i.diverge.is_some()) => {
            let init = l.init.as_ref()?;
            idents_of(&init.expr, &mut names);
            pat_idents(&l.pat, &mut names);
            Some((names, format!("let … else ({})", expr_label(&init.expr))))
        }
        syn::Stmt::Macro(m) => {
            let name = last_segment(&path_to_string(&m.mac.path)).to_string();
            if !name.starts_with("assert") && name != "ensure" {
                return None;
            }
            for e in crate::macro_scan::macro_exprs(&m.mac) {
                idents_of(&e, &mut names);
            }
            Some((names, format!("{}!", name)))
        }
        _ => None,
    }
}

/// Every target call inside a statement, with the identifiers its arguments
/// mention. Used only by the guard scan, which needs argument names before the
/// main traversal has reached them.
fn collect_target_calls(
    query: &str,
    s: &syn::Stmt,
    out: &mut Vec<((usize, usize), BTreeSet<String>)>,
) {
    struct V<'a> {
        query: &'a str,
        out: &'a mut Vec<((usize, usize), BTreeSet<String>)>,
    }
    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
            if let syn::Expr::Path(p) = &*e.func {
                if crate::callers::matches_target(&path_to_string(&p.path), self.query) {
                    let mut names = BTreeSet::new();
                    for a in &e.args {
                        idents_of(a, &mut names);
                    }
                    self.out.push((pos(&p.path), names));
                }
            }
            visit::visit_expr_call(self, e);
        }
        fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
            if crate::callers::matches_target(&format!(".{}", e.method), self.query) {
                let mut names = BTreeSet::new();
                idents_of(&e.receiver, &mut names);
                for a in &e.args {
                    idents_of(a, &mut names);
                }
                self.out.push((pos(&e.method), names));
            }
            visit::visit_expr_method_call(self, e);
        }
    }
    V { query, out }.visit_stmt(s);
}

/// Collect every call site of `query`, with its evidence.
fn collect(ctx: &AnalysisCtx, query: &str) -> Vec<Site> {
    let mut all: Vec<Site> = Vec::new();
    for f in ctx.files {
        let mut v = ContractVisitor {
            file: &display_path(&f.path),
            query,
            scope: ScopeTracker::new(f.module.as_str()),
            sites: Vec::new(),
            disp: HashMap::new(),
            guards: HashMap::new(),
            locals: Vec::new(),
            pending_params: BTreeSet::new(),
            loop_depth: 0,
            cond_depth: 0,
        };
        v.visit_file(&f.ast);
        let (disp, guards) = (v.disp, v.guards);
        let uses = ctx.sem.uses_for(&f.path);
        for mut s in v.sites {
            if let Some(d) = disp.get(&s.key) {
                s.ret = d.clone();
            }
            if guards.contains_key(&s.key) {
                s.env.push("guarded".to_string());
            }
            if let Some(u) = uses {
                s.target_resolved = crate::callers::resolve_target_via_uses(&s.target, u, ctx.idx);
            }
            all.push(s);
        }
    }
    // `repeated` needs the whole set: a second call in the same fn is a fact
    // about the caller, not about either site alone.
    let mut per_caller: HashMap<&str, usize> = HashMap::new();
    for s in &all {
        *per_caller.entry(s.caller.as_str()).or_default() += 1;
    }
    let repeated: BTreeSet<String> = per_caller
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(c, _)| (*c).to_string())
        .collect();
    for s in &mut all {
        if repeated.contains(&s.caller) {
            s.env.push("repeated".to_string());
        }
    }
    all
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// Take `top` callers spread across modules rather than the first `top` in file
/// order. A truncated caller set produces a confidently wrong contract, and the
/// cheapest defence is to make the survivors as unalike as possible: one per
/// module before a second from any.
fn diverse_take(sites: &[Site], top: usize) -> Vec<&Site> {
    let mut by_module: BTreeMap<&str, Vec<&Site>> = BTreeMap::new();
    for s in sites {
        by_module.entry(s.module.as_str()).or_default().push(s);
    }
    let mut picked: Vec<&Site> = Vec::new();
    let mut round = 0;
    loop {
        let mut added = false;
        for group in by_module.values() {
            if let Some(s) = group.get(round) {
                picked.push(s);
                added = true;
                if picked.len() == top {
                    return picked;
                }
            }
        }
        if !added {
            return picked;
        }
        round += 1;
    }
}

// ---------------------------------------------------------------------------
// Phase 1 — callers, body withheld
// ---------------------------------------------------------------------------

pub fn run(ctx: &AnalysisCtx, query: &str, opts: &ContractOpts) -> anyhow::Result<usize> {
    if opts.candidates {
        return run_candidates(ctx, opts);
    }
    // `--top` is a whole-run row budget, and only `audit` re-sets it per
    // section. Left alone, `--top 2` spent the allowance on the target header
    // and left the usage table empty — so here `--top` means what a reader of
    // this command means by it: how many callers to list. The selection below
    // enforces it, and the budget must not cut a second time on top.
    ctx.out.set_row_budget(None);
    if !crate::callers::query_known(ctx.idx, query) {
        ctx.warn_unknown("fn, method, or macro", query);
    }
    let target = resolve_target(ctx, query);
    if opts.reveal {
        return run_reveal(ctx, query, target, opts);
    }

    let key = search_key(ctx, query, target);
    let unique = crate::callers::query_unique(ctx.idx, key.conf_query(query));

    // Scanned on the bare name when the query is qualified, so the sites the
    // qualified spelling would miss are *counted* even when they are then
    // dropped. Without both numbers there is nothing to warn with.
    let mut sites = collect(ctx, key.scan(query));
    let mut unattributed = 0usize;
    if key.narrow {
        let before = sites.len();
        sites.retain(|s| {
            crate::callers::matches_target(&s.target, query)
                || target.map(|d| resolves_locally(d, &s.target, &s.module)).unwrap_or(false)
        });
        unattributed = before - sites.len();
    }
    let mut unreachable = 0usize;
    let mut foreign = 0usize;
    if key.widened {
        if let Some(d) = target {
            // The item's own call forms: `.name`/`::name` as the widening
            // decided, plus a written path that names it, plus `self`/`Self`
            // inside its own impl.
            sites.retain(|s| {
                crate::callers::matches_target(&s.target, key.form_str())
                    || written_names_item(d, &s.target)
                    || resolves_on_self(d, &s.target, &s.module, s.receiver_is_self)
            });
            let before = sites.len();
            sites.retain(|s| !names_another_item(d, &s.target, s.target_resolved.as_deref()));
            foreign = before - sites.len();
            let before = sites.len();
            sites.retain(|s| !out_of_visibility(d, &s.module));
            unreachable = before - sites.len();
        }
    }

    if let Some(min) = opts.min_confidence {
        sites.retain(|s| tier(&key, s, query, unique, target) >= min);
    }
    ctx.retain_changed(&mut sites, |s| &s.file);

    // A recursive call is not a caller: it is the implementation, and phase 1
    // undertook not to show that. Left in, the arm shapes of the target's own
    // `match` leak into the caller table as evidence — on this tool's own
    // `infer_expr_type`, six of thirteen "callers" were the body itself, and
    // their `args` column spelled out the expression kinds it dispatches on.
    let recursive = match target {
        Some(d) => {
            let before = sites.len();
            sites.retain(|s| strip_span(&s.caller) != d.qpath);
            before - sites.len()
        }
        None => 0,
    };

    sites.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));

    if sites.is_empty() {
        // A bare zero here is the answer that sent one session to `grep`: it
        // reads as "nobody calls this" when it means "nothing spells the path
        // out". Say which one it is.
        if unattributed > 0 {
            ctx.out.answer(&format!(
                "no call site spells out `{}`, but {} site(s) call something named `{}`. More \
                 than one fn here has that name, so they cannot be attributed to this one — \
                 `contract-drift {}` lists them all and marks them `heuristic`, and \
                 `--min-confidence resolved` is the wrong tool here because the ambiguity is \
                 in the name, not in the sites.",
                query, unattributed, key.bare, key.bare
            ));
        }
        ctx.out.summary("(0 caller(s); nothing to infer a contract from)");
        if target.is_none() {
            return Err(TargetNotFound::err("fn, method, or macro matching", query));
        }
        return Ok(0);
    }

    emit_target_header(ctx, query, target, sites.len(), false);

    // `--top 0` lifts the cap, matching `--max-lines 0` and the global flag.
    let top = match opts.top {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => DEFAULT_TOP,
    };
    let shown = diverse_take(&sites, top);

    ctx.out.section("callers");
    for s in &shown {
        let conf = tier(&key, s, query, unique, target);
        let cells: Vec<(&'static str, Val)> = vec![
            ("via", Val::from(conf.as_str())),
            ("at", site(&s.file, s.line)),
            ("in", Val::from(s.caller.clone())),
            (
                "in_at",
                match s.caller_span {
                    Some((a, b)) => span_site(&s.file, a, b),
                    None => site(&s.file, s.line),
                },
            ),
            ("ret", Val::from(s.ret.label())),
            ("args", Val::from(s.args_cell())),
            ("env", Val::from(s.env_cell())),
        ];
        ctx.out.row(cells);
    }
    if sites.len() > shown.len() {
        // Its own note rather than the global `--top` one, because the cut is
        // not the global cut: these rows were chosen to be unalike, and a
        // reader who thinks they are "the first N" will read the sample as the
        // population.
        // `row_note`, not `note`: the global `--top` cut lands on stdout so a
        // reader who redirected stderr still learns the answer was cut, and
        // this cut needs it more — these rows were chosen to be unalike, and a
        // reader who takes them for "the first N" reads a sample as the whole.
        ctx.out.row_note(&format!(
            "(note: showing {} of {} caller(s), spread across modules rather than taken in \
             file order — `--top {}` for all of them)",
            shown.len(),
            sites.len(),
            sites.len()
        ));
    }

    emit_usage(ctx, &sites);

    if !opts.no_bodies {
        emit_bodies(ctx, &shown, opts);
    }

    let modules: BTreeSet<&str> = sites.iter().map(|s| s.module.as_str()).collect();
    let low = shown
        .iter()
        .filter(|s| {
            tier(&key, s, query, unique, target) < Confidence::Resolved
        })
        .count();
    if low > 0 {
        ctx.out.note(&format!(
            "(note: {} of the listed caller(s) are below `resolved` — they may not be calling \
             this item at all, and one wrong caller poisons the expectation. \
             `--min-confidence resolved` drops them)",
            low
        ));
    }
    // Disclosed rather than dropped in silence, even though it says one thing
    // about the implementation: the alternative is a caller count that
    // disagrees with `unruster callers` for no stated reason, and a reader who
    // trusts a number they cannot reproduce.
    if recursive > 0 {
        ctx.out.note(&format!(
            "(note: {} recursive call site(s) excluded — a fn calling itself is the \
             implementation, not evidence about it; `unruster callers {}` counts them)",
            recursive, query
        ));
    }
    // Both of these go to stdout: a caller set that is quietly a sample is the
    // failure this command cannot survive, and `note` is the channel a reader
    // who redirected stderr never sees.
    // A method widened by name alone is the case where "unique in the index"
    // says least: the index holds no `Vec::len`, so every `.len()` in the tree
    // arrives looking like a caller. Lead with that rather than bury it.
    if key.by_method_name && key.widened {
        ctx.out.row_note(&format!(
            "(warning: `{}` was matched by METHOD NAME — every `.{}()` in the tree is here, \
             whatever its receiver's type, and nothing syntactic separates them. Rows are \
             tiered `heuristic` for that reason. Read this as a lead list, not a caller set; \
             a contract derived from it is a contract for every `{}` in the language)",
            query, key.bare, key.bare
        ));
    }
    if key.widened && !key.by_method_name {
        // The first wording of this note said "every site calling it is this
        // one". It is not a guarantee this tool can make: the index holds only
        // the fns in the scanned tree, so a name unique *here* can still be a
        // method on a std or third-party type. Say what was matched and leave
        // the reader able to check it.
        ctx.out.row_note(&format!(
            "(note: `{}` was matched as an item, in its own call form `{}` — call sites \
             record the callee as written, so matching `{}` textually would have found only \
             the sites that spell the path out. No other fn in this tree is called `{}`, but \
             a same-named method on a type defined outside it would not be visible here)",
            query,
            key.form_str(),
            query,
            key.bare
        ));
    }
    if foreign > 0 {
        ctx.out.row_note(&format!(
            "(note: {} widened site(s) dropped — their written path names a different \
             `{}` (a std or third-party item this tree does not define), not `{}`)",
            foreign,
            key.bare,
            target.map(|d| d.qpath.as_str()).unwrap_or(query)
        ));
    }
    if unreachable > 0 {
        ctx.out.row_note(&format!(
            "(note: {} widened site(s) dropped as unreachable — `{}` is `{}` in `{}`, so a \
             call from outside that module is a different item of the same name)",
            unreachable,
            key.bare,
            target.map(|d| d.vis).unwrap_or("priv"),
            target.map(|d| d.module.as_str()).unwrap_or("")
        ));
    }
    if unattributed > 0 {
        ctx.out.row_note(&format!(
            "(warning: {} further site(s) call something named `{}` and are NOT in the {} \
             above — more than one fn here is called `{}`, so they cannot be attributed to \
             this one. The contract you derive below is from a SUBSET of the callers. \
             `contract-drift {}{}` includes them all, at heuristic confidence)",
            unattributed,
            key.bare,
            sites.len(),
            key.bare,
            if query.contains("::") && target.map(|d| d.kind != "fn").unwrap_or(false) {
                "."
            } else {
                ""
            },
            key.bare
        ));
    }

    // Through `answer`, not `note`: this is the instruction the whole command
    // exists to deliver, and `note` goes to stderr, which agents routinely
    // discard. A blindfold that lands on a suppressed channel is not applied.
    // Naming the bypasses, because withholding the body here does not make it
    // unreadable anywhere else. One session ran `contract-drift <fn>` and
    // `unruster show <fn>` as two halves of a single shell command, labelled
    // the second "=== REVEAL ===", and so had no moment in which an
    // expectation could exist. It was not cheating; it did not know that
    // `show` was the thing being avoided.
    ctx.out.answer(&format!(
        "the implementation of `{}` was withheld on purpose. write the expectation these \
         {} caller(s) imply — what it must accept, return, and guarantee — and only then run \
         `unruster contract-drift {} --reveal`. that is the only reading step that keeps \
         this honest: `show {}`, `sed`, `cat`, or opening {} yourself reaches the same body \
         and spends the exercise, and doing it in the same breath as this command leaves no \
         moment in which an expectation could exist. an expectation written afterwards \
         describes the code instead of testing it, and nothing downstream can tell the \
         difference.",
        query,
        sites.len(),
        query,
        query,
        target.map(|d| d.file.as_str()).unwrap_or("the file"),
    ));
    ctx.out.summary(&format!(
        "({} caller(s) across {} module(s); body withheld — `--reveal` for the \
         implementation; explain: contract-drift)",
        sites.len(),
        modules.len()
    ));
    // Material, not findings. An explicit `--fail-on-findings` must not exit 1
    // on a dossier — there is no judgment here for a build to fail on, and the
    // count a reader wants is in the summary line above.
    Ok(0)
}

/// How a qualified query is turned into a call-site search.
///
/// A call site records the callee **as written** — `n(…)`, `.leaf(…)` — not as
/// resolved, so `ends_with("svg::n")` matches only the sites that spell the
/// path out. For `callers` that yields a short list; here it yields a *wrong
/// contract*, because the whole premise is "everything that calls it". On one
/// real run `svg::n` reported 2 callers out of 164 and said nothing, and eight
/// other qualified queries reported a confident zero — after which the session
/// stopped trusting qualified names at all and hand-translated every target to
/// `.method` / `::name`.
///
/// So a qualified query is resolved to the *item* when that is unambiguous:
/// one indexed fn, and a bare last segment no other fn shares. Matching on the
/// bare segment is then `resolved` confidence under the rule `site_confidence`
/// already encodes — the name cannot belong to anything else.
///
/// When the bare name is shared, widening would mix items, so the narrow match
/// stands and the sites it could not attribute are reported. Silence is the one
/// option that is not available: `callers::note_narrower_than_bare` was added
/// for exactly this failure and fires only at zero, which is why the 2-of-164
/// case slipped through.
pub(crate) struct SearchKey<'a> {
    bare: &'a str,
    /// What the widened scan actually searches for: `::name` for a free fn,
    /// `.name` for a method. **Never the bare name** — see `search_key`.
    form: String,
    /// Qualified, but the bare name is shared — match narrowly and disclose.
    narrow: bool,
    /// Qualified and unambiguous — matched by item, in its own call form.
    widened: bool,
    /// The widened form is `.name`, which matches by method name alone. Nothing
    /// syntactic separates `Suppressions::len` from `Vec::len`, so a site whose
    /// receiver type is unknown is a *lead*, not a caller.
    by_method_name: bool,
}

/// The tier one widened site earns.
///
/// `confidence_of` promotes a name unique in the index to `resolved`, which is
/// right for a free fn and wrong for a method: the index holds no `Vec::len`,
/// so `len` looks unique and 416 `.len()` calls were reported as callers of
/// `Suppressions::len` at full confidence. `--candidates` already tiers method
/// rows `heuristic` for exactly this reason (§9.3); this is the same rule,
/// applied on the other path.
fn tier(key: &SearchKey, s: &Site, query: &str, unique: bool, d: Option<&Defn>) -> Confidence {
    // Proved beats named. `self.push(1)` inside `impl V` is as resolved as a
    // call gets, and demoting it made `--min-confidence resolved` return
    // nothing for a method with three certain callers — while `callers`, whose
    // copy of this rule had already been fixed, returned all three.
    if let Some(d) = d.filter(|_| !s.shadowed) {
        if written_names_item(d, &s.target)
            || resolves_on_self(d, &s.target, &s.module, s.receiver_is_self)
            || resolves_locally(d, &s.target, &s.module)
        {
            return Confidence::Resolved;
        }
    }
    if key.by_method_name && s.target.starts_with('.') {
        return Confidence::Heuristic;
    }
    crate::callers::confidence_of(
        s.target_resolved.as_deref(),
        s.shadowed,
        key.conf_query(query),
        unique,
    )
}

impl<'a> SearchKey<'a> {
    pub(crate) fn is_widened(&self) -> bool {
        self.widened
    }
    pub(crate) fn is_narrow(&self) -> bool {
        self.narrow
    }
    pub(crate) fn form_str(&self) -> &str {
        &self.form
    }

    /// The query to scan call sites with.
    /// Scanned on the bare name whenever the query resolves to an item, so the
    /// filters below choose from a superset rather than from whatever one call
    /// form happens to catch. Scanning on `.push` alone meant an associated fn
    /// invoked as `V::push(v, 3)` never entered the collection at all — while
    /// `callers`, which collects everything and filters afterwards, saw it.
    fn scan(&self, query: &'a str) -> &str {
        if self.widened || self.narrow {
            self.bare
        } else {
            query
        }
    }
    /// The query the confidence tiers are computed against — the widened form
    /// once widened, since that is what actually matched.
    fn conf_query(&self, query: &'a str) -> &str {
        if self.widened {
            &self.form
        } else {
            query
        }
    }
}

pub(crate) fn search_key<'a>(ctx: &AnalysisCtx, query: &'a str, target: Option<&Defn>) -> SearchKey<'a> {
    search_key_for(ctx.idx, query, target)
}

/// [`search_key`] over the index alone, for `callers`, which resolves its query
/// before an `AnalysisCtx` is convenient to hand around.
pub(crate) fn search_key_for<'a>(
    idx: &crate::index::NameIndex,
    query: &'a str,
    target: Option<&Defn>,
) -> SearchKey<'a> {
    let bare = last_segment(query);
    // `::name` and `.method` already match by last segment; a bare name is the
    // last segment. Only `a::b` needs resolving.
    let qualified = query.contains("::") && !query.starts_with("::");
    if !qualified {
        return SearchKey {
            bare,
            form: bare.to_string(),
            narrow: false,
            widened: false,
            by_method_name: false,
        };
    }
    // Widening keeps the *call form*, and matching the bare name would throw
    // it away. `trace::round` is one private free fn, and the tree's index
    // holds no other `round` — but `f64::round` is not in the index either, so
    // a bare-name scan claimed 65 callers across 13 modules, nearly all of
    // them `.round()` on a float, and asserted `resolved` on every one. A
    // private fn cannot have callers in thirteen modules; the answer was
    // rejected on sight, which is the cheapest kind of wrong answer to
    // produce and the most expensive kind to have produced.
    //
    // `::name` matches free-fn paths only and `.name` matches method calls
    // only, so neither can collect the other's homonyms. Same-named methods on
    // *other* types remain possible — nothing syntactic separates them — which
    // is why the note below claims a resolution, not a guarantee.
    let form = match target.map(|d| d.kind) {
        Some("fn") => format!("::{}", bare),
        Some("impl-fn") | Some("trait-fn") => format!(".{}", bare),
        _ => bare.to_string(),
    };
    let unambiguous = target.is_some() && crate::callers::query_unique(idx, bare);
    let by_method_name = form.starts_with('.');
    SearchKey {
        bare,
        form,
        narrow: !unambiguous,
        widened: unambiguous,
        by_method_name,
    }
}

/// A widened site whose *written* path names something else.
///
/// `::name` matches free-fn paths by last segment, which keeps `.round()` out
/// but not `std::fs::write` — 53 of `baseline::write`'s 62 reported callers
/// were `std::fs::write` in the test file. Restricting to the target's own
/// call form fixed the method half of this problem and left the free-fn half
/// standing, which is the same defect twice.
///
/// A written path that carries any qualification has to be *compatible* with
/// the target's: `crate::baseline::write` and `baseline::write` are, `std::fs::write`
/// is not. A bare name is kept unless the file's use-map says it resolves
/// somewhere else — `use std::fs::write; write(…)` is a bare spelling of a
/// foreign item.
pub(crate) fn names_another_item(d: &Defn, target: &str, resolved: Option<&str>) -> bool {
    let written = relative_head(target);
    if written.contains("::") {
        return !path_agrees(d, written);
    }
    match resolved {
        Some(r) => !path_agrees(d, relative_head(r)),
        None => false,
    }
}

/// Strip the prefixes that make a path relative rather than foreign, so
/// `super::write` and `self::write` compare as the bare name they resolve to
/// inside their own tree.
fn relative_head(path: &str) -> &str {
    let mut p = path.trim_start_matches("::");
    p = p.strip_prefix("crate::").unwrap_or(p);
    p = p.strip_prefix("self::").unwrap_or(p);
    // `Self::name(…)` inside the impl is this item as surely as `self.name(…)`
    // is; the caller checks the enclosing scope.
    p = p.strip_prefix("Self::").unwrap_or(p);
    while let Some(rest) = p.strip_prefix("super::") {
        p = rest;
    }
    p
}

/// The written call path names this item: `Disp::bare`, `crate::x::Disp::bare`,
/// or a bare `bare`.
///
/// Widening an `impl-fn` to `.name` matches method calls and nothing else, so
/// an associated fn invoked as `Disp::bare(…)` — which is how constructors are
/// almost always written — matched neither the widened form nor the qualified
/// query. `self-check` found ten of them here in one pass.
pub(crate) fn written_names_item(d: &Defn, target: &str) -> bool {
    if target.starts_with('.') || target.ends_with('!') {
        return false;
    }
    let written = relative_head(target);
    // Must be *qualified*. `path_agrees` accepts a bare last segment, which is
    // right when resolving a free fn and catastrophic here: `qpath.ends_with("bare")`
    // is true of every `bare(…)` in the tree, and the first draft of this
    // matched 772 sites the token oracle could not see at all.
    written.contains("::") && path_agrees(d, written)
}

fn path_agrees(d: &Defn, written: &str) -> bool {
    written == d.name || ends_on_segment(written, &d.qpath) || ends_on_segment(&d.qpath, written)
}

/// Suffix match on whole `::` segments.
///
/// A plain `str::ends_with` is a substring test, and `"std::fs::write"` ends
/// with `"s::write"` — so `s::write` claimed both `std::fs::write` calls in a
/// fixture written to prove it did not. `show`'s own help already states the
/// rule this restores: "Matching is on whole `::` segments, so a suffix that
/// isn't one won't silently resolve to something else."
pub(crate) fn ends_on_segment(hay: &str, needle: &str) -> bool {
    match hay.strip_suffix(needle) {
        Some("") => true,
        Some(prefix) => prefix.ends_with("::"),
        None => false,
    }
}

/// A bare call that Rust's own scoping resolves to this item.
///
/// A shared bare name makes a qualified query go narrow, and narrow means "only
/// the sites that spell the path out" — which inside the defining module is
/// nobody. Four modules here define a `score`, so `arith_drift::score` reported
/// zero callers while `arith_drift::run` called it four times as bare `score`.
/// Twenty-five of this tree's 362 fns answered zero for that reason alone.
///
/// Inside the module that defines it, the bare name is not ambiguous: the
/// compiler binds it to the local item, and so can this. Outside, it stays
/// unattributable, which is what the shortfall warning is for.
///
/// Free fns only. A bare `.method()` in the same module can still be on some
/// other type, and no scoping rule says otherwise.
pub(crate) fn resolves_locally(d: &Defn, target: &str, module: &str) -> bool {
    // `relative_head` first: `super::helper(…)` from a child module is the same
    // item as a bare `helper(…)` beside it, and Rust resolves both by scope.
    // Comparing the raw target missed every `super::`/`self::` call.
    d.kind == "fn" && relative_head(target) == d.name && in_module_subtree(d, module)
}

/// `self.name(…)` written inside the impl that defines `name`.
///
/// The method analogue of [`resolves_locally`], and the same argument: an
/// inherent method wins over anything else `self` could offer, so the compiler
/// binds this and so can we. Found by `self-check`, which noticed that
/// `dead-code` knew `ArithVisitor::push` was called while the call-site walk
/// reported nobody — twenty-one methods in this tree were in that state.
///
/// Requires the receiver to be *literally* `self`: `self.sites.push(…)` inside
/// `impl ArithVisitor` is a `Vec`, and attributing it here would trade a
/// missing caller for a wrong one.
pub(crate) fn resolves_on_self(d: &Defn, target: &str, module: &str, on_self: bool) -> bool {
    if d.kind != "impl-fn" {
        return false;
    }
    let by_self = on_self && target == format!(".{}", d.name);
    let by_self_type = target == format!("Self::{}", d.name);
    if !(by_self || by_self_type) {
        return false;
    }
    let Some(owner) = &d.owner else { return false };
    let home = if d.module.is_empty() {
        owner.clone()
    } else {
        format!("{}::{}", d.module, owner)
    };
    module == home
}

/// `site_module` is the target's module or one nested inside it.
pub(crate) fn in_module_subtree(d: &Defn, site_module: &str) -> bool {
    site_module == d.module || site_module.starts_with(&format!("{}::", d.module))
}

/// Widened sites that the target's visibility makes impossible.
///
/// A `priv` or `pub(self)` item is reachable only from its own module and that
/// module's descendants, so a widened match landing anywhere else is a homonym
/// this tool cannot see the definition of. Cheap, sound, and it catches
/// precisely the class that made `trace::round` report 65 callers.
pub(crate) fn out_of_visibility(d: &Defn, site_module: &str) -> bool {
    if d.vis != "priv" && d.vis != "pub(self)" {
        return false;
    }
    !in_module_subtree(d, site_module)
}

/// Drop a `--spans` `@start-end` suffix from an enclosing-fn label, so a caller
/// can be compared to a `qpath` under either setting of the flag.
fn strip_span(caller: &str) -> &str {
    caller.split('@').next().unwrap_or(caller)
}

/// The target's own definition, if the name resolves to exactly one item. A
/// query matching several (or a macro, which is not indexed) still lists
/// callers — the header just cannot name a span.
fn resolve_target<'a>(ctx: &'a AnalysisCtx, query: &str) -> Option<&'a Defn> {
    let bare = query
        .trim_start_matches('.')
        .trim_start_matches("::")
        .trim_end_matches('!');
    let hits = ctx.idx.lookup(bare);
    let fns: Vec<&Defn> = hits
        .iter()
        .copied()
        .filter(|d| matches!(d.kind, "fn" | "impl-fn" | "trait-fn"))
        .collect();
    (fns.len() == 1).then(|| fns[0])
}

fn emit_target_header(
    ctx: &AnalysisCtx,
    query: &str,
    target: Option<&Defn>,
    callers: usize,
    reveal: bool,
) {
    ctx.out.section("target");
    let Some(d) = target else {
        ctx.out.row(vec![
            ("name", Val::from(query)),
            ("at", Val::from("—")),
            ("body", Val::from(if reveal { "shown" } else { "withheld" })),
            ("callers", Val::from(callers)),
        ]);
        ctx.out.note(&format!(
            "(note: `{}` does not resolve to exactly one indexed fn, so no signature is \
             shown; the caller evidence below is still what it is)",
            query
        ));
        return;
    };
    ctx.out.row(vec![
        ("kind", Val::from(d.kind)),
        ("vis", Val::from(d.vis)),
        ("name", Val::from(d.qpath.clone())),
        ("at", span_site(&d.file, d.line, d.end)),
        ("body", Val::from(if reveal { "shown" } else { "withheld" })),
        ("callers", Val::from(callers)),
    ]);
    if !reveal {
        // The signature *is* contract, not implementation: types, `Result`,
        // `&mut` and lifetimes are all promises the compiler already enforces.
        // Withholding them would only make the reader invent expectations that
        // are ruled out on sight.
        blank(ctx);
        crate::show::print_range(ctx, &d.file, d.line, d.sig_end.max(d.line), Some(0), false, None);
        blank(ctx);
    }
}

/// A blank separator line — in TSV only. In JSON it would be an empty row, and
/// a consumer iterating `rows` would have to know to skip it.
fn blank(ctx: &AnalysisCtx) {
    if ctx.out.format != Format::Json {
        ctx.out.line("");
    }
}

/// The aggregate view: one row per distinct fact, so a reader sees "seven of
/// eight callers propagate with `?`, one unwraps" without counting rows.
fn emit_usage(ctx: &AnalysisCtx, sites: &[Site]) {
    #[derive(Default)]
    struct Agg {
        sites: usize,
        callers: BTreeSet<String>,
        detail: BTreeSet<String>,
    }
    let mut facts: BTreeMap<String, Agg> = BTreeMap::new();
    let mut add = |key: String, s: &Site, detail: Option<String>| {
        let e = facts.entry(key).or_default();
        e.sites += 1;
        e.callers.insert(s.caller.clone());
        if let Some(d) = detail {
            e.detail.insert(d);
        }
    };
    for s in sites {
        add(format!("ret:{}", s.ret.label()), s, None);
        for (i, a) in s.args.iter().enumerate() {
            add(
                format!("arg{}:{}", i + 1, a.shape),
                s,
                Some(a.text.clone()),
            );
        }
        for e in &s.env {
            add(format!("env:{}", e), s, None);
        }
    }
    if facts.is_empty() {
        return;
    }
    ctx.out.section("usage");
    let mut rows: Vec<(&String, &Agg)> = facts.iter().collect();
    rows.sort_by(|a, b| b.1.sites.cmp(&a.1.sites).then_with(|| a.0.cmp(b.0)));
    for (fact, agg) in rows {
        let mut detail: Vec<&str> = agg.detail.iter().map(|s| s.as_str()).collect();
        detail.truncate(3);
        ctx.out.row(vec![
            ("fact", Val::from(fact.clone())),
            ("sites", Val::from(agg.sites)),
            ("callers", Val::from(agg.callers.len())),
            (
                "detail",
                Val::from(if detail.is_empty() {
                    "—".to_string()
                } else {
                    detail.join(" ")
                }),
            ),
        ]);
    }
}

/// One whole enclosing fn per caller. The whole fn, not a `--context` window:
/// the expectation lives in what the caller does *after* the call — the `?`,
/// the `match`, the `let _`, the loop around it — and a fixed window is exactly
/// as likely to cut that off as to include it.
fn emit_bodies(ctx: &AnalysisCtx, shown: &[&Site], opts: &ContractOpts) {
    ctx.out.section("caller sources");
    let budget = opts.max_lines.or(Some(DEFAULT_MAX_LINES));
    for s in shown {
        let Some((start, end)) = s.caller_span else {
            continue;
        };
        ctx.out.row(vec![
            ("in", Val::from(s.caller.clone())),
            ("at", span_site(&s.file, start, end)),
        ]);
        crate::show::print_range(ctx, &s.file, start, end, budget, false, Some(s.line));
        blank(ctx);
    }
}


// ---------------------------------------------------------------------------
// Phase 2 — the implementation
// ---------------------------------------------------------------------------

fn run_reveal(
    ctx: &AnalysisCtx,
    query: &str,
    target: Option<&Defn>,
    opts: &ContractOpts,
) -> anyhow::Result<usize> {
    let Some(d) = target else {
        return Err(TargetNotFound::err("a single indexed fn named", query));
    };
    ctx.out.section("target");
    ctx.out.row(vec![
        ("kind", Val::from(d.kind)),
        ("vis", Val::from(d.vis)),
        ("name", Val::from(d.qpath.clone())),
        ("at", span_site(&d.file, d.doc_start, d.end)),
        ("body", Val::from("shown")),
        ("lines", Val::from(d.end.saturating_sub(d.line) + 1)),
    ]);
    blank(ctx);
    // From `doc_start`: the doc comment is the *stated* contract, withheld in
    // phase 1 so the caller-derived expectation could not be contaminated by
    // it, and revealed here so the comparison is three-way — what callers
    // assume, what the doc promises, what the code does.
    crate::show::print_range(
        ctx,
        &d.file,
        d.doc_start,
        d.end,
        opts.max_lines.or(Some(0)),
        false,
        None,
    );
    blank(ctx);
    // Deliberately no caller material: it is already above this in the
    // transcript, and reprinting it doubles the cost of the expensive half
    // while blurring the boundary the two phases exist to draw.
    ctx.out.section("callees");
    // `callees` writes its own trailing summary, and two summary lines under
    // one command reads as two commands having run.
    let prev = ctx.out.hold_summary(true);
    let n = crate::callers::run_callees(ctx, &d.qpath).unwrap_or(0);
    ctx.out.hold_summary(prev);
    ctx.out.take_held_summary();
    ctx.out.summary(&format!(
        "({} line(s); {} callee row(s); compare against the expectation you wrote from the \
         callers; explain: contract-drift)",
        d.end.saturating_sub(d.line) + 1,
        n
    ));
    Ok(0)
}

// ---------------------------------------------------------------------------
// Target selection
// ---------------------------------------------------------------------------

/// Rank the fns worth blindfolding. Not a verdict — nothing here says a
/// function is wrong, only that it has enough callers, enough body, and little
/// enough written down that the exercise can pay.
fn run_candidates(ctx: &AnalysisCtx, opts: &ContractOpts) -> anyhow::Result<usize> {
    // A plain ranked listing, so `--top` keeps its usual meaning here and the
    // usual note announces the cut. (`run` clears the budget for the two-phase
    // mode, where `--top` counts callers instead of rows.)
    ctx.out.set_row_budget(opts.top.filter(|n| *n > 0));
    let sites = crate::callers::collect_sites(ctx.files, ctx.sem, ctx.idx, false);
    let mut by_name: HashMap<&str, (usize, BTreeSet<&str>)> = HashMap::new();
    for s in &sites {
        let name = last_segment(s.target.trim_start_matches('.').trim_end_matches('!'));
        let e = by_name.entry(name).or_default();
        e.0 += 1;
        e.1.insert(crate::config_drift::module_of(&s.caller));
    }

    struct Cand<'a> {
        d: &'a Defn,
        callers: usize,
        mods: usize,
        loc: usize,
        ret: &'static str,
        doc: bool,
        via: &'static str,
        score: f64,
    }
    // How many fns share each bare name. Call sites are matched by last
    // segment, so a name with several definitions cannot have its callers
    // attributed to any one of them — this tree has thirty fns called `run`,
    // and crediting each with all 55 `run(…)` sites ranked every command's
    // entry point at the top on evidence belonging to the other twenty-nine.
    let mut defns_named: HashMap<&str, usize> = HashMap::new();
    for d in ctx.idx.iter() {
        if matches!(d.kind, "fn" | "impl-fn" | "trait-fn") {
            *defns_named.entry(d.name.as_str()).or_default() += 1;
        }
    }

    let mut cands: Vec<Cand> = Vec::new();
    let mut ambiguous = 0usize;
    for d in ctx.idx.iter() {
        if !matches!(d.kind, "fn" | "impl-fn") || !ctx.in_scope(&d.file) {
            continue;
        }
        let Some((callers, mods)) = by_name.get(d.name.as_str()) else {
            continue;
        };
        let callers = *callers;
        if callers < opts.min_callers {
            continue;
        }
        if defns_named.get(d.name.as_str()).copied().unwrap_or(1) > 1 {
            ambiguous += 1;
            continue;
        }
        let loc = d.end.saturating_sub(d.line) + 1;
        let ret = return_shape(d);
        let doc = d.doc.is_some();
        // A method's call sites are matched by bare name, and the tree is full
        // of same-named methods this tool never indexed — `Suppressions::len`
        // was credited with all 321 `.len()` calls in the crate, most of them
        // on a `Vec`. The count is an upper bound, so the row says so and the
        // score does not let it outrank a free fn whose count is attributable.
        let via = if d.kind == "fn" {
            "resolved"
        } else {
            "heuristic"
        };
        let score = candidate_score(callers, mods.len(), loc, ret, doc, via == "resolved");
        cands.push(Cand {
            d,
            callers,
            mods: mods.len(),
            loc,
            ret,
            doc,
            via,
            score,
        });
    }
    ctx.retain_changed(&mut cands, |c| &c.d.file);
    cands.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.d.file.cmp(&b.d.file))
            .then_with(|| a.d.line.cmp(&b.d.line))
    });

    let total = cands.len();
    if !ctx.summary {
        ctx.out.section("candidates");
        // No local cut: this section is a plain ranked listing, so the global
        // `--top` budget caps it and announces itself the same way it does
        // everywhere else.
        for c in &cands {
            ctx.out.row(vec![
                ("score", Val::from(format!("{:.2}", c.score))),
                ("via", Val::from(c.via)),
                ("name", Val::from(c.d.qpath.clone())),
                ("at", span_site(&c.d.file, c.d.line, c.d.end)),
                ("callers", Val::from(c.callers)),
                ("mods", Val::from(c.mods)),
                ("loc", Val::from(c.loc)),
                ("ret", Val::from(c.ret)),
                ("doc", Val::from(if c.doc { "yes" } else { "—" })),
            ]);
        }
    }
    // Said out loud rather than dropped: a reader who does not know these were
    // skipped reads the list as the whole field of candidates.
    if ambiguous > 0 {
        // The old wording recommended `contract-drift <Type::method>`, which is
        // the one thing that cannot work for *these* rows: they are here
        // precisely because the bare name is shared, which is the same
        // condition that stops a qualified query resolving to the item. A
        // session followed the advice, got four confident zeros, and stopped
        // trusting this column.
        ctx.out.note(&format!(
            "(note: {} fn(s) with enough callers were skipped because their bare name has \
             more than one definition here — call sites match by last segment, so the count \
             cannot be attributed to one of them. Running one by name still works, but its \
             caller set is a subset and the command says by how much; the listed rows above \
             do not have this problem)",
            ambiguous
        ));
    }
    ctx.out.summary(&format!(
        "({} candidate(s); min_callers={}; `contract-drift <name>` starts one; \
         explain: contract-drift)",
        total, opts.min_callers
    ));
    Ok(0)
}

/// `Result` / `Option` / plain, read off the rendered signature. The failure
/// axis is where a contract drifts, so a fallible fn outranks an infallible one
/// of the same size.
fn return_shape(d: &Defn) -> &'static str {
    let Ok(src) = std::fs::read_to_string(&d.file) else {
        return "—";
    };
    let lines: Vec<&str> = src.lines().collect();
    let lo = d.line.saturating_sub(1);
    let hi = d.sig_end.max(d.line).min(lines.len());
    if lo >= hi {
        return "—";
    }
    let sig = lines[lo..hi].join(" ");
    let Some(arrow) = sig.find("->") else {
        return "()";
    };
    let ret = &sig[arrow + 2..];
    if ret.contains("Result") {
        "Result"
    } else if ret.contains("Option") {
        "Option"
    } else {
        "value"
    }
}

/// 0..1. Weighted so caller breadth dominates: a contract crossing module
/// boundaries is one nobody owns, and that is the case the exercise was built
/// for. Size and the missing doc comment break ties.
fn candidate_score(
    callers: usize,
    mods: usize,
    loc: usize,
    ret: &str,
    doc: bool,
    attributable: bool,
) -> f64 {
    // Halved rather than dropped for a name-matched method: the count is an
    // upper bound, not a fiction, and a method with many real callers is still
    // worth the exercise — it just must not outrank a free fn whose count is
    // known to be its own.
    let trust = if attributable { 1.0 } else { 0.5 };
    let breadth = (callers as f64 / 12.0).min(1.0) * 0.35 * trust;
    let spread = ((mods.saturating_sub(1)) as f64 / 4.0).min(1.0) * 0.25 * trust;
    let size = (loc as f64 / 80.0).min(1.0) * 0.20;
    let fallible = if matches!(ret, "Result" | "Option") {
        0.12
    } else {
        0.0
    };
    let undocumented = if doc { 0.0 } else { 0.08 };
    breadth + spread + size + fallible + undocumented
}
