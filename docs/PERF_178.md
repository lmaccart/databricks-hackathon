# Optimization trials against the 1.78 s practice build

Leif reported **1.78 s** for the rebuild. The native comparisons here use `4f75f52` as the immutable control unless a different control is noted. Each row is a separate four-worker Linux/amd64 job; compare **within a row**, since runner CPUs vary. Seed 7 and seed 15 both have 20M events and 5M clicks; seed 15 includes leap day. Each time is the process start-to-exit median of five alternating-order pairs after warmup. The jobs also compare all rows on both inputs and check the published dev answer and 36 fixtures.

A [native sampling profile](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869205518) of the control found 11.0% of on-CPU samples in the event CSV scanner, 9.9% in the timestamp parser, 9.0% in the event routing closure, 8.8% in click parsing, and 6.8% in shopper-table prefetch. Event quicksort and smallsort together accounted for 12.5%. These are sampled symbols with inlining, not percentages of elapsed time. A separate five-pair comparison of the control against itself differed by 0.1% on this runner.

| Trial (CI logs) | Seed 7, control → trial (s) | Seed 15, control → trial (s) |
|---|---:|---:|
| [Dense exact dimension lookups and pipelined click routing](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869352799) | 2.179 → **2.079** | 2.191 → **2.078** |
| [Staged per-destination event writes](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869200797) | 2.046 → 2.081 | 2.058 → 2.062 |
| [Group exact people, radix-sort long timelines](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869203233) | 2.201 → 2.141 | 2.119 → 2.073 |
| [Reversible prefix-in-suffix ID encoding](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869526826) | 1.808 → 1.787 | 1.869 → 1.912 |
| [Count possible duplicates before routing claims](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869353054) | 2.055 → 3.136 | 2.053 → 3.173 |
| [Learned shopper-key family and hot-key cache](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869352857) | 2.037 → 2.009 | 1.995 → 1.999 |
| [Bounded row-local AVX2 masks](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869352865) | 2.023 → 2.153 | 2.014 → 2.125 |
| [Validate fixed-layout CSV quotes with block masks](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869531675) | 2.955 → 2.893 | 2.957 → 2.896 |
| [Linux read-ahead during identity construction](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869532216) | 2.015 → 2.031 | 2.044 → 2.028 |
| [Cache exact civil dates and decode time pairs](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869678937) | 2.076 → 2.018 | 2.042 → 1.976 |
| [Sort compact descriptors with exact collision repair](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869797256) | 2.153 → 2.099 | 2.114 → 2.034 |
| [Single cache-line shopper alias buckets](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869797440) | 2.196 → 2.162 | 2.187 → 2.097 |
| [Microbatch event timestamp decoding and routing](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869892726) | 2.033 → 2.087 | 2.052 → 2.093 |
| [Defer ingest decoding with input offsets](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869892882) | 2.673 → 2.730 | 2.618 → 2.688 |
| [Inline hot callbacks and the F1 ingest case](https://github.com/lmaccart/databricks-hackathon/actions/runs/44869974324) | 2.630 → 2.585 | 2.637 → 2.544 |
| [Thin collision-witness table](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870072846) | 1.974 → 1.981 | 1.885 → 1.954 |
| [Read click IDs from the source only on ties, vs direct dimensions](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870072900) | 2.502 → 2.499 | 2.468 → 2.477 |
| [Range-count assists and visit purchased categories](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870177766) | 2.036 → 2.006 | 2.034 → 2.008 |
| [Narrow Parquet columns where proven safe](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870147865) | 2.634 → 2.594 | 2.622 → 2.581 |
| [Sort exact linked-person ranks and keep purchases separately](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870267294) | 1.774 → **1.676** | 1.777 → **1.647** |
| [Table-driven clock digits](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870416867) | 2.601 → 2.594 | 2.582 → 2.556 |
| [Resolve link endpoint slots while building lanes](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870443966) | 2.697 → 2.654 | 2.627 → 2.627 |
| [Date cache plus verified sort descriptors, vs direct dimensions](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870476068) | 1.975 → **1.881** | 1.959 → **1.873** |
| [Unroll fixed headers in the original CSV cursor](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870495651) | 2.699 → 2.688 | 2.657 → 2.648 |
| [Chronological bins: aggregate extrema, sort sessions](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870495905) | 2.240 → **2.073** | 2.181 → **2.055** |
| [Radix-sort only meaningful descriptor bits](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870496064) | 2.613 → 2.565 | 2.605 → 2.508 |
| [Dispatch AVX-512 compressed separator positions](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870496496) | 2.021 → 2.032 | 2.033 → 2.021 |
| [Optimistic typed CSV decoding with one-line fallback](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870521940) | 2.188 → 2.179 | 2.166 → 2.178 |
| [Linked ranks, sparse purchases, exact catalogs and click prefetch](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870672021) | 2.087 → **1.876** | 2.074 → **1.859** |
| [Local ranks for standalone shoppers, vs linked ranks alone](https://github.com/lmaccart/databricks-hackathon/actions/runs/44870760171) | 1.936 → 1.919 | 1.920 → 1.908 |

The duplicate census roughly halved the time spent resolving claims, but the extra full-input scan cost more than it saved. The deferred-ingest variant had a similar tradeoff: reading the original input again on suspect IDs made duplicate resolution more expensive. The dimension/click change worked on both seeds: the median click phase fell from 0.218 to 0.154 s and from 0.221 to 0.156 s. Its direct table accepts only canonical, unpadded decimal suffixes after an exact learned prefix; all other spellings still use the original byte map.

Two different session representations also helped. The linked-rank sort reuses the node index from the union-find as an exact person ID; a compact event carries only rank and time unless it is a purchase. The chronological sweep counts events in bins no wider than the session gap and merges each person's extrema across adjacent occupied bins. It falls back to exact sorting if aggregation does not compress the input sufficiently. Neither optimization changes which events join a session or the UTC start day credited.
