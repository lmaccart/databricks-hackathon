import unittest

from test_reference import DAY, HOUR, MIN, ev, ms, run_case


class CopyWindow(unittest.TestCase):
    def test_timestamp_ties_across_click_files_keep_lexical_credit_and_all_assists(self):
        t = ms("2026-10-02T00:00:00Z")
        _, camps = run_case(
            events=[ev("e0", "alias", str(t)), ev("e1", "u", str(t + 2 * HOUR)),
                    ev("e2", "alias", str(t + 7 * DAY)),
                    ev("e3", "u", str(t + 7 * DAY + 30 * MIN + 1))],
            links=["alias,u"],
            clicks=["z,alias,c_1," + str(t)],
            extra={
                "clicks/part-1.csv": ["10,u,c_2," + str(t)],
                "clicks/part-2.csv": ["2,alias,c_3," + str(t),
                                      "2,alias,c_3," + str(t)],
            },
        )
        day = "2026-10-02"
        next_day = "2026-10-09"
        self.assertEqual(camps[("c_2", day)]["sessions"], 2)
        self.assertEqual(camps[("c_1", day)]["sessions"], 0)
        self.assertEqual(camps[("c_3", day)]["sessions"], 0)
        for campaign in ("c_1", "c_2", "c_3"):
            self.assertEqual(camps[(campaign, day)]["assisted_sessions"], 2)
            self.assertEqual(camps[(campaign, next_day)]["assisted_sessions"], 1)
        self.assertEqual(camps[("c_2", next_day)]["sessions"], 1)
        self.assertEqual(camps[("organic", next_day)]["sessions"], 1)
        self.assertEqual(camps[("organic", next_day)]["assisted_sessions"], 0)

    def test_wide_click_fallback_preserves_opaque_ids_and_offset_time(self):
        t = ms("2026-10-04T00:00:00Z")
        uid = "用戶\0with-a-long-identifier"
        _, camps = run_case(
            events=[ev("e", uid, str(t))],
            clicks=[f"z,{uid},c_1,2026-10-04T02:00:00+02:00",
                    f"a,{uid},c_2,{t}"],
        )
        self.assertEqual(camps[("c_2", "2026-10-04")]["sessions"], 1)
        self.assertEqual(camps[("c_1", "2026-10-04")]["assisted_sessions"], 1)
        self.assertEqual(camps[("c_2", "2026-10-04")]["assisted_sessions"], 1)

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
