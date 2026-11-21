//! Balanced ownership of multiple independent, line-aligned input chunks.
use crate::{CHUNK, NS, csv::Chunk};
use std::cmp::Reverse;
use std::collections::BinaryHeap;

const MAX_BYTES: usize = 64 << 20;
// Even if every payload shares one shard, the greatest encoded target is
// row_count * NS - 1. Keeping row_count <= floor(u32::MAX / NS) guarantees it
// is strictly smaller than NONE (u32::MAX), with no narrowing or wraparound.
pub const MAX_EVENT_ROWS: usize = u32::MAX as usize / NS;

pub struct Batch<'a> {
    pub chunks: Vec<Chunk<'a>>,
    pub row_bound: usize,
    bytes: usize,
}

// Each accepted seven-column event has at least six commas. Every line
// except possibly the final unterminated line also consumes a newline.
fn event_bound(bytes: usize, single_line: bool) -> usize {
    if single_line { 1 } else { bytes.saturating_add(1) / 7 }
}

pub fn group<'a>(chunks: &[Chunk<'a>], workers: usize, events: bool) -> Vec<Batch<'a>> {
    let mut pieces = Vec::new();
    for &c in chunks {
        if c.data.len() > CHUNK {
            // chunks_of extends only to the first newline after CHUNK, so the
            // suffix after the preceding newline is exactly one line.
            let split = memchr::memrchr(b'\n', &c.data[..CHUNK]).map_or(0, |p| p + 1);
            if c.data.len() - split > CHUNK {
                if split > 0 {
                    let (a, b) = c.split_at(split);
                    pieces.push((a, false));
                    pieces.push((b, true));
                } else {
                    pieces.push((c, true));
                }
                continue;
            }
        }
        pieces.push((c, false));
    }
    pieces.sort_unstable_by_key(|(c, _)| Reverse(c.data.len()));
    let total = pieces.iter().fold(0usize, |n, (c, _)| n.saturating_add(c.data.len()));
    let target = total.div_ceil(workers.max(1).saturating_mul(4)).clamp(1, MAX_BYTES);
    let mut groups: Vec<Batch<'a>> = Vec::new();
    let mut heap: BinaryHeap<Reverse<(usize, usize)>> = BinaryHeap::new();
    for (c, single) in pieces {
        let bytes = c.data.len();
        // An extended chunk can start only one line at or beyond CHUNK:
        // chunks_of stops at its first subsequent newline. Full preceding
        // accepted rows consume at least seven bytes each. Intersect that
        // nominal limit with the bound from the actual slice length.
        let bound = if events {
            event_bound(bytes, single).min(CHUNK / 7 + 1)
        } else {
            0
        };
        // A giant line is one row; all other input pieces are at most 2*CHUNK.
        assert!(!events || bound <= MAX_EVENT_ROWS);
        let mut held = Vec::new();
        let mut selected = None;
        while let Some(Reverse((used, i))) = heap.pop() {
            if used.saturating_add(bytes) > target {
                held.push(Reverse((used, i)));
                break;
            }
            if groups[i].row_bound.saturating_add(bound) <= MAX_EVENT_ROWS {
                selected = Some(i);
                break;
            }
            held.push(Reverse((used, i)));
        }
        heap.extend(held);
        let i = selected.unwrap_or_else(|| {
            groups.push(Batch {
                chunks: Vec::new(),
                row_bound: 0,
                bytes: 0,
            });
            groups.len() - 1
        });
        let g = &mut groups[i];
        g.bytes += bytes;
        g.row_bound += bound;
        g.chunks.push(c);
        // Oversized units cannot accept any further work.
        if g.bytes < target {
            heap.push(Reverse((g.bytes, i)));
        }
    }
    groups.sort_unstable_by_key(|g| Reverse(g.bytes));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csv::{chunks_of, for_rows};
    use std::path::Path;

    #[test]
    fn groups_preserve_files_headers_and_bound_locations() {
        let cols = [
            "event_id",
            "user_id",
            "ts",
            "ingest_ts",
            "event_type",
            "product_id",
            "price_cents",
        ];
        let a = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\na,u,t,i,view,p,\n";
        let b = b"price_cents,product_id,event_type,ingest_ts,ts,user_id,event_id\n,p,view,i,t,v,b\n";
        let chunks: Vec<_> = [a.as_slice(), b.as_slice()]
            .into_iter()
            .flat_map(|f| chunks_of(f, &cols, Path::new("test")))
            .collect();
        let groups = group(&chunks, 1, true);
        let mut rows = Vec::new();
        for g in &groups {
            assert!(g.row_bound <= MAX_EVENT_ROWS);
            for c in &g.chunks {
                for_rows(c, |r| rows.push((r[0].to_vec(), r[1].to_vec())));
            }
        }
        rows.sort();
        assert_eq!(rows, [(b"a".to_vec(), b"u".to_vec()), (b"b".to_vec(), b"v".to_vec())]);

        let repeated: Vec<_> = chunks.iter().copied().cycle().take(32).collect();
        let groups = group(&repeated, 1, true);
        assert!(groups.len() <= 5);
        assert_eq!(groups.iter().map(|g| g.chunks.len()).sum::<usize>(), 32);
        assert!(groups.iter().any(|g| {
            let mut ids = Vec::new();
            for c in &g.chunks {
                for_rows(c, |r| ids.push(r[0]));
            }
            ids.contains(&b"a".as_slice()) && ids.contains(&b"b".as_slice())
        }));
    }

    #[test]
    fn oversize_line_isolated_and_real_rows_preserved() {
        let mut f = b"a,b,c,d,e,f,g\nx,,,,,,\n".to_vec();
        f.extend(std::iter::repeat_n(b'x', crate::CHUNK + 10));
        f.extend_from_slice(b",,,,,,\ny,,,,,,\n");
        let chunks = chunks_of(&f, &["a", "b", "c", "d", "e", "f", "g"], Path::new("test"));
        let groups = group(&chunks, 2, true);
        let mut ids = Vec::new();
        for g in &groups {
            assert!(g.row_bound <= MAX_EVENT_ROWS);
            for c in &g.chunks {
                for_rows(c, |r| ids.push(r[0].len()));
            }
        }
        ids.sort();
        assert_eq!(ids, [1, 1, crate::CHUNK + 10]);
        assert!(
            groups
                .iter()
                .any(|g| g.chunks.iter().any(|c| c.data.len() > crate::CHUNK) && g.row_bound == 1)
        );
    }

    #[test]
    fn dense_bound_and_skew_are_safe() {
        assert_eq!(event_bound(6, false), 1);
        assert_eq!(event_bound(7 * MAX_EVENT_ROWS, false), MAX_EVENT_ROWS);
        assert!(event_bound(7 * (MAX_EVENT_ROWS + 1), false) > MAX_EVENT_ROWS);
        let rows = b"a,b,c,d,e,f,g\n"
            .iter()
            .copied()
            .chain(std::iter::repeat_n(b",,,,,,\n".as_slice(), 2000).flatten().copied())
            .collect::<Vec<_>>();
        let chunks = chunks_of(&rows, &["a", "b", "c", "d", "e", "f", "g"], Path::new("test"));
        let groups = group(&chunks, 8, true);
        assert_eq!(groups.iter().map(|g| g.row_bound).sum::<usize>(), 2000);
    }

    #[test]
    fn many_dense_chunks_fit_and_group_without_assuming_shard_balance() {
        let mut data = b"a,b,c,d,e,f,g\n".to_vec();
        data.extend(std::iter::repeat_n(b",,,,,,\n".as_slice(), 800_000).flatten().copied());
        let chunks = chunks_of(&data, &["a", "b", "c", "d", "e", "f", "g"], Path::new("test"));
        // Reuse one actual slice to simulate many full pieces, without hundreds
        // of megabytes of test data. All rows could route to the same shard.
        let inputs = vec![chunks[0]; 30];
        let groups = group(&inputs, 2, true);
        assert!(groups.len() < inputs.len());
        assert_eq!(groups.iter().map(|g| g.chunks.len()).sum::<usize>(), 30);
        for g in &groups {
            assert!(g.row_bound <= MAX_EVENT_ROWS);
            assert!((g.row_bound - 1) * NS + (NS - 1) < u32::MAX as usize);
            assert!(g.bytes <= MAX_BYTES);
        }
    }
}
