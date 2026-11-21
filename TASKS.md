# Tasks (in order)

Each task is sized for one agent session. Do them in order; T1 to T4 decide whether we score at all. Mark `[x]` when the acceptance check passes and note the result (time, commit) on the line.

## Phase A: get an official green check (do first, today)

- [x] **T0. Set up the repo.** Public MIT repository; root Dockerfile builds `fast/` in place. Credits in README; generated/downloaded data excluded. `2880f71`.
  *Accept:* `git ls-files` shows Dockerfile at root; repo < 1 MB; `.gitignore` has `target/`.

- [x] **T1. Pull in the official files.** `2880f71`: imported official rules; only formatting and transcription preface differ. Archives unpacked into ignored `vendor/`.
  *Accept:* `docs/rules.md` is the official file; any diff is written down in `docs/NOTES.md`.

- [x] **T2. Run the 36 starter fixtures.** Both implementations passed 36/36; repeat with `make fixtures` (`cfdc59e`).
  *Accept:* 36/36 pass for both. Any failure: find the rules.md sentence, fix, add a case to `tests/test_reference.py`.

- [x] **T3. Pass the dev set.** Both checkers matched on seed 1, 1M events/250k clicks, 117,038 user rows and 27,400 campaign rows (`cfdc59e`).
  *Accept:* zero missing / extra / differing rows in both tables.

- [x] **T4. Docker parity.** Clean tracked-source snapshot, Rust source blob `a02e4eb`; amd64 build 114.90 s, 29.6 MB image; official restricted dev run 1.01 s, zero stdout, exact match. Docker VM memory caveat in `docs/NOTES.md`. Reproduce with:
  ```
  docker run --rm --network none --read-only --tmpfs /tmp:size=64m --user 1000 \
    --cpus 8 --memory 24g --memory-swap 24g \
    -e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch -e HOME=/scratch -e NCPU=8 -e MEM_GB=24 \
    -v $PWD/dev:/data:ro -v $PWD/out:/output -v $PWD/scratch:/scratch ss
  ```
  *Accept:* outputs match the dev answer; nothing written to stdout; image < 200 MB; build < 10 min.

- [x] **T5. First practice submission.** Leif reports the organizer scored `cc9f33e` at **5.03 s**. Reference point for optimization; local and organizer clocks differ.
  *Accept:* a practice score on the leaderboard.

## Phase B: make it bullet-proof

- [x] **T6. Differential testing on the official generator.** 60/60 matched: seeds 1–20 at each of 10k, 100k and 1M events, clicks at one quarter of events. Seed 15 covered leap day at all three sizes. Rerun `python tools/official_diff.py` (`5563fe7`).
  *Accept:* all match. Every mismatch becomes a minimized test case in `tests/`.

- [x] **T7. Native scale test at 60M.** Seed 13, 60M events and 15M clicks: 14.97 s wall, 9.38 GiB peak RSS, eight Rayon threads, zero spill; schema, keys and sums passed. Native Mac capacity check, not a scorer run; see `docs/NOTES.md`.
  *Accept:* < 60 s (target < 5 s), peak RSS < 20 GB, no `/scratch` needed. Output passes `compare_outputs.py --check`.

## Phase C: speed (only after A and B are green)

Measure every change on the 20M official dataset, 5 runs, report the median. Keep a table in `docs/NOTES.md`: commit, change, median, delta. Revert anything that does not help by > 3%.

- [x] **T8. Profile.** Sampled all native threads at 1 ms on the scored 20M seed-7 binary with macOS `sample -mayDie` (platform equivalent of a sampling profile). Five busiest non-wait leaves and phase timings in `docs/NOTES.md`.
- [x] **T9. Faster parse.** Runtime AVX2/SSE2, NEON and portable classifiers; the strict scanner fills selected columns without a copy. Word-based F1/F2/F3 checks and conversion. Integrated 20M median 0.949 → 0.774 s (-18.5%); differential scanner/timestamp tests and dev parity green.
- [x] **T10. Shrink rows.** The 64-byte event row no longer exists: a 24-byte claim and a 24-byte valid payload travel independently. Exact 64-bit person/click keys keep arbitrary spellings. Also tested 16-byte layouts; they missed the end-to-end gain threshold.
- [x] **T11. Fewer passes.** Payloads are routed directly, source buckets are consumed into correctly sized sort buffers, and identical click copies no longer require a separate click-ID partition. See paired medians in `docs/NOTES.md`.
- [x] **T12. Sort experiments.** Tried two-level bucketing and a gather fused with a radix pass; neither beat the compact exact-key comparison sorts by >3% overall. Kept the faster control.
- [x] **T13. Output.** Sonnet’s typed, uncompressed PLAIN writer uses the Parquet crate’s supported column API and explicitly closes both tables before exit. Official dev answer exact. Integrated 20M median 1.073 → 1.036 s (-3.4%).
- [x] **T14. Image and start-up experiment.** The static musl candidate exceeded the 10-minute local emulated build budget under load and was stopped before producing an image; no static runtime claim. The dynamic Debian candidate built in 59.61 s and passed restricted-container parity. Native allocator/ISA checks in `docs/NOTES.md`.
- [x] **T15. Thread tuning.** Honor `NCPU` when no explicit Rayon override is set. Native 4-vCPU matched-allocation vs double-oversubscribed trial: 2.068 vs 2.071 s. Four shard/chunk alternatives did not improve end-to-end time. Retain one worker per allocated CPU and the 256-shard / 8 MiB settings.

## Phase D: lock the final

- [ ] **T16. Pick the final** before Sun Nov 22, 16:00 UTC (10:00 AM Houston): the fastest commit that passed every check in A and B and whose last two practice runs agree within 20%. Never pick a commit that only ran once.
- [ ] **T17. Keep the repo public** and every submitted commit reachable (no force-push over them) until results are published.
