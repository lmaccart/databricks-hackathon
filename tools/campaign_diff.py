#!/usr/bin/env python3
"""Check the dense campaign-grid budget boundary against the Python oracle.

    python3 tools/campaign_diff.py /path/to/shopping_sessions

Needs pyarrow for the reference pipeline and Parquet assertions.
"""

import argparse
from datetime import date, datetime, timezone
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "reference"))
import pipeline

DAY = 86_400_000
GAP = 1_800_000
DAYS = 14
WORKERS = 8
WS = 1_790_812_800_000  # 2026-10-01T00:00:00Z
WE = WS + DAYS * DAY
BUDGET = 32 << 20
# campaign.rs accounts each day/campaign cell for assists, a credit slot, and
# two CreditTriples across the workers and the merge target.
BYTES_PER_CAMPAIGN = DAYS * (WORKERS + 1) * (8 + 4 + 2 * 32)
# The organic column consumes the last slot of the dense control.
FIRST_SPARSE = BUDGET // BYTES_PER_CAMPAIGN


def f1(ts):
    return datetime.fromtimestamp(ts // 1000, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def write_csv(data, filename, header, rows):
    path = data / filename
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as out:
        out.write(header + "\n")
        for row in rows:
            out.write(",".join(map(str, row)) + "\n")


def generate(data):
    data.mkdir()
    (data / "manifest.json").write_text(
        json.dumps({"window_start": f1(WS), "window_end": f1(WE)}) + "\n",
        encoding="utf-8",
    )
    write_csv(data, "products.csv", "product_id,category", [("p1", "shoes"), ("p2", "bags")])

    links, events, purchases, clicks_a, clicks_b = [], [], [], [], []
    for i in range(2, FIRST_SPARSE):
        person, mobile = f"acct_{i}", f"mobile_{i}"
        links.append((person, mobile))
        start = WS + (0 if i % 2 == 0 else 4) * DAY + 12 * 60 * 60_000 + (i % 180) * 1000
        after_gap = start + GAP
        split = after_gap + GAP + 1
        last = WE - 1 - (i % 180) * 1000
        events.extend([
            (f"view_{i}", person, f1(start), f1(start), "view", "p1", ""),
            (f"split_{i}", mobile, split, f1(split), "cart", "p2", ""),
            (f"last_{i}", person, last, f1(last), "view", "p1", ""),
        ])
        purchases.append((f"buy_{i}", mobile, f1(after_gap), f1(after_gap),
                          "purchase", f"p{1 + i % 2}", 500 + i % 750))
        clicks_a.extend([
            (f"a_{i}", mobile, "c_1", f1(start - 7 * DAY)),
            (f"b_{i}", person, f"c_{i}", f1(start)),
        ])
        # One millisecond after the first start: too late to credit that session.
        clicks_b.append((f"c_{i}", mobile, "c_2", start + 1))

    write_csv(data, "links/part-0.csv", "user_id_a,user_id_b", links)
    event_header = "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents"
    write_csv(data, "events/part-0.csv", event_header, events)
    write_csv(data, "events/part-1.csv", event_header, purchases)
    click_header = "click_id,user_id,campaign_id,ts"
    write_csv(data, "clicks/part-0.csv", click_header, clicks_a)
    write_csv(data, "clicks/part-1.csv", click_header, clicks_b)
    return len(links)


def write_campaigns(data, count):
    channels = ("search", "social", "email", "display", "video")
    rows = ((f"c_{i}", channels[i % len(channels)]) for i in range(1, count + 1))
    write_csv(data, "campaigns.csv", "campaign_id,channel", rows)


def check_sentinels(output, people):
    import pyarrow.parquet as pq

    campaigns = pq.read_table(output / "campaign_daily").to_pylist()
    dates = {date(2026, 10, 1), date(2026, 10, 5), date(2026, 10, 14)}
    assert {row["day"] for row in campaigns} == dates
    assist_only = {(r["campaign_id"], r["day"]): r["assisted_sessions"] for r in campaigns if not r["sessions"]}
    assert assist_only == {
        ("c_1", date(2026, 10, 1)): people // 2,
        ("c_1", date(2026, 10, 5)): people // 2,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=lambda value: Path(value).resolve(strict=True))
    binary = parser.parse_args().binary

    scratch = ROOT / "scratch"
    scratch.mkdir(exist_ok=True)
    case = Path(tempfile.mkdtemp(prefix="campaign-diff-", dir=scratch))
    try:
        data = case / "data"
        people = generate(data)
        assert 0 <= WE - WS - 1 <= 2**32 - 1
        assert FIRST_SPARSE * BYTES_PER_CAMPAIGN <= BUDGET < (FIRST_SPARSE + 1) * BYTES_PER_CAMPAIGN
        expected = case / "oracle"
        for count, mode in ((FIRST_SPARSE - 1, "dense"), (FIRST_SPARSE, "sparse-fallback")):
            write_campaigns(data, count)
            # The extra dimension has no clicks: either layout must yield the same answer.
            if mode == "dense":
                pipeline.run(str(data), str(expected))
                check_sentinels(expected, people)
            actual = case / mode
            env = dict(os.environ, DATA_DIR=str(data), OUTPUT_DIR=str(actual), SCRATCH_DIR=str(case),
                       RAYON_NUM_THREADS=str(WORKERS), NCPU=str(WORKERS))
            run = subprocess.run([binary], env=env, stdout=subprocess.PIPE, check=True)
            if run.stdout:
                raise ValueError(f"unexpected stdout ({len(run.stdout)} bytes)")
            subprocess.run([sys.executable, ROOT / "tools/compare_outputs.py", expected, actual], check=True)
            print(f"Campaign grid: {mode}, {count} campaigns, {WORKERS} workers: MATCH", flush=True)
    except BaseException:
        print(f"Retained failing case: {case}", file=sys.stderr)
        raise
    shutil.rmtree(case)


if __name__ == "__main__":
    main()
