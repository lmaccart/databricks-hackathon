"""The disposable PGO corpus is self-consistent and agrees with the oracle."""
import csv
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "reference"))
import pipeline  # noqa: E402


class TrainingCorpus(unittest.TestCase):
    def test_generated_layouts(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            gen = tmp / "generate"
            subprocess.run(["rustc", "-O", str(ROOT / "tools/pgo_generate.rs"), "-o", str(gen)], check=True)
            for layout in range(3):
                with self.subTest(layout=layout):
                    data = tmp / f"data-{layout}"
                    subprocess.run([str(gen), str(data), str(layout), "1000"], check=True)
                    manifest = json.loads((data / "manifest.json").read_text())
                    start = pipeline.parse_ts(manifest["window_start"].encode())
                    end = pipeline.parse_ts(manifest["window_end"].encode())
                    rows = []
                    for f in (data / "events").glob("*.csv"):
                        with f.open(newline="") as stream:
                            rows.extend(csv.DictReader(stream))
                    self.assertEqual(len(rows), 1000 + (999 // 37 + 1))
                    self.assertEqual(len({r["event_id"] for r in rows}), 1000)
                    self.assertTrue(any(r["event_type"] == "purchase" for r in rows))
                    self.assertTrue(any(len(r["user_id"]) < 7 for r in rows))
                    self.assertTrue(any(r["ts"].isdigit() for r in rows))
                    self.assertTrue(any(r["ts"].endswith("+00:00") for r in rows))
                    for row in rows:
                        ts = pipeline.parse_ts(row["ts"].encode())
                        self.assertLessEqual(start, ts)
                        self.assertLess(ts, end)
                        self.assertEqual(ts, pipeline.parse_ts(row["ingest_ts"].encode()))
                    ref = tmp / f"ref-{layout}"
                    pipeline.run(str(data), str(ref))
                    if binary := os.environ.get("SS_BIN"):
                        out = tmp / f"out-{layout}"
                        raw_profile = tmp / f"unexpected-{layout}.profraw"
                        result = subprocess.run([binary], env={**os.environ, "DATA_DIR": str(data),
                                                "OUTPUT_DIR": str(out), "NCPU": "2",
                                                "LLVM_PROFILE_FILE": str(raw_profile)},
                                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
                        self.assertEqual(result.stdout, b"")
                        self.assertFalse(raw_profile.exists(), "release executable must not emit profiles")
                        subprocess.run([sys.executable, str(ROOT / "tools/compare_outputs.py"),
                                        str(ref), str(out)], check=True, stdout=subprocess.DEVNULL)


if __name__ == "__main__":
    unittest.main()
