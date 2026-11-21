# Performance playbook

The score is wall time from container start to exit on 8 vCPUs. Correctness gates everything: one wrong row scores nothing. So every optimization lands only with green `tests/`, `tools/diff_impls.sh`, and the dev-set check.

## Budget for a ~1 s run on 20M events (≈1.5 to 2 GB of CSV)
| Phase | Target | Notes |
| --- | --- | --- |
| Container + process start | < 50 ms | Static binary, tiny image, no runtime. |
| Read + parse events | 0.3 s | ~0.25 GB/s per thread is enough at 8 threads. mmap with `MAP_POPULATE` or parallel `pread`; test both. |
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
6. **Sorting:** LSD radix on a u64 key built from (person-hash high bits, ts offset from window start in ms, fits in 31 bits since 14 days + 7 days < 2^31 ms).
7. **Page cache:** the scorer may run on a cold cache. If reads dominate, overlap I/O and parsing (one thread reads ahead while others parse).
8. **Do less:** clicks only matter for persons who have sessions; links only matter for IDs that appear. Skip what cannot affect output, but prove it with diffs.

## Safety rails for the final (60M, 3-minute cap per run)
- Peak memory must stay well under 24 GB with no swap: OOM = no score. Measure at 60M, then add 50%.
- Never use `target-cpu=native`. If you try `x86-64-v3`, keep a v2 build as the final unless you know the scorer CPU supports AVX2.
- No hard-coded windows, sizes, file counts, campaign counts or seeds.
- Logs to stderr only; stdout limit is 50 MB.
