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

## Two-seed comparisons

Each comparison uses five alternating-order process start-to-exit pairs per seed on one native amd64 runner after a warmup. Seed 7 and leap-day seed 15 contain 20M events and 5M clicks each. A successful challenge also compares every row on both inputs, checks the official dev answer, and runs 36 fixtures and the x86 Rust tests. Hardware varies between jobs; compare the control and challenger **within a pair**, never times from separate runners.
