"""Byte strings crossing the fast inline-ID boundary must round trip to Parquet."""
import unittest

from test_reference import ev, run_case, WS


class PersonKeys(unittest.TestCase):
    def test_coarse_owner_is_not_identity(self):
        # The two raw aliases ending in 00000 and 01847 share projection
        # slot (164, 1) but have different owners. The other three share
        # (164, 0), a homogeneous slot with two standalone false positives.
        first, second, third = "raw00000", "raw01847", "raw00336"
        s1, s2 = "raw00922", "raw13453"
        later = "2026-10-01T00:00:01Z"
        users, camps = run_case(
            links=[f"A-min,{first}", f"B-min,{second}", f"C-min,{third}", "A-min,other"],
            events=[ev("dup", second, later), ev("linked", second, WS),
                    ev("third", third, WS), ev("s1", s1, WS), ev("s2", s2, WS)],
            clicks=[f"k1,other,c_1,{WS}", f"k2,{s2},c_2,{WS}"],
            extra={"events/part-1.csv": [ev("dup", first, WS, "purchase", "p1", "19"),
                                          ev("s1-again", s1, later)]},
        )
        self.assertEqual({k[0]: r["events"] for k, r in users.items()},
                         {"A-min": 1, "B-min": 1, "C-min": 1, s1: 2, s2: 1})
        self.assertEqual({k[0]: (r["sessions"], r["revenue_cents"]) for k, r in camps.items()},
                         {"c_1": (1, 19), "c_2": (1, 0), "organic": (3, 0)})

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
