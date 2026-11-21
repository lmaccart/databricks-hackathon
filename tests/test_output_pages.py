"""Cross-reader checks for the binary's bounded, preencoded Parquet pages."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from test_reference import HEADERS, WE, WS, ev, write


@unittest.skipUnless(os.environ.get("SS_BIN"), "requires the native binary")
class OutputPages(unittest.TestCase):
    def test_many_row_groups_and_exact_strings(self):
        import json
        import pyarrow.parquet as pq

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            data, out = root / "data", root / "out"
            data.mkdir()
            (data / "manifest.json").write_text(json.dumps({"window_start": WS, "window_end": WE}))
            special = "é\0終" + "x" * 1_100_000
            category = "é\0終" + "z" * 32
            events = [ev(f"e{i}", f"u{i:06}", WS, "purchase" if i % 2 else "view",
                         "p", "1") for i in range(200000)]
            events.append(ev("special", special, WS, "purchase", "p", "1"))
            write(str(data / "events/part.csv"), HEADERS["events"], events)
            write(str(data / "clicks/part.csv"), HEADERS["clicks"], [])
            write(str(data / "links/part.csv"), HEADERS["links"], [])
            write(str(data / "products.csv"), "product_id,category", ["p," + category])
            write(str(data / "campaigns.csv"), "campaign_id,channel", [])
            result = subprocess.run([os.environ["SS_BIN"]], capture_output=True, check=True,
                                    env={**os.environ, "DATA_DIR": str(data),
                                         "OUTPUT_DIR": str(out), "NCPU": "1",
                                         "RAYON_NUM_THREADS": "1"})
            self.assertEqual(result.stdout, b"")
            file = out / "user_daily/part-0.parquet"
            parquet = pq.ParquetFile(file)
            self.assertGreaterEqual(parquet.metadata.num_row_groups, 4)
            table = parquet.read()
            self.assertEqual(table.num_rows, 200001)
            rows = {r["user_id"]: r for r in table.to_pylist()}
            self.assertEqual(rows[special]["top_category"], category)
            for i in range(200000):
                row = rows[f"u{i:06}"]
                self.assertEqual(row["top_category"], category if i % 2 else None)
                self.assertEqual(row["revenue_cents"], i % 2)
            try:
                import polars as pl
            except ImportError:
                return
            other = pl.read_parquet(file)
            self.assertTrue(other.sort("user_id").equals(pl.from_arrow(table).sort("user_id")))
            self.assertEqual(other.filter(pl.col("user_id") == special)["top_category"][0], category)

    def test_table_wide_integer_width_and_empty_tables(self):
        import json
        import pyarrow as pa
        import pyarrow.parquet as pq

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            data, out = root / "data", root / "out"
            data.mkdir()
            (data / "manifest.json").write_text(json.dumps({"window_start": WS, "window_end": WE}))
            write(str(data / "products.csv"), "product_id,category", ["p,cat"])
            write(str(data / "campaigns.csv"), "campaign_id,channel", [])
            write(str(data / "links/part.csv"), HEADERS["links"], [])
            write(str(data / "clicks/part.csv"), HEADERS["clicks"], [])
            events = [ev(f"e{i}", "large", WS, "purchase", "p", "10000000")
                      for i in range(215)]
            events += [ev(f"small{i}", f"small{i}", WS) for i in range(100)]
            write(str(data / "events/part.csv"), HEADERS["events"], events)
            env = {**os.environ, "DATA_DIR": str(data), "OUTPUT_DIR": str(out),
                   "NCPU": "4", "RAYON_NUM_THREADS": "4"}
            subprocess.run([os.environ["SS_BIN"]], env=env, check=True, capture_output=True)
            files = sorted((out / "user_daily").glob("*.parquet"))
            self.assertGreater(len(files), 1)
            for file in files:
                self.assertEqual(pq.read_schema(file).field("revenue_cents").type, pa.int64())
                self.assertEqual(pq.read_schema(file).field("sessions").type, pa.int32())
            table = pq.read_table(out / "user_daily")
            large = next(r for r in table.to_pylist() if r["user_id"] == "large")
            self.assertEqual(large["revenue_cents"], 2_150_000_000)
            write(str(data / "events/part.csv"), HEADERS["events"], [])
            empty = root / "empty"
            subprocess.run([os.environ["SS_BIN"]], env={**env, "OUTPUT_DIR": str(empty)},
                           check=True, capture_output=True)
            for table_name in ("user_daily", "campaign_daily"):
                self.assertEqual(pq.read_table(empty / table_name).num_rows, 0)
                try:
                    import polars as pl
                except ImportError:
                    continue
                self.assertEqual(pl.read_parquet(empty / table_name / "*.parquet").height, 0)
