"""Check a public generator input with long, NUL-containing UTF-8 IDs and reordered CSV.

    .venv/bin/python tools/opaque_diff.py vendor/dev fast/target/release/shopping_sessions

Builds fresh expected rows with the Python oracle. Retains the transformed case
under scratch/ on failure; removes it after successful single- and multi-worker runs.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "reference"))
import pipeline as reference

ID_COLS = {b"event_id", b"click_id", b"user_id", b"user_id_a", b"user_id_b", b"campaign_id", b"product_id"}
PREFIX = (b" \x00", "é ".encode(), b"\tLong ID/", "客".encode())


def rewrite(src, dst, index):
    with src.open("rb") as raw, dst.open("wb") as out:
        line = raw.readline().removesuffix(b"\n")
        header = reference.parse_line(line, line.count(b",") + 1)
        if not header:
            raise ValueError(f"{src}: expected a public-generator CSV header")
        order = list(reversed(range(len(header))))
        shift = index % len(order)
        order = order[shift:] + order[:shift]
        out.write(b",".join(header[j] for j in order))
        for n, line in enumerate(raw):
            row = line.removesuffix(b"\n")
            values = reference.parse_line(row, len(header))
            if values is not None:
                for j, col in enumerate(header):
                    v = values[j]
                    if col in ID_COLS and v:
                        # Identical across files and ID columns. Trailing NUL
                        # forces the lossless spill even on a very short ID.
                        values[j] = PREFIX[sum(v) % len(PREFIX)] + v + b"\0"
                row = b",".join(b'"' + values[j] + b'"' if (n + j) % 3 == 0 else values[j] for j in order)
            out.write(b"\n" + row)
        if index % 2 == 0:
            out.write(b"\n")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("data", type=lambda s: Path(s).resolve(strict=True))
    p.add_argument("binary", type=lambda s: Path(s).resolve(strict=True))
    args = p.parse_args()
    scratch = ROOT / "scratch"
    scratch.mkdir(exist_ok=True)
    case = Path(tempfile.mkdtemp(prefix="opaque-ids-", dir=scratch))
    data = case / "data"
    try:
        data.mkdir()
        shutil.copyfile(args.data / "manifest.json", data / "manifest.json")
        for i, name in enumerate(("products.csv", "campaigns.csv")):
            rewrite(args.data / name, data / name, i)
        for name in ("links", "clicks", "events"):
            (data / name).mkdir()
            for i, src in enumerate(sorted((args.data / name).glob("*.csv"), reverse=True)):
                rewrite(src, data / name / f"part-{i:06}.csv", i)
        expected = case / "expected"
        reference.run(str(data), str(expected))
        for workers in (1, 8):
            out = case / f"out-{workers}"
            env = dict(os.environ, DATA_DIR=str(data), OUTPUT_DIR=str(out), SCRATCH_DIR=str(case),
                       NCPU=str(workers), RAYON_NUM_THREADS=str(workers))
            cp = subprocess.run([args.binary], env=env, stdout=subprocess.PIPE, check=True)
            if cp.stdout:
                raise ValueError(f"unexpected stdout with {workers} workers")
            subprocess.run([sys.executable, ROOT / "tools/compare_outputs.py", expected, out], check=True)
            print(f"Opaque IDs: MATCH (workers={workers})", flush=True)
    except BaseException:
        print(f"Retained failed case: {case}", file=sys.stderr)
        raise
    shutil.rmtree(case)


if __name__ == "__main__":
    main()
