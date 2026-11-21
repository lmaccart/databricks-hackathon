"""Time native process start-to-exit on an existing data set, with fresh outputs."""
import argparse
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--binary", type=Path, default=Path("fast/target/release/shopping_sessions"))
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--threads", type=int, default=8)
    args = parser.parse_args()
    if min(args.threads, args.runs) < 1:
        parser.error("runs and threads must both be positive")
    binary = args.binary.resolve(strict=True)
    durations = []
    for i in range(args.runs):
        with tempfile.TemporaryDirectory() as out:
            env = dict(os.environ, DATA_DIR=str(args.data), OUTPUT_DIR=out,
                       RAYON_NUM_THREADS=str(args.threads), NCPU=str(args.threads))
            start = time.perf_counter()
            result = subprocess.run([binary], env=env, stdout=subprocess.PIPE, check=True)
            durations.append(time.perf_counter() - start)
            if result.stdout:
                raise SystemExit("pipeline wrote to stdout")
        print(f"run {i + 1}: {durations[-1]:.3f} s", flush=True)
    print(f"median: {statistics.median(durations):.3f} s")


if __name__ == "__main__":
    main()
