# Shopping Sessions

A native Rust entry for the [Databricks Shopping Sessions buildathon](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions). It reads the mounted CSVs, resolves linked identities, deduplicates events, sessionizes, attributes clicks and assists, and writes `user_daily/` and `campaign_daily/` as Parquet. [Official rules](docs/rules.md) · [architecture](ARCHITECTURE.md) · [review, measurements and open checks](docs/NOTES.md).

**Official practice: 0.99 s**, reported by Leif after 5.03 → 1.78 → 1.57 → 1.54 s. The [leaderboard](https://www.joinbuilders.org/buildathon/leaderboard) displays **0.30 s** at the top. The [verified release](docs/PERF_154.md#allocator-selection) cut warmed full Docker time by about 30% against the 1.54 s control on four-worker Linux runners; it also matched every output row on two 60M seeds. [Cold-input comparison](docs/PERF_154.md#input-page-cache) · [benchmark ledger](docs/PERF_154_RUNS.md).

## Shipped pipeline and attribution

The production pipeline is now **Team 67's MIT-licensed C++** ([github.com/mwkhettab/shopping-sessions-cpp](https://github.com/mwkhettab/shopping-sessions-cpp) at `29ee0cc5c26291c511cad44aec18c6531f050f79`), vendored unmodified in `cpp/pipeline/` with `build.sh` and the Dockerfile build recipe. `cpp/pipeline/LICENSE` is theirs and is copied into the image at `/app/LICENSE`. The Rust pipeline in `fast/` is retained but not shipped (`make build-rust`), and `reference/pipeline.py` remains the independent oracle that every change is checked against. Some older sections below describe the Rust entry.

## Build and run

```sh
docker build --platform linux/amd64 -t shopping-sessions .
# Create an empty output directory writable by uid 1000.
mkdir -p out
chmod 777 out
docker run --rm --platform linux/amd64 --network none --read-only --user 1000:1000 \
  --cpus 8 --memory 24g --memory-swap 24g --tmpfs /tmp:size=64m \
  -v "$PWD/data:/data:ro" -v "$PWD/out:/output" shopping-sessions
```

For a native build, run `make build` (`sh build.sh`, output `build/shopping`). The C++ binary reads `DATA_DIR` (default `/data`) and `OUTPUT_DIR` (default `/output`), or two trailing path arguments; `NCPU` sets the worker count (default 8, capped at 16). Use a **fresh output directory** each run; row order and part count are immaterial. The binary logs phase times to stderr.

## Verify against the organizers’ answers

Requires Rust, Python 3.10+, PyArrow and Polars (the official checker). Download `starter-duckdb.zip`, `generator.zip`, and `dev-answer.zip` from the track page. With those archives in `fromsite/`:

```sh
python3 -m venv .venv
. .venv/bin/activate
python -m pip install pyarrow polars
for name in starter-duckdb generator dev-answer; do unzip -qo "fromsite/$name.zip" -d vendor; done
make test test-rust test-bin diff fixtures
make dev
DATA_DIR=vendor/dev OUTPUT_DIR=out NCPU=8 build/shopping
make check-dev OUT=out
```

`make fixtures` checks the Rust binary and the independent Python oracle against the starter's hand-written answers. `make check-dev` uses both the official comparer and a second per-key diff. `tools/official_diff.py` runs differential checks on other official seeds. `python tools/bench_pair.py BASELINE CANDIDATE DATA_DIR` compares five alternating-order pairs, with fresh output directories and one warmup per binary. The competition clock also includes container startup.

`python tools/opaque_diff.py vendor/dev fast/target/release/shopping_sessions` rewrites all IDs with byte-significant prefixes and trailing NULs, permutes CSV headers, then compares single- and eight-worker runs with Python.

The **Native amd64 performance check** workflow accepts `revision` and `baseline` commit SHAs. Use `challenge` for all-row comparisons and timing on two 20M seeds, `containers` for five paired practice-size Docker runs, and `scale_pairs` for three paired 60M Docker runs under a 14 GiB no-swap cap. Setting both `scale` and `scale_pairs` checks two different 60M inputs. `oracle` checks 60 generated cases against Python on amd64. The default baseline is the 5.03 s entry; [this round’s reproduction command](docs/PERF_154.md#reproduce-the-comparisons) pins the 1.54 s control explicitly.

## Credits

Rust: Rayon, memchr, memmap2, aHash, serde_json, bytes and the Apache Arrow project's Parquet crate. Linux builds link [mimalloc 3.3.2](https://github.com/microsoft/mimalloc) through the pinned `mimalloc` 0.1.52 Rust crate; other platforms use the system allocator. Python: PyArrow; Polars in the organizers’ test harness. The official generator, fixtures and dev answer are distributed by the organizers and are not checked in. Entry code: [MIT](LICENSE).
