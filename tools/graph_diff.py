"""Compare contended singleton slots and cross-file link graphs to the Python oracle."""
import argparse
from collections import defaultdict
import json
import os
from pathlib import Path
import random
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "reference"))
import pipeline

EVENT_HEADER = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n"
CLICK_HEADER = b"click_id,user_id,campaign_id,ts\n"
LINK_HEADER = b"user_id_a,user_id_b\n"
TS = b"2028-02-29T00:00:00Z"


def colliders():
    """Aim test IDs at a few homes in the small slot table. Identity is still bytewise."""
    buckets = defaultdict(list)
    for n in range(100_000):
        name = f"s{n:08x}".encode()
        key = sum(c << (7 * i) for i, c in enumerate(reversed(name)))
        h = ((key ^ (key >> 27)) * 0xa24baed4963ee407) & ((1 << 64) - 1)
        home = (h ^ (h >> 33)) & 255
        if home in (0, 1, 127, 250, 255) and len(buckets[home]) < 30:
            buckets[home].append(name)
        if len(buckets) == 5 and all(len(v) == 30 for v in buckets.values()):
            return list(buckets.values())
    raise AssertionError("insufficient collision groups")


def write_parts(root, folder, header, lines, rng):
    base = root / folder
    base.mkdir()
    rng.shuffle(lines)
    for i in range(9):
        (base / f"part-{i}.csv").write_bytes(header + b"\n".join(lines[i::9]) + b"\n")


def generate(root, large):
    root.mkdir()
    rng = random.Random(408 if large else 407)
    (root / "products.csv").write_bytes(b"product_id,category\np1,one\np2,two\n")
    (root / "campaigns.csv").write_bytes(b"campaign_id,channel\nc1,email\nc2,ads\n")
    links, people = [], []
    for group, family in enumerate(colliders()):
        name = b"\0unused/" + str(group).encode() + "客".encode()
        links.extend([(family[0], name), (family[1], family[0]), (family[1], family[1])])
        people.extend(family)
    if large:
        # The canonical spelling never has a row. Endpoints start in many
        # components and are later joined across files by narrow bridges.
        for group in range(4):
            chain = [f"g{group}{i:07x}".encode() for i in range(10_000)]
            rng.shuffle(chain)
            edges = list(zip(chain, chain[1:]))
            links.extend(edges)
            links.extend((b, a) for a, b in edges)
            links.append((b"\0" + str(group).encode() + b"-unused\0", chain[-1]))
            # More than 4,096 events per component exercises the bin reducer;
            # rules still require the exact 30-minute comparison on both edges.
            people.extend(chain[1::6])
    else:
        # More independent people than statically available ranks.
        people.extend(f"extra-{i}".encode() for i in range(1024))
    write_parts(root, "links", LINK_HEADER, [a + b"," + b for a, b in links], rng)
    events, clicks = [], []
    t = pipeline.parse_ts(TS)
    for i, person in enumerate(people):
        for j, delta in enumerate((0, 1_800_000, 3_600_001, 90_000_000)):
            eid = f"e{i:06x}{j}".encode()
            tick = str(t + delta).encode()
            pid = b"p1" if (i + j) % 2 else b"p2"
            events.append(b",".join([eid, person, tick, TS, b"purchase", pid, str(1 + i % 997).encode()]))
        for j, (delta, camp) in enumerate(((-604_800_000, b"c1"), (0, b"c2"), (0, b"c1"), (1_800_001, b"c1"))):
            click = b",".join((f"k{i:06x}{j}".encode(), person, camp, str(t + delta).encode()))
            clicks.extend([click, click])
    write_parts(root, "events", EVENT_HEADER, events, rng)
    write_parts(root, "clicks", CLICK_HEADER, clicks, rng)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=lambda v: Path(v).resolve(strict=True))
    binary = parser.parse_args().binary
    scratch = ROOT / "scratch"
    scratch.mkdir(exist_ok=True)
    case = Path(tempfile.mkdtemp(prefix="graph-diff-", dir=scratch))
    try:
        for graph in ("colliding", "transitive"):
            data = case / graph
            generate(data, graph == "transitive")
            for width, bounds in (
                ("short", ("2028-02-28", "2028-03-04")),
                ("wide", ("0002-01-01", "9999-01-01")),
            ):
                start, end = bounds
                (data / "manifest.json").write_text(json.dumps({
                    "window_start": start + "T00:00:00Z",
                    "window_end": end + "T00:00:00Z",
                }))
                expected = case / f"{graph}-{width}-oracle"
                pipeline.run(str(data), str(expected))
                for workers in (1, 8):
                    out = case / f"{graph}-{width}-{workers}"
                    env = dict(os.environ, DATA_DIR=str(data), OUTPUT_DIR=str(out), SCRATCH_DIR=str(case),
                               NCPU=str(workers), RAYON_NUM_THREADS=str(workers))
                    run = subprocess.run([binary], env=env, stdout=subprocess.PIPE, check=True)
                    if run.stdout:
                        raise ValueError("unexpected stdout")
                    subprocess.run([sys.executable, ROOT / "tools/compare_outputs.py", expected, out], check=True)
                    print(f"Graph stress: {graph}, {width} window, {workers} workers: MATCH", flush=True)
    except BaseException:
        print(f"Retained failing case: {case}", file=sys.stderr)
        raise
    shutil.rmtree(case)


if __name__ == "__main__":
    main()
