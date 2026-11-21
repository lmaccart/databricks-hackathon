# Tasks (in order)

Each task is sized for one agent session. Do them in order; T1 to T4 decide whether we score at all. Mark `[x]` when the acceptance check passes and note the result (time, commit) on the line.

## Phase A: get an official green check (do first, today)

- [ ] **T0. Set up the repo.** Create a public GitHub repo (MIT license). Copy `fast/` contents to the repo root (so `Dockerfile` is at the root), plus `reference/`, `tools/`, `tests/`, `docs/`. Add the README credits for open-source crates. No data files.
  *Accept:* `git ls-files` shows Dockerfile at root; repo < 1 MB; `.gitignore` has `target/`.

- [ ] **T1. Pull in the official files.** From the track page download `rules.md`, `starter-duckdb.zip` (or any starter), `dev-answer.zip`, `generator.zip` into `vendor/` (gitignored except rules.md). Diff the official `rules.md` against `docs/rules-transcribed.md`; if anything differs, the official text wins: update the reference and the binary, then re-run `tests/`.
  *Accept:* `docs/rules.md` is the official file; any diff is written down in `docs/NOTES.md`.

- [ ] **T2. Run the 36 starter fixtures** against `reference/pipeline.py` and the binary. Read the starter's test scripts to see how fixtures are laid out and checked (expected outputs, harness command). Wire both implementations into that harness (`SS_BIN` style or the starter's own entrypoint contract).
  *Accept:* 36/36 pass for both. Any failure: find the rules.md sentence, fix, add a case to `tests/test_reference.py`.

- [ ] **T3. Pass the dev set.** Build the generator (`cargo build --release` in the unzipped generator), generate the 1M-event dev set with seed 1 exactly as the starter's Makefile does, unzip `dev-answer.zip`, run the binary, then `make check-dev` and `python3 tools/compare_outputs.py <dev-answer-dir> <output-dir>`.
  *Accept:* zero missing / extra / differing rows in both tables.

- [ ] **T4. Docker parity.** Build `docker build --platform linux/amd64 -t ss .` from a clean clone. Run it the way the scorer does:
  ```
  docker run --rm --network none --read-only --tmpfs /tmp:size=64m --user 1000 \
    --cpus 8 --memory 24g --memory-swap 24g \
    -e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch -e HOME=/scratch -e NCPU=8 -e MEM_GB=24 \
    -v $PWD/dev:/data:ro -v $PWD/out:/output -v $PWD/scratch:/scratch ss
  ```
  *Accept:* outputs match the dev answer; nothing written to stdout; image < 200 MB; build < 10 min.

- [ ] **T5. First practice submission** (Leif does the submit on the track page: public repo URL + commit). Record the score.
  *Accept:* a practice score on the leaderboard.

## Phase B: make it bullet-proof

- [ ] **T6. Differential testing on the official generator.** Script: for 20+ seeds × sizes {10k, 100k, 1M}, generate, run reference and binary, compare. Include a seed whose window holds 2028-02-29 (try seeds until `manifest.json` shows a Feb 2028 window). Use the reference for ≤ 1M only (it is slow).
  *Accept:* all match. Every mismatch becomes a minimized test case in `tests/`.

- [ ] **T7. Scale test at 60M.** Generate ~60M events (and the click count the starter uses at that scale; check the Makefile or ask organizers' ratio from the dev set). Run with `NCPU=8`, `/usr/bin/time -v`.
  *Accept:* < 60 s (target < 5 s), peak RSS < 20 GB, no `/scratch` needed. Output passes `compare_outputs.py --check`.

## Phase C: speed (only after A and B are green)

Measure every change on the 20M official dataset, 5 runs, report the median. Keep a table in `docs/NOTES.md`: commit, change, median, delta. Revert anything that does not help by > 3%.

- [ ] **T8. Profile.** `perf record -g` (or `cargo flamegraph`) on 20M. Write down the top 5 hot spots.
- [ ] **T9. Faster parse.** Single-pass scanner for `,` `"` `\n` per line; avoid `[&[u8]; 8]` copies; parse F1/F2/F3 with fixed-offset digit math. Keep `parse_ts` tests green.
- [ ] **T10. Shrink rows.** Store event_id / user / click_id as (u64 hash, offset, len) into the mmap; compare bytes only on hash ties. Target EvRow ≤ 32 bytes.
- [ ] **T11. Fewer passes.** Fuse dedup-shard and person-shard work where possible; avoid the `flat_map` gather copy by pre-sizing per-shard vectors.
- [ ] **T12. Radix sort** person shards by (hash, ts) instead of `sort_unstable_by`.
- [ ] **T13. Output.** Try writing Parquet directly (PLAIN, no stats) vs arrow-rs; keep whichever is faster and passes `compare_outputs.py`.
- [ ] **T14. Image and start-up.** Try `FROM scratch` with a static musl build (`x86_64-unknown-linux-musl`, mimalloc or jemalloc for the allocator). Measure container start to exit.
- [ ] **T15. Thread tuning.** Read `NCPU`; try rayon pool = 8 vs 16; try `CHUNK` 2/4/8/16 MB and `NS` 64/256/1024.

## Phase D: lock the final

- [ ] **T16. Pick the final** before Sun Nov 22, 16:00 UTC (10:00 AM Houston): the fastest commit that passed every check in A and B and whose last two practice runs agree within 20%. Never pick a commit that only ran once.
- [ ] **T17. Keep the repo public** and every submitted commit reachable (no force-push over them) until results are published.
