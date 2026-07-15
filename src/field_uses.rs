use syn::visit::{self, Visit};

use crate::ast::{fn_span, line_of, path_last, path_to_string, type_short, ScopeTracker};
use crate::context::{warn_unknown_target, AnalysisCtx, Confidence, TargetNotFound};
use crate::parse::{display_path, ParsedFile};
use crate::semantic::{FnSigIndex, FnTypes};

#[derive(Debug)]
struct FieldHit {
    kind: FieldKind,
    /// Why we believe this hit refers to the target type:
    /// "self" — self in `impl Type`
    /// "init" — `Type { f: ... }` struct literal
    /// "ti"   — receiver type inferred (function-local) to be Type
    /// "?"    — receiver type unknown (only emitted in --candidates mode)
    via: &'static str,
    file: String,
    line: usize,
    context: String, // enclosing fn or impl
    receiver: String,
}

struct FieldVisitor<'a> {
    file: &'a str,
    target_type: &'a str,
    target_field: &'a str,
    /// strict: only emit "self" / "init" / "ti" hits.
    /// non-strict (candidates): also emit "?" hits.
    strict: bool,

    scope: ScopeTracker,
    /// Per-fn-body type maps. Pushed on fn entry, popped on exit. The last
    /// entry is the currently-enclosing fn.
    fn_types_stack: Vec<FnTypes>,
    in_write_lhs: bool,

    fn_sigs: &'a FnSigIndex,
    hits: Vec<FieldHit>,
}

impl<'a> FieldVisitor<'a> {
    fn enclosing(&self) -> String {
        self.scope.enclosing()
    }

    fn in_target_impl(&self) -> bool {
        self.scope
            .impl_stack
            .last()
            .map(|t| t == self.target_type)
            .unwrap_or(false)
    }

    fn record(&mut self, kind: FieldKind, via: &'static str, line: usize, receiver: String) {
        self.hits.push(FieldHit {
            kind,
            via,
            file: self.file.to_string(),
            line,
            context: self.enclosing(),
            receiver,
        });
    }

    /// Try to infer the receiver type using the enclosing fn's `FnTypes`.
    /// Returns `Some(type_last)` if known, `None` if no inference possible.
    fn infer_receiver(&self, base: &syn::Expr) -> Option<String> {
        self.fn_types_stack
            .last()
            .and_then(|ft| ft.type_of(base, self.fn_sigs))
    }

    /// Classify the receiver of a `<base>.<target_field>` access.
    /// Returns `Some((via, receiver-display))` when the access matches the
    /// target type (under strict + candidates rules), `None` when it should
    /// be dropped (inferred to a different type, or strict-mode unknown).
    fn classify_base(&self, base: &syn::Expr) -> Option<(&'static str, String)> {
        // `self` in an `impl Type` — always emit.
        if let syn::Expr::Path(p) = base {
            if p.path.is_ident("self") {
                if self.in_target_impl() {
                    return Some(("self", "self".to_string()));
                }
                if self.strict {
                    return None;
                }
                let owner = self.scope.impl_stack.last().cloned().unwrap_or_default();
                return Some(("?", format!("self (in impl {})", owner)));
            }
        }
        // Non-self: try the local type inferencer.
        match self.infer_receiver(base) {
            Some(t) if t == self.target_type => Some(("ti", recv_display(base))),
            Some(_) => None, // inferred to a different type — definitely not target
            None if self.strict => None,
            None => Some(("?", recv_display(base))),
        }
    }
}

fn recv_display(base: &syn::Expr) -> String {
    match base {
        syn::Expr::Path(p) => path_to_string(&p.path),
        _ => "<expr>".to_string(),
    }
}

impl<'ast, 'a> Visit<'ast> for FieldVisitor<'a> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.scope.enter_mod(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.scope.leave_mod();
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.scope
            .enter_fn(i.sig.ident.to_string(), fn_span(&i.sig, &i.block));
        self.fn_types_stack
            .push(FnTypes::build(
                &i.sig,
                &i.block,
                self.fn_sigs,
                self.scope.impl_stack.last().map(String::as_str),
            ));
        visit::visit_item_fn(self, i);
        self.fn_types_stack.pop();
        self.scope.leave_fn();
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.scope.enter_impl(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.scope.leave_impl();
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.scope
            .enter_fn(i.sig.ident.to_string(), fn_span(&i.sig, &i.block));
        self.fn_types_stack
            .push(FnTypes::build(
                &i.sig,
                &i.block,
                self.fn_sigs,
                self.scope.impl_stack.last().map(String::as_str),
            ));
        visit::visit_impl_item_fn(self, i);
        self.fn_types_stack.pop();
        self.scope.leave_fn();
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.scope.enter_trait(i.ident.to_string());
        visit::visit_item_trait(self, i);
        self.scope.leave_trait();
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        let Some(body) = &i.default else { return };
        self.scope
            .enter_fn(i.sig.ident.to_string(), fn_span(&i.sig, body));
        self.fn_types_stack
            .push(FnTypes::build(&i.sig, body, self.fn_sigs, None));
        visit::visit_trait_item_fn(self, i);
        self.fn_types_stack.pop();
        self.scope.leave_fn();
    }

    fn visit_expr_assign(&mut self, e: &'ast syn::ExprAssign) {
        self.in_write_lhs = true;
        self.visit_expr(&e.left);
        self.in_write_lhs = false;
        self.visit_expr(&e.right);
    }

    fn visit_expr_binary(&mut self, e: &'ast syn::ExprBinary) {
        let is_compound = matches!(
            e.op,
            syn::BinOp::AddAssign(_)
                | syn::BinOp::SubAssign(_)
                | syn::BinOp::MulAssign(_)
                | syn::BinOp::DivAssign(_)
                | syn::BinOp::RemAssign(_)
                | syn::BinOp::BitXorAssign(_)
                | syn::BinOp::BitAndAssign(_)
                | syn::BinOp::BitOrAssign(_)
                | syn::BinOp::ShlAssign(_)
                | syn::BinOp::ShrAssign(_)
        );
        if is_compound {
            self.in_write_lhs = true;
            self.visit_expr(&e.left);
            self.in_write_lhs = false;
            self.visit_expr(&e.right);
        } else {
            visit::visit_expr_binary(self, e);
        }
    }

    fn visit_expr_reference(&mut self, e: &'ast syn::ExprReference) {
        if e.mutability.is_some() {
            let saved = self.in_write_lhs;
            self.in_write_lhs = true;
            self.visit_expr(&e.expr);
            self.in_write_lhs = saved;
        } else {
            visit::visit_expr_reference(self, e);
        }
    }

    fn visit_expr_field(&mut self, e: &'ast syn::ExprField) {
        let syn::Member::Named(id) = &e.member else {
            visit::visit_expr_field(self, e);
            return;
        };
        if id != self.target_field {
            visit::visit_expr_field(self, e);
            return;
        }
        let is_write = self.in_write_lhs;
        let was_write = self.in_write_lhs;
        self.in_write_lhs = false;
        let kind = if is_write { FieldKind::Write } else { FieldKind::Read };

        if let Some((via, recv)) = self.classify_base(&e.base) {
            self.record(kind, via, line_of(id), recv);
        }

        self.visit_expr(&e.base);
        self.in_write_lhs = was_write;
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        let lit_ty = path_last(&e.path);
        let resolved_ty = if lit_ty == "Self" {
            self.scope
                .impl_stack
                .last()
                .cloned()
                .unwrap_or_else(|| lit_ty.clone())
        } else {
            lit_ty.clone()
        };
        if resolved_ty == self.target_type {
            for fv in &e.fields {
                if let syn::Member::Named(id) = &fv.member {
                    if id == self.target_field {
                        let line = line_of(id);
                        self.record(FieldKind::Init, "init", line, resolved_ty.clone());
                    }
                }
            }
        }
        visit::visit_expr_struct(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        for expr in crate::macro_scan::macro_exprs(m) {
            self.visit_expr(&expr);
        }
    }
}

/// `--kind` filter values for `field-uses`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FieldKind {
    Read,
    Write,
    Init,
}

impl FieldKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FieldKind::Read => "read",
            FieldKind::Write => "write",
            FieldKind::Init => "init",
        }
    }
}

/// Strict-mode (self / init / type-inferred) read/write/init counts for one
/// (Type, field) pair. `fields` aggregates with this, so its per-field counts
/// always equal the sum of `field-uses` strict rows for the same field.
pub fn count_kinds(
    files: &[ParsedFile],
    ty: &str,
    field: &str,
    fn_sigs: &FnSigIndex,
) -> (usize, usize, usize) {
    let (mut reads, mut writes, mut inits) = (0, 0, 0);
    for h in collect(files, ty, field, true, fn_sigs, false) {
        match h.kind {
            FieldKind::Read => reads += 1,
            FieldKind::Write => writes += 1,
            FieldKind::Init => inits += 1,
        }
    }
    (reads, writes, inits)
}

fn collect(
    files: &[ParsedFile],
    ty: &str,
    field: &str,
    strict: bool,
    fn_sigs: &FnSigIndex,
    spans: bool,
) -> Vec<FieldHit> {
    let mut all = Vec::new();
    for f in files {
        let mut v = FieldVisitor {
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()).with_spans(spans),
            target_type: ty,
            target_field: field,
            strict,
            fn_types_stack: Vec::new(),
            in_write_lhs: false,
            fn_sigs,
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }
    all
}

/// Confidence tier of a `via` label: `self`/`init` are structurally certain,
/// `ti` came from local type inference, `?` is receiver-unknown.
fn conf_of_via(via: &str) -> Confidence {
    match via {
        "self" | "init" => Confidence::Exact,
        "ti" => Confidence::Inferred,
        _ => Confidence::Heuristic,
    }
}

pub fn run(
    ctx: &AnalysisCtx,
    ty: &str,
    field: &str,
    strict: bool,
    kinds: &[FieldKind],
    via_receiver: Option<&str>,
    min_confidence: Option<Confidence>,
) -> anyhow::Result<usize> {
    let files = ctx.files;
    let fn_sigs = &ctx.sem.fn_sigs;
    let summary = ctx.summary;
    let known = ctx.idx.knows_name(ty);
    if !known {
        warn_unknown_target("type", ty);
    }
    let mut all = collect(files, ty, field, strict, fn_sigs, ctx.spans);

    if !kinds.is_empty() {
        all.retain(|h| kinds.contains(&h.kind));
    }
    if let Some(pat) = via_receiver {
        all.retain(|h| h.receiver.contains(pat));
    }
    if let Some(min) = min_confidence {
        all.retain(|h| conf_of_via(h.via) >= min);
    }
    ctx.retain_changed(&mut all, |h| &h.file);

    all.sort_by(|a, b| {
        a.kind
            .as_str()
            .cmp(b.kind.as_str())
            .then_with(|| a.via.cmp(b.via))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    let mut reads = 0usize;
    let mut writes = 0usize;
    let mut inits = 0usize;
    let mut ti_count = 0usize;
    let mut q_count = 0usize;
    for h in &all {
        match h.kind {
            FieldKind::Read => reads += 1,
            FieldKind::Write => writes += 1,
            FieldKind::Init => inits += 1,
        }
        match h.via {
            "ti" => ti_count += 1,
            "?" => q_count += 1,
            _ => {}
        }
        if !summary {
            println!(
                "{}\t{}\t{}\t{}\t{}:{}",
                h.kind.as_str(),
                h.via,
                h.receiver,
                h.context,
                h.file,
                h.line
            );
            ctx.print_context(&h.file, h.line);
        }
    }
    eprintln!(
        "({} reads, {} writes, {} inits; via: {} type-inferred, {} unknown receiver; strict={})",
        reads, writes, inits, ti_count, q_count, strict
    );

    if strict && all.is_empty() && via_receiver.is_none() && kinds.is_empty() {
        let cand = collect(files, ty, field, false, fn_sigs, false);
        if !cand.is_empty() {
            eprintln!(
                "hint: strict matched 0; --candidates would report {} hit(s) (mostly unknown receivers). \
                 Try `--candidates` or `--candidates --via-receiver <substring>`.",
                cand.len()
            );
        }
    }
    if !known && all.is_empty() {
        return Err(TargetNotFound::err("type", ty));
    }
    Ok(all.len())
}
