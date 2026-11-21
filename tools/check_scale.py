#!/usr/bin/env python3
"""Check scale output without loading all Parquet rows as Python objects.

Usage: python3 tools/check_scale.py OUTPUT_DIR
Requires polars and pyarrow. Exact key distinct counts use memory proportional
to the number of distinct keys. This self-check is not a reference-output diff.
"""
import argparse
import sys
from pathlib import Path

import polars as pl
import pyarrow as pa
import pyarrow.parquet as pq

from compare_outputs import TABLES


def scan_table(root, name):
    spec = TABLES[name]
    parts = []
    for path in sorted((root / name).glob("*.parquet")):
        if path.name.startswith((".", "_")) or path.is_symlink() or not path.is_file():
            continue
        schema = pq.read_schema(path)
        if sorted(schema.names) != sorted(spec["cols"]):
            raise ValueError(f"{path}: unexpected columns {schema.names}")
        columns = []
        for key in spec["cols"]:
            typ = schema.field(key).type
            if key == "day":
                ok, target = typ == pa.date32(), pl.Date
            elif key in spec["ints"]:
                ok, target = pa.types.is_integer(typ), pl.Int128
            else:
                string_type = typ.value_type if pa.types.is_dictionary(typ) else typ
                ok = pa.types.is_string(string_type) or pa.types.is_large_string(string_type)
                target = pl.String
            if not ok:
                raise ValueError(f"{path}: invalid type {typ} for {key}")
            columns.append(pl.col(key).cast(target))
        parts.append(pl.scan_parquet(path).select(columns))
    if not parts:
        raise ValueError(f"{root / name}: no Parquet parts")
    return pl.concat(parts)


def summarize(root, name):
    df = scan_table(root, name)
    spec = TABLES[name]
    c = pl.col
    invalid = pl.any_horizontal(c(k).is_null() for k in spec["cols"] if k != "top_category")
    metrics = [pl.len().alias("rows"), pl.struct(*spec["key"]).n_unique().alias("unique_keys")]
    metrics.extend(c(k).sum() for k in spec["ints"])
    if name == "campaign_daily":
        organic = c("campaign_id") == "organic"
        invalid |= organic & ((c("assisted_sessions") != 0) | (c("channel") != "organic"))
        invalid |= c("purchasing_sessions") > c("sessions")
        metrics.append(c("sessions").filter(~organic).sum().alias("paid"))
    else:
        invalid |= c("top_category").is_null() != (c("revenue_cents") == 0)
    stats = df.select(*metrics, invalid.sum().alias("invalid")).collect(engine="streaming")
    row = stats.row(0, named=True)
    print(name, row, flush=True)
    if row["rows"] != row["unique_keys"] or row["invalid"]:
        raise ValueError(f"{name}: duplicate keys or invalid rows (see totals)")
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output_dir", type=Path, help="parent of user_daily/ and campaign_daily/")
    root = parser.parse_args().output_dir
    u = summarize(root, "user_daily")
    c = summarize(root, "campaign_daily")
    for key in ("sessions", "revenue_cents"):
        if u[key] != c[key]:
            raise ValueError(f"unequal {key}: {u[key]} vs {c[key]}")
    if c["assisted_sessions"] < c["paid"]:
        raise ValueError("assist total is smaller than credited non-organic sessions")
    print("PASS")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, pl.exceptions.PolarsError) as exc:
        sys.exit(f"FAIL: {exc}")
