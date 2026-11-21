#!/usr/bin/env python3
"""Shopping Sessions reference pipeline (correctness oracle, NOT the fast path).

Written line by line from rules.md so an agent can diff a fast implementation
against it on any generated dataset. It favours clarity over speed: expect
roughly 10-40 s per million events in CPython.

Usage:
    python3 reference/pipeline.py [DATA_DIR] [OUTPUT_DIR]
Defaults come from $DATA_DIR / $OUTPUT_DIR, then /data and /output.

Needs: Python 3.9+ and pyarrow (only for writing Parquet).

Every string is kept as raw bytes, so all comparisons are byte-wise exactly as
rules.md requires (no decoding, no trimming, no case folding).
"""
from __future__ import annotations

import datetime as _dt
import glob
import json
import os
import sys
from bisect import bisect_left, bisect_right
from collections import defaultdict

DAY_MS = 86_400_000
SESSION_GAP_MS = 1_800_000
WEEK_MS = 7 * DAY_MS
LATE_MS = DAY_MS
MAX_PRICE = 10_000_000
EVENT_TYPES = (b"view", b"cart", b"purchase")
BAD = None  # marker for a bad timestamp


# --------------------------------------------------------------------------
# CSV dialect
# --------------------------------------------------------------------------
def _unquote(field: bytes):
    """Return the field value, or None if its quoting is malformed."""
    if b'"' not in field:
        return field
    if len(field) >= 2 and field[0] == 0x22 and field[-1] == 0x22 and b'"' not in field[1:-1]:
        return field[1:-1]
    return None


def parse_line(line: bytes, nfields: int):
    """Split one line into values, or return None if the line is malformed."""
    if not line:
        return None  # empty line is malformed
    parts = line.split(b",")
    if len(parts) != nfields:
        return None
    out = []
    for p in parts:
        v = _unquote(p)
        if v is None:
            return None
        out.append(v)
    return out


def read_csv_rows(path: str, columns: list):
    """Yield dicts {column: bytes} for each well-formed data line of one file.

    Lines end only at b"\\n"; a trailing b"\\r" stays in the last field.
    Columns are matched by header name so column order never matters.
    """
    with open(path, "rb") as fh:
        data = fh.read()
    if not data:
        return
    lines = data.split(b"\n")
    if data.endswith(b"\n"):
        lines.pop()  # the piece after the final newline is not a line
    if not lines:
        return
    header = [(_unquote(h) or h) for h in lines[0].split(b",")]
    try:
        idx = [header.index(c.encode()) for c in columns]
    except ValueError as exc:
        raise SystemExit(f"{path}: header {header!r} lacks a column: {exc}")
    n = len(header)
    for line in lines[1:]:
        vals = parse_line(line, n)
        if vals is None:
            continue
        yield [vals[i] for i in idx]


def read_folder(data_dir: str, sub: str, columns: list):
    for path in sorted(glob.glob(os.path.join(data_dir, sub, "*.csv"))):
        yield from read_csv_rows(path, columns)


# --------------------------------------------------------------------------
# Timestamps
# --------------------------------------------------------------------------
def _days_from_civil(y: int, m: int, d: int) -> int:
    """Days since 1970-01-01 for a proleptic Gregorian date (Hinnant)."""
    y -= m <= 2
    era = (y if y >= 0 else y - 399) // 400
    yoe = y - era * 400
    doy = (153 * (m + (-3 if m > 2 else 9)) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def _is_leap(y: int) -> bool:
    return y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)


_MDAYS = (0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31)


def _digits(b: bytes) -> bool:
    return len(b) > 0 and all(0x30 <= c <= 0x39 for c in b)


def parse_ts(s: bytes):
    """Return epoch milliseconds (int) or BAD for any form rules.md rejects."""
    if not s:
        return BAD
    if _digits(s):  # F3: digits only, leading zeros fine, any length
        return int(s)
    # F1 "YYYY-MM-DDTHH:MM:SSZ" (20) or F2 "YYYY-MM-DDTHH:MM:SS+HH:MM" (25)
    if len(s) not in (20, 25):
        return BAD
    if not (s[4:5] == b"-" and s[7:8] == b"-" and s[10:11] == b"T"
            and s[13:14] == b":" and s[16:17] == b":"):
        return BAD
    ys, mos, ds, hs, mis, ss = s[0:4], s[5:7], s[8:10], s[11:13], s[14:16], s[17:19]
    if not all(_digits(x) for x in (ys, mos, ds, hs, mis, ss)):
        return BAD
    y, mo, d, h, mi, se = int(ys), int(mos), int(ds), int(hs), int(mis), int(ss)
    if not (1 <= mo <= 12) or h > 23 or mi > 59 or se > 59:
        return BAD
    mdays = 29 if (mo == 2 and _is_leap(y)) else _MDAYS[mo]
    if not (1 <= d <= mdays):
        return BAD
    tail = s[19:]
    if tail == b"Z":
        off_min = 0
    elif len(tail) == 6 and tail[0:1] in (b"+", b"-") and tail[3:4] == b":":
        oh, om = tail[1:3], tail[4:6]
        if not (_digits(oh) and _digits(om)):
            return BAD
        ohi, omi = int(oh), int(om)
        if ohi > 23 or omi > 59:
            return BAD
        off_min = ohi * 60 + omi
        if tail[0:1] == b"-":
            off_min = -off_min
    else:
        return BAD
    secs = _days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se
    return (secs - off_min * 60) * 1000  # subtract the offset to get UTC


def ms_to_date(ms: int) -> _dt.date:
    return _dt.date(1970, 1, 1) + _dt.timedelta(days=ms // DAY_MS)


# --------------------------------------------------------------------------
# Linked IDs
# --------------------------------------------------------------------------
class UnionFind:
    def __init__(self):
        self.parent = {}

    def find(self, x):
        parent = self.parent
        if x not in parent:
            return x
        root = x
        while parent.get(root, root) != root:
            root = parent[root]
        while parent.get(x, x) != root:  # path compression
            nxt = parent[x]
            parent[x] = root
            x = nxt
        return root

    def union(self, a, b):
        ra, rb = self.find(a), self.find(b)
        if ra == rb:
            self.parent.setdefault(ra, ra)
            return
        # keep the byte-wise smallest ID as the root, so root == person name
        if rb < ra:
            ra, rb = rb, ra
        self.parent[rb] = ra
        self.parent.setdefault(ra, ra)


# --------------------------------------------------------------------------
# Pipeline
# --------------------------------------------------------------------------
def run(data_dir: str, out_dir: str) -> dict:
    with open(os.path.join(data_dir, "manifest.json"), "rb") as fh:
        manifest = json.load(fh)
    ws = parse_ts(manifest["window_start"].encode())
    we = parse_ts(manifest["window_end"].encode())
    assert ws is not BAD and we is not BAD, manifest

    products = {}
    for pid, cat in read_csv_rows(os.path.join(data_dir, "products.csv"), ["product_id", "category"]):
        products[pid] = cat
    campaigns = {}
    for cid, ch in read_csv_rows(os.path.join(data_dir, "campaigns.csv"), ["campaign_id", "channel"]):
        campaigns[cid] = ch

    # Linked IDs
    uf = UnionFind()
    for a, b in read_folder(data_dir, "links", ["user_id_a", "user_id_b"]):
        if not a or not b:
            continue
        uf.union(a, b)

    # Events: parse -> dedup (smallest ingest_ts) -> validate -> late
    kept = {}  # event_id -> (ingest_ms, row)
    for row in read_folder(data_dir, "events",
                           ["event_id", "user_id", "ts", "ingest_ts", "event_type", "product_id", "price_cents"]):
        eid = row[0]
        if not eid:
            continue
        ing = parse_ts(row[3])
        ing_key = ing if ing is not BAD else float("inf")
        cur = kept.get(eid)
        if cur is None or ing_key < cur[0]:
            kept[eid] = (ing_key, row)

    per_person = defaultdict(list)  # person -> [(ts, type, product, price)]
    for ing, row in kept.values():
        _eid, uid, ts_raw, _ing_raw, etype, pid, price_raw = row
        if not uid or not pid or etype not in EVENT_TYPES:
            continue
        ts = parse_ts(ts_raw)
        if ts is BAD or not (ws <= ts < we):
            continue
        price = 0
        if etype == b"purchase":
            if not _digits(price_raw):
                continue
            price = int(price_raw)
            if not (1 <= price <= MAX_PRICE):
                continue
        if ing - ts > LATE_MS:
            continue
        per_person[uf.find(uid)].append((ts, etype, pid, price))

    # Clicks: parse -> dedup -> validate
    seen_clicks = set()
    clicks_pp = defaultdict(list)  # person -> [(ts, click_id, campaign)]
    cws = ws - WEEK_MS
    for cid, uid, camp, ts_raw in read_folder(data_dir, "clicks", ["click_id", "user_id", "campaign_id", "ts"]):
        if not cid or cid in seen_clicks:
            continue
        seen_clicks.add(cid)
        if not uid or camp not in campaigns:
            continue
        ts = parse_ts(ts_raw)
        if ts is BAD or not (cws <= ts < we):
            continue
        clicks_pp[uf.find(uid)].append((ts, cid, camp))

    # Sessions, credit, assists, aggregation
    user_daily = {}      # (person, date) -> [sessions, events, revenue, {cat: cents}]
    camp_daily = {}      # (campaign, date) -> [sessions, purchasing, revenue, assisted]

    def camp_row(key):
        r = camp_daily.get(key)
        if r is None:
            r = camp_daily[key] = [0, 0, 0, 0]
        return r

    for person, evs in per_person.items():
        evs.sort(key=lambda e: e[0])
        sessions = []
        cur = None
        prev_ts = None
        for e in evs:
            if cur is None or e[0] - prev_ts > SESSION_GAP_MS:
                cur = [e[0], []]
                sessions.append(cur)
            cur[1].append(e)
            prev_ts = e[0]

        clicks = clicks_pp.get(person, [])
        clicks.sort(key=lambda c: (c[0], c[1]))  # by ts, then click_id bytes
        cts = [c[0] for c in clicks]
        for start, sevs in sessions:
            day = ms_to_date(start)
            i = bisect_left(cts, start - WEEK_MS)   # first click with ts >= start - 7d
            j = bisect_right(cts, start)            # one past last click with ts <= start
            if i < j:
                best_ts = cts[j - 1]
                k = bisect_left(cts, best_ts, i, j)  # smallest click_id at the latest ts
                credit_camp = clicks[k][2]
                assists = {clicks[x][2] for x in range(i, j)}
            else:
                credit_camp = b"organic"
                assists = set()

            revenue = 0
            cats = defaultdict(int)
            purchased = False
            for ts, etype, pid, price in sevs:
                if etype == b"purchase":
                    purchased = True
                    revenue += price
                    cats[products.get(pid, b"unknown")] += price

            u = user_daily.get((person, day))
            if u is None:
                u = user_daily[(person, day)] = [0, 0, 0, defaultdict(int), False]
            u[0] += 1
            u[1] += len(sevs)
            u[2] += revenue
            for k, v in cats.items():
                u[3][k] += v
            u[4] = u[4] or purchased

            r = camp_row((credit_camp, day))
            r[0] += 1
            r[1] += 1 if purchased else 0
            r[2] += revenue
            for a in assists:
                camp_row((a, day))[3] += 1

    write_outputs(out_dir, user_daily, camp_daily, campaigns)
    return {
        "user_rows": len(user_daily),
        "campaign_rows": len(camp_daily),
        "sessions": sum(v[0] for v in user_daily.values()),
        "revenue_cents": sum(v[2] for v in user_daily.values()),
    }


def _top_category(cats: dict, purchased: bool):
    if not purchased:
        return None
    best = max(cats.values())
    return min(k for k, v in cats.items() if v == best)


def write_outputs(out_dir, user_daily, camp_daily, campaigns):
    import pyarrow as pa
    import pyarrow.parquet as pq

    def dec(b):
        return None if b is None else b.decode("utf-8", errors="surrogateescape")

    ud = os.path.join(out_dir, "user_daily")
    cd = os.path.join(out_dir, "campaign_daily")
    os.makedirs(ud, exist_ok=True)
    os.makedirs(cd, exist_ok=True)

    keys = list(user_daily.keys())
    t1 = pa.table({
        "user_id": pa.array([dec(k[0]) for k in keys], pa.string()),
        "day": pa.array([k[1] for k in keys], pa.date32()),
        "sessions": pa.array([user_daily[k][0] for k in keys], pa.int64()),
        "events": pa.array([user_daily[k][1] for k in keys], pa.int64()),
        "revenue_cents": pa.array([user_daily[k][2] for k in keys], pa.int64()),
        "top_category": pa.array([dec(_top_category(user_daily[k][3], user_daily[k][4])) for k in keys], pa.string()),
    })
    pq.write_table(t1, os.path.join(ud, "part-0.parquet"))

    keys = list(camp_daily.keys())
    t2 = pa.table({
        "campaign_id": pa.array([dec(k[0]) for k in keys], pa.string()),
        "channel": pa.array([dec(b"organic" if k[0] == b"organic" else campaigns[k[0]]) for k in keys], pa.string()),
        "day": pa.array([k[1] for k in keys], pa.date32()),
        "sessions": pa.array([camp_daily[k][0] for k in keys], pa.int64()),
        "purchasing_sessions": pa.array([camp_daily[k][1] for k in keys], pa.int64()),
        "revenue_cents": pa.array([camp_daily[k][2] for k in keys], pa.int64()),
        "assisted_sessions": pa.array([camp_daily[k][3] for k in keys], pa.int64()),
    })
    pq.write_table(t2, os.path.join(cd, "part-0.parquet"))


def main(argv):
    data_dir = argv[1] if len(argv) > 1 else os.environ.get("DATA_DIR", "/data")
    out_dir = argv[2] if len(argv) > 2 else os.environ.get("OUTPUT_DIR", "/output")
    stats = run(data_dir, out_dir)
    print(json.dumps(stats), file=sys.stderr)


if __name__ == "__main__":
    main(sys.argv)
