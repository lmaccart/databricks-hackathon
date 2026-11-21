#!/usr/bin/env python3
"""Hand-written edge cases for the reference pipeline, each traced to a rules.md line.

    python3 -m unittest discover -s tests -v      (from the kit root)

These are the kit author's own cases, not the organisers' 36 starter fixtures.
Run those too (see TASKS.md T1) before trusting any implementation.
"""
from __future__ import annotations

import datetime as dt
import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "reference"))
import pipeline as P  # noqa: E402

WS = "2026-10-01T00:00:00Z"
WE = "2026-10-15T00:00:00Z"
WS_MS = P.parse_ts(WS.encode())
MIN = 60_000
HOUR = 60 * MIN
DAY = 24 * HOUR


def ms(iso: str) -> int:
    return P.parse_ts(iso.encode())


def write(path, header, lines):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as fh:
        fh.write(header.encode() + b"\n")
        for ln in lines:
            fh.write((ln if isinstance(ln, bytes) else ln.encode()) + b"\n")


def run_case(events=(), clicks=(), links=(), products=("p1,shoes", "p2,hats", "p3,bags"),
             campaigns=("c_1,search", "c_2,social", "c_3,email")):
    tmp = tempfile.mkdtemp()
    data, out = os.path.join(tmp, "data"), os.path.join(tmp, "out")
    os.makedirs(data)
    with open(os.path.join(data, "manifest.json"), "w") as fh:
        json.dump({"window_start": WS, "window_end": WE}, fh)
    write(os.path.join(data, "products.csv"), "product_id,category", products)
    write(os.path.join(data, "campaigns.csv"), "campaign_id,channel", campaigns)
    write(os.path.join(data, "events", "part-0.csv"),
          "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", events)
    write(os.path.join(data, "events", "part-1.csv"),
          "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", [])  # header only
    write(os.path.join(data, "clicks", "part-0.csv"), "click_id,user_id,campaign_id,ts", clicks)
    write(os.path.join(data, "links", "part-0.csv"), "user_id_a,user_id_b", links)
    if os.environ.get("SS_BIN"):  # run an implementation binary instead of the reference
        import subprocess
        subprocess.run([os.environ["SS_BIN"]], check=True, stderr=subprocess.DEVNULL,
                       env={**os.environ, "DATA_DIR": data, "OUTPUT_DIR": out})
    else:
        P.run(data, out)
    import pyarrow.parquet as pq
    u = pq.read_table(os.path.join(out, "user_daily")).to_pylist()
    c = pq.read_table(os.path.join(out, "campaign_daily")).to_pylist()
    return ({(r["user_id"], r["day"].isoformat()): r for r in u},
            {(r["campaign_id"], r["day"].isoformat()): r for r in c})


def ev(eid, uid, ts, etype="view", pid="p1", price="", ingest=None):
    if ingest is None:  # default: ingested at ts (F1, whole seconds), so never late
        t = P.parse_ts(ts.encode())
        t = WS_MS if t is P.BAD else t
        ingest = dt.datetime.fromtimestamp(t // 1000, dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    return f"{eid},{uid},{ts},{ingest},{etype},{pid},{price}"


class Timestamps(unittest.TestCase):
    def test_forms(self):
        f1 = ms("2026-10-03T23:58:10Z")
        self.assertEqual(f1, 1791071890000)
        self.assertEqual(P.parse_ts(b"2026-10-03T19:58:10-04:00"), f1)
        self.assertEqual(P.parse_ts(b"2026-10-04T05:28:10+05:30"), f1)
        self.assertEqual(P.parse_ts(b"1791071890000"), f1)
        self.assertEqual(P.parse_ts(b"0001791071890000"), f1)
        self.assertEqual(P.parse_ts(b"2026-10-03T23:58:10-00:00"), f1)
        self.assertEqual(P.parse_ts(b"86400000"), 86400000)
        self.assertEqual(P.parse_ts(b"2028-02-29T00:00:00Z"), ms("2028-02-28T00:00:00Z") + DAY)
        self.assertEqual(P.parse_ts(b"99999999999999999999999"), 99999999999999999999999)

    def test_bad(self):
        for bad in [b"", b"2026-10-03T23:58:10.000Z", b"2026-10-03T23:58:10z", b"2026-10-03t23:58:10Z",
                    b"2026-10-03 23:58:10Z", b"2026-10-03T23:58:10", b"1791071890000.0", b"1.79e12",
                    b"-5", b"2026-13-01T00:00:00Z", b"2026-02-30T00:00:00Z", b"2026-02-29T00:00:00Z",
                    b"2026-10-03T24:00:00Z", b"2026-10-03T23:59:60Z", b"2026-10-03T23:58:10+24:00",
                    b"2026-10-03T23:58:10+05:60", b"2026-10-03T23:58:10+0200",
                    b"2026-10-03T23:58:10Z\r", "١٧٩١٠٧١٨٩٠٠٠٠".encode(), b"+1791071890000",
                    b" 2026-10-03T23:58:10Z"]:
            self.assertIs(P.parse_ts(bad), P.BAD, bad)


class CsvDialect(unittest.TestCase):
    def test_lines(self):
        self.assertEqual(P.parse_line(b'"u1",,""', 3), [b"u1", b"", b""])
        self.assertIsNone(P.parse_line(b'u"1,a,b', 3))
        self.assertIsNone(P.parse_line(b'"u1,a,b', 3))
        self.assertIsNone(P.parse_line(b'"""",a,b', 3))
        self.assertIsNone(P.parse_line(b"a,b", 3))
        self.assertIsNone(P.parse_line(b"", 1))
        self.assertEqual(P.parse_line(b"a,b,1000\r", 3), [b"a", b"b", b"1000\r"])

    def test_dialect_end_to_end(self):
        t = "2026-10-02T10:00:00Z"
        u, _ = run_case(events=[
            ev("e1", '"u1"', t, "purchase", "p1", '"500"'),
            ev("e2", " u1", t, "purchase", "p1", "700"),        # different user (leading space)
            ev("e3", "u1", t, "purchase", "p1", "1000\r"),       # bad price because of \r
            ev("e4", "u1", t, "view", "p1", "garbage"),          # price ignored for views
            'e5,u1,2026-10-02T10:00:00Z,2026-10-02T10:00:00Z,view,p1',  # wrong field count
            'e6,u"1,2026-10-02T10:00:00Z,2026-10-02T10:00:00Z,view,p1,',  # stray quote
            b"",                                                  # empty line
            ev("e7", "u1", t, "Purchase", "p1", "10"),            # bad type
            ev("e8", "u1", t, "cart ", "p1", ""),                 # bad type (trailing space)
            ev("e9", "u1", t, "purchase", "p1", "0"),             # price zero
            ev("e10", "u1", t, "purchase", "p1", "10000001"),     # price too big
            ev("e11", "u1", t, "purchase", "p2", "0250"),         # leading zero = 250
            ev("e12", "u1", t, "purchase", "", "100"),            # empty product
            ev("e13", "", t, "view"),                             # empty user
            ev("", "u1", t, "view"),                              # empty event_id
        ])
        self.assertEqual(set(u), {("u1", "2026-10-02"), (" u1", "2026-10-02")})
        r = u[("u1", "2026-10-02")]
        self.assertEqual((r["sessions"], r["events"], r["revenue_cents"], r["top_category"]),
                         (1, 3, 750, "shoes"))
        self.assertEqual(u[(" u1", "2026-10-02")]["revenue_cents"], 700)


class EventsOrder(unittest.TestCase):
    def test_dedup_before_validation(self):
        t = "2026-10-02T10:00:00Z"
        u, _ = run_case(events=[
            ev("e1", "u1", "2026-13-02T10:00:00Z", ingest="2026-10-02T10:00:00Z"),  # earliest copy, bad ts
            ev("e1", "u1", t, ingest="2026-10-02T10:00:05Z"),                       # later valid copy
            ev("e2", "u2", t, ingest="2026-10-02T10:05:00Z"),                       # later copy, other user
            ev("e2", "u3", t, ingest="2026-10-02T10:00:01Z"),                       # kept: smallest ingest
        ])
        self.assertEqual(set(u), {("u3", "2026-10-02")})

    def test_late(self):
        t = "2026-10-02T10:00:00Z"
        u, _ = run_case(events=[
            ev("e1", "u1", t, ingest="2026-10-03T10:00:00Z"),   # exactly 24 h: kept
            ev("e2", "u2", t, ingest="2026-10-03T10:00:01Z"),   # 24 h + 1 s: dropped
            ev("e3", "u3", t, ingest="2026-10-01T10:00:00Z"),   # ingest before ts: kept
        ])
        self.assertEqual(set(u), {("u1", "2026-10-02"), ("u3", "2026-10-02")})

    def test_window(self):
        u, _ = run_case(events=[
            ev("e1", "u1", WS, ingest=WS),
            ev("e2", "u2", str(WS_MS - 1), ingest=WS),
            ev("e3", "u3", WE, ingest=WE),
            ev("e4", "u4", str(ms(WE) - 1), ingest=WE),
        ])
        self.assertEqual(set(u), {("u1", "2026-10-01"), ("u4", "2026-10-14")})

    def test_sessions(self):
        base = ms("2026-10-05T23:40:00Z")
        events = [
            ev("a1", "u1", str(base)),
            ev("a2", "u1", str(base + 30 * MIN)),            # gap exactly 30:00.000 -> same session
            ev("a3", "u1", str(base + 60 * MIN + 1)),        # gap 30:00.001 -> new session (Oct 6)
            ev("a4", "u1", str(base + 60 * MIN + 1)),        # equal ts joins it
            ev("a5", "u1", str(base + 80 * MIN), "purchase", "p1", "100"),
        ]
        u, c = run_case(events=events)
        self.assertEqual(u[("u1", "2026-10-05")]["sessions"], 1)
        self.assertEqual(u[("u1", "2026-10-05")]["events"], 2)
        self.assertEqual(u[("u1", "2026-10-06")]["events"], 3)
        self.assertEqual(u[("u1", "2026-10-06")]["revenue_cents"], 100)
        self.assertEqual(c[("organic", "2026-10-05")]["sessions"], 1)
        self.assertEqual(c[("organic", "2026-10-06")]["purchasing_sessions"], 1)

    def test_midnight_counts_on_start_day(self):
        u, _ = run_case(events=[
            ev("e1", "u1", "2026-10-05T23:50:00Z"),
            ev("e2", "u1", "2026-10-06T00:10:00Z", "purchase", "p1", "999"),
        ])
        self.assertEqual(set(u), {("u1", "2026-10-05")})
        self.assertEqual(u[("u1", "2026-10-05")]["revenue_cents"], 999)

    def test_top_category(self):
        t = "2026-10-02T10:00:00Z"
        u, _ = run_case(events=[
            ev("e1", "u1", t, "purchase", "p2", "300"),    # hats 300
            ev("e2", "u1", t, "purchase", "p1", "300"),    # shoes 300 -> tie -> "hats" < "shoes"
            ev("e3", "u2", t, "purchase", "zz", "50"),     # unknown product
            ev("e4", "u2", t, "purchase", "P1", "40"),     # case variant: also unknown
            ev("e5", "u2", t, "purchase", "p3", "80"),     # bags 80 < unknown 90
            ev("e6", "u3", t, "view", "p1"),
        ])
        self.assertEqual(u[("u1", "2026-10-02")]["top_category"], "hats")
        self.assertEqual(u[("u2", "2026-10-02")]["top_category"], "unknown")
        self.assertIsNone(u[("u3", "2026-10-02")]["top_category"])


class LinkedIds(unittest.TestCase):
    def test_chain_naming(self):
        t1, t2 = "2026-10-02T10:00:00Z", "2026-10-02T10:20:00Z"
        u, _ = run_case(
            events=[ev("e1", "c", t1), ev("e2", "b", t2), ev("e3", "x", t1), ev("e4", "U9", t1)],
            links=["b,c", "c,b", "d,c", "d,a0", "x,x", "y,", ",x", "x"],
        )
        # a0 never appears in an event but is the smallest ID of {a0,b,c,d}
        self.assertEqual(set(u), {("a0", "2026-10-02"), ("x", "2026-10-02"), ("U9", "2026-10-02")})
        self.assertEqual(u[("a0", "2026-10-02")]["sessions"], 1)
        self.assertEqual(u[("a0", "2026-10-02")]["events"], 2)

    def test_uppercase_sorts_first(self):
        u, _ = run_case(events=[ev("e1", "u5", "2026-10-02T10:00:00Z")], links=["u5,U5"])
        self.assertEqual(set(u), {("U5", "2026-10-02")})


class Clicks(unittest.TestCase):
    def test_credit_boundaries(self):
        s = ms("2026-10-09T12:00:00Z")
        events = [ev(f"e{i}", f"u{i}", str(s)) for i in range(1, 7)]
        clicks = [
            f"k1,u1,c_1,{s - 7 * DAY}",          # exactly 7 days before: counts
            f"k2,u2,c_1,{s - 7 * DAY - 1}",      # 7 days + 1 ms: does not
            f"k3,u3,c_2,{s}",                    # at the session start: counts
            f"k4,u4,c_2,{s + 1}",                # 1 ms after: does not
            f"kb,u5,c_3,{s - HOUR}",             # same ms tie: smallest click_id wins
            f"ka,u5,c_1,{s - HOUR}",
            f"k0,u5,c_2,{s - 2 * HOUR}",         # older, but still assists
            f"k5,u6,c_9,{s - HOUR}",             # unknown campaign: invalid
            f"k6,u6,C_1,{s - HOUR}",             # case variant: invalid
        ]
        u, c = run_case(events=events, clicks=clicks)
        d = "2026-10-09"
        self.assertEqual(c[("c_1", d)]["sessions"], 2)          # u1 and u5 (ka)
        self.assertEqual(c[("c_2", d)]["sessions"], 1)          # u3
        self.assertEqual(c[("organic", d)]["sessions"], 3)      # u2, u4, u6
        self.assertEqual(c[("organic", d)]["assisted_sessions"], 0)
        self.assertEqual(c[("organic", d)]["channel"], "organic")
        self.assertEqual(c[("c_1", d)]["assisted_sessions"], 2)   # u1, u5
        self.assertEqual(c[("c_2", d)]["assisted_sessions"], 2)   # u3, u5
        self.assertEqual(c[("c_3", d)]["assisted_sessions"], 1)   # u5 only, credited nothing
        self.assertEqual(c[("c_3", d)]["sessions"], 0)
        self.assertEqual(c[("c_3", d)]["channel"], "email")

    def test_assist_once_and_dup_and_click_window(self):
        s = WS_MS + HOUR
        clicks = [
            f"k1,u1,c_1,{s - DAY}", f"k2,u1,c_1,{s - 2 * DAY}",   # same campaign twice: assists once
            f'"k1","u1","c_1","{s - DAY}"',                        # duplicate, quoted differently
            f"k3,u2,c_2,{WS_MS - 7 * DAY}",                        # exactly at click window start: valid
            f"k4,u3,c_2,{WS_MS - 7 * DAY - 1}",                    # before click window: invalid
        ]
        events = [ev("e1", "u1", str(s)), ev("e2", "u2", str(WS_MS)), ev("e3", "u3", str(WS_MS))]
        u, c = run_case(events=events, clicks=clicks)
        d = "2026-10-01"
        self.assertEqual(c[("c_1", d)]["assisted_sessions"], 1)
        self.assertEqual(c[("c_1", d)]["sessions"], 1)
        self.assertEqual(c[("c_2", d)]["sessions"], 1)
        self.assertEqual(c[("organic", d)]["sessions"], 1)

    def test_clicks_follow_links(self):
        s = ms("2026-10-09T12:00:00Z")
        u, c = run_case(events=[ev("e1", "phone", str(s), "purchase", "p1", "1234")],
                        clicks=[f"k1,laptop,c_2,{s - HOUR}"], links=["laptop,phone"])
        self.assertEqual(set(u), {("laptop", "2026-10-09")})
        r = c[("c_2", "2026-10-09")]
        self.assertEqual((r["sessions"], r["purchasing_sessions"], r["revenue_cents"]), (1, 1, 1234))


if __name__ == "__main__":
    unittest.main()
