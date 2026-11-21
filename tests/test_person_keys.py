"""Byte strings crossing the fast inline-ID boundary must round trip to Parquet."""
import unittest

from test_reference import ev, run_case, WS


class PersonKeys(unittest.TestCase):
    def test_roundtrip_and_min_across_encodings(self):
        # Include a would-be collision with zero padding and non-ASCII byte 8.
        names = ["a", "a\0", "a\0b", "abcdefgh", "abcdefghi", "abcdefghij",
                 "abcdefghé", "ß", "AReallyLongLinkedName"]
        users, _ = run_case(events=[ev(f"e{i}", n, WS) for i, n in enumerate(names)],
                            links=["AReallyLongLinkedName,B", "abcdefghij,abcdefghij!",
                                   "ß,Ω"])
        self.assertEqual({name for name, _ in users}, set(names))
        self.assertTrue(all(row["events"] == 1 for row in users.values()))
