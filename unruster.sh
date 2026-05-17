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
#   ./unruster.sh <path>                      # analyze (refresh cache)
#   ./unruster.sh --view <path>               # launch egui viewer (no refresh)
#   ./unruster.sh --export <path>             # headless: write unruster-report.md
#   ./unruster.sh --refresh --view <path>     # refresh, then launch viewer
#   ./unruster.sh --refresh --export <path>   # refresh, then export report
#   ./unruster.sh --demo                      # bundled fixture (fires warnings)
#   ./unruster.sh --self                      # analyze UnRuster's own source
#
# Flag rules:
#   --refresh re-runs the analysis (otherwise --view/--export use cached facts).
#   With no mode flags, --refresh is implied.
#   --quiet redirects rustc/cargo output to a log file (auto-on when --refresh
#     is paired with --view or --export). --verbose forces full output.
#   Any unrecognized argument is forwarded to `cargo check`
#   (e.g. --all-features, -p somecrate).
#
# Env:
#   UNRUSTER_PROFILE=release    use release build (default: debug)
#   UNRUSTER_REBUILD=1          force a rebuild before running

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROFILE="${UNRUSTER_PROFILE:-debug}"

usage() {
    sed -n '3,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-1}"
}

if [[ $# -eq 0 ]]; then usage 1; fi

# --- Parse flags ------------------------------------------------------------

REFRESH=0
VIEW=0
EXPORT=0
QUIET=-1   # -1 = auto, 0 = verbose, 1 = quiet
TARGET=""
EXTRA_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)   usage 0 ;;
        --refresh)   REFRESH=1; shift ;;
        --view)      VIEW=1;    shift ;;
        --export)    EXPORT=1;  shift ;;
        --quiet)     QUIET=1;   shift ;;
        --verbose)   QUIET=0;   shift ;;
        --demo)      TARGET="$HERE/tests/fixtures/leaky"; shift ;;
        --self)      TARGET="$HERE"; shift ;;
        --)          shift; EXTRA_ARGS+=("$@"); break ;;
        -*)          EXTRA_ARGS+=("$1"); shift ;;
        *)
            if [[ -z "$TARGET" ]]; then
                TARGET="$1"
            else
                EXTRA_ARGS+=("$1")
            fi
            shift
            ;;
    esac
done

# No mode flags → refresh (legacy default).
if [[ "$VIEW" == 0 && "$EXPORT" == 0 && "$REFRESH" == 0 ]]; then
    REFRESH=1
fi

# Auto-quiet when refresh is paired with view/export — user wants the
# follow-up, not the cargo output. Plain `--refresh` keeps full output.
if [[ "$QUIET" == -1 ]]; then
    if [[ "$REFRESH" == 1 && ( "$VIEW" == 1 || "$EXPORT" == 1 ) ]]; then
        QUIET=1
    else
        QUIET=0
    fi
fi

# Default target for --view/--export with no path: CWD.
if [[ -z "$TARGET" ]]; then
    if [[ "$VIEW" == 1 || "$EXPORT" == 1 ]]; then
        TARGET="$PWD"
    else
        echo "error: no target path given" >&2
        usage 1
    fi
fi

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

export PATH="$BIN_DIR:$PATH"

# --- Refresh cache (if requested) ------------------------------------------

if [[ "$REFRESH" == 1 ]]; then
    echo "==> analyzing $TARGET  (toolchain $CHANNEL)" >&2

    # Cargo skips rustc re-invocation when its fingerprint cache thinks
    # nothing changed — and then our RUSTC_WRAPPER never runs and no facts
    # get refreshed. Touching sources forces a re-check.
    find "$TARGET/src" -name '*.rs' -exec touch {} + 2>/dev/null || true

    if [[ "$QUIET" == 1 ]]; then
        # Stream cargo to a log file in the cache dir. Forward only our own
        # bookkeeping lines and any UnRuster diagnostic to the terminal so
        # the user still sees `[api_leak]` findings without the nightly
        # `float_literal_f32_fallback` flood.
        CACHE_DIR="$HOME/Library/Caches/unruster"
        mkdir -p "$CACHE_DIR"
        LOG_FILE="$CACHE_DIR/refresh.log"
        echo "    (full output → $LOG_FILE)" >&2

        # The grep extracts: UnRuster's own status lines, and the start of
        # any `[api_leak]` warning + its surrounding context. Everything
        # else lands in the log file only.
        (cd "$TARGET" && cargo unruster ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}) \
            > "$LOG_FILE" 2>&1 \
            || { tail -40 "$LOG_FILE" >&2; echo "==> refresh failed (see $LOG_FILE)" >&2; exit 1; }
        grep -E '^(unruster:|warning: \[)' "$LOG_FILE" >&2 || true
    else
        (cd "$TARGET" && cargo unruster ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"})
    fi
fi

# --- View / export ----------------------------------------------------------

if [[ "$EXPORT" == 1 ]]; then
    echo "==> exporting report for $TARGET" >&2
    "$BIN_DIR/unruster-viewer" --export "$TARGET"
fi

if [[ "$VIEW" == 1 ]]; then
    echo "==> launching viewer on $TARGET" >&2
    exec "$BIN_DIR/unruster-viewer" "$TARGET"
fi
