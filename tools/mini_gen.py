#!/usr/bin/env python3
"""Small dirty-data generator for differential testing (NOT the official generator).

It imitates the dirt listed in rules.md (duplicates, bad timestamps, late events,
quoting, malformed lines, link chains, shared devices, boundary clicks, \\r, case
variants) so two implementations can be diffed on many seeds without the
official Rust generator. Use the official generator (generator.zip) for anything
you report as "correct".

    python3 tools/mini_gen.py --seed 7 --events 50000 --out /tmp/ss-data
"""
from __future__ import annotations

import argparse
import datetime as dt
import itertools
import json
import os
import random

DAY = 86_400_000
MIN = 60_000


def f1(ms):
    return dt.datetime.fromtimestamp(ms // 1000, dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def f2(ms, rnd):
    off = rnd.randint(-10 * 60, 12 * 60)
    t = dt.datetime.fromtimestamp(ms // 1000, dt.timezone.utc) + dt.timedelta(minutes=off)
    sign = "+" if off >= 0 else "-"
    return t.strftime("%Y-%m-%dT%H:%M:%S") + f"{sign}{abs(off) // 60:02d}:{abs(off) % 60:02d}"


def ts_text(ms, rnd):
    if ms % 1000:
        return str(ms) if rnd.random() < 0.9 else "000" + str(ms)
    r = rnd.random()
    return f1(ms) if r < 0.58 else f2(ms, rnd) if r < 0.68 else str(ms)


BAD_TS = ["", "2026-10-03T23:58:10.000Z", "2026-10-03T23:58:10z", "2026-10-03 23:58:10Z",
          "2026-10-03T23:58:10", "1.79e12", "-5", "2026-13-01T00:00:00Z", "2026-02-30T00:00:00Z",
          "2027-02-29T00:00:00Z", "2026-10-03T24:00:00Z", "2026-10-03T23:59:60Z",
          "2026-10-03T23:58:10+24:00", "2026-10-03T23:58:10+05:60", "2026-10-03T23:58:10+0200",
          "86400000", "99999999999999999999999", "１７９１０７１８９００００"]


def quote(fields, rnd):
    return ",".join(f'"{f}"' if rnd.random() < 0.2 else f for f in fields)


def malform(line, rnd):
    r = rnd.random()
    if r < 0.3:
        return line + ",x"
    if r < 0.6:
        return '"' + line
    if r < 0.8:
        return ""
    return line.rsplit(",", 1)[0]


def write_parts(path, header, lines, nparts, rnd):
    os.makedirs(path, exist_ok=True)
    rnd.shuffle(lines)
    buckets = [[] for _ in range(nparts)]
    cum = list(itertools.accumulate(1 / (i + 1) for i in range(nparts)))
    for ln in lines:
        buckets[rnd.choices(range(nparts), cum_weights=cum)[0]].append(ln)
    buckets.append([])  # a header-only file
    for i, b in enumerate(buckets):
        with open(os.path.join(path, f"part-{i:03d}.csv"), "w", newline="") as fh:
            fh.write(header + "\n")
            for ln in b:
                fh.write(ln + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--events", type=int, default=20000)
    ap.add_argument("--clicks", type=int, default=0)
    ap.add_argument("--out", required=True)
    # Wider windows and more categories exercise the fast path's wide layouts.
    ap.add_argument("--window-days", type=int, default=14)
    ap.add_argument("--categories", type=int, default=0,
                    help="use exactly this many categories (default: 20, drawn at random per product)")
    a = ap.parse_args()
    rnd = random.Random(a.seed)
    E = a.events
    C = a.clicks or E // 4
    year = rnd.choice([2025, 2026, 2027, 2028])
    start = dt.datetime(year, rnd.randint(1, 12), rnd.randint(1, 28), tzinfo=dt.timezone.utc)
    if year == 2028 and rnd.random() < 0.5:
        start = dt.datetime(2028, 2, 20, tzinfo=dt.timezone.utc)
    ws = int(start.timestamp() * 1000)
    we = ws + a.window_days * DAY
    os.makedirs(a.out, exist_ok=True)
    with open(os.path.join(a.out, "manifest.json"), "w") as fh:
        json.dump({"window_start": f1(ws), "window_end": f1(we)}, fh)

    cats = [f"cat_{i:02d}" for i in range(a.categories or 20)]
    products = [f"p_{i}" for i in range(max(50, E // 50, a.categories))]
    with open(os.path.join(a.out, "products.csv"), "w") as fh:
        fh.write("product_id,category\n")
        for i, p in enumerate(products):
            fh.write(f"{p},{cats[i] if i < a.categories else rnd.choice(cats)}\n")
    camps = [f"c_{i}" for i in range(max(20, E // 500))]
    with open(os.path.join(a.out, "campaigns.csv"), "w") as fh:
        fh.write("campaign_id,channel\n")
        for c in camps:
            fh.write(f"{c},{rnd.choice(['search', 'social', 'email', 'display', 'affiliate'])}\n")

    # people and IDs
    npeople = max(10, E // 30)
    people, links = [], []
    nid = 0
    for p in range(npeople):
        k = rnd.choices([1, 2, 3, 4, 6], [45, 30, 13, 7, 5])[0]
        if p < 2:
            k = rnd.randint(50, 120)  # long chains
        ids = []
        for _ in range(k):
            nid += 1
            ids.append(f"u{nid:07d}x{rnd.randint(0, 9)}")
        order = ids[:]
        if p < 2:
            rnd.shuffle(order)
            for x, y in zip(order, order[1:]):
                links.append((x, y))
        else:
            for i in range(1, len(ids)):
                links.append((ids[rnd.randrange(i)], ids[i]))
        if rnd.random() < 0.05:
            nid += 1
            extra = f"a{nid:07d}"  # linked, never used, may become the name
            links.append((rnd.choice(ids), extra))
        people.append(ids)
    shared = [f"dev{i}" for i in range(3)]
    for ids in rnd.sample(people, max(1, npeople // 250)):
        links.append((rnd.choice(shared), rnd.choice(ids)))
        ids.append(rnd.choice(shared))

    link_lines = []
    for x, y in links:
        if rnd.random() < 0.5:
            x, y = y, x
        if rnd.random() < 1 / 500:
            y = y.upper()
        if rnd.random() < 0.002:
            y = ""
        link_lines.append(quote([x, y], rnd))
        if rnd.random() < 0.08:
            link_lines.append(quote([y, x], rnd))
    for ids in rnd.sample(people, max(1, npeople // 100)):
        link_lines.append(f"{ids[0]},{ids[0]}")
    for _ in range(max(1, len(link_lines) // 2000)):
        link_lines.append(malform(link_lines[rnd.randrange(len(link_lines))], rnd))
    write_parts(os.path.join(a.out, "links"), "user_id_a,user_id_b", link_lines, rnd.randint(2, 6), rnd)

    weights = [1 / (i + 1) ** 0.8 for i in range(npeople)]
    for w in range(min(3, npeople)):
        weights[w] *= 20  # whales
    cum_people = list(itertools.accumulate(weights))

    def pick_uid(ids):
        return ids[0] if rnd.random() < 0.5 else rnd.choice(ids)

    # events grouped into sessions
    ev_lines, eid = [], 0
    session_starts = []  # (person index, start ms)
    while eid < E:
        pi = rnd.choices(range(npeople), cum_weights=cum_people)[0]
        ids = people[pi]
        t = rnd.randint(ws - 2 * 3600_000, we + 3600_000)
        if rnd.random() < 0.01:
            t = ws + rnd.randint(0, a.window_days - 1) * DAY + DAY - rnd.randint(1, 10) * MIN  # near midnight
        session_starts.append((pi, t))
        for _ in range(rnd.randint(1, 12)):
            eid += 1
            uid = pick_uid(ids)
            r = rnd.random()
            etype = "purchase" if r < 0.05 else "cart" if r < 0.2 else "view"
            pid = rnd.choice(products)
            price = str(rnd.randint(100, 50000)) if etype == "purchase" else rnd.choice(["", "", "12", "x"])
            if rnd.random() < 0.002:
                uid = "" if rnd.random() < 0.8 else uid.upper()
            if rnd.random() < 0.001:
                etype = rnd.choice(["Purchase", "cart ", "click", "VIEW"])
            if etype == "purchase" and rnd.random() < 0.05:
                price = rnd.choice(["12.5", "-3", "0", "", "10000001", "0250", "+5", "10000000", "000000000001"])
            if rnd.random() < 0.005:
                pid = rnd.choice(["", "zzz", pid.upper()])
            ts_ms = t + (rnd.randint(0, 999) if rnd.random() < 0.3 else 0)
            ing = (ts_ms // 1000) * 1000 + rnd.randint(0, 120) * 1000
            r = rnd.random()
            if r < 0.01:
                ing = ts_ms + rnd.choice([DAY, DAY + 1000, DAY - 1000, 3 * DAY])
            ts_txt = ts_text(ts_ms, rnd)
            if rnd.random() < 0.005:
                ts_txt = rnd.choice(BAD_TS + [f1(ws - 1000), f1(we)])
            row = [f"e{eid}", uid, ts_txt, f1(ing), etype, pid, price]
            if rnd.random() < 0.001:
                row[-1] = row[-1] + "\r"
            ev_lines.append(quote(row, rnd))
            if rnd.random() < 0.03:  # duplicates, ingested later
                for _ in range(rnd.randint(1, 3)):
                    dup = row[:]
                    dup[3] = f1(ing + rnd.randint(1, 600) * 1000)
                    if rnd.random() < 1 / 64:
                        dup[1] = pick_uid(rnd.choice(people))
                    if rnd.random() < 0.3:
                        dup[2] = ts_text(ts_ms, rnd)
                    ev_lines.append(quote(dup, rnd))
                if rnd.random() < 0.02:  # earliest copy bad, later valid copy must not rescue it
                    bad = row[:]
                    bad[2] = rnd.choice(BAD_TS)
                    bad[3] = f1(ing - 1000)
                    ev_lines.append(quote(bad, rnd))
            gap = rnd.choice([rnd.randint(1, 300) * 1000] * 20 + [30 * MIN, 30 * MIN + 1, 0])
            t += gap
    for _ in range(max(1, len(ev_lines) // 2000)):
        ev_lines.append(malform(ev_lines[rnd.randrange(len(ev_lines))], rnd))
    write_parts(os.path.join(a.out, "events"), "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents",
                ev_lines, rnd.randint(3, 10), rnd)

    # clicks, many at boundaries of session starts
    cl_lines, cid = [], 0
    whale_camps = camps[:3]
    while cid < C:
        cid += 1
        pi, s = rnd.choice(session_starts)
        off = rnd.choice([7 * DAY, 7 * DAY + 1, 0, -1, 1, rnd.randint(0, 7 * DAY), rnd.randint(0, 9 * DAY)])
        t = s - off
        camp = rnd.choice(whale_camps) if rnd.random() < 0.3 else rnd.choice(camps)
        if rnd.random() < 0.005:
            camp = rnd.choice(["c_99999", camp.upper(), ""])
        uid = pick_uid(people[pi])
        if rnd.random() < 0.002:
            uid = ""
        ts_txt = ts_text(t, rnd)
        if rnd.random() < 0.005:
            ts_txt = rnd.choice(BAD_TS + [f1(ws - 7 * DAY), str(ws - 7 * DAY - 1)])
        row = [f"k{cid:06d}", uid, camp, ts_txt]
        cl_lines.append(quote(row, rnd))
        if rnd.random() < 0.05:  # a second click at the same millisecond
            cid += 1
            cl_lines.append(quote([f"k{cid:06d}", uid, rnd.choice(camps), ts_txt], rnd))
        if rnd.random() < 0.02:
            cl_lines.append(quote(row, rnd))  # duplicate, quoted differently
    for _ in range(max(1, len(cl_lines) // 2000)):
        cl_lines.append(malform(cl_lines[rnd.randrange(len(cl_lines))], rnd))
    write_parts(os.path.join(a.out, "clicks"), "click_id,user_id,campaign_id,ts", cl_lines, rnd.randint(2, 6), rnd)


if __name__ == "__main__":
    main()
