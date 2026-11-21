"""The optional shard profiler and forced nested sorts must not affect answers."""
import json
import os
import subprocess
import tempfile
import unittest

from test_reference import WS, WE, ev, write


@unittest.skipUnless(os.environ.get("SS_BIN"), "requires the binary")
class SessionDiagnostics(unittest.TestCase):
    def test_forced_parallel_and_diagnostic_output(self):
        import pyarrow.parquet as pq

        with tempfile.TemporaryDirectory() as tmp:
            data = os.path.join(tmp, "data")
            os.makedirs(data)
            with open(os.path.join(data, "manifest.json"), "w") as f:
                json.dump({"window_start": WS, "window_end": WE}, f)
            write(os.path.join(data, "products.csv"), "product_id,category", ["p1,z", "p2,a"])
            write(os.path.join(data, "campaigns.csv"), "campaign_id,channel", ["c1,one", "c2,two"])
            write(os.path.join(data, "links", "part.csv"), "user_id_a,user_id_b", ["a,b"])
            events = [
                ev(f"e{i}", "a" if i % 2 else "standalone-long-id", "2026-10-01T00:00:00Z",
                   "purchase" if i % 3 == 0 else "view", "p1" if i % 2 else "p2", "100")
                for i in range(180)
            ]
            clicks = [
                f"c{i},{'b' if i % 2 else 'standalone-long-id'},{'c1' if i % 3 else 'c2'},2026-10-01T00:00:00Z"
                for i in range(100)
            ]
            write(os.path.join(data, "events", "part.csv"),
                  "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", events)
            write(os.path.join(data, "clicks", "part.csv"), "click_id,user_id,campaign_id,ts", clicks)
            base_env = {k: v for k, v in os.environ.items()
                        if k not in ("SS_SESSION_TIMING", "SS_PAR_SORT_THRESHOLD")}
            results = []
            for mode in ("default", "parallel"):
                out = os.path.join(tmp, mode)
                env = {**base_env, "DATA_DIR": data, "OUTPUT_DIR": out, "RAYON_NUM_THREADS": "4"}
                if mode == "parallel":
                    env.update(SS_SESSION_TIMING="1", SS_PAR_SORT_THRESHOLD="1")
                proc = subprocess.run([os.environ["SS_BIN"]], env=env, capture_output=True, check=True)
                self.assertEqual(proc.stdout, b"")
                if mode == "default":
                    self.assertNotIn(b"session timing:", proc.stderr)
                else:
                    self.assertIn(b"session timing:", proc.stderr)
                    self.assertNotIn(b"parallel_sorts=0", proc.stderr)
                results.append([
                    sorted(pq.read_table(os.path.join(out, table)).to_pylist(),
                           key=lambda r: (r[key], r["day"]))
                    for table, key in (("user_daily", "user_id"), ("campaign_daily", "campaign_id"))
                ])
            self.assertEqual(results[0], results[1])
