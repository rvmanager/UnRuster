//! Detector: internal container types in public APIs.
//!
//! Flags `pub fn` / `pub` impl-methods / trait-method signatures that return
//! `&Vec<T>`, `&HashMap<K,V>`, etc. These leak the concrete owning container
//! into the API surface when a slice or iterator would suffice — callers can
//! then depend on container-specific methods, preventing later refactors.

use rustc_hir as hir;
use rustc_hir::def_id::LocalDefId;
use rustc_hir::intravisit::{self, Visitor};
use rustc_middle::hir::nested_filter;
use rustc_middle::ty::TyCtxt;
use rustc_span::Span;

use crate::facts::{ApiLeakFact, CrateFacts};
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
    if let hir::FnRetTy::Return(ret_ty) = decl.output {
        if let hir::TyKind::Ref(_, mut_ty) = &ret_ty.kind {
            if let Some((name, span)) = path_last_segment(mut_ty.ty) {
                let is_mut = matches!(mut_ty.mutbl, hir::Mutability::Mut);
                if report_if_leaky(name, span, is_mut, reporter) {
                    let (file, line) = super::util::span_to_file_line(tcx, span);
                    facts.api_leaks.push(ApiLeakFact {
                        function: tcx.def_path_str(def_id.to_def_id()),
                        container: name.to_string(),
                        is_mut,
                        file,
                        line,
                    });
                }
            }
        }
    }
}


fn path_last_segment<'tcx>(ty: &'tcx hir::Ty<'tcx>) -> Option<(&'tcx str, Span)> {
    if let hir::TyKind::Path(hir::QPath::Resolved(_, path)) = &ty.kind {
        let seg = path.segments.last()?;
        return Some((seg.ident.as_str(), seg.ident.span));
    }
    None
}

fn report_if_leaky(name: &str, span: Span, is_mut: bool, reporter: &mut Reporter) -> bool {
    let help = match name {
        "Vec" => "return `&[T]` instead — callers rarely need `Vec`-specific methods",
        "String" => "return `&str` instead",
        "VecDeque" | "LinkedList" => "return `impl Iterator<Item = &T>` instead of the concrete container",
        "HashMap" | "BTreeMap" => {
            "expose getters and an iterator instead of returning the whole map by reference"
        }
        "HashSet" | "BTreeSet" => {
            "expose `contains(&T)` and an iterator instead of returning the set by reference"
        }
        _ => return false,
    };
    let kind = if is_mut { "&mut" } else { "&" };
    reporter
        .warn(
            "api_leak",
            span,
            format!(
                "public API returns `{kind} {name}<…>`; the internal container type leaks into the signature"
            ),
        )
        .with_help(help);
    true
}
