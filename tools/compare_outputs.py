#!/usr/bin/env python3
"""Compare two Shopping Sessions outputs the way the scorer reports them.

Each side may be:
  * an output folder holding user_daily/ and campaign_daily/ Parquet folders, or
  * a folder holding user_daily.csv and campaign_daily.csv (e.g. unzipped dev-answer.zip).

    python3 tools/compare_outputs.py EXPECTED ACTUAL      # missing / extra / differing rows
    python3 tools/compare_outputs.py --check ACTUAL       # schema + rules.md self-check only

Exit code 0 only when everything matches (or, with --check, the self-check passes).
Needs pyarrow for Parquet sides.
"""
from __future__ import annotations

import csv
import os
import sys

TABLES = {
    "user_daily": {
        "key": ("user_id", "day"),
        "cols": ("user_id", "day", "sessions", "events", "revenue_cents", "top_category"),
        "ints": ("sessions", "events", "revenue_cents"),
    },
    "campaign_daily": {
        "key": ("campaign_id", "day"),
        "cols": ("campaign_id", "channel", "day", "sessions", "purchasing_sessions",
                 "revenue_cents", "assisted_sessions"),
        "ints": ("sessions", "purchasing_sessions", "revenue_cents", "assisted_sessions"),
    },
}


def _norm_csv(table, col, v):
    if v is None:
        return None
    if col in TABLES[table]["ints"]:
        return int(v)
    s = str(v)
    if col == "top_category" and s == "":
        return None  # CSV answers may write null as an empty field
    return s


def load_parquet_dir(path, table, problems):
    import pyarrow as pa
    import pyarrow.parquet as pq

    files = sorted(
        f for f in os.listdir(path)
        if f.endswith(".parquet") and not f.startswith((".", "_"))
        and os.path.isfile(os.path.join(path, f)) and not os.path.islink(os.path.join(path, f))
    )
    if not files:
        problems.append(f"{table}: no *.parquet files in {path}")
        return []
    want = set(TABLES[table]["cols"])
    rows = []
    for f in files:
        t = pq.read_table(os.path.join(path, f))
        names = set(t.column_names)
        if names != want or len(t.column_names) != len(want):
            problems.append(f"{table}/{f}: columns {sorted(names)} != {sorted(want)}")
            continue
        if not pa.types.is_date32(t.schema.field("day").type):
            problems.append(f"{table}/{f}: day is {t.schema.field('day').type}, must be a Parquet DATE")
        for col in TABLES[table]["ints"]:
            if col in names and not pa.types.is_integer(t.schema.field(col).type):
                problems.append(f"{table}/{f}: {col} is {t.schema.field(col).type}, must be an integer")
        for col in want - set(TABLES[table]["ints"]) - {"day"}:
            dtype = t.schema.field(col).type
            if pa.types.is_dictionary(dtype):
                dtype = dtype.value_type
            if not (pa.types.is_string(dtype) or pa.types.is_large_string(dtype)
                    or getattr(pa.types, "is_string_view", lambda _: False)(dtype)):
                problems.append(f"{table}/{f}: {col} is {dtype}, must be a string")
        for r in t.to_pylist():
            if r["day"] is not None:
                r["day"] = str(r["day"])
            rows.append(r)
    return rows


def load_csv(path, table):
    rows = []
    with open(path, newline="", encoding="utf-8") as fh:
        for r in csv.DictReader(fh):
            rows.append({c: _norm_csv(table, c, r.get(c)) for c in TABLES[table]["cols"]})
    return rows


def load(side, table, problems):
    pdir = os.path.join(side, table)
    if os.path.isdir(pdir):
        return load_parquet_dir(pdir, table, problems)
    pcsv = os.path.join(side, table + ".csv")
    if os.path.isfile(pcsv):
        return load_csv(pcsv, table)
    problems.append(f"{table}: neither {pdir}/ nor {pcsv} exists")
    return []


def keyed(table, rows, problems, label):
    out = {}
    for r in rows:
        k = tuple(r[c] for c in TABLES[table]["key"])
        if k in out:
            problems.append(f"{label} {table}: duplicate key {k}")
        out[k] = r
    return out


def self_check(user_rows, camp_rows):
    msgs = []
    us = sum(r["sessions"] for r in user_rows)
    cs = sum(r["sessions"] for r in camp_rows)
    ur = sum(r["revenue_cents"] for r in user_rows)
    cr = sum(r["revenue_cents"] for r in camp_rows)
    assisted = sum(r["assisted_sessions"] for r in camp_rows)
    credited_paid = sum(r["sessions"] for r in camp_rows if r["campaign_id"] != "organic")
    if us != cs:
        msgs.append(f"sessions differ across tables: user_daily {us} vs campaign_daily {cs}")
    if ur != cr:
        msgs.append(f"revenue differs across tables: user_daily {ur} vs campaign_daily {cr}")
    if assisted < credited_paid:
        msgs.append(f"assisted_sessions {assisted} < credited non-organic sessions {credited_paid}")
    for r in camp_rows:
        if r["campaign_id"] == "organic" and (r["assisted_sessions"] or r["channel"] != "organic"):
            msgs.append(f"bad organic row: {r}")
            break
        if r["purchasing_sessions"] > r["sessions"]:
            msgs.append(f"purchasing_sessions > sessions: {r}")
            break
    for r in user_rows:
        if (r["top_category"] is None) != (r["revenue_cents"] == 0):
            msgs.append(f"top_category / revenue mismatch: {r}")
            break
    print(f"self-check: sessions={us} revenue={ur} assisted={assisted} "
          f"user_rows={len(user_rows)} campaign_rows={len(camp_rows)}", file=sys.stderr)
    return msgs


def main(argv):
    if len(argv) == 3 and argv[1] == "--check":
        problems = []
        u = load(argv[2], "user_daily", problems)
        c = load(argv[2], "campaign_daily", problems)
        keyed("user_daily", u, problems, "actual")
        keyed("campaign_daily", c, problems, "actual")
        problems += self_check(u, c)
        for p in problems:
            print("FAIL", p)
        print("OK" if not problems else f"{len(problems)} problem(s)")
        return 0 if not problems else 1
    if len(argv) != 3:
        print(__doc__)
        return 2
    exp_side, act_side = argv[1], argv[2]
    problems = []
    total_bad = 0
    for table in TABLES:
        e = keyed(table, load(exp_side, table, problems), problems, "expected")
        a = keyed(table, load(act_side, table, problems), problems, "actual")
        missing = [k for k in e if k not in a]
        extra = [k for k in a if k not in e]
        differing = [k for k in e if k in a and e[k] != a[k]]
        total_bad += len(missing) + len(extra) + len(differing)
        print(f"{table}: expected {len(e)} rows, actual {len(a)}; "
              f"missing {len(missing)}, extra {len(extra)}, differing {len(differing)}")
        for k in missing[:5]:
            print("  missing ", e[k])
        for k in extra[:5]:
            print("  extra   ", a[k])
        for k in differing[:5]:
            diff = {c: (e[k][c], a[k][c]) for c in TABLES[table]["cols"] if e[k][c] != a[k][c]}
            print("  differ  ", k, "(expected, actual):", diff)
    for p in problems:
        print("FAIL", p)
    ok = total_bad == 0 and not problems
    print("MATCH" if ok else "MISMATCH")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
