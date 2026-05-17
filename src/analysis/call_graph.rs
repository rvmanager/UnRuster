//! Collector: function inventory, struct inventory, call edges.
//!
//! Walks all bodies with MIR, records one `FunctionFact` per fn-like item, and
//! one `CallEdge` per direct (statically-resolvable) call terminator. Walks
//! HIR items to record struct definitions and their fields.

use rustc_hir as hir;
use rustc_hir::def::DefKind;
use rustc_hir::def_id::{DefId, LOCAL_CRATE};
use rustc_hir::intravisit::{self, Visitor};
use rustc_middle::hir::nested_filter;
use rustc_middle::mir::TerminatorKind;
use rustc_middle::ty::TyCtxt;

use crate::facts::{CallEdge, CrateFacts, FieldDef, FunctionFact, StructFact};

pub fn collect<'tcx>(tcx: TyCtxt<'tcx>, facts: &mut CrateFacts) {
    collect_structs(tcx, facts);
    collect_fns_and_calls(tcx, facts);
}

fn collect_structs<'tcx>(tcx: TyCtxt<'tcx>, facts: &mut CrateFacts) {
    let mut v = StructVisit { tcx, facts };
    tcx.hir_visit_all_item_likes_in_crate(&mut v);
}

struct StructVisit<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    facts: &'a mut CrateFacts,
}

impl<'a, 'tcx> Visitor<'tcx> for StructVisit<'a, 'tcx> {
    type NestedFilter = nested_filter::OnlyBodies;
    fn maybe_tcx(&mut self) -> Self::MaybeTyCtxt {
        self.tcx
    }

    fn visit_item(&mut self, item: &'tcx hir::Item<'tcx>) {
        if let hir::ItemKind::Struct(_ident, _generics, variant_data) = &item.kind {
            let def_id = item.owner_id.def_id.to_def_id();
            let def_path = self.tcx.def_path_str(def_id);
            let (file, line) = super::util::span_to_file_line(self.tcx, item.span);
            let module_path = module_of(&def_path);
            let fields = variant_data
                .fields()
                .iter()
                .map(|f| FieldDef {
                    name: f.ident.to_string(),
                    is_public: matches!(f.vis_span.is_dummy(), false)
                        || self.tcx.visibility(f.def_id.to_def_id()).is_public(),
                })
                .collect();
            self.facts.structs.push(StructFact {
                def_path,
                module_path,
                file,
                line,
                fields,
            });
        }
        intravisit::walk_item(self, item);
    }
}

fn collect_fns_and_calls<'tcx>(tcx: TyCtxt<'tcx>, facts: &mut CrateFacts) {
    for local_def_id in tcx.mir_keys(()) {
        let def_id = local_def_id.to_def_id();
        let kind = tcx.def_kind(def_id);
        if !matches!(kind, DefKind::Fn | DefKind::AssocFn) {
            continue;
        }

        let caller_path = tcx.def_path_str(def_id);
        let (file, line) = super::util::span_to_file_line(tcx, tcx.def_span(def_id));
        let module_path = module_of(&caller_path);
        let is_public = tcx.visibility(def_id).is_public();
        let is_test = has_test_attr(tcx, def_id);
        let param_count = tcx
            .fn_sig(def_id)
            .skip_binder()
            .inputs()
            .skip_binder()
            .len();

        facts.functions.push(FunctionFact {
            def_path: caller_path.clone(),
            module_path,
            file,
            line,
            is_public,
            is_test,
            param_count,
        });

        // Skip walking MIR for items in foreign crates (shouldn't happen for
        // mir_keys, but defensive) and for items that don't actually have MIR
        // available (some traits etc.).
        if !tcx.is_mir_available(def_id) {
            continue;
        }
        let body = tcx.optimized_mir(def_id);
        for bb in body.basic_blocks.iter() {
            let Some(term) = &bb.terminator else { continue };
            if let TerminatorKind::Call { func, .. } = &term.kind {
                if let Some(callee_did) = callee_def_id(tcx, func) {
                    facts.calls.push(CallEdge {
                        caller: caller_path.clone(),
                        callee: tcx.def_path_str(callee_did),
                    });
                }
            }
        }
    }

    // Mention crate so it's clear which one this is even if functions vec is empty.
    if facts.crate_name.is_empty() {
        facts.crate_name = tcx.crate_name(LOCAL_CRATE).to_string();
    }
}

fn callee_def_id<'tcx>(
    _tcx: TyCtxt<'tcx>,
    func: &rustc_middle::mir::Operand<'tcx>,
) -> Option<DefId> {
    func.const_fn_def().map(|(did, _)| did)
}

fn module_of(def_path: &str) -> String {
    match def_path.rsplit_once("::") {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    }
}

fn has_test_attr(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
    // `#[test]` — matches the rustc test harness attribute path.
    let sym = rustc_span::Symbol::intern("test");
    tcx.get_attrs(def_id, sym).next().is_some()
}

#[allow(dead_code)]
pub fn run<'tcx>(_tcx: TyCtxt<'tcx>, _reporter: &mut crate::report::Reporter) {
    // Legacy entry kept so other code can still reference the symbol.
}
