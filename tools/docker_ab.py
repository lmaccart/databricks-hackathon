"""Alternating control/candidate full-Docker pairs on a Linux host.

Inputs live under --data-root/<name>; outputs go to fresh directories under
--out-root. Bind mounts replace the VM volumes of tools/vm_bench.py. Each timed run gets a fresh output directory. Input
page residency of the generated files only is set (warm: read; cold: fsync +
POSIX_FADV_DONTNEED) and then verified with mincore before the clock starts.
Every timed output is compared with the expected output directory afterwards.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import sys
import uuid
from time import perf_counter_ns

PAGESIZE = os.sysconf("SC_PAGE_SIZE")
libc = ctypes.CDLL(None, use_errno=True)
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int,
                      ctypes.c_int, ctypes.c_long]
libc.mmap.restype = ctypes.c_void_p
libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]
libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
FAILED = ctypes.c_void_p(-1).value
DATA = Path(os.environ.get("AB_DATA", "/data"))
OUT = Path(os.environ.get("AB_OUT", "/out"))
PHASE = re.compile(r"\[\s*([0-9.]+)s\] (.*)")


def residency(path):
    size = path.stat().st_size
    if not size:
        return 0, 0
    count = (size + PAGESIZE - 1) // PAGESIZE
    with path.open("rb") as f:
        addr = libc.mmap(None, size, 0, 1, f.fileno(), 0)  # PROT_NONE, MAP_SHARED: never faults
        if addr == FAILED:
            raise OSError(ctypes.get_errno(), str(path))
        try:
            vec = (ctypes.c_ubyte * count)()
            if libc.mincore(addr, size, vec):
                raise OSError(ctypes.get_errno(), str(path))
            return sum(b & 1 for b in vec), count
        finally:
            libc.munmap(addr, size)


def set_cache(files, mode):
    for p in files:
        with p.open("rb") as f:
            if mode == "cold":
                os.fsync(f.fileno())
                os.posix_fadvise(f.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
            else:
                while f.read(8 << 20):
                    pass
    present = total = 0
    for p in files:
        r, n = residency(p)
        present += r
        total += n
    ratio = present / total
    if mode == "cold" and ratio > 0.001:
        raise SystemExit(f"cold precondition failed: {present}/{total} resident")
    if mode == "warm" and ratio < 0.99:
        # A small runner may not keep the whole 60M input resident; record it.
        print(f"WARNING warm residency only {present}/{total} pages", flush=True)
    return present, total


def run(image, sub, ncpu, cpuset, mem):
    name = f"o-{uuid.uuid4().hex[:12]}"
    out = OUT / name
    out.mkdir()
    os.chown(out, 1000, 1000)
    cmd = ["docker", "run", "--rm", "--network", "none", "--read-only", "--user", "1000:1000",
           "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
           "--tmpfs", "/tmp:rw,size=64m,mode=1777", "--tmpfs", "/scratch:rw,size=64m,mode=1777",
           "--cpuset-cpus", cpuset, "--cpus", str(ncpu),
           "--memory", f"{mem}g", "--memory-swap", f"{mem}g",
           "-e", f"NCPU={ncpu}", "-e", "DATA_DIR=/data", "-e", "OUTPUT_DIR=/output",
           "-v", f"{DATA / sub}:/data:ro", "-v", f"{out}:/output", image]
    t0 = perf_counter_ns()
    r = subprocess.run(cmd, capture_output=True)
    secs = (perf_counter_ns() - t0) / 1e9
    if r.returncode or r.stdout:
        raise SystemExit(f"{image}: exit {r.returncode} stdout {len(r.stdout)}\n{r.stderr.decode()}")
    phases, prev = {}, 0.0
    for line in r.stderr.decode().splitlines():
        m = PHASE.search(line)
        if m:
            t = float(m.group(1))
            key = m.group(2).split(":")[0]
            phases[key] = round(t - prev, 4)
            prev = t
    phases["internal"] = prev
    return secs, phases, out


def compare(expected, actual):
    r = subprocess.run([sys.executable, str(Path(__file__).with_name("compare_outputs.py")), str(expected), str(actual)],
                       capture_output=True, text=True)
    if r.returncode:
        raise SystemExit(f"OUTPUT MISMATCH {actual}\n{r.stdout}\n{r.stderr}")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("control")
    p.add_argument("candidate")
    p.add_argument("--data", required=True)
    p.add_argument("--expected", required=True, help="name under AB_OUT of the expected output; made from the baseline if absent")
    p.add_argument("--pairs", type=int, default=5)
    p.add_argument("--mode", choices=["warm", "cold"], default="warm")
    p.add_argument("--ncpu", type=int, default=8)
    p.add_argument("--cpuset", default="0-7")
    p.add_argument("--mem", type=int, default=24)
    p.add_argument("--json")
    a = p.parse_args()
    files = sorted(x for x in (DATA / a.data).rglob("*") if x.is_file())
    expected = OUT / a.expected
    if not expected.exists():
        # The baseline's first output is the reference for every later run.
        set_cache(files, "warm")
        _, _, out = run(a.control, a.data, a.ncpu, a.cpuset, a.mem)
        out.rename(expected)
    images = {"control": a.control, "candidate": a.candidate}
    # One untimed warmup per image; it also proves both outputs match.
    for k in images:
        set_cache(files, "warm")
        _, _, out = run(images[k], a.data, a.ncpu, a.cpuset, a.mem)
        compare(expected, out)
        shutil.rmtree(out)
    rows = []
    for i in range(a.pairs):
        order = ["control", "candidate"] if i % 2 == 0 else ["candidate", "control"]
        pair = {}
        for k in order:
            res = set_cache(files, a.mode)
            secs, phases, out = run(images[k], a.data, a.ncpu, a.cpuset, a.mem)
            compare(expected, out)
            shutil.rmtree(out)
            pair[k] = {"seconds": secs, "phases": phases, "resident": res}
            print(f"pair {i} {k:9s} {a.mode} resident {res[0]}/{res[1]} "
                  f"{secs:.3f}s {phases}", flush=True)
        rows.append(pair)
    med = {k: statistics.median(r[k]["seconds"] for r in rows) for k in images}
    wins = sum(r["candidate"]["seconds"] < r["control"]["seconds"] for r in rows)
    ph = {}
    for k in images:
        keys = rows[0][k]["phases"].keys()
        ph[k] = {q: round(statistics.median(r[k]["phases"][q] for r in rows), 4) for q in keys}
    print(f"SUMMARY {a.data} {a.mode}: control {med['control']:.3f} -> candidate "
          f"{med['candidate']:.3f} ({(med['candidate'] / med['control'] - 1) * 100:+.1f}%), "
          f"candidate won {wins}/{a.pairs}; all outputs matched", flush=True)
    print(f"PHASES control {ph['control']}\nPHASES candidate {ph['candidate']}", flush=True)
    if a.json:
        Path(a.json).write_text(json.dumps({"rows": rows, "median": med, "wins": wins,
                                            "phases": ph, "args": vars(a)}, indent=1))


if __name__ == "__main__":
    main()
