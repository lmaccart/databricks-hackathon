# Trials against the 1.57 s Shopping Sessions entry

The official time reported by Leif is **1.57 s** on the build with production sources from [`9758471`](https://github.com/lmaccart/databricks-hackathon/commit/9758471d06bbd916f8d029780d4d734e81bcc13f). The reported record, **0.63 s**, would require a further **59.9% reduction**. CI timings below measure actual inputs, but are not official practice times.

## Where the time goes

An [identical-build comparison and fresh Intel profile](https://github.com/lmaccart/databricks-hackathon/actions/runs/44874878855) gave **1.813 vs 1.817 s** on seed 7. Median phase intervals in the control were 0.099 s for the link graph and startup, 0.853 s for event parsing/routing, 0.119 s for event deduplication, 0.141 s for clicks, 0.461 s for sessions, and 0.096 s for output. The clock outside these phases contributed 0.041 s. Profiles include inlined work: the event scanner accounted for 15.2% of CPU samples, the two event closures for 15.3% together, identity prefetch for 6.9%, and fast F1 clock decoding for 5.5%.

An [independent component measurement](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875266190) on four AMD workers read the same **1,661,455,555 event-body bytes**. These are warmed, nested kernels, not end-to-end timings:

| Work performed | Median | Input throughput |
|---|---:|---:|
| Count physical line endings | 0.0437 s | 38.04 GB/s |
| Scan all fields and validate the strict CSV dialect | 0.2347 s | 7.08 GB/s |
| Do the above and decode event ID, raw user ID, event time and ingest time | 0.4414 s | 3.76 GB/s |

The complete event parse/route phase on that runner took 0.918 s. Even a free newline scanner would leave most of the work. Avoiding scatter writes and random identity lookups matters alongside parsing.

On all physical event rows of seeds 7 and 15, the typed grammar hit **21,235,584 / 21,593,666 (98.34%)** and **21,177,964 / 21,537,085 (98.33%)**. In a separate untimed field-level count, the product/price guard accounted for 57.9% and 53.6% of fallbacks, respectively. This is grammar coverage, not a claim that all these events passed validation or deduplication.

After the typed grammar landed, [a second identical-source comparison and CPU profile](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876652111) gave 1.652 vs 1.655 s on Intel. The new event phase took 0.721 s; sessions still took 0.450 s. About 27% of on-CPU samples were in the compiler-inlined chunk worker, 9.9% in its event emission helper, 5.9% in F1 seconds, and 8.2% in the two event quicksort symbols combined. Sample percentages for an inlined worker cannot be attributed to tokenization alone.

## Two-seed comparisons

Each comparison uses five alternating-order process start-to-exit pairs per seed on one native amd64 runner after a warmup. Seed 7 and leap-day seed 15 contain 20M events and 5M clicks each. A successful challenge also compares every row on both inputs, checks the official dev answer, and runs 36 fixtures and the x86 Rust tests. Hardware varies between jobs; compare the control and challenger **within a pair**, never times from separate runners.

Times below are seconds, **control → candidate**. The control is `9758471` unless named otherwise.

| Approach and CI run | Seed 7 | Seed 15 |
|---|---:|---:|
| [Typed event grammar: validate and pack the ID with the same load](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875889484) | 1.744 → **1.628** | 1.719 → **1.604** |
| [Repeat on AMD EPYC 9V45; actual images also compared](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876117068) | 1.419 → **1.333** | 1.410 → **1.311** |
| [Erase and validate quotes in SIMD tiles](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876025834) | 1.803 → **1.723** | 1.771 → **1.689** |
| [Join aliases after parsing into raw-user partitions](https://github.com/lmaccart/databricks-hackathon/actions/runs/44874977224) | 1.795 → 1.741 | 1.777 → 1.710 |
| [Repeat partition-local join on AMD EPYC 7763](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875630980) | 2.201 → 2.155 | 2.216 → 2.120 |
| [Typed grammar plus partition-local join vs **join alone**](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875992068) | 1.740 → **1.660** | 1.719 → **1.628** |
| [Same combination vs **typed grammar alone**](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876123028) | 1.669 → 1.656 | 1.682 → 1.645 |
| [Hardware bit-extract/deposit exact short IDs](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876029787) | 2.223 → 2.150 | 2.191 → 2.131 |
| [Independent prefix codecs for each role](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875312303) | 1.763 → 1.747 | 1.748 → 1.744 |
| [Calendar-free civil labels for earliest ingest](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875583146) | 1.830 → 1.781 | 1.790 → 1.762 |
| [Exact manifest prefixes for ISO, offset and numeric epochs](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875921500) | 1.773 → 1.763 | 1.754 → 1.727 |
| [Decode multiple independent physical-line lanes](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875632814) | 1.758 → 1.892 | 1.735 → 1.869 |
| [Contiguous event blocks and scatter only compact indexes](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875865022) | 1.761 → 1.854 | 1.753 → 1.864 |
| [One compound event-ID stream with late payload materialization](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875941524) | 1.838 → 1.839 | 1.824 → 1.810 |
| [One CSV pass plus mergeable repeat masks and sparse repair](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875260572) | 1.416 → 1.704 | 1.402 → 1.664 |
| [Block negative filter before exact link lookup](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875582703) | 1.504 → 1.547 | 1.458 → 1.530 |
| [Verified XOR-retrieval alias index](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875889460) | 1.776 → 2.440 | 1.768 → 2.450 |
| [Sort raw timelines, merge runs across aliases](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875864828) | 1.549 → 1.640 | 1.535 → 1.618 |
| [Build disjoint sessions online from unsorted events](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875489042) | 2.018 → 1.991 | 1.991 → 1.972 |
| [Compress hot persons into source-tile intervals](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875485590) | 1.777 → 1.786 | 1.772 → 1.766 |
| [Hot-person exact occupancy bitmap](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875864779) | 2.360 → 2.348 | 2.330 → 2.330 |
| [Two-dimensional person/time radix histogram](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876029903) | 1.800 → 1.854 | 1.751 → 1.807 |
| [32-bit whole seconds and exact fractional side stream](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876029768) | 1.938 → 2.032 | 1.904 → 1.968 |
| [Joint compact ordering for clicks and event bins](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876029421) | 1.779 → 1.805 | 1.731 → 1.775 |
| [AVX2 partition sort for 64-bit temporal keys](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875127108) | 1.785 → 1.822 | 1.766 → 1.802 |
| [Nested parallel sort on large shards](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875950339) | 1.828 → 1.828 | 1.809 → 1.813 |
| [Direct campaign/day arrays and sparse click walks](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875127263) | 2.439 → 2.409 | 2.437 → 2.390 |
| [Session workers stream rows straight to Parquet](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875362952) | 1.771 → 1.731 | 1.749 → 1.700 |
| [Streaming output vs partition-local join alone](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875969438) | 1.715 → 1.693 | 1.684 → 1.678 |
| [Direct, preencoded PLAIN Parquet pages](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876167101) | 2.235 → 2.240 | 2.223 → 2.208 |
| [Schedule independent input graphs concurrently](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875093386) | 1.769 → 1.874 | 1.725 → 1.744 |
| [Explicit Linux worker affinity within available cores](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876123134) | 2.237 → 2.248 | 2.212 → 2.251 |
| [Bounded Linux hugepage-advised input reads](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875885322) | 2.238 → 2.286 | 2.248 → 2.275 |
| [Guarded short-ID parsing for clicks and link endpoints vs typed event parser](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876381054) | 1.743 → 1.720 | 1.747 → 1.718 |
| [Preencoded output pages vs typed event parser](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876439131) | 1.343 → 1.314 | 1.318 → 1.309 |
| [Preencoded pages vs typed parser with partition-local join](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876377984) | 1.680 → 1.667 | 1.657 → 1.649 |
| [Batched exact link lanes vs partition-local join](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876386056) | 1.720 → 1.834 | 1.688 → 1.789 |
| [Give local owners dense ranks and avoid moving them, vs partition-local join](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876516182) | 1.840 → 1.996 | 1.829 → 1.944 |
| [One-pass quotient duplicate witnesses and sparse swap removal](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876551100) | 2.215 → 2.238 | 2.210 → 2.213 |
| [Pre-route sampled hot aliases vs the typed partition-local join](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876662705) | 1.342 → 1.336 | 1.295 → 1.318 |
| [Lossless radix-36 standalone subshards with exact length](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876749093) | 1.764 → 2.041 | 1.738 → 1.985 |
| [AVX-512 register-gather parser on a runner using the portable fallback](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876662877) | 2.263 → 2.333 | 2.239 → 2.294 |
| [Same candidate on Intel, with the native gather observed in `perf`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876963238) | 1.777 → 1.912 | 1.763 → 1.889 |
| [Column-ordered packets of 16 structurally checked rows](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877010150) | 1.786 → 1.897 | 1.786 → 1.908 |
| [Reuse the event date/hour for a matching ingest time, vs typed parser](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876956786) | 2.077 → 2.070 | 2.058 → 2.026 |
| [Project directly to final identity shards, vs typed partition-local join](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876902527) | 1.676 → 1.886 | 1.650 → 1.882 |
| [Same direct projection vs typed parser alone](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876923461) | 1.768 → 1.836 | 1.787 → 1.876 |
| [Shared UTC minute and exact residue throughout the pipeline](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877010665) | 1.801 → 1.958 | 1.764 → 1.914 |
| [Fuse local identity numbering and timestamp-only person bands, vs typed partition-local join](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877040950) | 1.665 → 1.878 | 1.637 → 1.846 |
| [Decode an event from one guarded 128-byte packet, vs typed parser](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877103230) | 1.651 → 1.708 | 1.623 → 1.696 |
| [Collision-tolerant routing components vs typed partition-local join](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877103589) | 1.650 → 2.431 | 1.636 → 2.420 |
| [Adaptive tile choice between typed and generic parsing, vs typed parser](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877138299) | 2.088 → 2.120 | 2.071 → 2.101 |
| [Exact one-word quotient/rank lookup vs typed parser](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877138125) | 1.645 → 1.673 | 1.617 → 1.649 |
| [Hardware bit extraction inside the fused ID load vs typed parser](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877191802) | 1.657 → 1.655 | 1.626 → 1.626 |
| [Common-date reuse plus typed click and link parsing vs typed event parser](https://github.com/lmaccart/databricks-hackathon/actions/runs/44877061972) | 2.179 → **2.092** | 2.152 → **2.085** |

Two instructive tradeoffs:

- Postponing the exact alias lookup reduced the Intel event phase from **0.844 to 0.637 s** and click parsing from **0.139 to 0.095 s**, but added a **0.165 s join** plus cost in the session phase. With the typed parser already present, this design saved only 0.8% and 2.2% in its first direct comparison.
- Compressing quote pairs did help on both seeds. Fusing validation and ID encoding saved more. In contrast, compact duplicate evidence followed by random rereads, or a new retrieval graph built from scratch at startup, cost more than their smaller intermediates saved.

## Eight-worker Mac comparison

Five alternating process start-to-exit pairs on an Apple M2 Pro, native arm64, warmed input, no Docker. Control `9758471` was built from a fresh worktree; candidate `7fd1b76` used the same release settings. On seed 7 the median fell **0.610 → 0.570 s** (5/5 pairs); on seed 15, **0.682 → 0.601 s** (4/5). All five candidate times were 0.573, 0.569, 0.576, 0.560, 0.570 s on seed 7 and 0.569, 0.556, 0.601, 0.644, 0.840 s on seed 15. Seed 15 slowed over the course of the run for both binaries. The Mac medians are below 0.63 s, but even the scored control ran seed 7 below 0.63 s here. They cannot establish an organizer score.

## Exactness and reproduction

The independent Python oracle checks 20 seeds each at 10k, 100k and 1M events. The added grammar corpus mutates timestamps, quoted fields, IDs and ignored prices on real physical lines. Each mutant competes with a later valid event for a different shopper, and valid lines follow malformed ones. This catches the difference between *invalid earliest* (suppresses the event) and *malformed earliest* (never enters dedup). As a negative control, deliberately dropping invalid claims before dedup produced **2,658 extra user-day rows** and a differing campaign row; the new verifier rejected it.

For the typed parser, six additional Python comparisons passed on three seeds outside the timing and standard oracle sets (6331, 20261121, 31415927; 250k and 1M events each). A second final-scale input, seed 20261121 with **60M events and 15M clicks**, matched the scored control row-for-row on native ARM64: **6,643,504 user-day and 28,014 campaign-day rows**. This was an exactness check, not an extra timing result.

The 60M timing comparisons below use the same workload (seed 13, 15M clicks, 14 GiB memory and no swap), but each row ran on its own host. Every comparison matched **6,587,957 user-day and 28,014 campaign-day rows**.

| Candidate vs scored `9758471` | Full 20M Docker, five pairs | Full 60M Docker, three pairs |
|---|---:|---:|
| [Typed event grammar](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876117068) | 1.868 → 1.766 s | 5.546 → 5.358 s |
| [Packaged typed parser `7fd1b76` with independent oracle and grammar checks](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876665326) | 1.942 → 1.831 s | 6.065 → 5.722 s |
| [Partition-local identity join](https://github.com/lmaccart/databricks-hackathon/actions/runs/44875630980) | 2.402 → 2.334 s | 7.285 → 6.856 s |
| [Typed event grammar plus partition-local join](https://github.com/lmaccart/databricks-hackathon/actions/runs/44876241143) | 1.900 → 1.758 s | 5.879 → 5.284 s |

To reproduce the native and Docker comparisons for a published candidate, set `CANDIDATE` to its full SHA:

```sh
gh workflow run native-bench.yml --ref main \
  -f revision="$CANDIDATE" \
  -f baseline=9758471d06bbd916f8d029780d4d734e81bcc13f \
  -f challenge=true -f oracle=true -f containers=true -f scale_pairs=true
```

To check mixed fast/fallback rows, opaque identities and header permutations locally:

```sh
make build
.venv/bin/python tools/grammar_diff.py fast/target/release/shopping_sessions
.venv/bin/python tools/opaque_diff.py vendor/dev fast/target/release/shopping_sessions
```
