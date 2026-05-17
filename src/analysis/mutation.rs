//! Collector: per-function field reads, writes, and mutable borrows.
//!
//! Uses rustc's MIR `Visitor` to find every `Place` access. For each place
//! whose projection chain ends in a `Field`, we record one `FieldAccessFact`
//! against the *innermost* field — that's the field the caller actually
//! "touched" in the source. The access kind (Read/Write/MutBorrow) comes from
//! the `PlaceContext` rustc gives us.
//!
//! Aggregation (writers-per-field, modules-touched, thresholds) is the
//! viewer's job; we only emit raw events.

use rustc_hir::def::DefKind;
use rustc_middle::mir::visit::{
    MutatingUseContext, NonMutatingUseContext, PlaceContext, Visitor as MirVisitor,
};
use rustc_middle::mir::{Body, Location, Place, ProjectionElem};
use rustc_middle::ty::TyCtxt;

use crate::facts::{AccessKind, CrateFacts, FieldAccessFact};

pub fn collect<'tcx>(tcx: TyCtxt<'tcx>, facts: &mut CrateFacts) {
    for local_def_id in tcx.mir_keys(()) {
        let def_id = local_def_id.to_def_id();
        let kind = tcx.def_kind(def_id);
        if !matches!(kind, DefKind::Fn | DefKind::AssocFn) {
            continue;
        }
        if !tcx.is_mir_available(def_id) {
            continue;
        }
        let body = tcx.optimized_mir(def_id);
        let caller = tcx.def_path_str(def_id);

        let mut v = FieldVisit { tcx, body, caller, facts };
        v.visit_body(body);
    }
}

struct FieldVisit<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'tcx Body<'tcx>,
    caller: String,
    facts: &'a mut CrateFacts,
}

impl<'a, 'tcx> MirVisitor<'tcx> for FieldVisit<'a, 'tcx> {
    fn visit_place(&mut self, place: &Place<'tcx>, context: PlaceContext, location: Location) {
        let kind = match context {
            PlaceContext::MutatingUse(
                MutatingUseContext::Store
                | MutatingUseContext::Call
                | MutatingUseContext::AsmOutput
                | MutatingUseContext::Yield
                | MutatingUseContext::SetDiscriminant
                | MutatingUseContext::Drop,
            ) => Some(AccessKind::Write),
            PlaceContext::MutatingUse(MutatingUseContext::Borrow) => Some(AccessKind::MutBorrow),
            PlaceContext::NonMutatingUse(
                NonMutatingUseContext::Copy
                | NonMutatingUseContext::Move
                | NonMutatingUseContext::Inspect
                | NonMutatingUseContext::SharedBorrow
                | NonMutatingUseContext::FakeBorrow,
            ) => Some(AccessKind::Read),
            _ => None,
        };

        if let Some(kind) = kind {
            // Find the innermost (rightmost) Field projection in the chain.
            // That's the field whose semantics this access affects.
            let mut last_field: Option<(_, _)> = None;
            for (parent_ref, proj) in place.iter_projections() {
                if let ProjectionElem::Field(field_idx, _) = proj {
                    last_field = Some((parent_ref, field_idx));
                }
            }
            if let Some((parent_ref, field_idx)) = last_field {
                let parent_ty = parent_ref.ty(self.body, self.tcx).ty;
                if let rustc_middle::ty::TyKind::Adt(adt_def, _) = parent_ty.kind() {
                    if adt_def.is_struct() {
                        let did = adt_def.did();
                        let variant = adt_def.non_enum_variant();
                        if let Some(fdef) = variant.fields.get(field_idx) {
                            let span = self.body.source_info(location).span;
                            let (file, line) = super::util::span_to_file_line(self.tcx, span);
                            self.facts.field_accesses.push(FieldAccessFact {
                                caller: self.caller.clone(),
                                struct_def_path: self.tcx.def_path_str(did),
                                field_name: fdef.name.to_string(),
                                kind,
                                file,
                                line,
                            });
                        }
                    }
                }
            }
        }

        self.super_place(place, context, location);
    }
}

#[allow(dead_code)]
pub fn run<'tcx>(_tcx: TyCtxt<'tcx>, _reporter: &mut crate::report::Reporter) {
    // Legacy stub kept for backwards compatibility.
}
