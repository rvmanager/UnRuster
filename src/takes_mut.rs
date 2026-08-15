use syn::visit::{self, Visit};

use crate::ast::{is_mut_ref, line_of, scope_visits, ScopeTracker, type_last_segment, type_short, type_to_string};
use crate::context::{AnalysisCtx, TargetNotFound};
use crate::parse::display_path;
use crate::emit::{row, site};

#[derive(Debug)]
struct Hit {
    file: String,
    line: usize,
    qpath: String,
    params: Vec<String>, // formatted as `name: &mut Type`
}

struct TakesMutVisitor<'a> {
    target: &'a str,
    file: &'a str,
    scope: ScopeTracker,
    out: Vec<Hit>,
}

impl<'a> TakesMutVisitor<'a> {
    fn qualify(&self, name: &str) -> String {
        self.scope.qualify(name)
    }

    fn check_sig(&mut self, sig: &syn::Signature) {
        let hits: Vec<String> = sig
            .inputs
            .iter()
            .filter_map(|input| self.input_hit(input))
            .collect();
        if hits.is_empty() {
            return;
        }
        let qpath = self.qualify(&sig.ident.to_string());
        self.out.push(Hit {
            file: self.file.to_string(),
            line: line_of(&sig.ident),
            qpath,
            params: hits,
        });
    }

    fn input_hit(&self, input: &syn::FnArg) -> Option<String> {
        match input {
            syn::FnArg::Receiver(r) => self.receiver_hit(r),
            syn::FnArg::Typed(t) => self.typed_hit(t),
        }
    }

    /// `&mut self` only counts when the enclosing impl is for the target type.
    fn receiver_hit(&self, r: &syn::Receiver) -> Option<String> {
        if r.mutability.is_none() || r.reference.is_none() {
            return None;
        }
        let in_target = self
            .scope
            .impl_stack
            .last()
            .map(|t| t == self.target)
            .unwrap_or(false);
        if !in_target {
            return None;
        }
        Some("&mut self".to_string())
    }

    /// `name: &mut Type` where last-segment of the type matches the target.
    fn typed_hit(&self, t: &syn::PatType) -> Option<String> {
        if !is_mut_ref(&t.ty) {
            return None;
        }
        let last = type_last_segment(&t.ty)?;
        if last != self.target {
            return None;
        }
        let pname = match &*t.pat {
            syn::Pat::Ident(p) => p.ident.to_string(),
            _ => "_".to_string(),
        };
        Some(format!("{}: {}", pname, type_to_string(&t.ty)))
    }
}

impl<'ast, 'a> Visit<'ast> for TakesMutVisitor<'a> {
    scope_visits!(item_mod, item_impl, item_trait);
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.check_sig(&i.sig);
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.check_sig(&i.sig);
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.check_sig(&i.sig);
    }
}

/// `takes-mut` with no type argument. Erroring here cost a full round-trip:
/// the caller has to guess a type name before the tool will tell them anything,
/// and "which type has the biggest mutation surface" is usually the actual
/// question. Ranks every type in the tree by how many fns take `&mut` it.
pub fn run_candidates(ctx: &AnalysisCtx) -> anyhow::Result<usize> {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for f in ctx.files {
        let mut v = MutSurfaceVisitor {
            scope: ScopeTracker::new(f.module.as_str()),
            counts: &mut counts,
        };
        v.visit_file(&f.ast);
    }
    let mut rows: Vec<(String, usize)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if !ctx.summary {
        for (ty, n) in &rows {
            row!(ctx.out, "mut_fns" => *n, "type" => ty.clone());
        }
    }
    ctx.out.summary(&format!(
        "({} type(s) with a `&mut` surface; run `unruster takes-mut <Type>` for the sites)",
        rows.len()
    ));
    Ok(rows.len())
}

/// Counts `&mut T` parameters and `&mut self` receivers per type name.
struct MutSurfaceVisitor<'a> {
    scope: ScopeTracker,
    counts: &'a mut std::collections::BTreeMap<String, usize>,
}

impl MutSurfaceVisitor<'_> {
    fn tally(&mut self, sig: &syn::Signature) {
        let mut seen: Vec<String> = Vec::new();
        for input in &sig.inputs {
            let ty = match input {
                syn::FnArg::Receiver(r) => {
                    if r.mutability.is_none() || r.reference.is_none() {
                        continue;
                    }
                    self.scope.impl_stack.last().cloned()
                }
                syn::FnArg::Typed(t) if is_mut_ref(&t.ty) => type_last_segment(&t.ty),
                syn::FnArg::Typed(_) => None,
            };
            // One fn counts once per type, even if it takes two `&mut T`s.
            if let Some(t) = ty {
                if !seen.contains(&t) {
                    *self.counts.entry(t.clone()).or_insert(0) += 1;
                    seen.push(t);
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for MutSurfaceVisitor<'_> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.scope.enter_mod(i.ident.to_string());
        visit::visit_item_mod(self, i);
        self.scope.leave_mod();
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.scope.enter_impl(type_short(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.scope.leave_impl();
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.tally(&i.sig);
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.tally(&i.sig);
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.tally(&i.sig);
    }
}

pub fn run(ctx: &AnalysisCtx, ty: &str) -> anyhow::Result<usize> {
    // `TakesMutVisitor` compares against `type_last_segment` of the parameter
    // and against a bare name on the impl stack, so the target has to arrive in
    // the same shape — the convention `fields`/`field-uses`/`variants` follow.
    let ty = crate::ast::last_segment(ty);
    let files = ctx.files;
    let index = ctx.idx;
    let summary = ctx.summary;
    let known = index.knows_name(ty);
    if !known {
        ctx.warn_unknown("type", ty);
    }
    let mut all: Vec<Hit> = Vec::new();
    for f in files {
        let mut v = TakesMutVisitor {
            target: ty,
            file: &display_path(&f.path),
            scope: ScopeTracker::new(f.module.as_str()),
            out: Vec::new(),
        };
        v.visit_file(&f.ast);
        all.extend(v.out);
    }
    ctx.retain_changed(&mut all, |h| &h.file);
    all.sort_by(|a, b| a.qpath.cmp(&b.qpath).then_with(|| a.file.cmp(&b.file)));

    if !summary {
        for h in &all {
            row!(
                ctx.out,
                "qpath" => h.qpath.clone(),
                "params" => h.params.join(", "),
                "at" => site(&h.file, h.line),
            );
        }
    }
    ctx.out.summary(&format!("({} fn(s) take `&mut {}`)", all.len(), ty));
    if !known && all.is_empty() {
        return Err(TargetNotFound::err("type", ty));
    }
    Ok(all.len())
}
