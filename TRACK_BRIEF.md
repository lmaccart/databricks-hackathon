# Track brief: Databricks · Shopping Sessions

Source (read 2026-11-21): https://www.joinbuilders.org/buildathon/tracks/shopping-sessions
Event page: https://www.joinbuilders.org/buildathon
Full spec: `rules.md` from the track page (kit copy: `docs/rules-transcribed.md`; the official file wins).

## The task in one line
Turn ~60 million messy click-stream CSV events into two exact daily Parquet tables. **Fastest correct pipeline wins.**

## What the pipeline must do
Read `/data` (read-only): `events/*.csv`, `clicks/*.csv`, `links/*.csv`, `products.csv`, `campaigns.csv`, `manifest.json`.
1. Clean the logs (strict CSV dialect, duplicates, malformed lines, bad timestamps, late events).
2. Join each person's user IDs through `links/` (union-find; long chains, shared devices).
3. Sessionize each person's events (30-minute gap).
4. Credit each session to the person's latest ad click in the 7 days before it (else `organic`).
5. Count assisting campaigns (every campaign with a qualifying click in that 7-day window).
6. Write `/output/user_daily/` and `/output/campaign_daily/` as Parquet.

"The rules are exact so there is one right answer, and a fast answer that is almost right scores nothing."

## Scoring
| Item | Value |
| --- | --- |
| Metric | Container run time in seconds, start to exit (includes engine start-up and reading CSVs). Lower is better. |
| Baseline | 172.00 s (PySpark starter, local mode) |
| Formula | One run on hidden data; runs under 20 s are timed 3 times and scored on the median |
| Ties | Earlier submission ranks first |
| Scorer version | shopping-sessions-4 |
| Judging panel | None. Only the scoreboard. |

Each submission first runs on the public 1M-event dev set (max 3 min). If it fails there you get the differing rows and your logs, and it stops. Hidden runs show only exit code, whether memory ran out, and counts of missing / extra / differing rows.

Practice: about 20M events, 2 submissions per team per hour (rejected ones count toward the limit but never lower your best). A practice run may take at most 2× baseline and never more than 5 min.

**Final:** at the cutoff the chosen commit (your best unless you pick another) is rebuilt and timed 3 times on ~60M new events, each run capped at 3 minutes. Any run over 3 minutes = no final score. The 10+ fastest are re-timed 7 times alone on an idle machine and that median replaces their score. Practice times vary up to ~20% across machines.

Practice leaderboard when read (2026-11-21 ~02:45 UTC, 98 teams): best 0.75 s, top 10 under ~1.05 s, median around 2 s.

## What to hand in
A **public GitHub repository** with a `Dockerfile` at its root, plus the commit to score. Repo ≤ 50 MB, no data checked in (data is mounted at run time). Keep the repo public with every submitted commit until results are published.

## Container limits
- Image linux/amd64, ≤ 8 GB. Build has network for up to 10 minutes; runs have **no network**.
- 8 pinned vCPUs (hyperthreads), 24 GB RAM, no swap.
- `/data` read-only; `/scratch` writable for spills; `/output` + `/scratch` ≤ 50 GB total.
- Root filesystem read-only except `/tmp` (64 MB, in memory). Process runs as **uid 1000**.
- Env: `DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch HOME=/scratch NCPU=8 MEM_GB=24`.
- ≤ 4,096 processes+threads; ≤ 50 MB on stdout (log to stderr).

## Starter files on the track page (download them yourself; the kit could not)
- `rules.md`: full spec.
- `starter-pyspark.zip`, `starter-duckdb.zip`, `starter-polars.zip`: skeletons that write empty tables with the right columns, plus test scripts and **36 edge-case fixtures**.
- `dev-answer.zip`: expected `user_daily.csv` and `campaign_daily.csv` for the 1M-event dev set (seed 1), used by `make check-dev`.
- `generator.zip`: Rust source for `generate --seed N --events E --clicks C --output DIR`.

Track rules: where a starter and rules.md disagree, rules.md wins; ask the organizers (hi@joinbuilders.org) if rules.md is unclear.

## Event rules that apply (from the event page)
- Any current student, any school, any country; register with a school email.
- Teams of up to 2, or solo; one team per person.
- Any tools, AI included. Open-source libraries and pre-existing code allowed (name them in the README).
- Everything you build is released under the **MIT License**.
- One entry per team in the queue at a time. Pick a final before the cutoff, or your best-scoring entry is used.
- Prize: $5,000, winner takes all. Separate $100 prize for best video (YouTube, Unlisted, entered from the team page by Nov 22, 11 AM ET).

## Deadline
Submissions close **Sunday Nov 22, 2026, 16:00 UTC** = 11:00 AM ET = **10:00 AM Houston time**. The final entry locks then.
