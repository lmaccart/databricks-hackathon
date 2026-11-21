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

    def test_compact_canonical_links_lengths_and_fallbacks(self):
        # This sample deliberately chooses x as the prefix without relying on
        # a generator spelling. Every linked alias must take its root's route.
        compact = ["x0", "x00", "x000000", "x0000000", "x00000000", "xzzzzzzzz"]
        events = [ev(f"event{i:03}", name, WS) for i, name in enumerate(compact)]
        events += [ev("eventA01", "xabcdefg", WS), ev("eventA02", "xabcdefh", WS),
                   ev("eventA03", "xabcdefi", WS), ev("eventA04", "Xabcdefg", WS),
                   ev("eventA05", "xabcdefghé", WS), ev("eventA06", "x1234567", WS),
                   ev("eventA07", "xabcdefg-long", WS),
                   ev("eventA08", "xabcdefg", WS, ingest="2026-10-01T00:00:01Z")]
        links = ["xabcdefg,xabcdefh", "xabcdefi,xabcdefi", ",x1234567",
                 "x000000,x000000", "x0000000,x0000000", "x00000000,x00000000",
                 "xabcdefg,xabcdefg-long"]
        extra = {"links/part-2.csv": ["xabcdefh,xabcdefi", "xabcdefghé,!unused-long-name"],
                 "events/part-1.csv": [ev("eventA08", "", WS)],
                 "clicks/part-2.csv": ["click02,xabcdefi,c_2,2026-10-01T00:00:00Z"]}
        clicks = ["click01,xabcdefh,c_1,2026-10-01T00:00:00Z"]
        users, camps = run_case(events=events, links=links, clicks=clicks, extra=extra)
        by_name = {name: row["events"] for (name, _), row in users.items()}
        self.assertEqual(by_name, {**{name: 1 for name in compact},
                                   "xabcdefg": 4, "Xabcdefg": 1,
                                   "!unused-long-name": 1, "x1234567": 1})
        self.assertEqual(camps[("c_1", "2026-10-01")]["sessions"], 1)
        self.assertEqual(camps[("c_2", "2026-10-01")]["assisted_sessions"], 1)
