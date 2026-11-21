# Where one input row goes

This experiment parses events and clicks into raw-user hash partitions before resolving identities. A much smaller, independent stream of claims identifies event payloads to discard before the partition-local identity join.

```text
manifest, products, campaigns ────────────────► windows and dimension lookups
links ─► exact ID keys ─► parallel union-find ─► immutable ID → canonical person
events ─► strict CSV ┬─ valid payload ─────────────────► raw-user bucket
                    └─ claim, including invalid copies ► event-ID bucket
                                                            │
                                          duplicate filter + exact earliest copy
                                                            │
                                      remove losing payloads from raw-user buckets
clicks ─► validate ────────────────────────────────────► raw-user buckets
                 └─ cache-local exact alias join ──────► person buckets
                 │
        gather by person partition ─► time ordering ─► sessions, credit, assists
                 │
        user/day rows and campaign/day counters ─► Parquet
```

- **Identity (`person.rs`, `links.rs`).** Up to nine nonempty ASCII bytes fit losslessly in 63 bits if the final byte is nonzero. Other spellings are interned using the **entire** ID as key. Leading zeroes, NULs, Unicode, spaces and case remain significant. Parallel link unions always attach the larger root name to the smaller using atomic compare-and-exchange. Unknown IDs map to themselves. A reversible permutation spreads person keys across shards, so a sort key also serves as the routing key. Reversing it recovers the exact ID for output. Link-table lanes are grouped exact tables keyed directly by the mixed raw token, with 32-bit root tags; the per-partition join caches compact root/route tags and prefetches misses in 16-row blocks. Standalone rows compact in place, while linked rows move to their canonical-owner shards.
- **Timestamps (`ts.rs`).** The manifest supplies a bounded set of likely UTC dates. A fast F1 lookup checks all ten date bytes, every clock separator and digit, and the valid hour/minute/second ranges. Other dates use the full Gregorian calculation; offsets, raw epoch milliseconds and invalid values retain their regular parser.
- **Events (`events.rs`).** Even a well-formed but invalid event can suppress a later valid copy. It emits an ingest claim and no payload. Malformed lines and empty event IDs emit neither. A 16-byte claim stores a reversible event-ID token (top eight bits implicit in the shard), raw-partition payload location, and exact ingest order. Four-digit F1 years fit as signed whole seconds; exceptional timestamps use full milliseconds in a per-chunk side table. A per-shard Bloom filter identifies IDs that *might* repeat. False positives send extra claims to the exact table; they never suppress a row. Actual duplicates mark their repeated bits on the second occurrence, so the exact pass includes the first occurrence too. A winning invalid copy has no payload; all losing valid copies are removed before joining. Category lookup only runs for valid purchases.
- **Sessions (`stream.rs`, `main.rs`).** Linked people can sort as 8-byte (canonical node rank, manifest-relative timestamp) keys. Unlinked people and over-wide windows use full (exact person key, timestamp) pairs. Only purchases carry price and category; after dedup, each contributes one key to the temporal sort and one record to the sorted purchase stream. The scan consumes all purchases through each session's inclusive end key. A gap strictly greater than 1,800,000 ms starts a new session. All counters, including purchases after midnight, use its start day. There is no click-ID shuffle: the rules guarantee identical copies, all of which enter and expire together. Multiplicity changes neither the minimum click ID at the latest timestamp nor the set of active campaigns. Resolve byte-wise ties only while advancing the window.
- **Output (`output.rs`).** Blocked, typed column writes use the Parquet crate's page and footer encoders without building Arrow record batches. `day` is DATE, counts are integers, only `top_category` is nullable. Both tables always have a readable file, including on empty input. Files are explicitly closed before exit.

The independent slow oracle is `reference/pipeline.py`. `make fixtures` compares both implementations to the official hand-written cases. `tools/bench_pair.py` alternates baseline/candidate order in five pairs and times whole processes with fresh output directories. See [measurements and rejected experiments](docs/NOTES.md).
