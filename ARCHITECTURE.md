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

- **Identity (`person.rs`, `links.rs`).** Up to nine ASCII bytes fit losslessly in 63 bits if the final byte is nonzero; all other spellings are interned by the **entire** ID. A fixed open-addressed table assigns link endpoints to physical slots. A slot is both its union-find node and a permanent exact rank. Parallel unions attach the larger root name to the smaller. The low rank byte identifies the destination partition. An unlinked person may permanently claim one of a bounded set of originally empty slots; if other names occupy them, the full reversible identity goes to the wide path. Linked aliases never move, and two unequal names can never claim the same rank.
- **Timestamps (`ts.rs`).** The manifest supplies a bounded set of likely UTC dates. A fast F1 lookup checks all ten date bytes, every clock separator and digit, and the valid hour/minute/second ranges. Other dates use the full Gregorian calculation; offsets, raw epoch milliseconds and invalid values retain their regular parser.
- **Events (`events.rs`).** Even a well-formed but invalid event can suppress a later valid copy. It emits an ingest claim and no payload. Malformed lines and empty event IDs emit neither. A 16-byte claim stores a reversible event-ID token (top eight bits implicit in the shard), payload location, and exact ingest order. Four-digit F1 years fit as signed whole seconds; exceptional timestamps use full milliseconds in a per-chunk side table. A per-shard Bloom filter identifies IDs that *might* repeat. False positives send extra claims to the exact table; they never suppress a row. Actual duplicates mark their repeated bits on the second occurrence, so the exact pass includes the first occurrence too. A winning invalid copy has no payload; all losing valid copies are removed. Category lookup only runs for valid purchases.
- **Sessions (`stream.rs`, `ranges.rs`, `main.rs`).** If the manifest span fits `u32` milliseconds, a histogram of occupied local ranks gives each ranked person's timestamps, clicks and purchases contiguous ranges. A small person sorts only timestamp offsets. A busy person fills bins narrower than 30 minutes with exact minimum, maximum and count; only neighboring nonempty extrema can start a new session. Unranked identities and wider windows sort full identity/time keys. Both paths use the same session reducer, consuming all purchases through the inclusive end, crediting the latest click with byte-wise ID tie-break, and counting each active assisting campaign once. Identical click copies enter and expire together and do not change credit or assist membership.
- **Output (`output.rs`).** Each session worker appends finalized user-days to typed columns and records the end of each person's run. The writer encodes the name once per run and uses bounded row groups and dictionaries for categories and sparse cents. The Parquet crate frames the pages and owns the footer. `day` is DATE, counts are integers, only `top_category` is nullable. Both tables always have a readable file, including on empty input. Files are explicitly closed before exit.

The independent slow oracle is `reference/pipeline.py`. `make fixtures` compares both implementations to the official hand-written cases. `tools/bench_pair.py` alternates baseline/candidate order in five pairs and times whole processes with fresh output directories. See [measurements and rejected experiments](docs/NOTES.md).
