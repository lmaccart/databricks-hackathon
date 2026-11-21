# Where one input row goes

This experiment routes raw events and clicks directly to their final owner, without proving link membership during parsing. A separate stream of claims identifies event payloads to discard before the exact owner-local identity join.

```text
manifest, products, campaigns ────────────────► windows and dimension lookups
links ─► exact ID keys ─► parallel union-find ┬─► coarse owner projection
                                             └─► owner-local exact alias → rank
events ─► strict CSV ┬─ valid payload ──► projection ──► final-owner bucket
                    └─ claim, including invalid copies ► event-ID bucket
                                                            │
                                          duplicate filter + exact earliest copy
                                                            │
                                      mark losing payloads in final-owner buckets
clicks ─► validate ─────────────────────► projection ──► final-owner buckets
                                                           │
                   per-owner gather + exact alias join ────┘
                   │
        time ordering ─► sessions, credit, assists
                 │
        user/day rows and campaign/day counters ─► Parquet
```

- **Identity (`person.rs`, `links.rs`).** Up to nine nonempty ASCII bytes fit losslessly in 63 bits if the final byte is nonzero. Other spellings are interned using the **entire** ID as key. Leading zeroes, NULs, Unicode, spaces and case remain significant. Parallel link unions always attach the larger root name to the smaller using atomic compare-and-exchange. A reversible permutation produces the full token; reversing it recovers the exact ID for output. The low token bits select a byte in the raw lane's projection. Only slots with different linked owners need exact overflow resolution. An unknown ID may share a slot and inherit the owner's partition, but placement never establishes identity. The final owner's exact table and direct-mapped cache compare full tokens when gathering into the sort buffers. A miss keeps the original token; a hit supplies the canonical rank. No payload is routed again.
- **Timestamps (`ts.rs`).** The manifest supplies a bounded set of likely UTC dates. A fast F1 lookup checks all ten date bytes, every clock separator and digit, and the valid hour/minute/second ranges. Other dates use the full Gregorian calculation; offsets, raw epoch milliseconds and invalid values retain their regular parser.
- **Events (`events.rs`).** Even a well-formed but invalid event can suppress a later valid copy. It emits an ingest claim and no payload. Malformed lines and empty event IDs emit neither. A 16-byte claim stores a reversible event-ID token (top eight bits implicit in the shard), final-owner payload location, and exact ingest order. Four-digit F1 years fit as signed whole seconds; exceptional timestamps use full milliseconds in a per-chunk side table. A per-shard Bloom filter identifies IDs that *might* repeat. False positives send extra claims to the exact table; they never suppress a row. Actual duplicates mark their repeated bits on the second occurrence, so the exact pass includes the first occurrence too. A winning invalid copy has no payload; losing valid copies are marked in place and skipped by the gather before joining. Category lookup only runs for valid purchases.
- **Sessions (`stream.rs`, `main.rs`).** Linked people can sort as 8-byte (canonical node rank, manifest-relative timestamp) keys. Unlinked people and over-wide windows use full (exact person key, timestamp) pairs. Only purchases carry price and category; after dedup, each contributes one key to the temporal sort and one record to the sorted purchase stream. The scan consumes all purchases through each session's inclusive end key. A gap strictly greater than 1,800,000 ms starts a new session. All counters, including purchases after midnight, use its start day. There is no click-ID shuffle: the rules guarantee identical copies, all of which enter and expire together. Multiplicity changes neither the minimum click ID at the latest timestamp nor the set of active campaigns. Resolve byte-wise ties only while advancing the window.
- **Output (`output.rs`).** Blocked, typed column writes use the Parquet crate's page and footer encoders without building Arrow record batches. `day` is DATE, counts are integers, only `top_category` is nullable. Both tables always have a readable file, including on empty input. Files are explicitly closed before exit.

The independent slow oracle is `reference/pipeline.py`. `make fixtures` compares both implementations to the official hand-written cases. `tools/bench_pair.py` alternates baseline/candidate order in five pairs and times whole processes with fresh output directories. See [measurements and rejected experiments](docs/NOTES.md).
