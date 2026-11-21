# Trials against the 1.54 s Shopping Sessions entry

Leif reported **1.54 s** for the latest repository build and a **0.30 s** practice lead shared by several teams. `main` at the report (`dfbd330`) has the same production source as [`7fd1b76`](https://github.com/lmaccart/databricks-hackathon/commit/7fd1b7602280cfce05ac97b6afb703fa42c0b6ba). All new comparisons use that code as the control unless explicitly labeled otherwise. See the [previous round](PERF_157.md) for changes already measured.

Seed 7 has 1.921 GB of event, click and link CSVs before the dimension files and output. Completing those inputs in 0.30 s requires processing at least **6.40 GB/s** while also resolving identities, deduplicating, sessionizing and writing the two tables. This is a throughput requirement, not a measured hardware limit.

## Verified release checkpoint

### Allocator selection

**Practice SHA:** [`1a3d0fdf38c14e858a92ba53bfa29da2f0f9b94a`](https://github.com/lmaccart/databricks-hackathon/commit/1a3d0fdf38c14e858a92ba53bfa29da2f0f9b94a). Its Docker inputs and tests match the [timed `14ccc10` trial](https://github.com/lmaccart/databricks-hackathon/commit/14ccc10f1dfc0845760640eeee2fa3db62b3e87a). It combines physical identity slots, person-local temporal ranges, typed Parquet columns, bounded worker-local campaign/day counters and linked mimalloc 3.3.2 on Linux. Its `fast/` Git tree is `4f81720222297220b46c957167e202e3709c1091`. Extra parser reservations and allocator overrides did not consistently clear the incremental practice threshold head-to-head.

Direct comparisons against the production source of the 1.54 s entry, on four-worker native AMD runners. Each arrow is a pair of medians in seconds, control → candidate. The Docker clock starts before `docker run` and includes container creation and removal. Both executables receive a warmup and write fresh output on every run. The candidate won **53/54 pairs**, including all nine 60M pairs.

| Release check | 20M seed 7, native | 20M seed 15, native | 20M seed 7, full Docker | 60M seed 13, full Docker | Pairs won |
|---|---:|---:|---:|---:|---:|
| [Python oracle and packaged entry](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937040474) | 1.744 → 1.113 | 1.731 → 1.116 | 2.359 → **1.683** (−28.7%) | 6.617 → **4.627** (−30.1%) | 17/18 |
| [Independent run and second 60M input](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937344723) | 1.727 → 1.119 | 1.720 → 1.116 | 2.239 → **1.541** (−31.2%) | 6.633 → **4.676** (−29.5%) | 18/18 |
| [Repeat on EPYC 7763 with Python and peak memory](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937264092) | 2.100 → 1.349 | 2.081 → 1.330 | 2.357 → **1.552** (−34.2%) | 7.120 → **4.729** (−33.6%) | 18/18 |

**Correctness:** both 20M inputs, 60 independent Python comparisons, the official dev answer, all 36 official fixtures, opaque IDs, malformed CSV, and colliding/transitive identity graphs matched. The restricted image matched every row against `7fd1b76` on two 60M inputs: **6,615,971** rows on seed 13 and **6,629,430** on seed 948307. With 14 GiB and no swap, Linux `wait4` reported **7.80 and 7.76 GiB peak RSS** respectively; the corresponding cgroup charges were 3.20 and 3.11 GiB. Shared mapped input can be resident without being charged to this cgroup. Timings and memory were taken in separate runs.

On the oracle runner the first unpaired 60M container after data generation took **9.704 s**; the paired candidate median was **4.627 s**. The first run did not time the control under the same conditions. Use the paired numbers for the measured speedup; the organizers' input-cache state and eight pinned hyperthreads may produce different times.

For this exact source on the M2 Pro (system allocator, native arm64, eight workers), medians against `7fd1b76` fell **0.673 → 0.553 s** and **0.622 → 0.513 s** on the two 20M seeds, and **1.625 → 1.356 s** on the 60M holdout. It won 15/15 pairs. The holdout's peak process RSS was 8.00 GiB with four workers. The official dev checkers and 36 fixtures both passed locally; six independent Python holdouts at 250k and 1M events also matched.

A separate eight-worker correctness check kept the same ranked people, their timestamps and their clicks, then added one unused campaign dimension. With 3,503 campaigns the dense counters fit; with 3,504 the same 14-day manifest required sparse counters. Both native outputs matched Python on **7,004 user-days and 3,506 campaign-days**, including assist-only rows on two nonconsecutive dates.

### Earlier checkpoints

[`d754296`](https://github.com/lmaccart/databricks-hackathon/commit/d75429665a35366dfef26096d63cd71c778b19f0) added bounded, per-worker campaign/day counters to the graph and range changes below. Against the scored build it won 18/18 pairs and reduced full Docker by **17.3% / 19.3%** at 20M / 60M. [In a direct comparison to `0558eaa`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936619619), it also won every pair including **1.573 → 1.455 s** at 20M and **4.731 → 4.474 s** at 60M in Docker.

[`0558eaa`](https://github.com/lmaccart/databricks-hackathon/commit/0558eaa7acd59463d21a888e1d418d676301e07f) superseded the first checkpoint below. Its physical-slot identity graph and direct temporal ranges won **18/18 pairs** [against the scored control](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935810599) and **18/18** [against the otherwise identical build without direct ranges](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935810612). The latter comparison gained 3.6% / 3.1% in restricted Docker at 20M / 60M on another model of AMD CPU and passed the 60-case Python oracle. Details below.

[`4ad0e5d`](https://github.com/lmaccart/databricks-hackathon/commit/4ad0e5d) combines the singleton slots and column writer. It won all **18/18** alternating pairs on a four-worker Xeon 6975P-C in the [packaged comparison against `7fd1b76`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934668743).

| Input and timing scope | Pairs | Control | Candidate | Less time |
|---|---:|---:|---:|---:|
| 20M seed 7, native process | 5 | 1.692 s | 1.590 s | 6.0% |
| 20M seed 15, native process | 5 | 1.633 s | 1.555 s | 4.8% |
| 20M seed 7, full restricted Docker | 5 | 1.816 s | **1.722 s** | **5.2%** |
| 60M seed 13, full restricted Docker | 3 | 5.532 s | **5.199 s** | **6.0%** |

Both 20M outputs and every one of the 6,615,971 60M rows matched. The same job passed 60 comparisons to Python, the mixed grammar corpus, opaque-ID and header-order checks with one/eight workers, the dev answer and 36 official fixtures. The final-scale container was limited to 14 GiB without swap.

A second 60M input, seed **948307** with 15M clicks, matched the scored control on all **6,601,416 user-days + 28,014 campaign-days** on ARM64. The candidate used 9.29 GiB peak RSS and wrote no stdout. On an eight-worker M2 Pro the two 20M process medians against `7fd1b76` were 0.670 → 0.627 s and 0.621 → 0.617 s, with nine of ten pairs faster. These process-only numbers and the hosted Docker numbers are not practice scores.

## What the clocks measure

On a [four-worker Intel Xeon 6975P-C](https://github.com/lmaccart/databricks-hackathon/actions/runs/44933674959), two builds with identical production sources differed by 0.3% or less on both practice seeds. The host CLI includes Docker creation and removal; the Engine interval is `StartedAt` to `FinishedAt`; `written` is the pipeline's internal timer through closing its Parquet files. Five-run medians:

| Input | Native start to exit | Host Docker command | Docker Engine | `written` in Docker |
|---|---:|---:|---:|---:|
| Headers only, both output tables written | 0.0014 s | 0.0847 s | 0.0495 s | 0.0010 s |
| Seed 7, 20M events and 5M clicks | 1.6695 s | 1.8042 s | 1.7311 s | 1.6370 s |

On the same runner, counting newlines in 1,661,455,555 event bytes took 0.0352 s; validating the fields took 0.3127 s; adding both IDs and timestamps took 0.4971 s. These warmed component kernels produce no output. Neither the hosted launch measurement nor the [public leaderboard](https://www.joinbuilders.org/buildathon/tracks/shopping-sessions) establishes the organizer's timing floor or explains how the four displayed 0.30 s entries were achieved.

A [separate Intel diagnostic using the production event grammar](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936182346) measured more than scanning field boundaries. The decoded-record count and valid-payload count agreed across all three kernels. Each number is the median of five warmed passes over the same 1.661 GB on four workers; none includes dedup, sessions or Parquet:

| Work performed | Time | Input throughput |
|---|---:|---:|
| Decode and validate complete events, including fallback rows, invalid claims, exact clocks, prices and categories | 0.4468 s | 3.72 GB/s |
| Additionally resolve and prefetch exact identities | 0.5295 s | 3.14 GB/s |
| Include both real payload and claim scatter streams | 0.6339 s | 2.62 GB/s |

Adding exact owner lookup cost about 83 ms in this comparison; writing the two streams added about 104 ms. The full program's parse/route interval was 0.717 s rather than 0.634 s: it runs in a fresh process with different allocator, cache and scheduling state. Even the isolated complete event decoder takes longer than 0.30 s on this host; a faster sort or writer cannot close the gap alone.

The [release commit profiled against its production-identical trial](https://github.com/lmaccart/databricks-hackathon/actions/runs/44938196873) took **1.170 s** at 20M on four Intel workers; the trial took 1.161 s. Event parsing and routing remained the largest interval (**0.611 s**), then sessions (0.243 s), clicks (0.121 s) and dedup (0.095 s). Of about 4,000 user-space CPU samples, 31.5% landed in the inlined event chunk worker, 11.5% in its parse closure, and 7.9% in F1 seconds conversion. These are symbol-level measurements; the inlined worker includes more than the CSV decoder.

## Two-seed trials

Each row uses five alternating whole-process pairs per seed. Compare **across the arrow**, not across rows: hosts range from AMD EPYC to different Intel Xeons. A successful challenge also checks the official dev answer, 36 fixtures, both 20M outputs row-for-row and the native Rust tests. `+` savings in the subsequent text means **less elapsed time**.

[Complete run ledger with commit IDs, CPUs, Docker timings and pair counts](PERF_154_RUNS.md).

| Candidate and run | Seed 7: control → trial | Seed 15: control → trial |
|---|---:|---:|
| [Claim empty alias slots for exact singleton sort ranks](https://github.com/lmaccart/databricks-hackathon/actions/runs/44933922444) | 1.683 → **1.636** | 1.653 → **1.612** |
| [Finalize user-days into typed Parquet columns](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934274560) | 1.673 → **1.630** | 1.659 → **1.597** |
| [Three independent typed event cursors](https://github.com/lmaccart/databricks-hackathon/actions/runs/44933675050) | 2.104 → 2.214 | 2.090 → 2.205 |
| [Contiguous radix rewrite: coalesce rows, then scatter survivors](https://github.com/lmaccart/databricks-hackathon/actions/runs/44933986127) | 1.677 → 1.752 | 1.650 → 1.797 |
| [Manifest-month ingest keys and deferred clock conversion](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934066640) | 1.681 → 1.684 | 1.672 → 1.667 |
| [Batch busy persons' campaign assists by lifetime ranges](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934066598) | 2.130 → 2.118 | 2.088 → 2.045 |
| [Verify and reuse product/price row motifs](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934274229) | 2.168 → 2.197 | 2.115 → 2.178 |
| [Stage aligned claim cache lines for non-temporal stores](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934277810) | 1.721 → 1.728 | 1.701 → 1.706 |
| [Exact base-36 standalone key plus timestamp](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934323811) | 1.460 → 1.546 | 1.427 → 1.500 |
| [12-byte claims with window-relative seconds and lossless escapes](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934368753) | 1.663 → 1.669 | 1.620 → 1.647 |
| [Build-time LLVM PGO trained on three synthetic layouts](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934102542) | 1.730 → 1.833 | 1.668 → 1.778 |
| [One physical slot for graph construction, ownership and sort rank](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935176864) | 1.700 → **1.589** | 1.644 → **1.549** |
| [Highway vector sort, exact wide and compact keys](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934661503) | 1.659 → 1.559 | 1.615 → 1.622 |
| [SSSE3 timestamp digit-pair dot products](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935024259) | 2.084 → 2.097 | 2.059 → 2.061 |
| [Two-extent bucket-owner peeling before exact dedup](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935456488) | 1.692 → 1.768 | 1.650 → 1.711 |
| [Fold union components locally, then resolve bridges](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935516680) | 1.685 → 1.796 | 1.626 → 1.724 |
| [Find newlines ahead and decode the tail backwards](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935522155) | 1.709 → 1.818 | 1.683 → 1.784 |
| [Prove both common-width ID addresses concurrently](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935548399) | 1.650 → 1.659 | 1.630 → 1.623 |
| [Overlap Linux `MADV_POPULATE_READ` with link-graph work](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935576835) | 2.078 → 2.131 | 2.074 → 2.127 |
| [Direct per-person temporal ranges, vs first singleton ranks `a32d0b7`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935266183) | 1.651 → **1.594** | 1.631 → **1.591** |
| [Timestamp-run click winners plus distinct assisting campaigns, vs `a32d0b7`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935456168) | 1.667 → 1.660 | 1.665 → 1.658 |
| [Pooled payload and claim pages, vs `4ad0e5d`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935576863) | 1.552 → 1.540 | 1.559 → 1.562 |
| [Encode a guaranteed session gap between consecutive ranks, vs `4933dbe`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935698391) | 1.531 → 1.547 | 1.486 → 1.495 |
| [Pack small result counts at first write, vs `4ad0e5d`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935845158) | 1.579 → 1.576 | 1.555 → 1.532 |
| [Size temporal streams using an already parsed prefix, vs `4933dbe`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935956505) | 1.967 → **1.866** | 1.935 → **1.861** |
| [One bounded campaign/day counter table per worker, vs `4ad0e5d`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936005555) | 1.700 → **1.591** | 1.711 → **1.576** |
| [Manifest-assisted F2 zones and F3 decimal prefixes, vs `4933dbe`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936134526) | 1.531 → 1.547 | 1.498 → 1.493 |
| [64-byte stripes separating duplicate-ID evidence from metadata, vs `4933dbe`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936377632) | 1.500 → 1.580 | 1.492 → 1.560 |

The column writer won **10/10** pairs and cut the seed-7 output stage from 0.094 to 0.054 s. It emits each person's spelling once per run, carries typed numeric columns directly from session aggregation, and encodes nullable categories with bounded dictionaries. Singleton ranks won **9/10**; their strongest effect was in sessions (0.456 → 0.427 s on seed 7). A singleton can claim a previously empty slot only by comparing its full exact ID; occupants never move and a collision retains the full-width key.

The radix rewrite removed work from the initial parse/route phase (0.727 → 0.639 s) and session pass (0.454 → 0.390 s), but its additional grouping/coalescing passes cost more than that saved. The suffix motif hit almost 97% of dev rows and nevertheless regressed on both practice seeds. An isolated 1M-event 32-bit time-bin prototype also lost 14–19% against exact temporal sorting; it did not reach an end-to-end trial.

For PGO the workflow extracted and timed the trained executable actually shipped by the image. The candidate built in 73 s on this runner, passed 60 Python comparisons and matched all 6,615,971 final-scale rows. It lost **every** pair, including 1.789 → 1.897 s in full practice Docker and 5.463 → 5.725 s at 60M. The event parse/route regression outweighed the smaller gain on clicks.

## Component head-to-heads in the restricted container

Each row compares a component with and without the other change on the **same** runner, at 20M (five pairs) and 60M (three pairs, no swap). The Highway package uses the same `libhwy` in host-native and container tests.

| Trial (baseline → candidate) | 20M Docker | 60M Docker |
|---|---:|---:|
| [First-slot ranks → first-slot ranks + columns (`4ad0e5d`)](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935189493) | 2.482 → **2.372** | 7.736 → **7.438** |
| [Columns → columns + first-slot ranks](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935189403) | 2.360 → 2.361 | 6.843 → 6.850 |
| [First-slot ranks + columns → physical graph slots + columns (`4933dbe`)](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935485266) | 1.729 → **1.681** | 5.170 → **4.994** |
| [Physical graph slots → physical graph slots + columns](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935485357) | 1.697 → **1.642** | 5.335 → **5.007** |
| [Scored control → Highway vector sort](https://github.com/lmaccart/databricks-hackathon/actions/runs/44934661503) | 2.026 → 1.975 | 6.208 → 6.274 |
| [Scored control → slot-based union alone, AMD EPYC 9V74](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935468441) | 2.227 → **2.150** | 6.723 → **6.257** |
| [Scored control → slot-based union + columns, AMD EPYC 7763](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935468450) | 2.258 → **2.096** | 6.870 → **6.332** |
| [Scored control → slot-based union, direct time ranges and columns (`0558eaa`), AMD EPYC 9V45](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935810599) | 1.792 → **1.617** | 5.451 → **4.618** |
| [Add direct per-person ranges to `4933dbe`, AMD EPYC 9V74](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935810612) | 2.042 → **1.968** | 5.917 → **5.734** |
| [Scored control → physical graph slots + columns, independent Intel repeat](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936040920) | 1.816 → **1.651** | 5.336 → **4.852** |
| [Size event buffers from live data on top of `4933dbe`, Intel repeat](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936299795) | 1.585 → 1.563 | 4.719 → 4.663 |
| [Add the same buffer sizing to direct ranges](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936375552) | 1.602 → 1.593 | 4.779 → 4.719 |
| [First-slot ranks + columns → add worker-local campaign/day counts, Xeon 8370C](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936299807) | 2.281 → **2.090** | 6.923 → **6.480** |
| [Direct ranges → link Linux mimalloc 3.3.2, same application source](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936526432) | 1.614 → **1.404** | 4.801 → **4.174** |
| [Scored control → direct ranges + worker-local counts (`d754296`)](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936619511) | 1.779 → **1.471** | 5.512 → **4.448** |
| [Scored control → direct ranges, worker-local counts and reservation hints (`7619fa6`)](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936619670) | 2.239 → **1.837** | 7.079 → **5.434** |
| [Scored control → the same three changes plus linked mimalloc (`894f6a7`)](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937040811) | 1.793 → **1.267** | 5.437 → **3.851** |
| [Add worker-local campaign/day counts to the right-sized direct ranges](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936631125) | 2.072 → **1.928** | 6.378 → **6.181** |
| [Add allocation hints to direct ranges + worker-local counters (`7619fa6` vs `d754296`)](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936631348) | 2.044 → 1.990 | 5.968 → 5.932 |
| [Direct ranges vs global temporal sort, identical graph, hints and campaign reduction](https://github.com/lmaccart/databricks-hackathon/actions/runs/44936740314) | 1.517 → 1.488 | 4.642 → 4.598 |
| [Add linked mimalloc 3 to direct ranges, allocation hints and worker-local counts](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937041255) | 1.434 → **1.254** | 4.490 → **3.868** |
| [Add worker-local campaign counts to direct ranges + mimalloc 3 (`14ccc10` vs `29ccc90`)](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937040599) | 1.577 → **1.482** | 4.827 → **4.482** |
| [Add live allocation hints to otherwise identical `14ccc10`](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937042928) | 1.487 → 1.481 | 4.619 → 4.523 |
| [Disable mimalloc page purging vs its defaults](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937148686) | 1.245 → 1.231 | 3.811 → 3.663 |
| [Align mimalloc purge regions to transparent huge pages vs its defaults](https://github.com/lmaccart/databricks-hackathon/actions/runs/44937148704) | 1.296 → 1.277 | 3.886 → 3.815 |

The initial singleton technique by itself added almost nothing to the columns in the two full-Docker comparisons on the Xeon 8370C. Replacing the graph construction as well made a measurable difference on the 6975P-C: every pair favored physical slots, including all three final-scale pairs. Keeping a slot as the union node eliminates endpoint sorting/deduplication and gives the parser a direct rank to write.

For `0558eaa` each claimed slot already identifies the destination worker partition. A histogram gathers events, purchases and clicks into exact per-person ranges. Small ranges sort just `u32` millisecond offsets; large ranges summarize bins narrower than the gap, stitching only if the actual neighboring extrema are at most 1,800,000 ms apart. Sparse graphs, large windows and overflow identities retain exact alternatives. It won all 18 pairs versus `7fd1b76` in the AMD job. The direct-range build also matched all 6,629,430 rows on the second 60M seed with 8.31 GiB peak RSS, and matched the Python oracle on deliberately colliding singletons and four 10,000-node components at one and eight workers under short and centuries-wide windows.

On that holdout, the 6,601,416 user-day rows occupied 283,929,323 bytes in the scored output and 222,761,807 bytes in the direct-range build. The two tables agreed on 9,966,291 sessions and 20,610,334,350 cents of revenue; the campaign table recorded 104,722,509 assists and no organic assists.

The linked Linux allocator won more than a library microbenchmark: every practice and final-scale pair favored it over the same direct-range application using the system allocator. In the seed-7 process comparison the event parse interval fell from 0.704 to 0.610 s, sessions from 0.366 to 0.310 s, and time outside the program's own timer from 0.041 to 0.020 s. This is a **different version and workload** from the earlier Debian mimalloc v2 preload experiment.

`14ccc10` also passed a third, differently shaped input in a clean tracked-source Linux/amd64 image: **21M events, 3.7M clicks, seed 2468113**, under **6 GiB, no swap**. All 2,350,590 user-days and 28,014 campaign-days matched the scored control; stdout was empty. The eight-worker process peaked at **2.82 GiB RSS** and the cgroup at **3.05 GiB** including charged cache. This ran through Docker Desktop's amd64 emulation: useful for packaging and output verification, not scorer timing.

The alternative with live allocation hints (`894f6a7`) also matched all 2,378,604 rows on this odd-sized input at **2.64 GiB RSS**. Emulated start-to-exit medians were **1.227 s without vs 1.260 s with hints**, with hints winning 2/5 pairs. Both images were about 116 MB as reported by Docker, run as uid 1000, and link the allocator into the executable; no preload or runtime package installation is necessary.

On native AMD, adding those hints to `14ccc10` saved only **0.4% at 20M and 2.1% at 60M** (median paired final-scale savings: **0.4%**). Disabling purging and forcing huge-page-aligned purging also missed the two-seed 3% cutoff: disabling purging saved 0.9% / 1.8% in the native practice medians despite its 3.9% full-Docker gain at 60M. The release uses the allocator defaults and the simpler parser.

For `7619fa6` on the M2 Pro, eight-worker native medians vs `7fd1b76` were **0.622 → 0.502 s**, **0.599 → 0.479 s**, and **1.753 → 1.409 s** on the independent 60M seed (all 15 pairs won). A separate same-executable comparison of four versus eight workers went **0.708 → 0.476 s** on seed 7 and **2.322 → 1.471 s** on the 60M holdout: 1.49× and 1.58× throughput, not 2×. This host has eight performance cores and four efficiency cores; these measured ratios do not predict the speed of eight pinned x86 hyperthreads.

## Reproduce the comparisons

```sh
# Use an immutable, public full SHA for the trial and the exact scored control.
control=7fd1b7602280cfce05ac97b6afb703fa42c0b6ba
candidate=14ccc10f1dfc0845760640eeee2fa3db62b3e87a
gh workflow run native-bench.yml --ref perf/practice-154 \
  -f revision="$candidate" -f baseline="$control" -f challenge=true \
  -f oracle=true -f containers=true -f scale=true -f scale_pairs=true

# Locally: exact output through both the ranked and wide-window paths.
make build
.venv/bin/python tools/graph_diff.py fast/target/release/shopping_sessions
.venv/bin/python tools/campaign_diff.py fast/target/release/shopping_sessions
.venv/bin/python tools/grammar_diff.py fast/target/release/shopping_sessions
.venv/bin/python tools/opaque_diff.py vendor/dev fast/target/release/shopping_sessions
```

The timed inputs are the public generator’s seed 7 and leap-day seed 15 (20M events / 5M clicks) and seed 13 (60M / 15M). Setting both `scale` and `scale_pairs` also runs the packaged application on independent 60M seed 948307 and compares every row. After timing, a separate `wait4` wrapper records Linux application peak RSS and the cgroup's peak charge, which can include file cache. The native and Docker timers use fresh output directories; none of the pipelines read earlier results. Each input is warmed once per executable for the paired measurements. The inside-container self-reported timer alone is not used for promotion.
