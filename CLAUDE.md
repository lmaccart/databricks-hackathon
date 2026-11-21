# CLAUDE.md

Follow `AGENTS.md` exactly; it is the single source of agent instructions for this repo.
Start by reading, in order: `TRACK_BRIEF.md`, `docs/rules.md` (or `docs/rules-transcribed.md`),
`SPEC.md`, `ARCHITECTURE.md`, then take the first unchecked task in `TASKS.md`.

Hard rules (repeated from AGENTS.md):
- Exact output first, speed second. rules.md wins every disagreement.
- Any behaviour change lands in both `reference/pipeline.py` (the independent oracle) and the shipped C++ pipeline in `cpp/pipeline`, with a test.
- Before every commit: `make test && make build && make test-bin && make diff SEEDS=10`.
- Never hard-code dataset facts; no network at run time; stdout stays empty.
- Never submit or register anything on joinbuilders.org; Leif does that.
