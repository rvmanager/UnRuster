use std::env;
use std::path::PathBuf;

use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

use crate::analysis;
use crate::facts;
use crate::report::Reporter;

#[derive(Default)]
pub struct UnrusterCallbacks;

impl Callbacks for UnrusterCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let mut reporter = Reporter::new();
        let mut facts = analysis::run_all(tcx, &mut reporter);
        reporter.emit(tcx);

        if let Some(root) = env::var_os("UNRUSTER_PROJECT_ROOT") {
            let root = PathBuf::from(root);
            facts.project_root = root.to_string_lossy().into_owned();
            match facts::write_facts(&root, &facts) {
                Ok(path) => eprintln!("unruster: wrote facts to {}", path.display()),
                Err(e) => eprintln!("unruster: failed to write facts: {e}"),
            }
        }

        Compilation::Continue
    }
}
