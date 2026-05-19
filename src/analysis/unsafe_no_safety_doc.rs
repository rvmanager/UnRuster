//! Detector: `pub unsafe fn` without a `# Safety` doc section.
//!
//! RuleBook §16.4: every public `unsafe fn` must document the obligations
//! the caller is responsible for upholding. The convention is a rustdoc
//! `# Safety` heading. We flag two cases:
//!
//! * the function has no doc comment at all;
//! * the function is documented, but the doc has no `# Safety` heading.

use rustc_hir as hir;
use rustc_hir::def_id::LocalDefId;
use rustc_hir::intravisit::{self, Visitor};
use rustc_middle::hir::nested_filter;
use rustc_middle::ty::TyCtxt;

use crate::facts::{CrateFacts, UnsafeNoSafetyDocFact};
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
            check_fn(self.tcx, item.owner_id.def_id, sig, self.reporter, self.facts);
        }
        intravisit::walk_item(self, item);
    }

    fn visit_impl_item(&mut self, ii: &'tcx hir::ImplItem<'tcx>) {
        if let hir::ImplItemKind::Fn(sig, _) = &ii.kind {
            check_fn(self.tcx, ii.owner_id.def_id, sig, self.reporter, self.facts);
        }
        intravisit::walk_impl_item(self, ii);
    }

    fn visit_trait_item(&mut self, ti: &'tcx hir::TraitItem<'tcx>) {
        if let hir::TraitItemKind::Fn(sig, _) = &ti.kind {
            check_fn(self.tcx, ti.owner_id.def_id, sig, self.reporter, self.facts);
        }
        intravisit::walk_trait_item(self, ti);
    }
}

fn check_fn<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
    sig: &'tcx hir::FnSig<'tcx>,
    reporter: &mut Reporter,
    facts: &mut CrateFacts,
) {
    if !tcx.visibility(def_id).is_public() {
        return;
    }
    if !is_unsafe_header(&sig.header) {
        return;
    }

    let (has_doc, has_safety) = scan_doc_for_safety(tcx, def_id);
    if has_safety {
        return;
    }

    let message = if has_doc {
        "public `unsafe fn` is documented but has no `# Safety` section"
    } else {
        "public `unsafe fn` is undocumented; callers cannot know its invariants"
    };
    reporter
        .warn("unsafe_no_safety_doc", sig.span, message)
        .with_help(
            "add a `# Safety` rustdoc section listing the obligations the caller \
             must uphold for this call to be sound",
        );

    let (file, line) = super::util::span_to_file_line(tcx, sig.span);
    facts.unsafe_no_safety_docs.push(UnsafeNoSafetyDocFact {
        function: tcx.def_path_str(def_id.to_def_id()),
        undocumented: !has_doc,
        file,
        line,
    });
}

/// True if the function header is declared `unsafe fn`.
///
/// The shape of `FnHeader.safety` shifts across nightlies. We pattern-match
/// against the `Debug` rendering as a stable-enough fallback, but try the
/// direct field first.
fn is_unsafe_header(header: &hir::FnHeader) -> bool {
    // `header.safety` is `HeaderSafety` on recent nightlies, which wraps a
    // `Safety` enum (`Safe` / `Unsafe`). Rendering it via Debug includes
    // the substring "Unsafe" for any unsafe-fn case (plain or `target_feature`).
    format!("{:?}", header.safety).contains("Unsafe")
}

/// Returns `(has_any_doc, has_safety_heading)`.
fn scan_doc_for_safety(tcx: TyCtxt<'_>, def_id: LocalDefId) -> (bool, bool) {
    let hir_id = tcx.local_def_id_to_hir_id(def_id);
    let attrs = tcx.hir_attrs(hir_id);
    let mut doc = String::new();
    for attr in attrs {
        if let Some(sym) = attr.doc_str() {
            doc.push_str(sym.as_str());
            doc.push('\n');
        }
    }
    let has_doc = !doc.trim().is_empty();
    let has_safety = doc.lines().any(is_safety_heading);
    (has_doc, has_safety)
}

fn is_safety_heading(line: &str) -> bool {
    let line = line.trim_start();
    if !line.starts_with('#') {
        return false;
    }
    let rest = line.trim_start_matches('#').trim();
    rest.eq_ignore_ascii_case("safety")
}
