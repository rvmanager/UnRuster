#!/usr/bin/env bash
# Build UnRuster (if needed) and analyze a target Rust crate.
#
# Input model:
#   UnRuster works at the *crate* level. You point it at a directory
#   containing a Cargo.toml (or at the Cargo.toml itself). It runs
#   `cargo check` on that crate with itself wired in as RUSTC_WRAPPER,
#   which lets it hook rustc's HIR/MIR and emit findings about that
#   crate. Findings are about the TARGET crate, not about UnRuster.
#
# Usage:
#   ./unruster.sh <path-to-crate>           # analyze that crate
#   ./unruster.sh --demo                    # bundled fixture (fires warnings)
#   ./unruster.sh --self                    # analyze UnRuster's own source
#   ./unruster.sh --view [path]             # launch egui viewer on collected facts
#   ./unruster.sh <path> [extra cargo args] # forward extra args to `cargo check`
#
# Env:
#   UNRUSTER_PROFILE=release    use release build (default: debug)
#   UNRUSTER_REBUILD=1          force a rebuild before running

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROFILE="${UNRUSTER_PROFILE:-debug}"

usage() {
    sed -n '3,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-1}"
}

if [[ $# -eq 0 ]]; then usage 1; fi

VIEW_ONLY=0
case "$1" in
    -h|--help) usage 0 ;;
    --demo)    shift; TARGET="$HERE/tests/fixtures/leaky" ;;
    --self)    shift; TARGET="$HERE" ;;
    --view)    shift; VIEW_ONLY=1; TARGET="${1:-$PWD}"; if [[ $# -gt 0 ]]; then shift; fi ;;
    *)         TARGET="$1"; shift ;;
esac

if [[ -f "$TARGET" && "$(basename "$TARGET")" == "Cargo.toml" ]]; then
    TARGET="$(dirname "$TARGET")"
fi
TARGET="$(cd "$TARGET" && pwd)"

if [[ ! -f "$TARGET/Cargo.toml" ]]; then
    echo "error: no Cargo.toml at $TARGET" >&2
    exit 1
fi

# --- Toolchain wiring -------------------------------------------------------
#
# The driver binary dynamically links the nightly rustc_driver dylib. If we
# let cargo run inside the target crate use a different toolchain (e.g. the
# target pins stable), two things break:
#   1. dyld can't find librustc_driver-*.dylib (it searches the *active*
#      toolchain's lib/, which is the wrong one).
#   2. Even if it loaded, the embedded nightly compiler would auto-detect its
#      sysroot from argv[0] = stable rustc, producing an ABI mismatch.
#
# Fix: read UnRuster's pinned channel from rust-toolchain.toml, force the
# target's cargo onto it via RUSTUP_TOOLCHAIN, and add the nightly sysroot's
# lib dir to the dyld/ld search path so the wrapper always loads cleanly.

CHANNEL="$(awk -F'"' '/^[[:space:]]*channel[[:space:]]*=/{print $2; exit}' \
            "$HERE/rust-toolchain.toml")"
if [[ -z "$CHANNEL" ]]; then
    echo "error: could not parse channel from $HERE/rust-toolchain.toml" >&2
    exit 1
fi

if ! rustup toolchain list | grep -q "^${CHANNEL}"; then
    echo "==> installing rustup toolchain $CHANNEL (with rustc-dev)" >&2
    rustup toolchain install "$CHANNEL" --component rustc-dev --component llvm-tools-preview
fi

SYSROOT="$(rustup run "$CHANNEL" rustc --print sysroot)"
export RUSTUP_TOOLCHAIN="$CHANNEL"
export DYLD_FALLBACK_LIBRARY_PATH="$SYSROOT/lib:${DYLD_FALLBACK_LIBRARY_PATH:-}"
export LD_LIBRARY_PATH="$SYSROOT/lib:${LD_LIBRARY_PATH:-}"

# --- Build (if needed) ------------------------------------------------------

if [[ "$PROFILE" == "release" ]]; then
    BUILD_FLAGS=(--release)
    BIN_DIR="$HERE/target/release"
else
    BUILD_FLAGS=()
    BIN_DIR="$HERE/target/debug"
fi

if [[ "${UNRUSTER_REBUILD:-0}" == "1" \
      || ! -x "$BIN_DIR/unruster-driver" \
      || ! -x "$BIN_DIR/cargo-unruster" \
      || ! -x "$BIN_DIR/unruster-viewer" ]]; then
    echo "==> building unruster workspace ($PROFILE, toolchain $CHANNEL)" >&2
    (cd "$HERE" && cargo build --workspace ${BUILD_FLAGS[@]+"${BUILD_FLAGS[@]}"})
fi

if [[ "$VIEW_ONLY" == "1" ]]; then
    echo "==> launching viewer on $TARGET" >&2
    exec "$BIN_DIR/unruster-viewer" "$TARGET"
fi

# --- Run --------------------------------------------------------------------

echo "================================================================" >&2
echo "  UnRuster — analyzing: $TARGET" >&2
echo "  toolchain (forced):   $CHANNEL"  >&2
echo "  (findings below describe the target crate, not UnRuster)"      >&2
echo "================================================================" >&2

# Cargo skips rustc re-invocation when its fingerprint cache thinks nothing
# changed — but then our RUSTC_WRAPPER never runs and no facts get refreshed.
# Touching the source forces a re-check.
find "$TARGET/src" -name '*.rs' -exec touch {} + 2>/dev/null || true

# Put the bin dir on PATH so `cargo unruster` resolves, and so that
# `cargo-unruster` finds its sibling `unruster-driver` via current_exe().
export PATH="$BIN_DIR:$PATH"
cd "$TARGET"
exec cargo unruster "$@"
