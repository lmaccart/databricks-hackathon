# Shopping Sessions

A native Rust entry for the [Databricks Shopping Sessions buildathon](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions). It reads the mounted CSVs, resolves linked identities, deduplicates events, sessionizes, attributes clicks and assists, and writes `user_daily/` and `campaign_daily/` as Parquet. [Official rules](docs/rules.md) · [architecture](ARCHITECTURE.md) · [review, measurements and open checks](docs/NOTES.md).

**Official practice: 1.57 s** on the build represented by [`9758471`](https://github.com/lmaccart/databricks-hackathon/commit/9758471d06bbd916f8d029780d4d734e81bcc13f), down from 1.78 s and 5.03 s. **Verified practice candidate:** [`7fd1b76`](https://github.com/lmaccart/databricks-hackathon/commit/7fd1b7602280cfce05ac97b6afb703fa42c0b6ba). Its guarded event parser reduced [full Docker medians](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876665326) from **1.942 to 1.831 s at 20M** and **6.065 to 5.722 s at 60M** relative to the scored binary. Every final-scale row matched on two different seeds. [Current trials and hardware details](docs/PERF_157.md) · [earlier measurements](docs/PERF_178.md).

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

For a native build, run `make build`. Set `DATA_DIR`, `OUTPUT_DIR` and, for reproducible eight-worker timing, `RAYON_NUM_THREADS=8`. Use a **fresh output directory** each run; row order and part count are immaterial. The binary logs phase times to stderr.

## Verify against the organizers’ answers

Requires Rust, Python 3.10+, PyArrow and Polars (the official checker). Download `starter-duckdb.zip`, `generator.zip`, and `dev-answer.zip` from the track page. With those archives in `fromsite/`:

```sh
python3 -m venv .venv
. .venv/bin/activate
python -m pip install pyarrow polars
for name in starter-duckdb generator dev-answer; do unzip -qo "fromsite/$name.zip" -d vendor; done
make test test-rust test-bin diff fixtures
make dev
DATA_DIR=vendor/dev OUTPUT_DIR=out RAYON_NUM_THREADS=8 fast/target/release/shopping_sessions
make check-dev OUT=out
```

`make fixtures` checks the Rust binary and the independent Python oracle against the starter's hand-written answers. `make check-dev` uses both the official comparer and a second per-key diff. `tools/official_diff.py` runs differential checks on other official seeds. `python tools/bench_pair.py BASELINE CANDIDATE DATA_DIR` compares five alternating-order pairs, with fresh output directories and one warmup per binary. The competition clock also includes container startup.

`python tools/opaque_diff.py vendor/dev fast/target/release/shopping_sessions` rewrites all IDs with byte-significant prefixes and trailing NULs, permutes CSV headers, then compares single- and eight-worker runs with Python.

The **Native amd64 performance check** workflow accepts `revision` and `baseline` commit SHAs. Use `challenge` for all-row comparisons and timing on two 20M seeds, `containers` for five paired practice-size Docker runs, and `scale` for an exact 60M check. `scale_pairs` adds three paired 60M Docker runs, each under a 14 GiB no-swap cap. `oracle` checks all 60 generated cases against Python on amd64. The default baseline is the 5.03 s entry; use `4f75f526cef27623e406d2ec1008b40eb1935f23` for the 1.78 s rebuild control. All inputs are generated on the runner.

## Credits

Rust: Rayon, memchr, memmap2, aHash, serde_json, bytes, the Apache Arrow project's Parquet crate, and [mimalloc](https://github.com/microsoft/mimalloc) v3.3.2 via the Rust `mimalloc` 0.1.52 wrapper on Linux (system allocator fallback elsewhere). Python: PyArrow; Polars in the organizers’ test harness. The official generator, fixtures and dev answer are distributed by the organizers and are not checked in. Entry code: [MIT](LICENSE).
