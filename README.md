# Shopping Sessions agent kit

A ready-to-use folder for the **Databricks · Shopping Sessions** track of the Builders Buildathon (closes **Sun Nov 22, 2026, 16:00 UTC / 10:00 AM Houston**). Point Claude Code (or any coding agent) at this folder and it has the rules, a plan, a working fast pipeline, a slow oracle to check it against, and the tests.

## What's inside
| Path | What it is |
| --- | --- |
| `AGENTS.md`, `CLAUDE.md` | Instructions agents follow: read order, golden rules, commands, workflow |
| `TRACK_BRIEF.md` | Faithful summary of the track page: scoring, limits, deliverable, deadline |
| `docs/rules-transcribed.md` | Full rules.md as read on 2026-11-21 (replace with the official file as `docs/rules.md`) |
| `SPEC.md` | The entry, definition of done, must/should/could list |
| `ARCHITECTURE.md` | Data flow and design decisions of the fast pipeline |
| `TASKS.md` | Ordered, agent-sized tasks with acceptance checks |
| `DEMO_AND_SUBMISSION.md` | Submission checklist, how to pick the final, optional video script |
| `docs/PERF_PLAYBOOK.md` | Where the time goes and what to try next |
| `fast/` | **v1 Rust pipeline** + Dockerfile (this becomes your repo root) |
| `reference/pipeline.py` | Slow, literal Python implementation of rules.md (correctness oracle) |
| `tools/compare_outputs.py` | Scorer-style diff (missing / extra / differing rows) + schema and self-check |
| `tools/mini_gen.py`, `tools/diff_impls.sh` | Kit-made dirty data and a reference-vs-binary diff loop |
| `tests/test_reference.py` | 15 rule-by-rule edge cases (run against either implementation) |
| `Makefile` | `make test`, `make build`, `make test-bin`, `make diff` |

## Status of the kit (honest)
- Reference and fast pipeline agree on all 15 edge cases, on 17 kit-generated dirty datasets (20k to 80k events) and on a 2M-event kit dataset.
- Fast pipeline: 0.59 s for 2M events on a 4-core machine.
- **Not yet checked** against the organizers' 36 fixtures, the dev answer or the official generator: the kit's sandbox couldn't download the zips. That is TASKS.md T1 to T3, and it's the first thing to do.
- **Docker build not run** (no Docker daemon where the kit was built). TASKS.md T4.

## Quick start
```bash
# 1. requirements: Rust (rustup), Python 3.9+, pip install pyarrow, Docker
cd shopping-sessions-agent-kit
make test && make build && make test-bin && make diff SEEDS=5

# 2. download from the track page into vendor/: rules.md, a starter zip, dev-answer.zip, generator.zip
#    https://www.joinbuilders.org/buildathon/tracks/shopping-sessions  (section "Docs and starter files")

# 3. point your agent here
claude   # then: "Read AGENTS.md and do the first unchecked task in TASKS.md"
```

Suggested first prompt for an agent:
> Read AGENTS.md and follow its read order. Then do TASKS.md T0 to T4 in order: set up the repo with fast/ at the root, put the official files from vendor/ in place, run the 36 starter fixtures and the dev-set check against both implementations, and verify the Docker image the way the scorer runs it. Stop and report before anything that would submit.

For two agents in parallel (after T4): one on T6 and T7 (correctness at scale), one on T8 and T9 (profiling and parse speed) in a separate worktree. Merge only commits that pass `make test-bin`, `make diff` and the dev check.

## License and credits
Everything here is meant to be released under the MIT License with your entry (an event rule). Crates used: rayon, memchr, memmap2, ahash, serde_json, arrow-rs (arrow-array, arrow-schema, parquet). Python: pyarrow.
