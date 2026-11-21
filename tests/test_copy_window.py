import unittest

from test_reference import DAY, HOUR, WS_MS, ev, ms, run_case


class CopyWindow(unittest.TestCase):
    def test_cross_bin_session_credit_and_sparse_assist_expiry(self):
        # Straddle a radix boundary; the two events on either side are exactly
        # 30 minutes apart. The next gap is 30 minutes + one millisecond.
        origin = WS_MS - 7 * DAY
        radix = HOUR // 2
        midnight = ms("2026-10-03T00:00:00Z")
        boundary = origin + ((midnight - origin) // radix) * radix
        first = boundary - 500_000
        second = first + HOUR + 1
        self.assertLess(first, midnight)
        self.assertGreater(first + HOUR // 2, midnight)
        events = [
            ev("latebuy", "alias", str(first + HOUR // 2), "purchase", "p2", "150"),
            ev("sep", "person", str(second), "purchase", "p1", "90"),
            ev("early", "person", str(first)),
            ev("earlybuy", "alias", str(first), "purchase", "p1", "150"),
            ev("wideearly", "standalone", str(first)),
        ]
        clicks = [
            f"after,person,c_2,{first + 1}",
            f"z,alias,c_2,{first}",
            f"beyond,person,c_4,{first - 7 * DAY - 1}",
            f"inclusive,alias,c_3,{first - 7 * DAY}",
            f"a-long-opaque-click,person,c_1,{first}",
            f"second,alias,c_2,{second}",
            f"only,standalone,c_3,{first}",
        ]
        users, camps = run_case(
            events, clicks, links=["person,alias"],
            campaigns=[f"c_{i},search" for i in range(1, 5)],
            extra={"clicks/part-1.csv": [f'"a-long-opaque-click",person,c_1,{first}']},
        )
        days = ("2026-10-02", "2026-10-03")
        before, after = [users[("alias", day)] for day in days]
        self.assertEqual((before["sessions"], before["events"], before["revenue_cents"], before["top_category"]),
                         (1, 3, 300, "hats"))
        self.assertEqual((after["sessions"], after["events"], after["revenue_cents"]), (1, 1, 90))
        got = {key: (r["sessions"], r["purchasing_sessions"], r["revenue_cents"], r["assisted_sessions"])
               for key, r in camps.items()}
        self.assertEqual(got, {
            ("c_1", days[0]): (1, 1, 300, 1),
            ("c_2", days[0]): (0, 0, 0, 1),
            ("c_3", days[0]): (1, 0, 0, 2),
            ("c_1", days[1]): (0, 0, 0, 1),
            ("c_2", days[1]): (1, 1, 90, 1),
        })

    def test_promoted_assists_expire_and_reenter(self):
        start = WS_MS + HOUR
        days = ("2026-10-01", "2026-10-08")
        campaigns = [f"c_{i},search" for i in range(1, 13)]
        _, camps = run_case(
            [ev("first", "person", str(start)),
             ev("inclusive", "person", str(start + 7 * DAY)),
             ev("after", "person", str(start + 7 * DAY + HOUR)),
             ev("later", "person", str(start + 7 * DAY + 2 * HOUR))],
            [f"click{i},person,c_{i},{start}" for i in range(1, 13)]
            + [f"new,person,c_2,{start + 7 * DAY + HOUR + 1}"],
            campaigns=campaigns,
        )
        self.assertEqual(camps[("c_1", days[0])]["sessions"], 1)
        self.assertEqual(camps[("c_1", days[1])]["sessions"], 1)
        self.assertEqual(camps[("organic", days[1])]["sessions"], 1)
        self.assertEqual(camps[("c_2", days[1])]["sessions"], 1)
        self.assertEqual(camps[("c_2", days[1])]["assisted_sessions"], 2)
        self.assertEqual(camps[("c_12", days[1])]["assisted_sessions"], 1)

    def test_hot_person_summary_combines_counts_with_sparse_purchases(self):
        day = "2026-10-02"
        t = ms(day + "T23:59:59Z")
        ordinary = 1 << 17
        events = [f"v{i},a,{t},2026-10-02T23:59:59Z,view,p1," for i in range(ordinary)]
        events += [
            ev("buy", "b", str(t + HOUR // 2), "purchase", "p1", "12"),
            ev("new-session", "b", str(t + HOUR + 1), "purchase", "p1", "8"),
        ]
        users, camps = run_case(events, [f"k,a,c_1,{t}"], links=["a,b"])
        row = users[("a", day)]
        self.assertEqual((row["sessions"], row["events"], row["revenue_cents"]), (1, ordinary + 1, 12))
        self.assertEqual(camps[("c_1", day)]["purchasing_sessions"], 1)
        self.assertEqual(camps[("c_1", "2026-10-03")]["revenue_cents"], 8)

    def test_mixed_inline_and_interned_click_ties(self):
        t = ms("2026-10-02T00:00:00Z")
        events = [ev(f"e{u}", u, str(t)) for u in ("u1", "u2", "u3")]
        clicks = [f"{cid},{u},{campaign},{t}" for u, first, second in
                  [("u1", "a-very-long-click", "z"), ("u2", "zz", "é"),
                   ("u3", "a\0", "a\0a")]
                  for cid, campaign in [(second, "c_2"), (first, "c_1")]]
        _, c = run_case(events=events, clicks=clicks)
        self.assertEqual(c[("c_1", "2026-10-02")]["sessions"], 3)
        self.assertEqual(c[("c_2", "2026-10-02")]["sessions"], 0)

    def test_copies_count_once_until_last_session_on_each_day(self):
        t = ms("2026-10-02T12:00:00Z")
        times = [t-HOUR, t+HOUR, t+13*HOUR, t+7*DAY, t+7*DAY+HOUR]
        _, c = run_case(
            events=[ev(f"e{i}", "u", str(v)) for i, v in enumerate(times)],
            clicks=[f"kA,u,c_1,{t}", f"kC,u,c_3,{t-1}"],
            extra={"clicks/part-1.csv": [f'"kA",u,c_1,{t}', f"kZ,u,c_2,{t}"],
                   "clicks/part-2.csv": [f"kA,u,c_1,{t}", f'kZ,"u",c_2,{t}']},
        )
        got = {(camp, day[-2:]): (r["sessions"], r["assisted_sessions"])
               for (camp, day), r in c.items()}
        self.assertEqual(got, {
            ("organic", "02"): (1, 0), ("organic", "09"): (1, 0),
            **{("c_1", day): (1, 1) for day in ("02", "03", "09")},
            **{("c_2", day): (0, 1) for day in ("02", "03", "09")},
            **{("c_3", day): (0, 1) for day in ("02", "03")},
        })
