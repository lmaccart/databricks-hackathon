# Spec: what we are building

## The entry
A single native binary (Rust) in a tiny Docker image that reads `/data`, applies rules.md exactly, and writes the two Parquet tables, as fast as the 8-vCPU box allows. This track has no judges, so "product" means **the pipeline**: exact output, then raw speed, then safety margin for the 60M-event final.

Why Rust (not Spark / DuckDB / Polars):
- The clock includes engine start-up. A JVM or Python interpreter costs 0.3 to 3 s before any work; a static binary starts in milliseconds.
- The leaders sit at 0.75 to 1.0 s on 20M events. That is only reachable with a hand-written, parallel, allocation-light parser and hash-partitioned group-bys.
- The rules (byte-wise strings, exact quoting, dedup-before-validate, distinct-timestamp gaps) are easier to get exactly right in explicit code than in SQL with CSV readers that "helpfully" trim, cast or unquote.

This kit already contains a working v1 (`fast/`) that matches the kit's reference on every kit test and 17 random dirty datasets, and runs a 2M-event kit dataset in 0.59 s on 4 cores. It has **not** yet been run against the organizers' fixtures, dev answer or generator. That is task T1.

## Definition of done
1. `make check-dev` from the starter passes with our binary (zero missing / extra / differing rows on the 1M dev set).
2. All 36 starter fixtures pass.
3. Our binary and `reference/pipeline.py` agree on the official generator for at least 20 seeds and a few sizes (10k to 1M events), including a seed whose window holds Feb 29, 2028.
4. The Docker image builds on linux/amd64 from a clean clone in under 10 minutes and runs as uid 1000 with a read-only root filesystem.
5. 60M events (official generator) run locally in well under 60 s on 8 threads with < 20 GB peak RSS. Target: under 3 s.
6. A practice submission scores; the final pick is a commit with a correct, fast, *stable* run.

## Features, prioritized

### Must
- M1. Exact CSV dialect: only `\n` ends a line, `\r` is data, quotes only around whole fields, field count check, empty line malformed, no trimming. Map columns by header name.
- M2. Exact timestamp parsing: F1, F2 (offset ±HH:MM, 00-23/00-59, subtract it), F3 (digits, leading zeros, overflow = out of window), full calendar validation incl. leap years.
- M3. Links: union-find over all link lines with both sides non-empty; person name = byte-wise smallest ID (may never appear in an event).
- M4. Events: dedup on event_id keeping smallest ingest_ts **before** validation; then validation; then late drop (> 24 h; exactly 24 h kept).
- M5. Sessions per person across IDs, sorted by ts, new session when gap > 1,800,000 ms; day = UTC date of session start.
- M6. Clicks: dedup on click_id; validate (user, ts in [ws − 7d, we), campaign known); credit = latest ts in [start − 7d, start], ties → smallest click_id; else organic.
- M7. Assists: each campaign with ≥1 qualifying click assists once; organic never.
- M8. Outputs: exact columns, `day` as Parquet DATE, integers, nullable `top_category` (ties → byte-wise smallest, `unknown` competes).
- M9. Dockerfile at repo root, uid 1000, no network at run time, logs on stderr only.

### Should (speed)
- S1. mmap inputs; parallel chunked parsing with rayon; no per-row allocation (slices into the mmap).
- S2. Hash-partition by event_id for dedup, then by person for sessions (both embarrassingly parallel).
- S3. Uncompressed, dictionary-free Parquet written in parallel parts.
- S4. Profile on 20M and 60M official data; remove the largest phase first (usually parse + partition).
- S5. Keep peak memory well under 24 GB at 60M (compact row structs, drop phases' buffers early).

### Could (last tenths of a second)
- C1. SIMD line/field scanning (memchr already helps; try a single pass that finds `,` `"` `\n` together).
- C2. Replace byte-slice keys with 64-bit hashes plus a collision check, to shrink rows and speed sorts.
- C3. Radix sort by (person hash, ts) instead of comparison sort.
- C4. Write Parquet by hand (fixed schema, PLAIN encoding) to skip arrow-rs overhead and startup.
- C5. Static musl or `FROM scratch` image to trim container start; `-C target-cpu=x86-64-v3` only after confirming AVX2 on the scorer (a wrong guess is a crash = no score).
- C6. Read `/data` with parallel `pread` instead of mmap if page faults dominate.

## Out of scope
UI, dashboards, cloud services, anything needing network at run time, and the other three tracks.
