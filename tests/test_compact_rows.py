"""Exactness at the edges of compact fast-path rows.

Dedup claims may store ingest_ts relative to the window, so far, irregular and
pre-epoch ingest times must still pick the earliest copy. Session rows may store
time relative to window_start and the category in a byte, so windows on both
sides of 2**32 ms (and before the epoch) and category counts on both sides of
256, `unknown` included, must give the same answers as the oracle.
"""
import datetime as dt
import unittest

from test_reference import DAY, MIN, WS, WS_MS, ev, ms, run_case

EPOCH = dt.datetime(1970, 1, 1, tzinfo=dt.timezone.utc)


def f1(t):
    """F1 text for whole-second epoch milliseconds, years 1 to 9999."""
    assert t % 1000 == 0
    d = EPOCH + dt.timedelta(milliseconds=t)
    return f"{d.year:04d}-{d.month:02d}-{d.day:02d}T{d.hour:02d}:{d.minute:02d}:{d.second:02d}Z"


def stamp(t):
    return str(t) if t >= 0 else f1(t)  # F3 has no sign


def day(t):
    return (EPOCH + dt.timedelta(days=t // DAY)).date().isoformat()


class ExceptionalIngest(unittest.TestCase):
    def test_far_and_irregular_ingests_keep_the_earliest_copy(self):
        t = ms("2026-10-02T10:00:00Z")
        ws_s = WS_MS // 1000
        pairs = []  # (earlier, later) ingest_ts of two copies; the earlier one is kept
        for o in (-(2**32), -(2**31), -(2**30), -1, 0):  # before ts: never late
            for d in (-1, 0, 1):
                pairs.append((f1((ws_s + o + d) * 1000), f1((ws_s + o + d + 1) * 1000)))
        for o in (2**30, 2**31, 2**32):  # far after ts: a late copy that must lose
            for d in (-1, 0, 1):
                pairs.append((f1(t), f1((ws_s + o + d) * 1000)))
        pairs += [
            ("0000-01-01T00:00:00Z", "0000-01-01T00:00:01Z"),
            ("0000-02-29T00:00:00Z", "0000-03-01T00:00:00Z"),  # year 0 is a leap year
            ("1969-12-31T23:59:59Z", "1970-01-01T00:00:00Z"),
            (f1(t), "9999-12-31T23:59:59Z"),
            (str(t - 1), f1(t)),  # rules.md promises F1; the oracle parses every form
            (f1(t), str(t + 1)),
            (f1(t), "not a time"),  # a bad ingest_ts ranks after every valid one
        ]
        files = [[], [], []]
        winners = []
        for i, (early, late) in enumerate(pairs):
            winners.append(f"w{i}")
            copies = [ev(f"x{i}", f"w{i}", f1(t), ingest=early), ev(f"x{i}", f"l{i}", f1(t), ingest=late)]
            if i % 2:
                copies.reverse()
            files[i % 3].append(copies[0])
            files[(i + 1) % 3].append(copies[1])
        files[0] += [
            ev("bad", "bad", "2026-13-02T10:00:00Z", ingest="0000-01-01T00:00:00Z"),  # kept, invalid
            ev("twin", "twin", f1(t), ingest="0001-01-01T00:00:00Z"),
            ev("past", "past", f1(t), ingest="1900-01-01T00:00:00Z"),
            ev("future", "future", f1(t), ingest="2200-01-01T00:00:00Z"),  # late
        ]
        files[2] += [
            ev("bad", "rescued", f1(t), ingest=f1(t)),
            ev("twin", "twin", f1(t), ingest="0001-01-01T00:00:00Z"),  # equal ingest: identical copy
        ]
        u, _ = run_case(events=files[0], extra={"events/part-1.csv": files[1], "events/part-2.csv": files[2]})
        self.assertEqual(set(u), {(w, "2026-10-02") for w in winners + ["twin", "past"]})
        self.assertTrue(all(r["events"] == 1 for r in u.values()))


class EventWindows(unittest.TestCase):
    def check(self, ws, we):
        ws_ms, we_ms = ms(ws), ms(we)
        last = we_ms - 1
        s1 = last - 30 * MIN - 1  # u1's session spans gaps of exactly 30 min and 1 ms to `last`
        s5 = s1 - 30 * MIN - 1  # u5: one millisecond too far from s1 to share its session
        d = day(last)
        self.assertEqual(day(s5), d)
        self.assertNotEqual(day(ws_ms), d)
        events = [
            ev("a1", "u1", stamp(s1)),
            ev("a2", "u1", stamp(last - 1), "purchase", "p1", "500"),
            ev("a3", "u1", stamp(last), "cart"),
            ev("b1", "u2", stamp(last), "purchase", "p2", "300"),  # equal ts and sums: hats < shoes
            ev("b2", "u2", stamp(last), "purchase", "p1", "300"),
            ev("b3", "u2", stamp(last), "view"),
            ev("c1", "u3", stamp(we_ms)),  # window_end is outside
            ev("d1", "u4", ws),  # window_start is inside
            ev("e1", "u5", stamp(s5)),
            ev("e2", "u5", stamp(s1), "purchase", "zz", "7"),
            ev("f1", "u6", stamp(ws_ms + DAY - 10 * MIN)),  # crosses midnight
            ev("f2", "u6", stamp(ws_ms + DAY + 10 * MIN), "purchase", "p3", "11"),
        ]
        clicks = [
            f"k1,u1,c_1,{stamp(s1 - 7 * DAY)}",  # exactly 7 days before the start
            f"k3,u2,c_3,{stamp(last)}",  # at the start, same ms: k2 < k3 is credited
            f"k2,u2,c_2,{stamp(last)}",
            f"k4,u5,c_3,{stamp(s5 + 1)}",  # 1 ms after s5, in time for s1
        ]
        u, c = run_case(events=events, clicks=clicks, window=(ws, we))
        users = {k: (r["sessions"], r["events"], r["revenue_cents"], r["top_category"]) for k, r in u.items()}
        self.assertEqual(users, {
            ("u1", d): (1, 3, 500, "shoes"),
            ("u2", d): (1, 3, 600, "hats"),
            ("u4", day(ws_ms)): (1, 1, 0, None),
            ("u5", d): (2, 2, 7, "unknown"),
            ("u6", day(ws_ms)): (1, 2, 11, "bags"),
        })
        camps = {k: (r["sessions"], r["purchasing_sessions"], r["revenue_cents"], r["assisted_sessions"])
                 for k, r in c.items()}
        self.assertEqual(camps, {
            ("c_1", d): (1, 1, 500, 1),
            ("c_2", d): (1, 1, 600, 1),
            ("c_3", d): (1, 1, 7, 2),
            ("organic", d): (1, 0, 0, 0),
            ("organic", day(ws_ms)): (2, 1, 11, 0),
        })

    def test_last_millisecond_on_both_sides_of_2_pow_32(self):
        self.check(WS, str(WS_MS + 2**32))
        self.check(WS, str(WS_MS + 2**32 + 1))

    def test_last_millisecond_on_both_sides_of_2_pow_31(self):
        self.check(WS, str(WS_MS + 2**31))
        self.check(WS, str(WS_MS + 2**31 + 1))

    def test_long_and_pre_epoch_windows(self):
        self.check(WS, "2027-10-01T00:00:00Z")
        self.check("1969-12-20T00:00:00Z", "1970-01-10T00:00:00Z")
        self.check("1969-10-01T00:00:00Z", "1970-03-01T00:00:00Z")


class CategoryCounts(unittest.TestCase):
    def test_top_category_on_both_sides_of_256_names(self):
        t = "2026-10-02T10:00:00Z"
        for n in (255, 256, 300):  # plus `unknown`
            with self.subTest(categories=n):
                # Index order is the reverse of byte order: p{n-1} is cat000.
                products = [f"p{i},cat{n - 1 - i:03d}" for i in range(n)]
                first, prev, last = "p0", f"p{n - 2}", f"p{n - 1}"
                u, _ = run_case(products=products, events=[
                    ev("a1", "u1", t, "purchase", last, "400"),
                    ev("a2", "u1", t, "purchase", prev, "400"),
                    ev("a3", "u1", t, "view", first),
                    ev("b1", "u2", t, "purchase", "zz", "500"),
                    ev("b2", "u2", t, "purchase", last, "499"),
                    ev("c1", "u3", t, "purchase", first, "100"),
                    ev("c2", "u3", t, "purchase", "zz", "100"),
                    ev("d1", "u4", t, "purchase", last, "9"),
                    ev("d2", "u4", t, "purchase", first, "10"),
                ])
                d = "2026-10-02"
                got = {k: (r["events"], r["revenue_cents"], r["top_category"]) for k, r in u.items()}
                self.assertEqual(got, {
                    ("u1", d): (3, 800, "cat000"),
                    ("u2", d): (2, 999, "unknown"),
                    ("u3", d): (2, 200, f"cat{n - 1:03d}"),
                    ("u4", d): (2, 19, f"cat{n - 1:03d}"),
                })


if __name__ == "__main__":
    unittest.main()
