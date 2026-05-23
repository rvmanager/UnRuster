use syn::visit::{self, Visit};

use crate::ast::{line_of, path_last, path_to_string, type_short};
use crate::parse::{display_path, ParsedFile};
use crate::semantic::{FnSigIndex, FnTypes};

#[derive(Debug)]
struct FieldHit {
    kind: &'static str, // "read" | "write" | "init"
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
    module: &'a str,
    target_type: &'a str,
    target_field: &'a str,
    /// strict: only emit "self" / "init" / "ti" hits.
    /// non-strict (candidates): also emit "?" hits.
    strict: bool,

    impl_stack: Vec<String>,
    fn_stack: Vec<String>,
    /// Per-fn-body type maps. Pushed on fn entry, popped on exit. The last
    /// entry is the currently-enclosing fn.
    fn_types_stack: Vec<FnTypes>,
    mod_stack: Vec<String>,
    in_write_lhs: bool,

    fn_sigs: &'a FnSigIndex,
    hits: Vec<FieldHit>,
}

impl<'a> FieldVisitor<'a> {
    fn enclosing(&self) -> String {
        let mut path: Vec<String> = Vec::new();
        if !self.module.is_empty() {
            path.push(self.module.to_string());
        }
        path.extend(self.mod_stack.iter().cloned());
        if let Some(t) = self.impl_stack.last() {
            path.push(t.clone());
        }
        if let Some(f) = self.fn_stack.last() {
            path.push(f.clone());
        }
        if path.is_empty() {
            "<top-level>".into()
        } else {
            path.join("::")
        }
    }

    fn in_target_impl(&self) -> bool {
        self.impl_stack
            .last()
            .map(|t| t == self.target_type)
            .unwrap_or(false)
    }

    fn record(&mut self, kind: &'static str, via: &'static str, line: usize, receiver: String) {
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
                let owner = self.impl_stack.last().cloned().unwrap_or_default();
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
        self.mod_stack.push(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.mod_stack.pop();
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        self.fn_types_stack
            .push(FnTypes::build(&i.sig, &i.block, self.fn_sigs));
        visit::visit_item_fn(self, i);
        self.fn_types_stack.pop();
        self.fn_stack.pop();
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.impl_stack.push(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.impl_stack.pop();
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.fn_stack.push(i.sig.ident.to_string());
        self.fn_types_stack
            .push(FnTypes::build(&i.sig, &i.block, self.fn_sigs));
        visit::visit_impl_item_fn(self, i);
        self.fn_types_stack.pop();
        self.fn_stack.pop();
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
        let kind: &'static str = if is_write { "write" } else { "read" };

        if let Some((via, recv)) = self.classify_base(&e.base) {
            self.record(kind, via, line_of(id), recv);
        }

        self.visit_expr(&e.base);
        self.in_write_lhs = was_write;
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        let lit_ty = path_last(&e.path);
        let resolved_ty = if lit_ty == "Self" {
            self.impl_stack
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
                        self.record("init", "init", line, resolved_ty.clone());
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

fn collect(
    files: &[ParsedFile],
    ty: &str,
    field: &str,
    strict: bool,
    fn_sigs: &FnSigIndex,
) -> Vec<FieldHit> {
    let mut all = Vec::new();
    for f in files {
        let mut v = FieldVisitor {
            file: &display_path(&f.path),
            module: &f.module,
            target_type: ty,
            target_field: field,
            strict,
            impl_stack: Vec::new(),
            fn_stack: Vec::new(),
            fn_types_stack: Vec::new(),
            mod_stack: Vec::new(),
            in_write_lhs: false,
            fn_sigs,
            hits: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.hits);
    }
    all
}

pub fn run(
    files: &[ParsedFile],
    fn_sigs: &FnSigIndex,
    ty: &str,
    field: &str,
    strict: bool,
    kinds: &[&str],
    via_receiver: Option<&str>,
    summary: bool,
) -> anyhow::Result<()> {
    let mut all = collect(files, ty, field, strict, fn_sigs);

    if !kinds.is_empty() {
        all.retain(|h| kinds.contains(&h.kind));
    }
    if let Some(pat) = via_receiver {
        all.retain(|h| h.receiver.contains(pat));
    }

    all.sort_by(|a, b| {
        a.kind
            .cmp(b.kind)
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
            "read" => reads += 1,
            "write" => writes += 1,
            "init" => inits += 1,
            _ => {}
        }
        match h.via {
            "ti" => ti_count += 1,
            "?" => q_count += 1,
            _ => {}
        }
        if !summary {
            println!(
                "{}\t{}\t{}\t{}\t{}:{}",
                h.kind, h.via, h.receiver, h.context, h.file, h.line
            );
        }
    }
    eprintln!(
        "({} reads, {} writes, {} inits; via: {} type-inferred, {} unknown receiver; strict={})",
        reads, writes, inits, ti_count, q_count, strict
    );

    if strict && all.is_empty() && via_receiver.is_none() && kinds.is_empty() {
        let cand = collect(files, ty, field, false, fn_sigs);
        if !cand.is_empty() {
            eprintln!(
                "hint: strict matched 0; --candidates would report {} hit(s) (mostly unknown receivers). \
                 Try `--candidates` or `--candidates --via-receiver <substring>`.",
                cand.len()
            );
        }
    }
    Ok(())
}
