"""Check the Rust count-page boundary fixture with PyArrow and Polars.

Generate it with COUNT_TEST_OUT=$PWD/target/count-interop.parquet cargo test
count_tests, from fast/; then pass that file as this script's only argument.
"""
import sys

import polars as pl
import pyarrow.parquet as pq


def main(path):
    expected = [
        i % n
        for n in (1, 2, 3, 5, 9, 17, 33, 65, 129, 256, 257)
        for i in range(n * 3 + 1)
    ]
    expected.extend((-2**63, -1, 0, 254, 255, 256, 2**31 - 1, 2**63 - 1))
    expected.extend(i % 3 for i in range(16 * 1024))
    arrow = pq.read_table(path)["counts"].to_pylist()
    polars = pl.read_parquet(path)["counts"].to_list()
    assert arrow == expected == polars
    print(f"PyArrow and Polars: {len(expected)} exact counts")


if __name__ == "__main__":
    main(sys.argv[1])
