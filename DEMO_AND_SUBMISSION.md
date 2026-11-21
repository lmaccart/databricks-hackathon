# Submission checklist and (optional) video

There is no judging panel and no demo for the track prize: the scoreboard decides. The only "demo" is the optional $100 best-video prize.

## Before the first practice submission
- [ ] Repo is **public** on GitHub, MIT `LICENSE` file at the root.
- [ ] `Dockerfile` at the repo root; `docker build --platform linux/amd64 .` works from a fresh clone in < 10 min.
- [ ] Image ≤ 8 GB (ours should be < 150 MB); runs as uid 1000; works with `--read-only --network none --tmpfs /tmp:size=64m`.
- [ ] Reads `DATA_DIR`, writes `OUTPUT_DIR/user_daily/*.parquet` and `OUTPUT_DIR/campaign_daily/*.parquet`; creates the folders if missing.
- [ ] Exactly the required columns; `day` is Parquet DATE.
- [ ] Nothing on stdout (stderr logging only, well under 50 MB).
- [ ] Repo ≤ 50 MB, no data, no `target/`.
- [ ] README names the open-source crates used (rayon, memchr, memmap2, ahash, serde_json, arrow-rs/parquet) and says pre-existing code came from this kit.
- [ ] 36 starter fixtures pass; `make check-dev` passes; `tools/diff_impls.sh` matches.

## Submitting (Leif, on the site)
1. Sign in at the track page, open your team.
2. Hand in the public repo URL and the commit SHA to score.
3. Wait for the dev-set check, then the practice timing. If the dev check fails, read the differing rows and logs it returns, add the case to `tests/`, fix, resubmit.
4. Limit: 2 submissions per hour per team, one in the queue at a time. Rejected ones still count toward the hour.

## Choosing the final (before Sun Nov 22, 16:00 UTC = 10:00 AM Houston)
- Pick a commit that has a clean practice score **and** passed everything locally at 60M with plenty of headroom (the final runs 3× the data with a 3-minute cap per run; any run over = no score).
- Prefer a commit that scored twice with similar times over a one-off record. Practice varies up to ~20% by machine.
- If nothing is picked, the best practice entry goes through automatically.

## Optional video ($100, by Nov 22, 11 AM ET)
Upload to YouTube as **Unlisted** and enter the link from the team page. 60 to 90 seconds:
1. (10 s) The problem: 60M dirty events, two exact tables, wall-clock scoring.
2. (20 s) The trap: dedup before validation, distinct-timestamp gaps, byte-wise IDs. Show one nasty row and what the rules say.
3. (25 s) The design: mmap → parallel parse → hash shards → sliding 7-day window. Show the ARCHITECTURE diagram.
4. (15 s) Proof: `compare_outputs.py` printing MATCH, then a timed run (`time docker run ...`) and the leaderboard row.
5. (10 s) What you would do next.
