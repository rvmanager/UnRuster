//! Detector: ownership / lifetime smells.
//!
//! Planned checks:
//! - `Vec<T>` (or `String`, `HashMap`, ...) by-value parameters that are only
//!   read from inside the body — should be `&[T]` / `&str` / `&HashMap`.
//!   Detect by inspecting MIR: if the parameter local is never moved out of,
//!   never `drop`-elaborated against, and never has a mutable projection.
//! - Unnecessary `.clone()` on a container that is then only read.
//! - Functions that *return* an owned container they constructed from a
//!   borrowed input solely by cloning — likely the caller should clone.
//!
//! Stub for now.

use rustc_middle::ty::TyCtxt;

use crate::report::Reporter;

#[allow(dead_code)]
pub fn run<'tcx>(_tcx: TyCtxt<'tcx>, _reporter: &mut Reporter) {
    // TODO
}
