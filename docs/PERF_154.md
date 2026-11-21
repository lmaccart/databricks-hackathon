# Trials against the 1.54 s Shopping Sessions entry

Leif reported **1.54 s** for the latest repository build and a **0.30 s** practice lead shared by several teams. `main` at the report (`dfbd330`) has the same production source as [`7fd1b76`](https://github.com/lmaccart/databricks-hackathon/commit/7fd1b7602280cfce05ac97b6afb703fa42c0b6ba). All new comparisons use that code as the control unless explicitly labeled otherwise. See the [previous round](PERF_157.md) for changes already measured.

Seed 7 has 1.921 GB of event, click and link CSVs before the dimension files and output. Completing those inputs in 0.30 s requires processing at least **6.40 GB/s** while also resolving identities, deduplicating, sessionizing and writing the two tables. This is a throughput requirement, not a measured hardware limit.

## Verified release checkpoint

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

## Two-seed trials

Each row uses five alternating whole-process pairs per seed. Compare **across the arrow**, not across rows: hosts range from AMD EPYC to different Intel Xeons. A successful challenge also checks the official dev answer, 36 fixtures, both 20M outputs row-for-row and the native Rust tests. `+` savings in the subsequent text means **less elapsed time**.

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
| [Scored control → slot-based union, direct time ranges and columns (`0558eaa`), AMD EPYC 9V45](https://github.com/lmaccart/databricks-hackathon/actions/runs/44935810599) | 1.792 → **1.617** | 5.451 → **4.618** |

The initial singleton technique by itself added almost nothing to the columns in the two full-Docker comparisons on the Xeon 8370C. Replacing the graph construction as well made a measurable difference on the 6975P-C: every pair favored physical slots, including all three final-scale pairs. Keeping a slot as the union node eliminates endpoint sorting/deduplication and gives the parser a direct rank to write.

For `0558eaa` each claimed slot already identifies the destination worker partition. A histogram gathers events, purchases and clicks into exact per-person ranges. Small ranges sort just `u32` millisecond offsets; large ranges summarize bins narrower than the gap, stitching only if the actual neighboring extrema are at most 1,800,000 ms apart. Sparse graphs, large windows and overflow identities retain exact alternatives. It won all 18 pairs versus `7fd1b76` in the AMD job. The direct-range build also matched all 6,629,430 rows on the second 60M seed with 8.31 GiB peak RSS, and matched the Python oracle on deliberately colliding singletons and four 10,000-node components at one and eight workers under short and centuries-wide windows.

## Reproduce the comparisons

```sh
# Use an immutable, public full SHA for the trial and the exact scored control.
control=7fd1b7602280cfce05ac97b6afb703fa42c0b6ba
gh workflow run native-bench.yml --ref perf/practice-154 \
  -f revision="$CANDIDATE" -f baseline="$control" -f challenge=true \
  -f oracle=true -f containers=true -f scale_pairs=true

# Locally: exact output through both the ranked and wide-window paths.
make build
.venv/bin/python tools/graph_diff.py fast/target/release/shopping_sessions
.venv/bin/python tools/grammar_diff.py fast/target/release/shopping_sessions
.venv/bin/python tools/opaque_diff.py vendor/dev fast/target/release/shopping_sessions
```

The timed inputs are the public generator’s seed 7 and leap-day seed 15 (20M events / 5M clicks) and seed 13 (60M / 15M). The independent second 60M input uses seed 948307. The native and Docker timers use fresh output directories; none of the pipelines read earlier results. Each input is warmed once per executable for the paired measurements. The inside-container self-reported timer alone is not used for promotion.
