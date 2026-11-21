# Trials against the 1.54 s Shopping Sessions entry

Leif reported **1.54 s** for the latest repository build and a **0.30 s** practice lead shared by several teams. `main` at the report (`dfbd330`) has the same production source as [`7fd1b76`](https://github.com/lmaccart/databricks-hackathon/commit/7fd1b7602280cfce05ac97b6afb703fa42c0b6ba). All new comparisons use that code as the control unless explicitly labeled otherwise. See the [previous round](PERF_157.md) for changes already measured.

Seed 7 has 1.921 GB of event, click and link CSVs before the dimension files and output. Completing those inputs in 0.30 s requires processing at least **6.40 GB/s** while also resolving identities, deduplicating, sessionizing and writing the two tables. This is a throughput requirement, not a measured hardware limit.
