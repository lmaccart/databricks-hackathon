"""Verify fast-row hits and one-line fallbacks together against the independent Python oracle.

Every mutated event has a later, valid copy belonging to another shopper. This
makes dropping an invalid earliest claim, or keeping a malformed one, observable.
"""
import argparse
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

TS = b"2028-02-29T00:00:00Z"
MS = str(pipeline.parse_ts(TS)).encode()
E = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n"
C = b"click_id,user_id,campaign_id,ts\n"
L = b"user_id_a,user_id_b\n"
MUTATIONS = (b"", b'"', b'""', b",", b"\n", b"\r", b" ", b"\0", b"\x7f", "é".encode(), b"0", b"9", b"-")


def quoted(v, yes):
    return b'"' + v + b'"' if yes else v


def generate(root):
    for folder in ("events", "clicks", "links"):
        (root / folder).mkdir(parents=True)
    (root / "manifest.json").write_text(json.dumps({
        "window_start": "2028-02-28T00:00:00Z", "window_end": "2028-03-04T00:00:00Z"}))
    (root / "products.csv").write_bytes(b"product_id,category\nitem-7,a\nitem-07,b\n")
    (root / "campaigns.csv").write_bytes(b"campaign_id,channel\nc1,email\nc2,search\n")
    events, backups, clicks, links = [], [], [], []
    # Widths deliberately straddle the in-register ID, date, and suffix cutoffs.
    rng = random.Random(19)
    serial = 0
    for form in (TS, b"2028-02-28T19:00:00-05:00", MS, b"000" + MS):
        for kind, price in ((b"view", b""), (b"cart", b'bad'), (b"purchase", b"00250")):
            template = [b"User00001", form, TS, kind, b"item-7", price]
            # Include boundaries as mutation positions. Leave the guaranteed
            # ingest instant intact so equal-ingest copies never disagree.
            for col in (0, 1, 3, 4, 5):
                for pos in range(len(template[col]) + 1):
                    for mutation in MUTATIONS:
                        serial += 1
                        user = f"U{serial:08x}".encode()
                        backup = f"B{serial:08x}".encode()
                        eid = f"E{serial:08x}".encode()
                        fields = [user, *template[1:]]
                        body = fields[col]
                        fields[col] = body[:pos] + mutation + body[pos + (serial & 1):]
                        events.append(b",".join(quoted(v, rng.getrandbits(1)) for v in (eid, *fields)))
                        backups.append(b",".join((eid, backup, TS, b"2028-02-29T00:00:01Z", b"purchase", b"item-07", b"42")))
                        alias = f"A{serial:08x}".encode()
                        links.append(quoted(alias, serial % 3) + b"," + quoted(user, serial % 2))
                        cid = f"K{serial:08x}".encode()
                        clicks.append(b",".join((quoted(cid, serial % 3), quoted(alias, serial % 2), b"c1", MS)))
                        # Next physical row immediately tests recovery at a fresh valid start.
                        if serial % 17 == 0:
                            clicks.append(b",".join((b"J" + cid[1:], user, b"c2", TS)))
    # One ignored long price crosses an 8 MiB chunk boundary; an unterminated
    # quote after it must not consume the valid physical line that follows.
    large = (b"LongView1,U99999999," + TS + b"," + TS + b",view,item-7," +
             b"x" * (8 * 1024 * 1024 + 31))
    events[len(events) // 2:len(events) // 2] = [large, b'"dangling']
    rng.shuffle(backups)
    (root / "events" / "typed.csv").write_bytes(E + b"\n".join(events))
    (root / "events" / "backup.csv").write_bytes(E + b"\n".join(backups) + b"\n")
    (root / "clicks" / "c.csv").write_bytes(C + b"\n".join(clicks))
    (root / "links" / "l.csv").write_bytes(L + b"\n".join(links))
    return serial


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("binary", type=lambda s: Path(s).resolve(strict=True))
    args = p.parse_args()
    scratch = ROOT / "scratch"
    scratch.mkdir(exist_ok=True)
    case = Path(tempfile.mkdtemp(prefix="grammar-diff-", dir=scratch))
    try:
        n = generate(case / "data")
        pipeline.run(str(case / "data"), str(case / "expected"))
        for workers in (1, 8):
            out = case / f"actual-{workers}"
            env = dict(os.environ, DATA_DIR=str(case / "data"), OUTPUT_DIR=str(out), SCRATCH_DIR=str(case),
                       NCPU=str(workers), RAYON_NUM_THREADS=str(workers))
            cp = subprocess.run([args.binary], env=env, stdout=subprocess.PIPE, check=True)
            if cp.stdout:
                raise ValueError(f"unexpected stdout ({workers} workers)")
            subprocess.run([sys.executable, ROOT / "tools/compare_outputs.py", case / "expected", out], check=True)
            print(f"Grammar mutations: {n} competing copies matched ({workers} workers)", flush=True)
    except BaseException:
        print(f"Retained failed corpus: {case}", file=sys.stderr)
        raise
    shutil.rmtree(case)


if __name__ == "__main__":
    main()
