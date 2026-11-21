"""Measure host launch, Docker Engine lifetime, and pipeline timers on the same invocation."""
import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import re
from shutil import copyfile
import statistics
import subprocess
from tempfile import TemporaryDirectory
from time import perf_counter_ns


def header_only(src, dest):
    dest.mkdir()
    copyfile(src / "manifest.json", dest / "manifest.json")
    for role in ("events", "clicks", "links", "products", "campaigns"):
        if (src / role).is_dir():
            (dest / role).mkdir()
            files = sorted((src / role).glob("*.csv"))
            target = dest / role / "part.csv"
        else:
            files, target = [src / (role + ".csv")], dest / (role + ".csv")
        for f in files:
            with f.open("rb") as source:
                line = source.readline()
            if line:
                target.write_bytes(line)
                break


def run_once(adapter, binary, data, image, threads, work):
    env = dict(os.environ, SS_BENCH_IMAGE=image, DATA_DIR=str(data), NCPU=str(threads),
               RAYON_NUM_THREADS=str(threads))
    result = {}
    with TemporaryDirectory(dir=work) as raw:
        d = Path(raw)
        for name, program in (("native", binary), ("docker", adapter)):
            out, scratch = d / (name + "-out"), d / (name + "-scratch")
            out.mkdir()
            scratch.mkdir()
            env.update(OUTPUT_DIR=str(out), SCRATCH_DIR=str(scratch))
            cidfile = d / "container.id"
            if name == "docker":
                env["SS_BENCH_CIDFILE"] = str(cidfile)
            try:
                start = perf_counter_ns()
                cp = subprocess.run([program], env=env, capture_output=True, check=True)
                result[name] = (perf_counter_ns() - start) / 1e9
                if cp.stdout or not all(any((out / t).glob("*.parquet")) for t in ("user_daily", "campaign_daily")):
                    raise RuntimeError(f"{name}: missing output or unexpected stdout")
                if name == "docker":
                    cid = cidfile.read_text().strip()
                    state = json.loads(subprocess.check_output(
                        ["docker", "inspect", "--format", "{{json .State}}", cid]))
                    a, b = (datetime.fromisoformat(state[k].replace("Z", "+00:00"))
                            for k in ("StartedAt", "FinishedAt"))
                    result["engine"] = (b - a).total_seconds()
                    result["written"] = float(re.search(rb"\[\s*([\d.]+)s\] written", cp.stderr)[1])
            finally:
                if name == "docker" and cidfile.exists():
                    subprocess.run(["docker", "rm", "-f", cidfile.read_text().strip()], check=True,
                                   stdout=subprocess.DEVNULL)
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for arg in ("adapter", "binary", "data", "work"):
        p.add_argument(arg, type=lambda s: Path(s).resolve(strict=True))
    p.add_argument("image")
    p.add_argument("threads", type=int)
    args = p.parse_args()
    with TemporaryDirectory(dir=args.work) as case:
        empty = Path(case) / "headers"
        header_only(args.data, empty)
        for label, data in (("headers", empty), ("practice", args.data)):
            samples = [run_once(args.adapter, args.binary, data, args.image, args.threads, args.work)
                       for _ in range(6)][1:]  # warmup excluded
            vals = ", ".join(f"{k}={statistics.median(row[k] for row in samples):.4f}s"
                             for k in samples[0])
            print(f"clock-budget: {label}, five medians: {vals}", flush=True)


if __name__ == "__main__":
    main()
