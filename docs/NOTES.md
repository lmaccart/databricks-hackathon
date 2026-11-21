# Shopping Sessions review — 2026-11-20 (Houston)

## Click record size: keep the release layout — 2026-11-21

The [16-byte click trial](https://github.com/lmaccart/databricks-hackathon/actions/runs/44938958482) matched every row but lost 14/18 pairs against the practice build. Full Docker medians rose from **1.231 to 1.235 s** at 20M and **3.758 to 3.805 s** at 60M. Recommendation: `1a3d0fd`.

## Verified cold-input comparison — 2026-11-21

Keep `1a3d0fd` for practice. On the same four-worker Intel machine the change saved **27.3% / 28.4%** with fully resident inputs and **8.2% / 7.0%** with no input pages resident (20M / 60M). It won all six cold pairs and all six warm pairs. Outputs matched after every timed run. [Timing and cache verification](PERF_154.md#input-page-cache).

## Next practice build: link mimalloc 3 to the worker-local campaign pipeline — 2026-11-21

Practice SHA: [`1a3d0fd`](https://github.com/lmaccart/databricks-hackathon/commit/1a3d0fdf38c14e858a92ba53bfa29da2f0f9b94a), production-identical to `14ccc10`. Direct comparisons against the scored source won **53 of 54 pairs** across the [oracle run](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937040474), [two-60M-seed run](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937344723) and [EPYC 7763 repeat](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937264092). Full Docker medians improved **28.7–34.2% at 20M** and **29.5–33.6% at 60M**. The packaged entry matched the 60-case Python matrix and every row on both 60M seeds, with peak RSS below 7.82 GiB. Extra reservation logic and allocator tunables missed the practice threshold head-to-head. [Release evidence and reproduction](PERF_154.md#allocator-selection). Latest official practice: **1.54 s**.

## Practice candidate: merge campaign counters once per worker — 2026-11-21

[`d754296`](https://github.com/lmaccart/databricks-hackathon/commit/d75429665a35366dfef26096d63cd71c778b19f0) reduced full Docker medians to **1.471 s** from 1.779 s at 20M and **4.448 s** from 5.512 s at 60M. It also beat `0558eaa` directly (7.5% / 5.4% less Docker time). Both comparisons favored the candidate in 18/18 pairs. All 60 native Linux Python cases and the two final-scale row comparisons passed. [Evidence](PERF_154.md#verified-release-checkpoint).

## Practice candidate: physical-slot identity graph and direct person ranges — 2026-11-21

[`0558eaa`](https://github.com/lmaccart/databricks-hackathon/commit/0558eaa7acd59463d21a888e1d418d676301e07f) beat the scored build in 18/18 pairs including full Docker **1.792 → 1.617 s** at 20M and **5.451 → 4.618 s** at 60M. Against the physical-slot graph and column writer alone on a different CPU it also won 18/18, including 3.6% / 3.1% in Docker. All rows on two 60M inputs, the Linux Python matrix and new graph-contention checks matched. [Results](PERF_154.md#verified-release-checkpoint).

## Practice candidate: exact singleton ranks and direct Parquet columns — 2026-11-21

[`4ad0e5d` won 18/18 release pairs](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934668743) against the 1.54 s control. Under restricted Docker, medians fell **1.816 → 1.722 s** at 20M and **5.532 → 5.199 s** at 60M. It matched both 60M inputs (seeds 13 and 948307), all 60 Linux Python comparisons, dev, fixtures and opaque/malformed inputs. [Full evidence](PERF_154.md#verified-release-checkpoint). The latest organizer score is still 1.54 s.

## 1.54 s control: measure launch and test output/identity changes — 2026-11-21

On a four-worker hosted Intel, the complete header-only Docker run took 0.0847 s; full practice took 1.8042 s on the same clock. The in-container `written` medians were 0.0010 s and 1.6370 s. [Clocks, two-seed timings and rejected rewrites](PERF_154.md). The column writer won 10/10 native pairs (2.6% / 3.7% less time); exact singleton ranks won 9/10 (2.8% / 2.5%). Synthetic LLVM PGO lost all 18 practice and final-scale pairs. These are local-to-runner comparisons, not organizer scores.

## Official practice: 1.54 s — 2026-11-21

Leif reports **1.54 s** for the latest repo build and several teams tied at **0.30 s**. At the time of that report `main` was `dfbd330`, with the same Dockerfile, lockfile and production sources as `7fd1b76`. Use `7fd1b76` as the timing control. The change from the previous official result was 0.03 s (1.9%); reaching 0.30 s requires 1.24 s less (80.5%). [New trials](PERF_154.md).

## Round-three head-to-head decision — 2026-11-21 07:05 UTC

Keep `7fd1b76` for practice. The partition-local identity join saved 10.1% at 60M against the older scored build on one host, but **lost 2.2% at 60M** against this parser in [their direct Docker comparison](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877045584). Likewise the three-input parser with independent clocks [reduced full Docker time 8.8%/8.7% against the scored build on AMD EPYC 7763](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877595639), but [gained only 1.1% at 20M and lost 1.9% at 60M against the typed-event parser](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877595939) on another AMD CPU. Sharing the event and ingest civil fields also reversed between AMD and Intel. Full results, including negative trials and the second 60M seed, are in [PERF_157.md](PERF_157.md).

## Full-container verified event parser — 2026-11-21 06:50 UTC

`7fd1b76` beat the scored `9758471` by **6.3% / 7.1%** on the two 20M seeds and by **5.7%** at each of 20M and 60M in [full Docker](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876665326). All 60 generated Python comparisons, the mixed grammar corpus, 36 official fixtures, dev output, and 6,615,971 final-scale rows matched. Another 60M seed on ARM64 matched all 6,671,518 rows. The local eight-worker **native** medians were 0.570 s and 0.601 s; the latest organizer time remains **1.57 s** for the previous build.

## Strict typed row parsing on the 1.57 s baseline — 2026-11-21

[`f4ac682` won all ten native pairs](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875889484) relative to `9758471`: 1.744 → 1.628 s (seed 7) and 1.719 → 1.604 s (seed 15). Its event phase fell 0.833 → 0.709 s on seed 7. It validates ID bytes in the load used to encode them and parses the common event layout without first searching for a newline. No header order, date, ID spelling or field width is assumed for correctness. All output rows matched on both inputs. The [round ledger](PERF_157.md) also records alternatives and the measured scanning/decoding budget.

## Official practice: 1.57 s; new target 0.63 s — 2026-11-21

Leif reports **1.57 s** on the most recent entry, down from **1.78 s** (11.8% less time), and a **0.63 s** record. The current main build and the recommended `9758471` have identical production sources. Use `9758471` as the fixed control. Matching 0.63 s requires **2.49×** the current throughput, or **0.94 s / 59.9%** less wall time.

## Practice pick: compact claims, manifest clock and ranked events — 2026-11-21 05:48 UTC

Use [`9758471`](https://github.com/lmaccart/databricks-hackathon/commit/9758471d06bbd916f8d029780d4d734e81bcc13f) for the next practice slot. Its production source, lockfile and Dockerfile are identical to the tested `62ad4de` (`fast/` tree `f2ea2526a3bdbb4d7d9ece368d446f639ea7a07c`). The integrated suite passed 38 edge cases against each implementation, 19 Rust tests, 10 dirty seeds, all 36 official fixtures, and both dev comparers.

[Final verification against the 1.78 s rebuild control `4f75f52`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44871823659) used a single four-worker Intel Xeon 6975P-C. Medians are from alternating order with one warmup per build:

| Input | Pairs | Control | `9758471` | Less time |
|---|---:|---:|---:|---:|
| 20M events + 5M clicks, seed 7, native | 5 | 2.106 s | 1.805 s | 14.3% |
| 20M events + 5M clicks, seed 15, native | 5 | 2.088 s | 1.801 s | 13.7% |
| 20M events + 5M clicks, seed 7, full Docker | 5 | 2.201 s | **1.903 s** | **13.6%** |
| 60M events + 15M clicks, seed 13, full Docker | 3 | 6.755 s | **5.882 s** | **12.9%** |

The candidate won **18/18 pairs**. The 60M Docker pairs were 6.824 → 5.936, 6.755 → 5.882, and 6.619 → 5.828 s. A separate first 60M run took 11.327 s; the official comparer matched all **6,615,971 rows**, with no duplicate keys or stdout. The same job also passed all **60 official-generator comparisons against Python on Linux/amd64**.

It also beat the previous pick, `18d1d8c`, [directly on a four-worker Intel Xeon 6975P-C](https://github.com/lmaccart/databricks-hackathon/actions/runs/44872074349). Process medians fell from **1.848 to 1.783 s** and **1.825 to 1.757 s** on the two 20M seeds (3.5% / 3.7%). Full Docker medians were **1.949 → 1.890 s at 20M** and **6.141 → 5.972 s at 60M**. The candidate won all five practice-size and all three final-size Docker pairs. The full 60M output matched under **14 GiB, no swap**, including all 6,587,957 user-days and 28,014 campaign-days. These are CI times; the last confirmed practice score is **1.78 s**.

## Compact duplicate claims and manifest-derived UTC dates — 2026-11-21 UTC

The [`62ad4de` candidate](https://github.com/lmaccart/databricks-hackathon/commit/62ad4dec65ed865cc5dcccc95d013c3954998490) cuts a duplicate claim from 24 to 16 bytes. The shard supplies the high eight bits of a **reversible** 64-bit ID token; the remaining 56 bits are compared exactly within that shard. Whole-second F1 ingests fit as signed seconds through year 9999. Other ingests keep full milliseconds in a per-chunk side table. An immutable clock derived from the actual manifest checks exact UTC date bytes; a miss uses the full timestamp parser.

[Four-worker AMD 9V74](https://github.com/lmaccart/databricks-hackathon/actions/runs/44871821060), versus the already faster `012174f` event-stream checkpoint: **1.949 → 1.844 s** on seed 7 and **1.917 → 1.832 s** on seed 15 (5.4% / 4.4% less time). It won all ten pairs. [Against compact claims alone on Intel](https://github.com/lmaccart/databricks-hackathon/actions/runs/44871838302), the clock saved another 2.8% / 2.0%. Both studies matched all rows.

On the M2 Pro, 60M output matched all 6,615,971 rows and passed a separate schema/uniqueness/aggregate check. Maximum RSS was **8.67 GiB**, down from 9.11 GiB. Three warm 60M pairs against the event-stream checkpoint had medians of **2.322 → 2.178 s**; five pairs against `4f75f52` measured **2.419 → 2.231 s**. The two 20M Mac medians against the event-stream checkpoint were mixed (0.866 → 0.776 s; 0.735 → 0.749 s). The entire 60-case Python-oracle corpus matched, including leap day. The [prior event-stream checkpoint also completed the full matrix on Linux/amd64](https://github.com/lmaccart/databricks-hackathon/actions/runs/44871640765).

`tools/opaque_diff.py` also transformed all nonempty IDs in the 1M dev input into byte-significant names ending in NUL, varied the column order, and alternated files with and without a final newline. This forces ID interning, including during parallel ingestion. Both one and eight workers matched a fresh Python answer on **117,038 user rows and 27,400 campaign rows**.

## Same-machine, full-container finalist comparison — 2026-11-21 05:36 UTC

[`18d1d8c`](https://github.com/lmaccart/databricks-hackathon/commit/18d1d8ccec2559a5ca0e2fa97690bcc6eeeeb9dd) has exactly the production source and Dockerfile of `91892b1`, tested in [run 44871127261](https://github.com/lmaccart/databricks-hackathon/actions/runs/44871127261). Each row compares it to `4f75f52` on the **same four-worker Intel Xeon 6975P-C**, alternating order after one warmup per build. All runs include process startup and exit; Docker runs also include container creation and removal.

| Input and timing | Pairs | 1.78 s build control | Compact event build | Less time |
|---|---:|---:|---:|---:|
| 20M events, 5M clicks, seed 7, native | 5 | 2.098 s | 1.896 s | 9.6% |
| 20M events, 5M clicks, seed 15, native | 5 | 2.107 s | 1.872 s | 11.2% |
| 20M events, 5M clicks, seed 7, full Docker | 5 | 2.163 s | **1.962 s** | **9.3%** |
| 60M events, 15M clicks, seed 13, full Docker | 3 | 6.725 s | **6.116 s** | **9.0%** |

Final-scale Docker used a **14 GiB memory limit with swap disabled**. The three raw 60M pairs were 6.549 → 5.935, 6.851 → 6.172 and 6.725 → 6.116 s. A separate first candidate run on the newly generated data took **11.338 s**; its complete result matched the control: 6,587,957 user-day and 28,014 campaign-day rows, zero differing rows and no duplicate keys. Stdout was empty. This is CI evidence; the confirmed organizer time is still **1.78 s**.

## Compact event streams and direct click routing — 2026-11-21 UTC

The first combined candidate, `012174f`, uses the canonical link-node rank and a checked timestamp offset as one 64-bit sort key. It puts price and category in a separate purchase stream, and retains the full-width time and person key when packing is impossible. Catalog lookups and click-owner prefetch are included. [Four-worker Intel, five paired process runs per seed](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870672021): **2.087 → 1.876 s** on seed 7 and **2.074 → 1.859 s** on seed 15 (10.1% / 10.4% less wall time vs `4f75f52`). All rows matched on both seeds. Both dev checkers and 36 fixtures matched; all 60 official-generator cases (20 seeds at each of 10k, 100k and 1M) matched the independent Python implementation. [Component and alternative timings](PERF_178.md).

On a [four-worker AMD EPYC 7763](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870718540), `012174f` also beat its strongest individual component (`e81c18d`): **2.502 → 2.399 s** and **2.476 → 2.378 s** on seeds 7 and 15. Practice-size full Docker was **2.668 → 2.591 s**. A single 60M Docker run took **9.957 s** with a 14 GiB no-swap cap; all **6,615,971** rows matched. In a separate M2 Pro capacity check, the integrated source (`8e3525e`, moving only the fixed claim hasher into its consuming phase) completed in **8.40 s**, **9.11 GiB** max RSS, with empty stdout and the same exact result. The integrated binary also passed the complete 60-case Python matrix.

## Official practice: rebuild at 1.78 s — 2026-11-21 UTC

Leif reports **1.78 s** on the rebuild submitted before the final-scale CI job finished, down from **5.03 s** on `cc9f33e` (**2.83× faster**). The exact submitted SHA was not specified. Use [`4f75f52`](https://github.com/lmaccart/databricks-hackathon/commit/4f75f526cef27623e406d2ec1008b40eb1935f23) as the fixed control for the next round; the subsequent `bcdc149` commit only recorded the CI result. These official and CI clocks are tracked separately.

[Two-seed native experiments against the 1.78 s build](PERF_178.md) include per-trial measurements and exact-comparison logs.

## Final-scale Docker check — 2026-11-21 04:57 UTC

[CI run 44868041222](https://github.com/lmaccart/databricks-hackathon/actions/runs/44868041222) tested `57a1734` on native Linux/amd64 with four workers on an Intel Xeon 6975P-C. The recommended practice commit `4f75f52` differs only in documentation.

- **Final size, seed 13:** 60M events + 15M clicks took **12.323 s** for one full `docker run` under a **14 GiB memory cap with swap disabled**. Stdout was empty. The organizer’s comparer matched all **6,615,971 rows** against the scored baseline `cc9f33e`: 6,587,957 in `user_daily` and 28,014 in `campaign_daily`, with zero differing rows or duplicate keys.
- **Practice size, seed 7:** across five alternating pairs on 20M events + 5M clicks, full Docker medians were **5.779 s baseline → 2.153 s rebuild (2.68×)**, including startup and removal. Process-only medians on that runner were 5.783 s → 2.050 s.

## Rebuilt pipeline: whole-process comparisons

Scored baseline `cc9f33e` vs the SIMD/direct-routing/typed-writer candidate (`3f1d667`).

During testing, the [public Shopping Sessions board](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions) advanced to **0.65 s** (Mahalas, score timestamp 2026-11-21 03:45 UTC). The earlier 0.68 s figure below describes the target when this round started.

| Host, input | Scored binary | Rebuild | Speedup |
|---|---:|---:|---:|
| M2 Pro, 8 workers, 20M seed 7 | 2.339 s | 0.833 s | 2.81× |
| M2 Pro, 8 workers, 20M seed 15 (leap day) | 2.329 s | 0.773 s | 3.01× |
| [Native Linux Intel Xeon 6975P-C](https://github.com/lmaccart/databricks-hackathon/actions/runs/44865536651), 4 workers, 20M seed 7 | 6.112 s | 2.078 s | 2.94× |
| [Native Linux AMD EPYC 7763](https://github.com/lmaccart/databricks-hackathon/actions/runs/44865758103), 4 workers, 20M seed 7 | 7.708 s | 2.635 s | 2.93× |

Medians of five warm, alternating-order process pairs. Mac used the equivalent rustfmt/lint cleanup build. These are end-to-end **local/CI** measurements, not hidden practice results. Rows in each table are compared by key, not by Parquet file bytes.

Mac raw samples in run order, seconds:

| Seed | Baseline | Candidate |
|---|---|---|
| 7 | 2.694, 2.271, 2.300, 2.339, 2.349 | 0.847, 0.905, 0.833, 0.780, 0.791 |
| 15 | 2.381, 2.177, 2.329, 2.391, 2.226 | 0.688, 0.698, 0.916, 0.816, 0.773 |

Seed 7 candidate median phase increments: setup/links 0.042 s, event parse and routing 0.304 s, exact duplicate resolution 0.064 s, click routing 0.075 s, gather/sort/session/attribution 0.189 s, merge and write 0.108 s, residual startup/exit 0.041 s. Per-phase medians need not add to the total median. The old full-record event dedup phase alone took a median of 0.651 s in the same pairs.

The rebuilt binary repeated all **60 official-generator comparisons** against the Python oracle (20 seeds each at 10k, 100k, and 1M). All matched. Native final-scale exact diff and Docker dev/practice checks are detailed below.

## First hidden practice result

Leif reports **5.03 s** for `cc9f33e`. At the user-reported 0.68 s lead that is a 7.40× gap on the practice clock. The 2.45 s native baseline below is a different measurement; use interleaved local runs to rank changes and a fresh organizer run to evaluate rank.

`sample PID 8 1 -mayDie`, all eight worker threads, scored binary on seed 7 / 20M. Top non-blocked leaves (sample counts, **not** percentages of wall time): event-shard `main` worker closure 2,520; system `memcmp` 1,097; `split_fields` 926; event `quicksort` 910; `unquote` 785. `parse_ts` accounted for another 479 leaf samples. The profiled run reached `written` at 2.348 s; differences between consecutive logs were 0.220 setup, 0.556 event parsing, 0.613 event dedup, 0.233 clicks, 0.591 sessions and 0.135 s writing. Compiler inlining means a worker-closure sample cannot be attributed to one statement.

### Controlled 20M experiments

`tools/bench_pair.py BASE CANDIDATE vendor/practice` on the M2 Pro, seed 7, 20M events / 5M clicks, eight Rayon workers, identical input, a fresh output/scratch directory per process, one untimed warmup per build, then five alternating-order pairs. These medians compare **code versions**, unlike inter-machine scoreboard times. No filesystem cache flushing. Always compare against the measured control in the same row.

| Candidate vs control | Control | Candidate | Wall time reduction |
|---|---:|---:|---:|
| Exact 64-bit person keys vs `cc9f33e` | 1.909 s | 1.489 s | 22.0% |
| Direct payload routing + small duplicate claims vs person keys | 1.556 s | 1.253 s | 19.5% |
| Parallel identity graph + exact flat lookup vs direct routing | 1.525 s | 1.346 s | 11.7% |
| Reversible routing token, sampled bucket reservations and consuming gather vs flat lookup | 1.194 s | 1.157 s | 3.1% |
| Prefetch the canonical-name slot 16 rows ahead vs routing | 1.223 s | 1.089 s | 11.0% |
| Typed Parquet writer + exit after explicit close vs prefetch | 1.073 s | 1.036 s | 3.4% |
| Single-pass structural SIMD + fixed-word timestamp decoder vs writer | 0.949 s | 0.774 s | 18.5% |
| Direct click routing + compact ID + tie resolution during the window scan (original assist counters) vs SIMD | 0.786 s | 0.723 s | 8.1% |

The explicit prefetch hint dereferences no input and is issued only for an in-bounds slot in the live alias table. A small FIFO overlaps that lookup with the next rows' parsing. Other architectures can run the same FIFO without an ISA-specific hint.

**Native Linux check, not Rosetta:** [GitHub Actions run 44864533547](https://github.com/lmaccart/databricks-hackathon/actions/runs/44864533547) used four workers on an Intel Xeon 6975P-C. `cc9f33e`: **6.385 s**; the rebuilt writer/prefetch checkpoint (`d2cd1b6`): **2.876 s**, or **2.22×** faster. All five candidate runs were 2.843–2.922 s, and the exact dev check passed. The harness times process start to exit on the runner, not an official eight-hyperthread container.

**Allocator control:** identical amd64 executable under Rosetta, glibc vs Debian mimalloc: 3.534 vs 2.605 s across five interleaved full Docker runs. But on [native Intel](https://github.com/lmaccart/databricks-hackathon/actions/runs/44864914334), the identical-binary medians were 2.795 vs 2.721 s, only **2.6%**. No mimalloc dependency or preload in the submitted image. An emulation-only improvement is not enough.

**Whole-binary CPU multiversioning:** on the EPYC runner, building the application and all dependencies for SSE4.2/POPCNT/AVX2/BMI1/BMI2/AES behind a baseline feature-detecting launcher gave 2.554 vs 2.616 s (2.4%). Exact dev results, but below cutoff. Production ships the single portable binary with runtime SIMD selection inside the parser; the dispatch launcher is for the manual CI experiment only.

Static musl/`FROM scratch` was attempted once on the heavily loaded emulated Docker VM; the build exceeded ten minutes and was stopped at ~726 s. No static runtime sample exists; this is a build-risk finding for that environment, not a claim that native musl is slower.

**Rebuilt candidate `3f1d667`:** restricted amd64 Docker build 59.61 s; dev matches both official tables. On 20M seed 7, the full starter invocation took 3.38 s under emulation and matched every reference-checkpoint row (2,242,393 user-days / 28,014 campaign-days), with zero stdout. On native 60M seed 13, the same click/event data path finished in **7.88 s** with **9.25 GiB** max RSS in one capacity run (other work on the host). Both tables matched the scored implementation **row-for-row**, including 6,587,957 distinct user-days; the separate global self-check passed. These are not hidden scorer times.

Rejected trials: a second round of hash bucketing before the session sorts gave 1.879 vs 1.836 s on the compact-key control. Direct click routing plus lazy interval assist counters gave 1.841 vs 1.572 s in a noisy batch. Neither result meets the five-run end-to-end acceptance threshold; retain the existing sorts and click path for that checkpoint.

Shard/chunk sweep on the routing checkpoint (each is a five-pair candidate vs its own `256 shards / 8 MiB` control): 64/2 MiB: 1.412 vs 1.331 s; 64/8 MiB: 1.245 vs 1.215 s; 128/4 MiB: 1.255 vs 1.254 s; 32/2 MiB: 1.298 vs 1.271 s. None justify a parameter change.

Other exact representations that passed correctness but missed the cutoff on the integrated native run: 16-byte claims with a lossless outlier table (0.998 vs 0.971 s); 16-byte session events selected only when time offsets/category IDs provably fit, with a wide fallback (0.776 vs 0.791 s); and an ingestion-month cache (1.114 vs 1.132 s). These remain experiments; production retains full-width timestamps.

Two more negative results: fusing the gather with a most-significant-radix partition before local sorting yielded 0.852 vs 0.820 s. LLVM application PGO trained on a separate 250k-event kit input (no dev answers or benchmark seed in training) yielded 0.746 vs 0.709 s. Neither is in the production build.

**Finalist challenge on an isolated Xeon:** [two-seed, four-worker A/B](https://github.com/lmaccart/databricks-hackathon/actions/runs/44867392447). Compared directly with `3f1d667`. Grouping several shards into a bounded dense campaign/day table and clearing only purchased categories improved seed 7 by 1.9% (2.066 → 2.026 s) and seed 15 by 1.8% (2.052 → 2.015 s). A cache-window classifier that builds delimiter lists and validates all quotes via row popcounts was slower: 2.062 → 2.143 s and 2.039 → 2.096 s. Combining them also lost. A local run under severe host contention was discarded; the native comparison decided the result. Keep the simpler scanner and campaign accumulator. Doubling the Rayon pool above allocated logical CPUs did not help (2.068 vs 2.071 s).

**Replacing the session timeline:** Opus built an exact alternative that coalesced `(person, floor(ts/30 min))` cells before sorting. All points in one cell are within the session gap; consecutive cells join iff the next minimum minus the preceding maximum is at most 30 minutes. Preserving extrema and a sparse purchase list handles midnight, category ties and true session starts. The [native 20M trial](https://github.com/lmaccart/databricks-hackathon/actions/runs/44867815032) on the writer checkpoint found just 2.867 → 2.852 s on seed 7 and 2.817 → 2.802 s on seed 15. Despite a promising isolated session microbenchmark, this added complexity buys less than 1% end-to-end. Not shipped.

Opus also tried the opposite event layout, first scattering 40-byte source-span rows by event ID, then passing only frequency-filter collisions to a full byte comparison. On the unmodified scored pipeline this measured 2.149 → 1.927 s (10.3%). The integrated entry uses direct payload routing instead; these are alternative algorithms, not additive gains.

Short ASCII person names are byte-packed, reversible keys; non-ASCII, long names and IDs ending in NUL are assigned exact interned keys in a disjoint namespace. Canonical names are still chosen by comparing original bytes. Session-event records shrink from 48 to 24 bytes and the comparator no longer follows string pointers. Zero-padded IDs, mixed case, embedded NUL and long names are covered by the suite.

The event parser now sends valid payloads directly to the eventual person shard. Independently, **every** syntactically valid, identified copy sends a small claim to the event-ID shard, including copies with invalid contents. A blocked Bloom filter proves most IDs unique; collisions only cause extra exact-map work. Every real duplicate necessarily sets its “repeated” bits on its second sighting, so the second scan includes *all* copies of that ID in exact earliest-ingest selection. Only losing payload locations are removed. A purchase is the only event that probes the product catalog.

## What the official material established

The printed track page times **container start to exit** on eight pinned hyperthreads with 24 GB and no swap. The final runs approximately 60M events three times; any run over 180 s means no final score. The printout's practice leader was 0.75 s. Neither Opus's 0.59 s on 2M kit events nor a native Mac timing is comparable to that leaderboard.

`docs/rules.md` is the byte-for-byte organizer download. The transcription differs in its preface, table padding and paragraph breaks, not in rules. The starter pins the public dev arguments to seed 1, **1,000,000 events and 250,000 clicks**. The root Dockerfile now builds the locked Rust crate; the Docker context admits only its manifests and source.

For reproduction, the local organizer downloads had these SHA-256 hashes:

```
9dc9c600fb483292de92932ab30b0d40ec626b14df92777d84451c59cc2a2437  rules.md
13724cadbac5a99d847e27cea911cd2e338ff972b46c2039da87452011bb101e  generator.zip
7eb8e1f0b1f431d178d28ac9437ce85809c44eb6393fdd42fb59857f30834fa9  dev-answer.zip
b84ff8726c43749043c33c250548a9ac9987b9132d28ac818a47515bd128f77c  starter-duckdb.zip
```

## Correctness audit

- Both implementations pass the **36 official hand-written fixtures** and the local test suite. On the native dev run, both independent comparers found **117,038 user rows and 27,400 campaign rows**, with zero missing, extra, differing or duplicate keys. Stdout: zero bytes. The Python oracle independently matched the published dev answer as well. Ten kit-generated 20k-event seeds also matched the oracle.
- Both implementations previously returned success if a required part directory could not be enumerated. They now fail with the input error. The regression test uses a file where the directory should be, so it works regardless of uid.
- The Python oracle used `int()` on unbounded digit strings. A valid timestamp or price padded with 5,000 zeros crashed it on current Python. Strip leading zeros and bound conversion by significant length; the Rust parser already handled this input. A regression also covers an enormous out-of-window F3 and an over-limit purchase.
- The kit comparer converted a numeric Parquet ID to a string and could report a match that the scorer rejected. It now checks string types and keeps null distinct from an empty Parquet string. Local edge tests now detect duplicate keys before indexing outputs by key.
- Use a fresh output directory for each run. Reusing the same directory with fewer Rayon workers can leave extra `part-*.parquet` from a previous run. The official harness and the new native benchmark use fresh directories.
- Docker parity: built the final `linux/amd64` source snapshot in 114.90 s (cached layers, release binary recompiled). Image: 29,596,367 bytes. The official runner passed at uid 1000, read-only root, no network, `--cpus 8 --memory 24g --memory-swap 24g`, with zero stdout and an exact dev-answer match. Local run: 1.01 s, emulated on an ARM64 Docker VM with only 7.75 GiB physically allocated; this exercises the container contract, not full-scale Linux memory capacity.
- Official generator differential matrix: **60/60 matched** for seeds 1–20 at each of 10k, 100k and 1M events, with one click per four events. Seed 15's generated window was `[2028-02-17, 2028-03-02)`, so all three sizes exercised leap day.

## Native scale evidence

Baseline: original Rust data path on an Apple M2 Pro (32 GiB RAM), release build, `NCPU=8 RAYON_NUM_THREADS=8`, local APFS, fresh output directories, `/usr/bin/time -l` start to exit. The file cache was not flushed. These are neither pinned Linux hyperthreads nor hidden-score timings.

| Official input | Wall time | Maximum RSS | Result |
|---|---:|---:|---|
| Seed 7, 20M events + 5M clicks, 1.92 GB of input | **2.45 s median of five** | 6.25 GiB maximum across runs | Native and amd64 Docker outputs matched row-for-row |
| Seed 13, 60M events + 15M clicks, 5.71 GB of input | 14.97 s, one run | 9.38 GiB | 6,587,957 unique user-days, 28,014 unique campaign-days; both tables sum to 9,944,073 sessions and 25,416,324,016 cents |

20M wall times, in order: **3.67, 2.51, 2.45, 2.00, 1.88 s**. All five used the same executable and data. The downward trend and changing host load make this a baseline, not evidence of a code speedup. The fifth run was taken after the harness restarted; the first four results were preserved.

The 60M run overlapped compilation on the host. Its 14.97 s comprised 0.71 s setup/links, **3.43 s event parse, 4.40 s event dedup, 3.82 s sessions**, 1.65 s clicks, 0.40 s writing and ~0.56 s from the last log to process exit. No stdout or scratch usage. `tools/check_scale.py` verified schema, uniqueness and cross-table invariants on the full output; no hand-written expected answer exists for that seed. The 24 GB no-swap scorer limit was not enforced on this native run.

Interpret the phase labels carefully: “sessions” includes copying from the dedup buckets before sorting. “Written” includes the serial campaign-counter merge and Arrow batch construction as well as Parquet I/O. Split those timers before choosing a sort or writer replacement.

**Practice-size Docker cross-check:** the final, emulated amd64 image processed seed 7 (20M events + 5M clicks) in 4.60 s for the complete starter shell invocation, 4.129 s to the binary's `written` log. Empty stdout; 2,242,393 unique user-days and 28,014 unique campaign-days; schema, keys and sums passed. This is a single local measurement, not the native five-run median or a practice submission.

## Original review: hypotheses before the rebuild

These were hypotheses for the scored v1, not measured speedups. The controlled experiments at the top of this document record which ideas survived. Measure start-to-exit, since moving work between phases can look like an improvement in one log line.

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

**A larger experiment for assists:** the 60M output counted 103,189,966 assists. The current session loop performs a hash-table update for every one. For a person with many sessions and campaigns, invert the calculation: a click at `t` assists session starts in `[t, t + 7 days]`. Compute each campaign's union of such intervals for that person, find covered ranges in their sorted session starts, and add range lengths to the corresponding start-day totals. This counts overlapping clicks once and still includes the crediting campaign. Try it only for people with expensive windows; on people with one session, building intervals is likely more work than iterating the active campaign list. Diff exact assist-only rows, ties, midnight and both 7-day boundaries. The simpler first experiment is to retain the current algorithm but avoid hashing the `(campaign, day)` on each increment.
