# Architecture

```
/data ──mmap──> chunks (8 MB, line-aligned, per-file column map)
   links  ──parse ║──> pairs ──union-find (seq)──> id → person name (smallest ID)
   events ──parse ║──> EvRow buckets by hash(event_id) [256 shards]
                      └─ per shard ║: dedup (min ingest_ts) → keep if kept copy valid
                                     → map uid → person → PEv buckets by hash(person)
   clicks ──parse+validate ║──> buckets by hash(click_id)
                      └─ per shard ║: dedup → map uid → person → PCl buckets by hash(person)
   per person shard ║: scatter PEv by high person-key bits; coalesce (person, floor(ts/GAP))
                      → sort cells (wide-event fallback for sparse shards); sort PCl by (person, ts, click_id)
                      walk sessions: day → credit/assist via sliding 7-day window
                      → user rows + (campaign, day) counters
   merge campaign counters → write campaign_daily/part-0.parquet
   user rows ║──> user_daily/part-N.parquet (one file per shard group)
(║ = parallel with rayon)
```

## Files
- `fast/src/main.rs`: pipeline orchestration, timestamp/CSV parsing, sliding attribution and daily aggregation.
- `fast/src/cells.rs`: exact temporal-cell sessionization; 32-byte cells plus sparse purchase chains. Hash tables compare full person and cell keys, never truncated hashes. If cells exceed half the shard's events, retain the original wide-event sort instead. Partition sizing and fallback affect work only, never output.
- `fast/Cargo.toml`: rayon, memchr, memmap2, ahash, serde_json, arrow-array, arrow-schema, parquet (no compression codecs).
- `Dockerfile`: root two-stage build (Rust slim → Debian slim), runs as uid 1000.
- `reference/pipeline.py`: slow, literal oracle. Any disagreement = a bug in one of them; rules.md decides which.
- `tools/compare_outputs.py`: missing / extra / differing rows, schema check, rules.md self-check.
- `tools/mini_gen.py`: kit-made dirty data for quick diffs (the official generator is the real test).
- `tools/diff_impls.sh`: reference vs binary over many seeds.
- `tests/test_reference.py`: 15 edge cases; `SS_BIN=path` runs them against a binary instead.

## Key decisions
| Decision | Why |
| --- | --- |
| Byte slices into the mmap, never `String` | Byte-wise comparison is the rule; zero copies. |
| Validity computed per copy at parse time, applied after dedup | Dedup-before-validation is exact: the winning copy's own flag decides. |
| Bad `ingest_ts` treated as +∞ | rules.md says it is always valid F1; this only makes such a copy lose dedup or be late. |
| Union-find root = smallest name | The root is the person's name with no second pass. |
| Sort clicks by (ts, click_id) | Latest-ts tie → the first of the equal-ts run is the smallest click_id. |
| Sliding window with per-campaign counts + active list | Assists cost O(active campaigns) per session, and the window only moves forward. Shared-device "mega persons" stay linear. |
| Coalesce `[k·GAP, (k+1)·GAP)` per exact person; connect when `next.min − prev.max ≤ GAP` | Every within-cell gap is less than GAP. Extrema alone decide connectivity across cells; sparse purchase chains preserve category sums. Negative times use Euclidean division. |
| `day = start.div_euclid(86_400_000)` → Date32 | UTC date; correct for any epoch. |
| No `-C target-cpu=native` | The scoring CPU is unknown; an illegal instruction is a crash. |

## Data shapes (for memory planning at 60M events)
- On a 64-bit target, `EvRow` has two fat slices, two i64s, two u32s and two one-byte fields: 64 bytes including alignment. At 64M records its payload alone is ~4.1 GB, before spare capacity or the dedup table.
- `PEv` is 48 bytes and `PCl` is 56 bytes on that layout. The original per-person buckets can coexist with the gathered sort vectors. See [measured RSS and its limitations](docs/NOTES.md) rather than adding only the compact payload sizes.

## Where the time goes (v1, 2M kit events, 4 threads: 0.59 s total)
links 0.04 s · parse events 0.18 s · dedup 0.10 s · clicks 0.05 s · sessions 0.16 s · write 0.03 s.
The sessions phase includes two big comparison sorts; parse+dedup is the next target. See `docs/PERF_PLAYBOOK.md`.
