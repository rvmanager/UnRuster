//! `cargo unruster` subcommand.
//!
//! Sets `RUSTC_WRAPPER` to point at the sibling `unruster-driver` binary,
//! then invokes `cargo check` on the target project. The wrapper hooks
//! rustc's `Callbacks` to run UnRuster's analyses after analysis (HIR/MIR
//! available) without changing rustc's actual output.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();

    // When invoked as `cargo unruster …`, cargo passes `unruster` as argv[1].
    if args.first().map(String::as_str) == Some("unruster") {
        args.remove(0);
    }

    let driver = find_driver();
    let project_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Force re-check of the primary crate so the wrapper actually runs even if
    // cargo's incremental cache thinks nothing changed.
    let status = Command::new("cargo")
        .arg("check")
        .arg("--quiet")
        .args(&args)
        .env("RUSTC_WRAPPER", &driver)
        .env("UNRUSTER_ENABLED", "1")
        .env("UNRUSTER_PROJECT_ROOT", &project_root)
        // Bust incremental cache; analyses must re-run.
        .env("CARGO_INCREMENTAL", "0")
        .status();

    match status {
        Ok(s) => exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("cargo-unruster: failed to spawn cargo: {e}");
            exit(2);
        }
    }
}

fn find_driver() -> PathBuf {
    let exe_name = if cfg!(windows) { "unruster-driver.exe" } else { "unruster-driver" };
    // Same directory as `cargo-unruster`.
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(exe_name);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    // Fall back to PATH lookup.
    PathBuf::from(exe_name)
}

#[allow(dead_code)]
fn _silence_unused(_p: &Path) {}
