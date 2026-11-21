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
             campaigns=("c_1,search", "c_2,social", "c_3,email"), extra=None, window=(WS, WE)):
    with tempfile.TemporaryDirectory() as tmp:
        return _run_case_at(tmp, events, clicks, links, products, campaigns, extra, window)


HEADERS = {"events": "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents",
           "clicks": "click_id,user_id,campaign_id,ts", "links": "user_id_a,user_id_b"}


def _run_case_at(tmp, events, clicks, links, products, campaigns, extra=None, window=(WS, WE)):
    data, out = os.path.join(tmp, "data"), os.path.join(tmp, "out")
    os.makedirs(data)
    with open(os.path.join(data, "manifest.json"), "w") as fh:
        json.dump({"window_start": window[0], "window_end": window[1]}, fh)
    write(os.path.join(data, "products.csv"), "product_id,category", products)
    write(os.path.join(data, "campaigns.csv"), "campaign_id,channel", campaigns)
    write(os.path.join(data, "events", "part-0.csv"),
          "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", events)
    write(os.path.join(data, "events", "part-1.csv"),
          "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", [])  # header only
    write(os.path.join(data, "clicks", "part-0.csv"), "click_id,user_id,campaign_id,ts", clicks)
    write(os.path.join(data, "links", "part-0.csv"), "user_id_a,user_id_b", links)
    for rel, lines in (extra or {}).items():  # more part files; may replace the header-only events/part-1
        write(os.path.join(data, rel), HEADERS[rel.split("/")[0]], lines)
    if os.environ.get("SS_BIN"):  # run an implementation binary instead of the reference
        import subprocess
        subprocess.run([os.environ["SS_BIN"]], check=True, stderr=subprocess.DEVNULL,
                       env={**os.environ, "DATA_DIR": data, "OUTPUT_DIR": out})
    else:
        P.run(data, out)
    import pyarrow.parquet as pq
    u = pq.read_table(os.path.join(out, "user_daily")).to_pylist()
    c = pq.read_table(os.path.join(out, "campaign_daily")).to_pylist()
    def unique(rows, id_col):
        result = {(r[id_col], r["day"].isoformat()): r for r in rows}
        if len(result) != len(rows):
            raise AssertionError(f"duplicate ({id_col}, day) key")
        return result
    return unique(u, "user_id"), unique(c, "campaign_id")


def ev(eid, uid, ts, etype="view", pid="p1", price="", ingest=None):
    if ingest is None:  # default: ingested at ts (F1, whole seconds), so never late
        t = P.parse_ts(ts.encode())
        t = WS_MS if t is P.BAD else t
        ingest = dt.datetime.fromtimestamp(t // 1000, dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    return f"{eid},{uid},{ts},{ingest},{etype},{pid},{price}"


class EventClaims(unittest.TestCase):
    def test_wide_manifest_cached_and_uncached_ingest_claims(self):
        ancient = "0001-01-02T00:00:00Z"
        preepoch = "1969-12-31T23:59:59Z"
        far = "9999-12-31T00:00:00Z"
        events = [
            ev("ancient", "ancient", ancient, ingest=ancient),
            ev("pre", "pre", preepoch, ingest=preepoch),
            ev("zero", "zero", "00000000000000000000", ingest=preepoch),
            ev("far", "far", far, ingest="9999-12-31T23:59:59Z"),
            ev("far-too-late", "late", "9999-12-30T00:00:00Z", ingest="9999-12-31T00:00:01Z"),
            ev("drop-cached", "loser", preepoch, ingest=preepoch),
            ev("drop-uncached", "loser", far, ingest=far),
        ]
        copies = [
            ev("drop-cached", "", preepoch, ingest=ancient),
            ev("drop-uncached", "", far, ingest=preepoch),
            ev("pre", "", preepoch, pid='p"1', ingest=ancient),
        ]
        users, _ = run_case(events, extra={"events/part-1.csv": copies},
                            window=("0001-01-01T00:00:00Z", "9999-12-31T23:59:59Z"))
        self.assertEqual(set(users), {
            ("ancient", "0001-01-02"), ("pre", "1969-12-31"),
            ("zero", "1970-01-01"), ("far", "9999-12-31"),
        })

    def test_exact_bytes_and_distant_ingest_across_files(self):
        # Quoting does not change identity, but trailing spaces, NULs, Unicode
        # and prefix extensions do. The distant invalid copy still wins.
        uid = "a"
        distinct = ("e1 ", "e1\0", "é1", "e123456789", "e01", "E1")
        events = [ev(key, uid, WS) for key in ("e1", *distinct)]
        extra = {"events/part-1.csv": [
            ev('"e1"', "", WS, ingest="1900-01-01T00:00:00Z"),
            ev("e1\0", uid, WS, ingest="9999-01-01T00:00:00Z"),
            ev('"é1"', "", WS, ingest="1900-01-01T00:00:00Z"),
        ]}
        users, _ = run_case(events, extra=extra)
        self.assertEqual(users[(uid, "2026-10-01")]["events"], len(distinct) - 1)


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
        self.assertGreater(P.parse_ts(b"99999999999999999999999"), 2**63 - 1)

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
    def test_unreadable_part_listing_fails(self):
        # ENOTDIR is deterministic even under root; a glob used to swallow it.
        with tempfile.TemporaryDirectory() as tmp:
            from pathlib import Path
            import subprocess
            data = Path(tmp)
            (data / "manifest.json").write_text(json.dumps({"window_start": WS, "window_end": WE}))
            (data / "products.csv").write_text("product_id,category\n")
            (data / "campaigns.csv").write_text("campaign_id,channel\n")
            (data / "events").touch()
            for name in ("links", "clicks"):
                (data / name).mkdir()
            cmd = ([os.environ["SS_BIN"]] if os.environ.get("SS_BIN")
                   else [sys.executable, P.__file__])
            run = subprocess.run(cmd, cwd=tmp, env={**os.environ, "DATA_DIR": tmp,
                                 "OUTPUT_DIR": str(data / "out")}, capture_output=True)
            self.assertNotEqual(run.returncode, 0)

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

    def test_wide_shuffled_header_validates_ignored_quotes_before_dedup(self):
        # Malformed copies are dropped before dedup, even when their only
        # stray quotes are in columns that the event consumer never reads.
        from unittest.mock import patch
        t = "2026-10-02T10:00:00Z"
        header = "extra,price_cents,ts,event_id,product_id,user_id,event_type,ingest_ts,tail"
        with patch.dict(HEADERS, events=header):
            u, _ = run_case(
                events=[ev("e1", "u1", t, "purchase", price="500",
                           ingest="2026-10-02T10:01:00Z")],
                extra={"events/part-1.csv": [
                    f'a"b,1,{t},e1,p1,u1,purchase,{t},c"d',
                    f'"ok",700,{t},e2,p1,u1,purchase,{t},""',
                    f'"",,{t},e3,p1,u1,view,{t},plain',
                ]},
            )
        self.assertEqual(set(u), {("u1", "2026-10-02")})
        r = u[("u1", "2026-10-02")]
        self.assertEqual((r["sessions"], r["events"], r["revenue_cents"]),
                         (1, 3, 1200))


class EventsOrder(unittest.TestCase):
    def test_purchase_stream_preserves_multiplicity_dedup_and_session_start_day(self):
        before = "2026-10-01T23:55:00Z"
        after = "2026-10-02T00:05:00Z"
        isolated = "2026-10-02T00:35:00Z"
        uid = "very long álias"
        canonical = "A canonical name"
        u, c = run_case(
            events=[
                ev("first", uid, before),
                ev("p0", uid, after, "purchase", "p1", "13"),
                ev("p1", canonical, after, "purchase", "p2", "11"),
                ev("p2", canonical, after, "purchase", "p1", "7"),
                ev("killed", uid, after, "purchase", "p3", "999", ingest=after),
                ev("edge", uid, isolated, "cart", price="999"),
                ev("next", uid, str(ms(isolated) + 30 * MIN + 1), "purchase", "p3", "20"),
                ev("other", "other", after, "purchase", "p3", "5"),
            ],
            extra={"events/part-1.csv": [
                ev("killed", "different", "bad", ingest=before),
                ev("p1", "different", after, "purchase", "p3", "999", ingest=isolated),
            ]},
            links=[f"{uid},{canonical}"],
            clicks=[f"k1,{uid},c_1,{before}", f"k2,{canonical},c_2,{after}"],
        )
        first = u[(canonical, "2026-10-01")]
        self.assertEqual((first["sessions"], first["events"], first["revenue_cents"],
                          first["top_category"]), (1, 5, 31, "shoes"))
        self.assertEqual(u[(canonical, "2026-10-02")]["revenue_cents"], 20)
        self.assertEqual(u[("other", "2026-10-02")]["revenue_cents"], 5)
        self.assertEqual(set(k[0] for k in u), {canonical, "other"})
        self.assertEqual(c[("c_1", "2026-10-01")]["revenue_cents"], 31)
        self.assertEqual(c[("c_2", "2026-10-02")]["revenue_cents"], 20)

    def test_sparse_wide_window_with_large_timestamp_spans(self):
        lo = "0001-01-01T00:00:00Z"
        late = "9998-12-31T00:00:00Z"
        person = "person with a large span"
        for hi in ("9999-01-01T00:00:00Z", str(2**63 - 1)):
            with self.subTest(window_end=hi):
                u, c = run_case(
                    window=(lo, hi),
                    events=[
                        ev("e1", person, lo, ingest=lo),
                        ev("e2", "z alias", str(ms(late)), "purchase",
                           "missing", "10000000", ingest=late),
                        ev("e3", "another", late, ingest=late),
                    ],
                    links=[f"{person},z alias"],
                    clicks=[f"k,{person},c_3,{late}"],
                )
                self.assertEqual(u[(person, "0001-01-01")]["events"], 1)
                row = u[(person, "9998-12-31")]
                self.assertEqual((row["sessions"], row["events"], row["revenue_cents"],
                                  row["top_category"]), (1, 1, 10000000, "unknown"))
                self.assertEqual(c[("c_3", "9998-12-31")]["revenue_cents"], 10000000)

    def test_no_survivors_still_writes_both_empty_tables(self):
        u, c = run_case(events=[ev("e", "", WS)], clicks=[f"k,u,c_1,{WS}"])
        self.assertEqual((u, c), ({}, {}))

    def test_long_digit_fields(self):
        zeros = "0" * 5000
        huge = "9" * 5000
        u, _ = run_case(events=[
            ev("ok", "u", zeros + str(WS_MS), "purchase", price=zeros + "7", ingest=WS),
            ev("bad-ts", "u", huge, ingest=WS),
            ev("bad-price", "u", WS, "purchase", price=huge, ingest=WS),
        ])
        row = u[("u", "2026-10-01")]
        self.assertEqual((row["events"], row["revenue_cents"]), (1, 7))

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


class ByteDistinctIds(unittest.TestCase):
    def test_dimension_spellings_and_pipelined_click_ties(self):
        s = ms("2026-10-09T12:00:00Z")
        products = ["p1,first", "p2,second", "p3,third", "p4,fourth", "p5,fifth", "p01,padded", "P1,upper",
                    "p\u00e91,unicode", "p\x001,nul", "p999999999999999999999999999,large"]
        campaigns = ["c_1,one", "c_2,two", "c_3,three", "c_4,four", "c_5,five", "c_01,padded", "C_1,upper",
                     "c_\u00e91,unicode", "c_\x001,nul",
                     "c_999999999999999999999999999,large"]
        # More than one FIFO rotation, including external and zero-padded keys.
        clicks = [f"z{i},alias,c_1,{s - HOUR}" for i in range(40)]
        clicks += [f"\u00e9,user,c_2,{s - HOUR}", f"a\x00,alias,c_01,{s - HOUR}",
                   f"a,alias,C_1,{s - HOUR}", f"b,user,c_\u00e91,{s - HOUR}",
                   f"c,alias,c_\x001,{s - HOUR}",
                   f"d,alias,c_999999999999999999999999999,{s - HOUR}",
                   f"bad,alias,c_001,{s - HOUR}",
                   f'0,alias,c_4,{s - HOUR},extra',  # malformed; would win the tie
                   f'"a",alias,"C_1",{s - HOUR}',  # identical parsed copy
                   f"solo,standalone,c_3,{s - HOUR}"]
        events = [ev(f"e{i}", "user", str(s), "purchase", product, str(i + 1))
                  for i, product in enumerate(("p1", "p2", "p01", "P1", "p\u00e91",
                                                "p\x001", "p999999999999999999999999999", "p001"))]
        events.append(ev("solo", "standalone", str(s)))
        for window in ((WS, WE), ("0001-01-01T00:00:00Z", str(2**63 - 1))):
            with self.subTest(window=window):
                users, result = run_case(events=events, clicks=clicks, links=["user,alias"],
                                         products=products, campaigns=campaigns, window=window)
                day = "2026-10-09"
                self.assertEqual(set(users), {("alias", day), ("standalone", day)})
                self.assertEqual(users[("alias", day)]["top_category"], "unknown")
                self.assertEqual(result[("C_1", day)]["sessions"], 1)
                self.assertEqual(result[("c_3", day)]["sessions"], 1)
                for campaign in ("c_1", "c_2", "c_01", "C_1", "c_\u00e91", "c_\x001",
                                 "c_999999999999999999999999999"):
                    self.assertEqual(result[(campaign, day)]["assisted_sessions"], 1)
                self.assertNotIn(("c_001", day), result)
                self.assertNotIn(("c_4", day), result)

    def test_lookalike_ids_and_lexical_click_tie(self):
        s = ms("2026-10-09T12:00:00Z")
        k = s - HOUR
        u, c = run_case(
            events=[
                ev("e1", "u1", str(s), "purchase", "p1", "100"),     # shoes 100
                ev("E1", "u1", str(s), "purchase", "P1", "100"),     # event_id case differs: kept; P1 unknown
                ev("e01", "u1", str(s), "purchase", "p01", "0150"),  # zero-padded ids: kept; unknown 150
                ev("e2", "u01", str(s)),
                ev("e3", "U1", str(s)),
                ev("e4", '"u1 "', str(s)),                            # trailing space: its own person
            ],
            clicks=[
                f"10,u1,c_2,{k}", f"2,u1,c_1,{k}",        # "10" < "2" bytewise, not numerically -> c_2
                f"k1,u01,c_1,{k}", f"K1,u01,c_3,{k}",     # "K1" < "k1" -> c_3
                f"7,U1,c_1,{k}", f"007,U1,c_2,{k}",       # "007" < "7" -> c_2
                f"x,U1,c_01,{k}", f"y,U1,C_1,{k}",        # not in campaigns.csv: invalid
            ],
        )
        d = "2026-10-09"
        self.assertEqual(set(u), {(n, d) for n in ("u1", "u01", "U1", "u1 ")})
        r = u[("u1", d)]
        self.assertEqual((r["sessions"], r["events"], r["revenue_cents"], r["top_category"]),
                         (1, 3, 350, "unknown"))
        for n in ("u01", "U1", "u1 "):
            self.assertEqual((u[(n, d)]["events"], u[(n, d)]["revenue_cents"]), (1, 0))
        got = {cid: tuple(c[(cid, d)][f] for f in ("sessions", "purchasing_sessions",
                                                    "revenue_cents", "assisted_sessions"))
               for cid in ("c_1", "c_2", "c_3", "organic") if (cid, d) in c}
        self.assertEqual(got, {
            "c_1": (0, 0, 0, 3),      # u1, u01, U1 assisted, nobody credited
            "c_2": (2, 1, 350, 2),    # u1 and U1 credited; both assisted
            "c_3": (1, 0, 0, 1),      # u01
            "organic": (1, 0, 0, 0),  # "u1 " has no clicks
        })
        self.assertEqual(len(c), 4)


class DedupMalformedVsInvalid(unittest.TestCase):
    def test_earliest_copy_malformed_or_invalid(self):
        t = "2026-10-02T10:00:00Z"
        ing = lambda sec: f"2026-10-02T10:{sec // 60:02d}:{sec % 60:02d}Z"
        u, _ = run_case(events=[
            # malformed earliest copy is dropped in parse, so the later copy is the kept one
            f"eA,uX,{t},{ing(0)},view,p1",                            # 6 fields
            ev("eA", "uA", t, ingest=ing(5)),
            ev("eA", "uA2", t, ingest=ing(9)),                        # later still: ignored
            ev("ea", "uAlow", t, ingest=ing(5)),                      # event_id case differs: own event
            # well-formed but invalid earliest copy is kept, and kills the event
            ev("eB", "uB1", "2026-13-02T10:00:00Z", ingest=ing(0)),   # bad ts
            ev("eB", "uB2", t, ingest=ing(5)),
            ev("eC", "uC1", t, "View", ingest=ing(0)),                # bad type
            ev("eC", "uC2", t, ingest=ing(5)),
            ev("eD", "", t, ingest=ing(0)),                           # empty user
            ev("eD", "uD2", t, ingest=ing(5)),
            ev("eG", "uG1", t, "purchase", price="0", ingest=ing(0)), # bad price
            ev("eG", "uG2", t, "purchase", price="5", ingest=ing(5)),
            ev("eF", "uF1", t, ingest="2026-10-04T10:00:00Z"),        # kept copy is 48 h late
            ev("eF", "uF2", "2026-10-04T09:00:00Z", ingest="2026-10-04T10:00:01Z"),
            # malformed earliest, valid middle, different-user last: middle wins
            f'eE,u"E0",{t},{ing(0)},view,p1,',                        # stray quote
            ev("eE", "uE1", t, ingest=ing(10)),
            ev("eE", "uE2", t, ingest=ing(20)),
        ])
        self.assertEqual(set(u), {("uA", "2026-10-02"), ("uAlow", "2026-10-02"), ("uE1", "2026-10-02")})
        self.assertTrue(all(r["events"] == 1 for r in u.values()))


class CrossFilePeople(unittest.TestCase):
    def test_links_events_clicks_and_dups_across_files(self):
        s = ms("2026-10-09T12:00:00Z")
        k = s - HOUR
        u, c = run_case(
            events=[ev("e1", "u3", str(s))],
            clicks=[f"10,u2,c_1,{k}"],
            links=["u02,u2"],
            extra={
                "events/part-1.csv": [
                    ev("e3", "u2", str(s + 20 * MIN)),
                    ev("e2", "zz", str(s + 10 * MIN), "purchase", "p2", "300",
                       ingest="2026-10-09T12:20:00Z"),                # later copy, other user: ignored
                    ev("e4", "U02", str(s)),                          # upper-case: separate person
                ],
                "events/part-2.csv": [
                    ev("e2", "u10", str(s + 10 * MIN), "purchase", "p2", "300",
                       ingest="2026-10-09T12:10:00Z")],
                "clicks/part-1.csv": [f"2,u3,c_2,{k}"],
                "links/part-1.csv": ["u2,u10"],
                "links/part-2.csv": ["u10,u3"],
            },
        )
        d = "2026-10-09"
        self.assertEqual(set(u), {("u02", d), ("U02", d)})   # u02 < u10 < u2 < u3; named though only linked
        r = u[("u02", d)]
        self.assertEqual((r["sessions"], r["events"], r["revenue_cents"], r["top_category"]),
                         (1, 3, 300, "hats"))
        self.assertEqual((u[("U02", d)]["events"], u[("U02", d)]["revenue_cents"]), (1, 0))
        got = {cid: tuple(c[(cid, d)][f] for f in ("sessions", "purchasing_sessions",
                                                    "revenue_cents", "assisted_sessions"))
               for cid in ("c_1", "c_2", "organic") if (cid, d) in c}
        self.assertEqual(got, {"c_1": (1, 1, 300, 1),   # "10" < "2" across click files
                               "c_2": (0, 0, 0, 1),
                               "organic": (1, 0, 0, 0)})
        self.assertEqual(len(c), 3)


if __name__ == "__main__":
    unittest.main()
