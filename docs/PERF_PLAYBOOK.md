# Performance playbook

The score is wall time from container start to exit on 8 vCPUs. Correctness gates everything: one wrong row scores nothing. So every optimization lands only with green `tests/`, `tools/diff_impls.sh`, and the dev-set check.

## Budget for a ~1 s run on 20M events (≈1.5 to 2 GB of CSV)
| Phase | Target | Notes |
| --- | --- | --- |
| Container + process start | < 50 ms | Static binary, tiny image, no runtime. |
| Read + parse events | 0.3 s | For 1.5–2 GB in 0.3 s, the eight threads together need 5–6.7 GB/s (0.63–0.83 GB/s each), including parsing. Compare demand paging with parallel read-ahead. |
| Dedup | 0.15 s | Partition by hash while parsing so no extra pass is needed. |
| Clicks + links | 0.1 s | Small. Links union-find is sequential but small. |
| Sort + sessionize + attribute | 0.25 s | Radix sort on (person hash, ts). |
| Write Parquet | 0.05 s | Few hundred thousand rows; uncompressed, parallel files. |

## Ideas, roughly by expected payoff
1. **Profile first.** `perf record -F 999 -g ./shopping_sessions` then `perf report`. Do not guess.
2. **Allocation:** pre-size every Vec from file sizes (bytes / average line length); reuse per-thread buffers; switch the global allocator to mimalloc.
3. **Parsing:** one pass per line that records comma positions; F1/F2 parse without bounds checks (lengths are fixed); special-case the common unquoted path.
4. **Keys:** a 64-bit hash plus (offset,len) per ID instead of a 16-byte slice and a re-hash later; carry the person hash from the dedup shard.
5. **Partition once:** many teams win by noticing that most events are not duplicated. A two-level scheme (bucket by event_id hash, dedup, then scatter by person hash) is what v1 does; consider scattering into pre-sized per-thread arenas to avoid `Vec<Vec<Vec<_>>>`.
6. **Sorting:** consider radix sorting by an exact interned person ID and timestamp. Two distinct names can have the same hash: sorting on truncated hash bits and timestamp alone can interleave their sessions. Derive any offset width from the actual window and retain click-ID tie ordering.
7. **Page cache:** the scorer may run on a cold cache. If reads dominate, overlap I/O and parsing (one thread reads ahead while others parse).
8. **Do less:** after constructing the complete link graph, clicks only matter for persons who have sessions. Never prune links just because neither endpoint appears in the logs: an unused ID can bridge two active people or determine the canonical name. Category lookup, unlike nonempty product-ID validation, is only necessary for purchases.

## Safety rails for the final (60M, 3-minute cap per run)
- Peak memory must stay well under 24 GB with no swap: OOM = no score. Measure at 60M, then add 50%.
- Never use `target-cpu=native`. If you try `x86-64-v3`, keep a v2 build as the final unless you know the scorer CPU supports AVX2.
- No hard-coded windows, sizes, file counts, campaign counts or seeds.
- Logs to stderr only; stdout limit is 50 MB.
