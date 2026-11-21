import unittest

from test_reference import DAY, HOUR, ev, ms, run_case


class CopyWindow(unittest.TestCase):
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
