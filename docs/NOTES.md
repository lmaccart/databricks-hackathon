# Shopping Sessions review — 2026-11-20 (Houston)

## What the official material established

The printed track page times **container start to exit** on eight pinned hyperthreads with 24 GB and no swap. The final runs approximately 60M events three times; any run over 180 s means no final score. The printout's practice leader was 0.75 s. Neither Opus's 0.59 s on 2M kit events nor a native Mac timing is comparable to that leaderboard.

`docs/rules.md` is the byte-for-byte organizer download. The transcription differs in its preface, table padding and paragraph breaks, not in rules. The starter pins the public dev arguments to seed 1, **1,000,000 events and 250,000 clicks**. The root Dockerfile now builds the locked Rust crate; the Docker context admits only its manifests and source.

## Correctness audit

- Both implementations pass the **36 official hand-written fixtures** and the local test suite. On the native dev run, both independent comparers found **117,038 user rows and 27,400 campaign rows**, with zero missing, extra, differing or duplicate keys. Stdout: zero bytes. Ten kit-generated 20k-event seeds also matched the oracle.
- Both implementations previously returned success if a required part directory could not be enumerated. They now fail with the input error. The regression test uses a file where the directory should be, so it works regardless of uid.
- The Python oracle used `int()` on unbounded digit strings. A valid timestamp or price padded with 5,000 zeros crashed it on current Python. Strip leading zeros and bound conversion by significant length; the Rust parser already handled this input. A regression also covers an enormous out-of-window F3 and an over-limit purchase.
- The kit comparer converted a numeric Parquet ID to a string and could report a match that the scorer rejected. It now checks string types and keeps null distinct from an empty Parquet string. Local edge tests now detect duplicate keys before indexing outputs by key.
- Use a fresh output directory for each run. Reusing the same directory with fewer Rayon workers can leave extra `part-*.parquet` from a previous run. The official harness and the new native benchmark use fresh directories.

## Latency experiments to run in order

These are code-review findings, not measured speedups. The current entry retains the validated sessionization, duplicate selection, byte ordering and attribution algorithms. Run `tools/bench.py` on the same official 20M input for both builds; compare five-run medians, dev answer, and peak memory at 60M. Measure start-to-exit, since moving work between phases can look like an improvement in one log line.

| Priority | Where | Change to isolate and why |
|---|---|---|
| 1 | `parse_events` | Validate nonempty `product_id` on every row, but probe `product_cat` only for purchases. At the generator's ~3% purchase rate this removes roughly 97% of otherwise valid category lookups. |
| 2 | `parse_events`, `PEv` | Only a surviving purchase can have a nonzero price. Use that property to remove `etype` from the sort payload: expected 48 → 40 bytes on amd64. Verify layout and RSS, not just nominal bytes. |
| 3 | `dedup_events`, `main` | Transpose bucket **handles** by destination; let workers consume and release source vectors as they go. Reserve gathered vectors from exact lengths. Today both source buckets and sort copies survive together. |
| 4 | `dedup_events` | Store an index or reference to the winning parsed row instead of copying a whole `EvRow` plus its ID into the hash table. Compare working-set reduction against the added pointer chase. Preserve *earliest ingest even if invalid*. |
| 5 | `parse_ts`, `for_rows` | Specialize only guaranteed-valid F1 `ingest_ts`; keep strict three-form event/click parsing. Try a direct header-order path with mapped fallback, then a one-pass separator/quote scanner. Test each independently against malformed CSV and reordered headers. |
| 6 | `process_shard` | Time gather, sort and scan per shard. One whale can have 1–2% of all events against a mean shard share of 0.39%. Schedule large shards first; try a parallel sort only if the last shards leave other workers idle. An exact interned person ID can remove repeated name comparisons. |
| 7 | `parse_clicks`, `dedup_clicks` | Decoded copies of one click are identical. Send validated clicks directly to the person shard, sort once, and deduplicate adjacent equal IDs there. Benchmark parse **through sort**, including extra duplicate sort rows. |
| 8 | `process_shard` attribution | Sort equal-time clicks by descending byte-wise click ID, so the last qualifying click is the tie winner in O(1). Measure on long tied runs; keep all campaigns in the inclusive assist window. Compare dense campaign/day counters with hashed entries if assists dominate a profile. |
| 9 | output writers | Measure write time and teardown independently. Try disabling Parquet statistics and sizing Arrow builders directly from shard totals before replacing the writer. Plain/uncompressed and parallel output already exist. |
| 10 | process/runtime | Test chunk size, shard count, allocator and read-ahead separately on Linux. Track file-backed vs anonymous memory and cold vs warm reads. Rayon honors `RAYON_NUM_THREADS`; `NCPU` alone is not an explicit pool override in this implementation. |

Two shortcuts in the original playbook were unsafe: pruning unused link IDs (they can bridge active users or name the entire person), and packing only part of a person's hash plus time as a definitive sort key (collisions can interleave people). Both notes have been corrected.
