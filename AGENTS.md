# Instructions for coding agents

You are helping win the Databricks "Shopping Sessions" track of the Builders Buildathon: the fastest pipeline whose output is **exactly** right. Read this whole file before touching code.

## Read in this order
1. `TRACK_BRIEF.md`: scoring, limits, deadline.
2. `docs/rules.md` (official; if missing, `docs/rules-transcribed.md` and do TASKS.md T1 first). This is the only source of truth for behaviour.
3. `SPEC.md`: what done means and the priority list.
4. `ARCHITECTURE.md`: how `fast/src/main.rs` is laid out.
5. `TASKS.md`: pick the first unchecked task. Do not skip ahead to speed work while any Phase A or B task is open.
6. `docs/PERF_PLAYBOOK.md`: only when you reach Phase C.

## Golden rules
1. **Correct beats fast.** One differing row means no score. Never trade exactness for speed.
2. **rules.md wins** over the starters, over this kit, over your intuition. When you change behaviour, quote the rules.md sentence in the commit message.
3. **Two implementations must agree.** `reference/pipeline.py` is the literal, slow oracle; `fast/` is the entry. A behaviour change goes into both, with a test in `tests/test_reference.py`.
4. **Never hard-code** anything about a dataset (window, sizes, file counts, IDs, seeds, campaign counts). Data changes per seed and the final uses new data.
5. **No network at run time**, run as uid 1000, write only `/output` and `/scratch`, logs to stderr, stdout stays empty.
6. **Strings are bytes.** No trimming, case folding, Unicode normalisation or numeric coercion of IDs. Byte-wise comparison everywhere.
7. Don't commit data, generated datasets, `target/`, or anything over a few MB. Repo must stay < 50 MB and public. Never force-push over a submitted commit.
8. Don't submit, register, or change anything on joinbuilders.org. Leif does submissions himself.

## Commands
```bash
# kit root
make test                     # 15 reference edge cases
make build                    # cargo build --release in fast/
make test-bin                 # the same edge cases against the binary
make diff SEEDS=20 EVENTS=30000   # reference vs binary on kit-generated dirty data
python3 tools/compare_outputs.py EXPECTED_DIR ACTUAL_DIR   # dev-answer dir or output dir on each side
python3 tools/compare_outputs.py --check ACTUAL_DIR         # schema + rules.md self-check
DATA_DIR=/path/to/data OUTPUT_DIR=/tmp/out fast/target/release/shopping_sessions
```
`pip install pyarrow` is the only Python dependency.

## Workflow for every change
1. Write or update a failing test (edge case in `tests/`, or a seed that mismatches).
2. Change code.
3. `make test && make build && make test-bin && make diff SEEDS=10`.
4. Dev-set check (`make check-dev` in the starter harness, or `compare_outputs.py` against the unzipped dev answer).
5. For speed work: median of 5 runs on the 20M dataset, before and after. Log it in `docs/NOTES.md`.
6. Commit with a message that says what changed, the rules.md sentence if behaviour changed, and the timing delta if speed changed.

## Conventions
- Rust 2024 edition, `cargo fmt`, `cargo clippy` clean where reasonable.
- Keep `fast/src/main.rs` readable: one function per phase. Split into modules once it passes ~1,200 lines.
- Comments explain *why* (which rule) not *what*.
- Put findings and decisions in `docs/NOTES.md` (create it), newest first, dated.

## When unsure
If rules.md is ambiguous, write the question and your two candidate readings in `docs/NOTES.md`, implement the reading the official fixtures/dev answer support, and tell Leif so he can ask the organizers (hi@joinbuilders.org).
