# Trials against the 1.54 s Shopping Sessions entry

Leif reported **1.54 s** for the latest repository build and a **0.30 s** practice lead shared by several teams. `main` at the report (`dfbd330`) has the same production source as [`7fd1b76`](https://github.com/lmaccart/databricks-hackathon/commit/7fd1b7602280cfce05ac97b6afb703fa42c0b6ba). All new comparisons use that code as the control unless explicitly labeled otherwise. See the [previous round](PERF_157.md) for changes already measured.

Seed 7 has 1.921 GB of event, click and link CSVs before the dimension files and output. Completing those inputs in 0.30 s requires processing at least **6.40 GB/s** while also resolving identities, deduplicating, sessionizing and writing the two tables. This is a throughput requirement, not a measured hardware limit.

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

The column writer won **10/10** pairs and cut the seed-7 output stage from 0.094 to 0.054 s. It emits each person's spelling once per run, carries typed numeric columns directly from session aggregation, and encodes nullable categories with bounded dictionaries. Singleton ranks won **9/10**; their strongest effect was in sessions (0.456 → 0.427 s on seed 7). A singleton can claim a previously empty slot only by comparing its full exact ID; occupants never move and a collision retains the full-width key.

The radix rewrite removed work from the initial parse/route phase (0.727 → 0.639 s) and session pass (0.454 → 0.390 s), but its additional grouping/coalescing passes cost more than that saved. The suffix motif hit almost 97% of dev rows and nevertheless regressed on both practice seeds. An isolated 1M-event 32-bit time-bin prototype also lost 14–19% against exact temporal sorting; it did not reach an end-to-end trial.

For PGO the workflow extracted and timed the trained executable actually shipped by the image. The candidate built in 73 s on this runner, passed 60 Python comparisons and matched all 6,615,971 final-scale rows. It lost **every** pair, including 1.789 → 1.897 s in full practice Docker and 5.463 → 5.725 s at 60M. The event parse/route regression outweighed the smaller gain on clicks.
