# Optional instrumentation PGO trial

The default `PGO=0` Docker build is the unchanged compiler configuration. The
opt-in `PGO=1` build installs Rust's matching `llvm-tools-preview`, builds an
instrumented executable, runs it on three disposable synthetic inputs, merges
the counters, rebuilds with `-Cprofile-use`, and copies only the final binary
into the runtime image. Build failures are fatal; there is no silent fallback.
This is an experiment, **not a measured speedup or submission recommendation**.
In particular, clean amd64 Docker build time and image parity have not yet
been measured for this candidate.

`tools/pgo_generate.rs` is a self-contained deterministic generator, not the
official generator. It synthesizes three different calendar windows, three
7–9-byte ID shapes, skewed activity, links, identical duplicate event claims,
sparse clicks and purchases, F1, F2 and F3 timestamps and randomly quoted
fields. A small fraction of rows use shorter IDs, longer products or a
different header order to exercise the strict fallback. It has no competition
inputs, answers or official seeds.
The corpus is deliberately small and cannot reliably model all real duplicate
shapes, large-scale cache, sorting, allocator or thread-scheduling behavior.
Only training-process LLVM execution counters are merged; profiles from Cargo
build scripts are excluded. The temporary inputs, outputs, raw
profiles, profile and instrumented build are removed when the script exits;
nothing is read at runtime. The generator's correctness and timestamp
equivalence are tested in `tests/test_pgo_training.py`.

## Native build

Use the same Rust toolchain for both binaries and its exact matching
`llvm-profdata` (normally installed via `rustup component add llvm-tools-preview`).
The script also accepts `LLVM_PROFDATA=/absolute/path` and checks its LLVM
version. No custom `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` are accepted.
Set `TMPDIR` to a writable path without spaces. For a local trial:

```sh
CARGO_BUILD_JOBS=2 cargo build --locked --release --manifest-path fast/Cargo.toml
cp fast/target/release/shopping_sessions fast/target/control
CARGO_BUILD_JOBS=2 sh tools/pgo_build.sh "$PWD/fast/target/pgo"
```

These native executables are useful for a smoke test but are not necessarily
the packaged Linux executables. Use a Linux amd64 host compatible with the
image's Debian bookworm libc to measure the exact packaged executables:

```sh
docker build --platform linux/amd64 --build-arg PGO=0 -t ss-pgo-control .
docker build --platform linux/amd64 --build-arg PGO=1 -t ss-pgo-candidate .
mkdir -p fast/target/packaged
control_id=$(docker create ss-pgo-control)
docker cp "$control_id":/usr/local/bin/shopping_sessions fast/target/packaged/control
docker rm "$control_id"
pgo_id=$(docker create ss-pgo-candidate)
docker cp "$pgo_id":/usr/local/bin/shopping_sessions fast/target/packaged/pgo
docker rm "$pgo_id"
```

Neither executable embeds `target-cpu=native` or assumes AVX2; both use the
same generic target and runtime CPU dispatch already present in the source.
LLVM can change code layout, inlining and branch ordering based on synthetic
execution counts; its benefit is CPU- and workload-dependent. A native macOS
result does not establish Linux/x86-64 performance or correctness.

## Validation and two-seed comparison

Run the edge-case suite with each binary (`SS_BIN=... python -m unittest
discover -s tests`) and the official fixture/dev parity checks independently.
Then select **two distinct evaluation datasets**, neither used for training,
and compare actual results before timing. For each `DATASET`, with `WORK` an
empty writable directory:

```sh
DATA_DIR="$DATASET" OUTPUT_DIR="$WORK/control" NCPU=8 fast/target/packaged/control >"$WORK/control.stdout"
DATA_DIR="$DATASET" OUTPUT_DIR="$WORK/pgo" NCPU=8 fast/target/packaged/pgo >"$WORK/pgo.stdout"
test ! -s "$WORK/control.stdout" && test ! -s "$WORK/pgo.stdout"
python tools/compare_outputs.py "$WORK/control" "$WORK/pgo"
python tools/compare_outputs.py --check "$WORK/pgo"
python tools/bench_pair.py fast/target/packaged/control fast/target/packaged/pgo "$DATASET" --runs 5 --threads 8
```

Use fresh `WORK` for each seed. The benchmark alternates order and checks
stdout/exit status, but does not compare table contents itself. Use comparable
CPU allocation, cache state, toolchain, image and host for both sides. Record
all pairs, medians, dataset descriptions, CPU and build time; do not infer a
winner from a tiny smoke test. A speed change should clear the repository's
3% threshold on both seeds and still pass all correctness gates.
