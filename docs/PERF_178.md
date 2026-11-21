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

The duplicate census roughly halved the time spent resolving claims, but the extra full-input scan cost more than it saved. The deferred-ingest variant had a similar tradeoff: reading the original input again on suspect IDs made duplicate resolution more expensive. The dimension/click change worked on both seeds: the median click phase fell from 0.218 to 0.154 s and from 0.221 to 0.156 s. Its direct table accepts only canonical, unpadded decimal suffixes after an exact learned prefix; all other spellings still use the original byte map.
