import sys
import tempfile
import unittest
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import compare_outputs as compare


class ScorerParity(unittest.TestCase):
    def test_numeric_id_is_not_a_string_id(self):
        with tempfile.TemporaryDirectory() as tmp:
            pq.write_table(pa.table({
                "user_id": [123], "day": pa.array([0], pa.date32()),
                "sessions": [1], "events": [1], "revenue_cents": [0],
                "top_category": pa.nulls(1, pa.string()),
            }), Path(tmp) / "part.parquet")
            problems = []
            compare.load_parquet_dir(tmp, "user_daily", problems)
            self.assertTrue(any("user_id" in msg for msg in problems))
