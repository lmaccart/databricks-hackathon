"""Sessions whose boundaries fall on 30-minute epoch cells (rules.md Events 5-6, Clicks 4, 7).

An implementation may coalesce each person's events per floor(ts / 30 min) cell.
These cases put the extremes of a cell anywhere but first or last in the file,
leave cells empty between sessions, cross midnight and the epoch, and tie clicks
at a start that is not a cell boundary.
"""
import unittest

from test_reference import DAY, HOUR, MIN, ev, ms, run_case

G = 30 * MIN  # rules.md: a new session starts when the gap is over 1,800,000 ms


def shape(u, name, day):
    r = u[(name, day)]
    return r["sessions"], r["events"], r["revenue_cents"], r["top_category"]


class CellEdges(unittest.TestCase):
    def test_extremes_gaps_and_empty_cells(self):
        b = ms("2026-10-05T10:00:00Z")
        self.assertEqual(b % G, 0)  # every offset below is relative to a cell start
        users = {
            # one cell's extremes are neither first nor last in the file
            "span": ([G // 2, G - 1, 5, 2 * G - 1, G + 7, G], (1, 6)),
            "starts": ([G, 0], (1, 2)),                       # first to first: exactly 30:00.000
            "ends": ([2 * G - 1, G - 1], (1, 2)),             # last to last: exactly 30:00.000
            "empty_between": ([2 * G, G - 1], (2, 2)),        # 30:00.001 over an empty cell
            "adjacent_split": ([G + 1, 0], (2, 2)),           # adjacent cells, 30:00.001 apart
            "true_max": ([100, 10, G + 200, G + 50], (1, 4)), # G - 50 apart; G + 40 from the min
            "true_min": ([G + 500, 100, G + 101, 10], (2, 4)),  # G + 1 apart
            "chain": ([4 * G, 0, 2 * G, G, 3 * G, 6 * G + 1], (2, 6)),
            "ties": ([G, 2 * G + 1, G, 0, 2 * G + 1, G], (2, 6)),
            "x1": ([0], (1, 2)),                              # x2 links in exactly 30 min later
            "y": ([G // 2], (1, 1)),                          # same cells, another person
        }
        events = [ev(f"{name}{i}", name, str(b + off))
                  for name, (offs, _) in users.items() for i, off in enumerate(offs)]
        events.append(ev("x2-0", "x2", str(b + G)))
        # Equal-time views make these dense cells, not just sparse fallback cases.
        events += [ev(f"repeat-{name}-{i}", name, str(b + offs[0]))
                   for name, (offs, _) in users.items() for i in range(12)]
        u, c = run_case(events=events, links=["x2,x1"])
        d = "2026-10-05"
        got = {name: (u[(name, d)]["sessions"], u[(name, d)]["events"]) for name in users}
        self.assertEqual(got, {name: (s, n + 12) for name, (_, (s, n)) in users.items()})
        self.assertEqual(len(u), len(users))
        self.assertEqual(c[("organic", d)]["sessions"], sum(s for _, (s, _) in users.values()))


class CellMidnight(unittest.TestCase):
    def test_purchases_after_midnight_stay_on_start_day(self):
        m = ms("2026-10-06T00:00:00Z")
        u, c = run_case(events=[
            ev("l3", "late", str(m + 2 * G - 1), "purchase", "p2", "100"),  # exactly 30 min later
            ev("l1", "late", str(m), "purchase", "p1", "300"),
            ev("l5", "late", str(m + 3 * G + 5), "purchase", "p3", "70"),
            ev("l0", "late", str(m - 1)),                                  # 23:59:59.999 starts it
            ev("l4", "late", str(m + 3 * G), "purchase", "p1", "50"),     # 30:00.001 later: Oct 6
            ev("l2", "late", str(m + G - 1), "purchase", "p2", "200"),
            ev("e1", "edge", str(m), "purchase", "p1", "20"),             # 30:00.001 after e0
            ev("e0", "edge", str(m - G - 1), "purchase", "p3", "10"),
        ] + [ev(f"repeat-{name}-{i}", name, str(ts))
             for name, ts in (("late", m - 1), ("edge", m - G - 1)) for i in range(12)])
        self.assertEqual(shape(u, "late", "2026-10-05"), (1, 16, 600, "hats"))  # hats 300 ties shoes
        self.assertEqual(shape(u, "late", "2026-10-06"), (1, 2, 120, "bags"))
        self.assertEqual(shape(u, "edge", "2026-10-05"), (1, 13, 10, "bags"))
        self.assertEqual(shape(u, "edge", "2026-10-06"), (1, 1, 20, "shoes"))
        for day, revenue in (("2026-10-05", 610), ("2026-10-06", 140)):
            r = c[("organic", day)]
            self.assertEqual((r["sessions"], r["purchasing_sessions"], r["revenue_cents"]),
                             (2, 2, revenue))


class CellStartClicks(unittest.TestCase):
    def test_credit_and_assists_at_a_start_inside_a_cell(self):
        s = ms("2026-10-09T12:10:00Z") + 7
        t = ms("2026-10-09T15:00:00Z") + 3
        events = [
            ev("u1", "u", str(s + 1)),
            ev("u2", "u", str(s + 10 * MIN), "purchase", "p1", "400"),
            ev("u0", "u", str(s)),                 # the true start is read last
            ev("u3", "u", str(s + 2 * HOUR)),
            ev("v1", "v", str(t + 5)),
            ev("v0", "v", str(t)),
            ev("v2", "v", str(t + G + 5)),         # exactly 30 min after v1, next cell
        ]
        events += [ev(f"repeat-{name}-{i}", name, str(ts))
                   for name, ts in (("u", s + 1), ("v", t + 5)) for i in range(12)]
        clicks = [
            f"k_after,u,c_2,{s + 1}",              # 1 ms late for u's first session only
            f"k_at,u,c_1,{s}",
            f"k_week,u,c_3,{s - 7 * DAY}",
            f"k_week1,u,c_2,{s - 7 * DAY - 1}",
            f"b,v,c_1,{t - HOUR}", f"a,v,c_2,{t - HOUR}", f"B,v,c_3,{t - HOUR}",
            f"z,v,c_1,{t}", f"y,v,c_2,{t}",        # latest tie: "y" wins
            f"A,v,c_1,{t + 1}",
        ]
        u, c = run_case(events=events, clicks=clicks)
        d = "2026-10-09"
        self.assertEqual(shape(u, "u", d), (2, 16, 400, "shoes"))
        self.assertEqual(shape(u, "v", d), (1, 15, 0, None))
        got = {cid: tuple(r[f] for f in ("sessions", "purchasing_sessions", "revenue_cents",
                                          "assisted_sessions"))
               for (cid, _), r in c.items()}
        self.assertEqual(got, {"c_1": (1, 1, 400, 3), "c_2": (2, 0, 0, 2), "c_3": (0, 0, 0, 2)})


class CellBeforeEpoch(unittest.TestCase):
    def test_negative_timestamps_floor_cells_and_days(self):
        window = ("1969-12-31T00:00:00Z", "1970-01-02T00:00:00Z")
        u, c = run_case(window=window, events=[
            ev("n3", "n1", "1970-01-01T00:29:59Z"),
            ev("n1", "n1", "1969-12-31T23:59:59Z", "purchase", "p1", "100"),
            ev("n4", "n1", "1970-01-01T00:59:59Z"),        # exactly 30 min after n3
            ev("n0", "n1", "1969-12-31T23:30:00Z"),
            ev("n2", "n1", "0", "purchase", "p2", "50"),
            ev("n5", "n1", "1970-01-01T01:30:00Z"),        # 30:01 later: a new day's session
            ev("m1", "n2", "1970-01-01T00:59:59+01:00"),   # = 23:59:59Z, exactly 30 min after m0
            ev("m2", "n2", "1970-01-01T00:30:00Z"),        # 30:01 later, over an empty cell
            ev("m0", "n2", "1969-12-31T23:29:59Z"),
        ] + [ev(f"repeat-{name}-{i}", name, "1969-12-31T23:59:59Z")
             for name in ("n1", "n2") for i in range(12)], clicks=[
            "k1,n1,c_1,1969-12-24T23:30:00Z",              # exactly 7 days before n1's start
            "k2,n2,c_2,1969-12-24T23:29:58Z",              # 7 days + 1 s before n2's start
            "k3,n2,c_3,1970-01-01T00:30:00Z",              # at n2's second start
        ])
        self.assertEqual(shape(u, "n1", "1969-12-31"), (1, 17, 150, "shoes"))
        self.assertEqual(shape(u, "n1", "1970-01-01"), (1, 1, 0, None))
        self.assertEqual(shape(u, "n2", "1969-12-31"), (1, 14, 0, None))
        self.assertEqual(shape(u, "n2", "1970-01-01"), (1, 1, 0, None))
        got = {key: tuple(r[f] for f in ("sessions", "purchasing_sessions", "revenue_cents",
                                         "assisted_sessions"))
               for key, r in c.items()}
        self.assertEqual(got, {
            ("c_1", "1969-12-31"): (1, 1, 150, 1),
            ("organic", "1969-12-31"): (1, 0, 0, 0),
            ("organic", "1970-01-01"): (1, 0, 0, 0),
            ("c_3", "1970-01-01"): (1, 0, 0, 1),
        })


if __name__ == "__main__":
    unittest.main()
