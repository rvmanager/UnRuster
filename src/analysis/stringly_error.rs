//! Detector: stringly-typed errors in public APIs.
//!
//! Flags public functions whose return type is `Result<_, String>` or
//! `Result<_, &str>`. The string form throws away structure: callers can no
//! longer match on error kind, attach typed context, or rely on `?` to
//! convert into their own error enum. RuleBook §4.5 + §12.

use rustc_hir as hir;
use rustc_hir::def_id::LocalDefId;
use rustc_hir::intravisit::{self, Visitor};
use rustc_middle::hir::nested_filter;
use rustc_middle::ty::TyCtxt;
use rustc_span::Span;

use crate::facts::{CrateFacts, StringlyErrorFact};
use crate::report::Reporter;

pub fn run<'tcx>(tcx: TyCtxt<'tcx>, reporter: &mut Reporter, facts: &mut CrateFacts) {
    let mut v = Visit { tcx, reporter, facts };
    tcx.hir_visit_all_item_likes_in_crate(&mut v);
}

struct Visit<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    reporter: &'a mut Reporter,
    facts: &'a mut CrateFacts,
}

impl<'a, 'tcx> Visitor<'tcx> for Visit<'a, 'tcx> {
    type NestedFilter = nested_filter::OnlyBodies;

    fn maybe_tcx(&mut self) -> Self::MaybeTyCtxt {
        self.tcx
    }

    fn visit_item(&mut self, item: &'tcx hir::Item<'tcx>) {
        if let hir::ItemKind::Fn { sig, .. } = &item.kind {
            check_signature(self.tcx, item.owner_id.def_id, sig.decl, self.reporter, self.facts);
        }
        intravisit::walk_item(self, item);
    }

    fn visit_impl_item(&mut self, ii: &'tcx hir::ImplItem<'tcx>) {
        if let hir::ImplItemKind::Fn(sig, _) = &ii.kind {
            check_signature(self.tcx, ii.owner_id.def_id, sig.decl, self.reporter, self.facts);
        }
        intravisit::walk_impl_item(self, ii);
    }

    fn visit_trait_item(&mut self, ti: &'tcx hir::TraitItem<'tcx>) {
        if let hir::TraitItemKind::Fn(sig, _) = &ti.kind {
            check_signature(self.tcx, ti.owner_id.def_id, sig.decl, self.reporter, self.facts);
        }
        intravisit::walk_trait_item(self, ti);
    }
}

fn check_signature<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
    decl: &'tcx hir::FnDecl<'tcx>,
    reporter: &mut Reporter,
    facts: &mut CrateFacts,
) {
    if !tcx.visibility(def_id).is_public() {
        return;
    }
    let hir::FnRetTy::Return(ret_ty) = decl.output else { return };
    let Some((err_form, span)) = result_error_form(ret_ty) else { return };

    reporter
        .warn(
            "stringly_error",
            span,
            format!(
                "public API returns `Result<_, {err_form}>`; the error type carries no structure"
            ),
        )
        .with_help(
            "define an error enum (e.g. with `thiserror`) so callers can match on \
             specific variants and `?` can convert across error types",
        );

    let (file, line) = super::util::span_to_file_line(tcx, span);
    facts.stringly_errors.push(StringlyErrorFact {
        function: tcx.def_path_str(def_id.to_def_id()),
        error_form: err_form.to_string(),
        file,
        line,
    });
}

/// If `ty` is `Result<_, String>` or `Result<_, &str>`, return the textual
/// form of the error half plus the span pointing at it. Returns `None`
/// otherwise. We deliberately use a purely syntactic match on HIR — this
/// avoids depending on type inference being complete and keeps the lint
/// fast and predictable.
fn result_error_form<'tcx>(ty: &'tcx hir::Ty<'tcx>) -> Option<(&'static str, Span)> {
    let hir::TyKind::Path(hir::QPath::Resolved(_, path)) = &ty.kind else {
        return None;
    };
    let seg = path.segments.last()?;
    if seg.ident.as_str() != "Result" {
        return None;
    }
    let args = seg.args?;
    // Skip lifetimes/const args; we want the type args in order.
    let mut type_args = args.args.iter().filter_map(|a| match a {
        hir::GenericArg::Type(t) => Some(*t),
        _ => None,
    });
    let _ok = type_args.next()?;
    let err_ty = type_args.next()?;

    match &err_ty.kind {
        hir::TyKind::Path(hir::QPath::Resolved(_, p)) => {
            let last = p.segments.last()?;
            if last.ident.as_str() == "String" {
                Some(("String", last.ident.span))
            } else {
                None
            }
        }
        hir::TyKind::Ref(_, mut_ty) => {
            // `&str`, possibly with a lifetime.
            if let hir::TyKind::Path(hir::QPath::Resolved(_, p)) = &mut_ty.ty.kind {
                let last = p.segments.last()?;
                if last.ident.as_str() == "str" {
                    return Some(("&str", err_ty.span));
                }
            }
            None
        }
        _ => None,
    }
}
