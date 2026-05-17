//! `unruster-driver` — the rustc wrapper that runs UnRuster's analyses.
//!
//! Cargo invokes us through `RUSTC_WRAPPER`: argv[0] is our path, argv[1] is
//! the path to the real rustc, and the rest are the rustc args. We forward
//! everything to `rustc_driver::RunCompiler` and attach our `Callbacks` only
//! when compiling the user's primary package (so we don't analyze every
//! crate in the dependency graph).

#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_session;

use std::env;
use std::process::ExitCode;

use rustc_driver::Callbacks;

use unruster::driver::UnrusterCallbacks;

// A no-op Callbacks impl for non-primary crates (deps), so we don't run our
// analyses against every transitively-built crate.
struct NoOpCallbacks;
impl Callbacks for NoOpCallbacks {}

fn main() -> ExitCode {
    // Argv layout when invoked as RUSTC_WRAPPER:
    //   argv[0] = path to this binary
    //   argv[1] = path to real rustc
    //   argv[2..] = rustc args
    // We need to give `rustc_driver` an argv where argv[0] is the real rustc.
    let mut args: Vec<String> = env::args().collect();
    if args.len() >= 2 {
        let real_rustc = args.remove(1);
        args[0] = real_rustc;
    }

    let run_analysis = env::var_os("UNRUSTER_ENABLED").is_some()
        && env::var_os("CARGO_PRIMARY_PACKAGE").is_some();

    rustc_driver::catch_with_exit_code(move || {
        let mut no_op = NoOpCallbacks;
        let mut unr = UnrusterCallbacks;
        let cb: &mut (dyn Callbacks + Send) =
            if run_analysis { &mut unr } else { &mut no_op };
        rustc_driver::run_compiler(&args, cb);
    })
}
