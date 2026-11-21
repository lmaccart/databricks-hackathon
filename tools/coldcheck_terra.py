"""Paired packaged-image timings with verified Linux page-cache residency."""
import ctypes
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import tempfile
from time import perf_counter

PAGESIZE = os.sysconf("SC_PAGE_SIZE")
libc = ctypes.CDLL(None, use_errno=True)
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int,
                      ctypes.c_int, ctypes.c_long]
libc.mmap.restype = ctypes.c_void_p
libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]
libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
FAILED_MAP = ctypes.c_void_p(-1).value


def residency(path):
    """Inspect all pages via a PROT_NONE mapping; never fault them in."""
    size = path.stat().st_size
    if not size:
        return 0, 0
    count = (size + PAGESIZE - 1) // PAGESIZE
    with path.open("rb") as f:
        addr = libc.mmap(None, size, 0, 1, f.fileno(), 0)  # PROT_NONE, MAP_SHARED
        if addr == FAILED_MAP:
            raise OSError(ctypes.get_errno(), f"mmap: {path}")
        try:
            vector = (ctypes.c_ubyte * count)()
            if libc.mincore(addr, size, vector):
                raise OSError(ctypes.get_errno(), f"mincore: {path}")
            return sum(b & 1 for b in vector), count
        finally:
            if libc.munmap(addr, size):
                raise OSError(ctypes.get_errno(), f"munmap: {path}")


def setup_cache(files, mode):
    if mode == "cold":
        for path in files:
            # Only these private generated input files are synchronized and evicted.
            with path.open("rb") as f:
                os.fsync(f.fileno())
                os.posix_fadvise(f.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
    else:
        for path in files:
            with path.open("rb") as f:
                while f.read(4 * 1024 * 1024):
                    pass
    present = total = 0
    worst = []
    for path in files:
        resident, pages = residency(path)
        present += resident
        total += pages
        if pages * PAGESIZE >= 1024 * 1024:
            worst.append((resident / pages, str(path.name), pages))
    if not total:
        raise RuntimeError("No data pages to check")
    ratio = present / total
    print(f"residency {mode}: {present}/{total} pages ({ratio:.3%}); "
          f"large-file range {min((x[0] for x in worst), default=ratio):.3%}"
          f"–{max((x[0] for x in worst), default=ratio):.3%}", flush=True)
    # Permit at most 0.1% residual globally AND 1% in any file >= 1 MiB.
    # If the host cannot keep the input warm, do not label that measurement warm.
    if mode == "cold" and (ratio > 0.001 or any(x[0] > 0.01 for x in worst)):
        raise RuntimeError("Cold precondition failed: input pages did not evict")
    if mode == "warm" and (ratio < 0.95 or any(x[0] < 0.90 for x in worst)):
        raise RuntimeError("Warm precondition failed: input pages are not resident")


def run_image(image, data, out, scratch):
    cmd = ["docker", "run", "--rm", "--platform", "linux/amd64",
           "--network", "none", "--read-only", "--user", "1000:1000",
           "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
           "--tmpfs", "/tmp:rw,size=64m,mode=1777", "--pids-limit", "4096",
           "--cpus", "4", "--memory", "14g", "--memory-swap", "14g",
           "-e", "DATA_DIR=/data", "-e", "OUTPUT_DIR=/output",
           "-e", "SCRATCH_DIR=/scratch", "-e", "HOME=/scratch",
           "-e", "NCPU=4", "-e", "MEM_GB=14",
           "-v", f"{data}:/data:ro", "-v", f"{out}:/output",
           "-v", f"{scratch}:/scratch", image]
    start = perf_counter()
    result = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    elapsed = perf_counter() - start
    if result.returncode or result.stdout:
        raise RuntimeError(f"{image}: exit={result.returncode} stdout={result.stdout!r} "
                           f"stderr={result.stderr.decode(errors='replace')}")
    return elapsed


def main():
    root, generator, python, compare = map(Path, sys.argv[1:])
    root = root.resolve(strict=True)
    print("Host page cache only; no device, hypervisor or remote-storage cache eviction. "
          "Docker CLI start-to-exit; image build, setup and output checking excluded.",
          flush=True)
    with tempfile.TemporaryDirectory(prefix="coldcheck-owned-", dir=root) as name:
        work = Path(name)
        work.chmod(0o755)
        for label, seed, events, clicks in (("20M", 7, 20_000_000, 5_000_000),
                                            ("60M", 13, 60_000_000, 15_000_000)):
            data = work / "data"
            subprocess.run([str(generator), "--seed", str(seed), "--events", str(events),
                            "--clicks", str(clicks), "--output", str(data)], check=True)
            files = sorted(p for p in data.rglob("*") if p.is_file() and not p.is_symlink())
            if not files or any(p.is_symlink() for p in data.rglob("*")):
                raise RuntimeError("Generated input is empty or contains symlinks")
            print(f"\n{label}: seed={seed} events={events} clicks={clicks} "
                  f"input bytes={sum(p.stat().st_size for p in files)} files={len(files)}",
                  flush=True)
            ref = work / "reference"
            ref.mkdir(mode=0o777)
            ref.chmod(0o777)
            scratch = work / "scratch"
            scratch.mkdir(mode=0o777)
            scratch.chmod(0o777)
            setup_cache(files, "warm")
            run_image("coldcheck:control", data, ref, scratch)
            results = {"cold": [[], []], "warm": [[], []]}
            for mode in ("cold", "warm"):
                for pair in range(3):
                    for side in ([0, 1] if pair % 2 == 0 else [1, 0]):
                        out = work / "output"
                        out.mkdir(mode=0o777)
                        out.chmod(0o777)
                        shutil.rmtree(scratch)
                        scratch.mkdir(mode=0o777)
                        scratch.chmod(0o777)
                        setup_cache(files, mode)
                        seconds = run_image(("coldcheck:control", "coldcheck:finalist")[side],
                                            data, out, scratch)
                        results[mode][side].append(seconds)
                        print(f"{label} {mode} pair={pair + 1} "
                              f"{('control', 'finalist')[side]} {seconds:.3f}s", flush=True)
                        env = dict(os.environ, POLARS_MAX_THREADS="4")
                        subprocess.run([str(python), str(compare), str(ref), str(out)],
                                       env=env, check=True)
                        shutil.rmtree(out)
            for mode, (a, b) in results.items():
                ma, mb = statistics.median(a), statistics.median(b)
                print(f"RESULT {label} {mode}: control={ma:.3f}s finalist={mb:.3f}s "
                      f"less={(1-mb/ma)*100:.1f}% pairs={len(a)}", flush=True)
            shutil.rmtree(data)
            shutil.rmtree(ref)
            shutil.rmtree(scratch)


if __name__ == "__main__":
    main()
