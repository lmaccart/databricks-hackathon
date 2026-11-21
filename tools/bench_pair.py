"""Compare two native binaries on the same input; alternate AB and BA pairs."""
import argparse
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import tempfile
from time import perf_counter_ns


def measure(binary, data, threads, tmpdir, overrides):
    with tempfile.TemporaryDirectory(dir=tmpdir) as temp:
        root = Path(temp)
        out, scratch = root / "out", root / "scratch"
        out.mkdir()
        scratch.mkdir()
        env = dict(os.environ | overrides, DATA_DIR=str(data), OUTPUT_DIR=str(out),
                   SCRATCH_DIR=str(scratch), NCPU=str(threads), RAYON_NUM_THREADS=str(threads))
        start = perf_counter_ns()
        result = subprocess.run([binary], env=env, capture_output=True)
        seconds = (perf_counter_ns() - start) / 1e9
        if result.returncode or result.stdout:
            raise SystemExit(f"{binary}: exit {result.returncode}, stdout {len(result.stdout)} bytes\n"
                             + result.stderr.decode(errors="replace"))
        return {"seconds": seconds, "stderr": result.stderr.decode()}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for arg in ("baseline", "candidate", "data"):
        p.add_argument(arg, type=lambda v: Path(v).resolve(strict=True))
    p.add_argument("--runs", type=int, default=5)
    p.add_argument("--threads", type=int, default=8)
    p.add_argument("--no-warmup", action="store_true")
    p.add_argument("--json", type=Path)
    p.add_argument("--tmpdir", type=Path)
    p.add_argument("--phases", action="store_true", help="print median increments from the pipeline's stderr clock")
    for side in ("baseline", "candidate"):
        p.add_argument(f"--{side}-env", metavar="NAME=VALUE", action="append", default=[],
                       help=f"Override an environment variable for the {side} process only")
    args = p.parse_args()
    if min(args.threads, args.runs) < 1:
        p.error("runs and threads must be positive")
    bins = [args.baseline, args.candidate]
    try:
        envs = [dict(item.split("=", 1) for item in values)
                for values in (args.baseline_env, args.candidate_env)]
    except ValueError:
        p.error("environment overrides must use NAME=VALUE")
    if not args.no_warmup:
        for b, env in zip(bins, envs):
            measure(b, args.data, args.threads, args.tmpdir, env)
    pairs = []
    for i in range(args.runs):
        pair = [None, None]
        for j in ([0, 1] if i % 2 == 0 else [1, 0]):
            pair[j] = measure(bins[j], args.data, args.threads, args.tmpdir, envs[j])
        pairs.append(pair)
        print(f"pair {i+1}: baseline {pair[0]['seconds']:.3f} s, candidate {pair[1]['seconds']:.3f} s",
              flush=True)
    base, test = [statistics.median(pair[j]["seconds"] for pair in pairs) for j in (0, 1)]
    print(f"medians: {base:.3f} → {test:.3f} s, {(1-test/base)*100:+.1f}% less time, {base/test:.2f}×")
    if args.phases:
        for j, label in enumerate(("baseline", "candidate")):
            phases = {}
            for pair in pairs:
                prev = 0
                for stamp, name in re.findall(r"^\[\s*([\d.]+)s\] (.+)$", pair[j]["stderr"], re.M):
                    cur = float(stamp)
                    phases.setdefault(name.split(":")[0], []).append(cur - prev)
                    prev = cur
                phases.setdefault("outside internal timer", []).append(pair[j]["seconds"] - prev)
            print(label, ", ".join(f"{k}: {statistics.median(v):.3f} s" for k, v in phases.items()))
    if args.json:
        args.json.write_text(json.dumps(dict(baseline=str(bins[0]), candidate=str(bins[1]),
                                             data=str(args.data), threads=args.threads, pairs=pairs,
                                             medians=[base, test], environments=envs), indent=2) + "\n")


if __name__ == "__main__":
    main()
