//! Analysis runner.
//!
//! Each sub-module is a *collector*: it walks HIR/MIR and appends entries to a
//! shared `CrateFacts`. Aggregation / thresholding happens later in the viewer,
//! not here — the goal of this pass is to dump raw facts to disk, not to
//! decide what is "bad". The one exception is `api_leak`, which is a purely
//! per-function syntactic check that also emits an inline rustc warning so
//! the user sees it immediately on `cargo check`.

use rustc_middle::ty::TyCtxt;

use crate::facts::CrateFacts;
use crate::report::Reporter;

pub mod api_leak;
pub mod call_graph;
pub mod lifetime_smells;
pub mod mutation;
pub(crate) mod util;

pub fn run_all<'tcx>(tcx: TyCtxt<'tcx>, reporter: &mut Reporter) -> CrateFacts {
    let mut facts = CrateFacts {
        crate_name: tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE).to_string(),
        ..Default::default()
    };

    call_graph::collect(tcx, &mut facts);
    mutation::collect(tcx, &mut facts);
    api_leak::run(tcx, reporter, &mut facts);

    facts
}
