#!/bin/sh
# Build an opt-in executable using only synthetic branch histories. The
# disposable directory contains the training inputs, outputs and raw profiles.
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
if [ "$#" -ne 1 ]; then
    echo "usage: sh tools/pgo_build.sh ABSOLUTE_OUTPUT_BINARY" >&2
    exit 2
fi
case "$1" in /*) output=$1 ;; *) echo "output must be absolute" >&2; exit 2 ;; esac
if [ -n "${RUSTFLAGS:-}${CARGO_ENCODED_RUSTFLAGS:-}" ]; then
    echo "unset RUSTFLAGS and CARGO_ENCODED_RUSTFLAGS for a comparable build" >&2
    exit 2
fi
# An inherited path must not receive compiler/build-script instrumentation.
unset LLVM_PROFILE_FILE
host=$(rustc -vV | sed -n 's/^host: //p')
sysroot=$(rustc --print sysroot)
profdata=${LLVM_PROFDATA:-"$sysroot/lib/rustlib/$host/bin/llvm-profdata"}
if [ ! -x "$profdata" ]; then
    echo "matching llvm-profdata missing: install rustup component llvm-tools-preview or set LLVM_PROFDATA" >&2
    exit 2
fi
rust_llvm=$(rustc -vV | sed -n 's/^LLVM version: \([0-9.]*\).*/\1/p')
prof_llvm=$("$profdata" --version | sed -n 's/.*LLVM version \([0-9.]*\).*/\1/p' | head -1)
if [ -z "$rust_llvm" ] || [ "$rust_llvm" != "$prof_llvm" ]; then
    echo "llvm-profdata $prof_llvm does not match rustc LLVM $rust_llvm" >&2
    exit 2
fi
work=$(mktemp -d "${TMPDIR:-/tmp}/ss-pgo.XXXXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM
case "$work" in *' '*) echo "TMPDIR must not contain spaces" >&2; exit 2 ;; esac
mkdir -p "$work/raw"
rustc -O "$root/tools/pgo_generate.rs" -o "$work/generate"
export CARGO_TARGET_DIR="$work/target"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export RUSTFLAGS="-Cprofile-generate=$work/raw"
cargo build --locked --release --manifest-path "$root/fast/Cargo.toml"
for layout in 0 1 2; do
    "$work/generate" "$work/data-$layout" "$layout" 30000
    mkdir -p "$work/out-$layout" "$work/scratch-$layout"
    DATA_DIR="$work/data-$layout" OUTPUT_DIR="$work/out-$layout" \
        SCRATCH_DIR="$work/scratch-$layout" NCPU=8 RAYON_NUM_THREADS=8 \
        LLVM_PROFILE_FILE="$work/raw/train-%m-%p.profraw" \
        "$CARGO_TARGET_DIR/release/shopping_sessions" >"$work/stdout"
    if [ -s "$work/stdout" ]; then echo "training executable wrote to stdout" >&2; exit 1; fi
done
# Exclude profiles emitted by instrumented Cargo build scripts/proc macros.
"$profdata" merge -o "$work/profile.profdata" "$work"/raw/train-*.profraw
export RUSTFLAGS="-Cprofile-use=$work/profile.profdata"
cargo build --locked --release --manifest-path "$root/fast/Cargo.toml"
mkdir -p "$(dirname "$output")"
cp "$CARGO_TARGET_DIR/release/shopping_sessions" "$output"
