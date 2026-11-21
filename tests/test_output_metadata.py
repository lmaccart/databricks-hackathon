import datetime as dt
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

try:
    import polars as pl
except ImportError:
    pl = None


@unittest.skipUnless(os.environ.get("SS_BIN"), "requires the fast binary")
class TypedParquetOutput(unittest.TestCase):
    def test_pyarrow_and_polars_readback(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            data = root / "data"
            output = root / "output"
            for subdir in ("events", "clicks", "links"):
                (data / subdir).mkdir(parents=True)
            (data / "manifest.json").write_text(
                '{"window_start":"2026-10-01T00:00:00Z",'
                '"window_end":"2026-10-15T00:00:00Z"}'
            )
            (data / "products.csv").write_text(
                "product_id,category\np1,カテゴリ🎁\np2,zzz\n", encoding="utf-8"
            )
            (data / "campaigns.csv").write_text("campaign_id,channel\n", encoding="utf-8")
            (data / "clicks" / "part.csv").write_text(
                "click_id,user_id,campaign_id,ts\n", encoding="utf-8"
            )
            (data / "links" / "part.csv").write_text(
                "user_id_a,user_id_b\n", encoding="utf-8"
            )
            timestamp = "2026-10-02T00:00:00Z"
            user_id = "u\0雪🛒"
            events = [
                f"e{i},{user_id},{timestamp},{timestamp},purchase,p1,10000000"
                for i in range(222)
            ]
            events.append(
                f"last,{user_id},{timestamp},{timestamp},purchase,p2,10000000"
            )
            events.append(f"view,u2,{timestamp},{timestamp},view,p1,")
            (data / "events" / "part.csv").write_text(
                "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n"
                + "\n".join(events)
                + "\n",
                encoding="utf-8",
            )

            result = subprocess.run(
                [os.environ["SS_BIN"]],
                check=True,
                stdout=subprocess.PIPE,
                env={
                    **os.environ,
                    "DATA_DIR": str(data),
                    "OUTPUT_DIR": str(output),
                },
            )
            self.assertEqual(result.stdout, b"")

            user_file = next((output / "user_daily").glob("*.parquet"))
            campaign_file = next((output / "campaign_daily").glob("*.parquet"))
            user_parquet = pq.ParquetFile(user_file)
            campaign_parquet = pq.ParquetFile(campaign_file)
            user_schema = user_parquet.schema_arrow
            self.assertTrue(
                pa.types.is_string(user_schema.field("user_id").type)
                or pa.types.is_large_string(user_schema.field("user_id").type)
            )
            self.assertEqual(user_schema.field("day").type, pa.date32())
            self.assertEqual(user_schema.field("sessions").type, pa.int32())
            self.assertEqual(user_schema.field("events").type, pa.int32())
            self.assertEqual(user_schema.field("revenue_cents").type, pa.int64())
            self.assertTrue(user_schema.field("top_category").nullable)
            self.assertEqual(user_parquet.metadata.num_row_groups, 1)
            self.assertEqual(campaign_parquet.metadata.num_row_groups, 1)
            for parquet_file in (user_parquet, campaign_parquet):
                rg = parquet_file.metadata.row_group(0)
                for i in range(rg.num_columns):
                    column = rg.column(i)
                    self.assertEqual(column.compression, "UNCOMPRESSED")
                    self.assertIn("PLAIN", column.encodings)
                    self.assertIsNone(column.statistics)

            user_rows = sorted(
                pq.read_table(user_file).to_pylist(), key=lambda row: row["user_id"]
            )
            campaign_rows = pq.read_table(campaign_file).to_pylist()
            self.assertEqual(
                user_rows,
                [
                    {
                        "user_id": user_id,
                        "day": dt.date(2026, 10, 2),
                        "sessions": 1,
                        "events": 223,
                        "revenue_cents": 2_230_000_000,
                        "top_category": "カテゴリ🎁",
                    },
                    {
                        "user_id": "u2",
                        "day": dt.date(2026, 10, 2),
                        "sessions": 1,
                        "events": 1,
                        "revenue_cents": 0,
                        "top_category": None,
                    },
                ],
            )
            self.assertEqual(
                campaign_rows,
                [{
                    "campaign_id": "organic",
                    "channel": "organic",
                    "day": dt.date(2026, 10, 2),
                    "sessions": 2,
                    "purchasing_sessions": 1,
                    "revenue_cents": 2_230_000_000,
                    "assisted_sessions": 0,
                }],
            )
            if pl is not None:
                self.assertEqual(
                    pl.read_parquet(user_file).sort("user_id").to_dicts(), user_rows
                )
                self.assertEqual(
                    pl.read_parquet(campaign_file).to_dicts(), campaign_rows
                )
