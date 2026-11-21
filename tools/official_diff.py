"""T6: compare the reference and release binary on the official generator.

    .venv/bin/python tools/official_diff.py
    .venv/bin/python tools/official_diff.py --seeds 15 --sizes 1000000

Runs one oracle at a time. Successful cases are removed; the first failed
case is retained under scratch/official-diff/ with input, outputs and log.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from datetime import date
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PYTHON = sys.executable
GENERATOR = ROOT / "vendor/generator/target/release/generate"
BINARY = ROOT / "fast/target/release/shopping_sessions"


def compare(seed, events, base, binary):
    case = Path(tempfile.mkdtemp(prefix=f"s{seed}-e{events}-", dir=base))
    data, ref, fast, scratch = (case / p for p in ("data", "ref", "fast", "scratch"))
    clicks = events // 4  # Same ratio as the starter's check-dev.
    try:
        scratch.mkdir()
        env = {**os.environ, "SCRATCH_DIR": str(scratch), "DATA_DIR": str(data),
               "OUTPUT_DIR": str(fast), "PYTHONDONTWRITEBYTECODE": "1"}
        with (case / "run.log").open("w") as log:
            def run(*args):
                subprocess.run(args, cwd=case, env=env, check=True, stdout=log,
                               stderr=subprocess.STDOUT)

            run(GENERATOR, "--seed", str(seed), "--events", str(events), "--clicks",
                str(clicks), "--output", data)
            manifest = json.loads((data / "manifest.json").read_text())
            start, end = (date.fromisoformat(manifest[k][:10])
                          for k in ("window_start", "window_end"))
            leap = start <= date(2028, 2, 29) < end
            if seed == 15 and not leap:
                raise ValueError("Seed 15 no longer covers leap day; check generator version")
            run(PYTHON, ROOT / "reference/pipeline.py", data, ref)
            run(binary)
            run(PYTHON, ROOT / "tools/compare_outputs.py", ref, fast)
    except BaseException:
        print(f"Unfinished case retained at {case} (see run.log)", file=sys.stderr)
        raise
    shutil.rmtree(case)
    print(f"MATCH seed={seed} events={events} clicks={clicks} window=[{start},{end})"
          + (" leap-day" if leap else ""), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seeds", type=int, nargs="+", default=range(1, 21))
    parser.add_argument("--sizes", type=int, nargs="+", default=[10000, 100000, 1000000])
    parser.add_argument("--binary", type=Path, default=BINARY,
                        help="immutable release binary to check when comparing several candidates")
    args = parser.parse_args()
    if any(not 1000 <= size <= 1000000 for size in args.sizes):
        parser.error("sizes must be between 1000 and 1000000")
    if any(not 0 <= seed < 2**64 for seed in args.seeds):
        parser.error("seeds must be unsigned 64-bit integers")
    base = ROOT / "scratch/official-diff"
    base.mkdir(parents=True, exist_ok=True)
    binary = args.binary.resolve(strict=True)
    for size in args.sizes:
        for seed in args.seeds:
            compare(seed, size, base, binary)
    print(f"Passed {len(args.seeds) * len(args.sizes)} comparisons.")


if __name__ == "__main__":
    main()
