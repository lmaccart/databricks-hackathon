# Where one input row goes

The scored v1 (`cc9f33e`, 5.03 s) sent every parsed event through an event-ID hash table and **then** copied the winner to a person partition. The rebuilt pipeline sends a valid event's payload directly to the person partition. A much smaller, independent stream of claims tells it which payloads to throw away.

`events/word.rs` handles common rows whose header order, field boundaries, quoting, and typed values it can prove from bounded byte loads. Its ID load both validates the spelling and produces the exact inline token. A failed guard retries from the start of that physical line with the strict CSV scanner; noncanonical headers use the normal mapped-column path for the whole chunk. Both paths emit the same claim and tentative payload.

```text
manifest, products, campaigns ────────────────► windows and dimension lookups
links ─► exact ID keys ─► parallel union-find ─► immutable ID → canonical person
events ─► strict CSV ┬─ valid payload ─────────────────► person bucket
                    └─ claim, including invalid copies ► event-ID bucket
                                                            │
                                          duplicate filter + exact earliest copy
                                                            │
                                      remove losing payloads from person buckets
clicks ─► validate and resolve identities ──────────────────► person buckets
                 │
        gather by person partition ─► time ordering ─► sessions, credit, assists
                 │
        user/day rows and campaign/day counters ─► Parquet
```

- **Identity (`person.rs`, `links.rs`).** Up to nine nonempty ASCII bytes fit losslessly in 63 bits if the final byte is nonzero. Other spellings are interned using the **entire** ID as key. Leading zeroes, NULs, Unicode, spaces and case remain significant. Parallel link unions always attach the larger root name to the smaller using atomic compare-and-exchange. An originally empty link slot can admit one exact singleton; other singletons use their full reversible keys. Ranked identities use a reversible within-block permutation: the low byte is their owner, the remaining bits are their compact address within that owner. Dynamic ranks use `linked node count + slot * lanes + lane` before the permutation. Looking up the output name reverses that permutation and uses the canonical node or the atomic slot witness.
- **Timestamps (`ts.rs`).** The manifest supplies a bounded set of likely UTC dates. A fast F1 lookup checks all ten date bytes, every clock separator and digit, and the valid hour/minute/second ranges. Other dates use the full Gregorian calculation; offsets, raw epoch milliseconds and invalid values retain their regular parser.
- **Events (`events.rs`).** Even a well-formed but invalid event can suppress a later valid copy. It emits an ingest claim and no payload. Malformed lines and empty event IDs emit neither. A 16-byte claim stores a reversible event-ID token (top eight bits implicit in the shard), payload location, and exact ingest order. Four-digit F1 years fit as signed whole seconds; exceptional timestamps use full milliseconds in a per-chunk side table. A per-shard Bloom filter identifies IDs that *might* repeat. False positives send extra claims to the exact table; they never suppress a row. Actual duplicates mark their repeated bits on the second occurrence, so the exact pass includes the first occurrence too. A winning invalid copy has no payload; all losing valid copies are removed. Category lookup only runs for valid purchases.
- **Sessions (`stream.rs`, `ranges.rs`, `main.rs`).** For ranked people and windows with 32-bit millisecond offsets, histogram the local rank and scatter four-byte timestamps, compact purchases and clicks into exact person ranges. Only occupied addresses are prefixed; sparse shards use a sparse histogram and click-only ranks are ignored. Sort within each person, or for hot people use exact time-bin extrema and counts: the bin width is smaller than the session gap, so only consecutive nonempty bins can delimit a session. Singletons that lose their vacancy claim and wider windows use the full (exact person key, timestamp) sort. Purchases contribute to both timelines and sparse price/category streams. All purchases through a session's inclusive end count for its start day. There is no click-ID shuffle: identical click copies enter and expire together. Multiplicity changes neither the minimum click ID at the latest timestamp nor the set of active campaigns. Resolve byte-wise ties only while advancing the window.
- **Output (`output.rs`).** Blocked, typed column writes use the Parquet crate's page and footer encoders without building Arrow record batches. `day` is DATE, counts are integers, only `top_category` is nullable. Both tables always have a readable file, including on empty input. Files are explicitly closed before exit.

The independent slow oracle is `reference/pipeline.py`. `make fixtures` compares both implementations to the official hand-written cases. `tools/bench_pair.py` alternates baseline/candidate order in five pairs and times whole processes with fresh output directories. See [measurements and rejected experiments](docs/NOTES.md).
