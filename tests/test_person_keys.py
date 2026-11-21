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

    def test_parallel_link_components_take_byte_minimum(self):
        # Each part crosses the others. The minimum is a long, unused name;
        # choosing an arbitrary union root would produce the wrong user_id.
        nodes = [f"node{i}" for i in range(2500)] + ["AA long canonical name"]
        extra = {f"links/shard-{s}.csv": [
            f"{nodes[i+1]},{nodes[i]}" for i in range(s, len(nodes)-1, 16)
        ] for s in range(16)}
        users, _ = run_case(events=[ev("one", "node0", WS), ev("two", "node2499", WS)],
                            links=["node0,node0", "node45,node44"], extra=extra)
        self.assertEqual([(r["user_id"], r["events"]) for r in users.values()],
                         [("AA long canonical name", 2)])
