# Shopping Sessions

A native Rust entry for the [Databricks Shopping Sessions buildathon](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions). It reads the mounted CSVs, resolves linked identities, deduplicates events, sessionizes, attributes clicks and assists, and writes `user_daily/` and `campaign_daily/` as Parquet. [Official rules](docs/rules.md) · [architecture](ARCHITECTURE.md) · [review, measurements and open checks](docs/NOTES.md).

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
make test test-bin diff fixtures
make dev
DATA_DIR=vendor/dev OUTPUT_DIR=out RAYON_NUM_THREADS=8 fast/target/release/shopping_sessions
make check-dev OUT=out
```

`make fixtures` checks the Rust binary and the independent Python oracle against the starter's hand-written answers. `make check-dev` uses both the official comparer and a second per-key diff. `tools/official_diff.py` runs differential checks on other official seeds. `python tools/bench.py DATA_DIR` reports five native start-to-exit times and their median; the competition clock also includes container startup.

## Credits

Rust: Rayon, memchr, memmap2, aHash, serde_json and Apache Arrow Rust (arrow-array, arrow-schema, parquet). Python: PyArrow; Polars in the organizers’ test harness. The official generator, fixtures and dev answer are distributed by the organizers and are not checked in. Entry code: [MIT](LICENSE).
