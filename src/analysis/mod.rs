//! Analysis runner.
//!
//! Each sub-module is a *collector*: it walks HIR/MIR and appends entries to a
//! shared `CrateFacts`. Aggregation / thresholding happens later in the viewer,
//! not here — the goal of this pass is to dump raw facts to disk, not to
//! decide what is "bad". The one exception is `api_leak`, which is a purely
//! per-function syntactic check that also emits an inline rustc warning so
//! the user sees it immediately on `cargo check`.

use std::collections::{BTreeMap, BTreeSet};

use rustc_middle::ty::TyCtxt;

use crate::facts::{AccessKind, CrateFacts, FunctionProfile};
use crate::report::Reporter;

pub mod api_leak;
pub mod call_graph;
pub mod lifetime_smells;
pub mod mutation;
pub mod stringly_error;
pub mod unsafe_no_safety_doc;
pub(crate) mod util;

pub fn run_all<'tcx>(tcx: TyCtxt<'tcx>, reporter: &mut Reporter) -> CrateFacts {
    let mut facts = CrateFacts {
        crate_name: tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE).to_string(),
        ..Default::default()
    };

    call_graph::collect(tcx, &mut facts);
    mutation::collect(tcx, &mut facts);
    api_leak::run(tcx, reporter, &mut facts);
    stringly_error::run(tcx, reporter, &mut facts);
    unsafe_no_safety_doc::run(tcx, reporter, &mut facts);

    build_function_profiles(&mut facts);

    facts
}

/// Roll up raw `field_accesses` + `calls` into one `FunctionProfile`
/// per function. Lets the viewer (or any other consumer) do clustering
/// and similarity work without re-aggregating per use.
fn build_function_profiles(facts: &mut CrateFacts) {
    let mut reads:    BTreeMap<&str, BTreeSet<(String, String)>> = BTreeMap::new();
    let mut writes:   BTreeMap<&str, BTreeSet<(String, String)>> = BTreeMap::new();
    let mut callees:  BTreeMap<&str, BTreeSet<String>>           = BTreeMap::new();

    for acc in &facts.field_accesses {
        let key = (acc.struct_def_path.clone(), acc.field_name.clone());
        match acc.kind {
            AccessKind::Read => {
                reads.entry(acc.caller.as_str()).or_default().insert(key);
            }
            AccessKind::Write | AccessKind::MutBorrow => {
                writes.entry(acc.caller.as_str()).or_default().insert(key);
            }
        }
    }
    for edge in &facts.calls {
        callees.entry(edge.caller.as_str()).or_default().insert(edge.callee.clone());
    }

    facts.function_profiles = facts
        .functions
        .iter()
        .map(|f| FunctionProfile {
            def_path: f.def_path.clone(),
            fields_read:    reads   .get(f.def_path.as_str()).cloned().unwrap_or_default().into_iter().collect(),
            fields_written: writes  .get(f.def_path.as_str()).cloned().unwrap_or_default().into_iter().collect(),
            callees:        callees .get(f.def_path.as_str()).cloned().unwrap_or_default().into_iter().collect(),
            name_suffix:    name_suffix_of(&f.def_path),
        })
        .collect();
}

fn name_suffix_of(def_path: &str) -> String {
    let last = def_path.rsplit("::").next().unwrap_or(def_path);
    last.rsplit('_').next().unwrap_or(last).to_string()
}
