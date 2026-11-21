//! Compress a sampled heavy person's surviving events into exact minute extrema.
use super::stream::{Packing, Temporal};

const MINUTE: u64 = 60_000;
const MAX_BINS: usize = 65_536;
const MIN_EVENTS: usize = 4096;
const SAMPLE: usize = 1024;

#[derive(Clone, Copy, Default)]
struct Cell<T> {
    min: T,
    max: T,
    count: usize,
}

pub(super) struct Bins<T> {
    pub rank: u64,
    origin: i64,
    cells: Vec<Cell<T>>,
}

impl<T: Temporal> Bins<T> {
    #[inline]
    pub fn weight(&self, key: T, layout: T::Layout) -> usize {
        let bin = (key.ts(layout) as u64).wrapping_sub(self.origin as u64) / MINUTE;
        let cell = &self.cells[bin as usize];
        if key == cell.min { cell.count } else { 0 }
    }
}

pub(super) fn compress(keys: &mut Vec<u64>, pack: Packing, origin: i64, span: u64) -> Option<Bins<u64>> {
    let bins = usize::try_from(span / MINUTE + 1).ok()?;
    if bins > MAX_BINS || keys.len() < MIN_EVENTS {
        return None;
    }
    // Only the sampled IDs are sorted; all decisions use exact rank equality.
    let n = keys.len().min(SAMPLE);
    let mut sample: Vec<u64> = (0..n).map(|i| keys[i * keys.len() / n].person(pack)).collect();
    sample.sort_unstable();
    let (mut rank, mut best, mut start) = (0, 0, 0);
    while start < n {
        let mut end = start + 1;
        while end < n && sample[end] == sample[start] {
            end += 1;
        }
        if end - start > best {
            (rank, best) = (sample[start], end - start);
        }
        start = end;
    }
    if best < n / 8 {
        return None;
    }
    // Exact observed count determines whether direct cells are worthwhile.
    let count = keys.iter().filter(|&&key| key.person(pack) == rank).count();
    if count < MIN_EVENTS || count < bins / 8 {
        return None;
    }
    let mut cells = vec![Cell::<u64>::default(); bins];
    let mut occupied = vec![0u64; bins.div_ceil(64)];
    keys.retain(|&key| {
        if key.person(pack) != rank {
            return true;
        }
        let bin = (key.ts(pack) as u64).wrapping_sub(origin as u64) / MINUTE;
        let cell = &mut cells[bin as usize];
        if cell.count == 0 {
            cell.min = key;
            cell.max = key;
            occupied[bin as usize / 64] |= 1 << (bin % 64);
        } else {
            cell.min = cell.min.min(key);
            cell.max = cell.max.max(key);
        }
        cell.count += 1;
        false
    });
    for (word_index, mut word) in occupied.into_iter().enumerate() {
        while word != 0 {
            let bit = word.trailing_zeros() as usize;
            let cell = cells[word_index * 64 + bit];
            keys.push(cell.min);
            if cell.max != cell.min {
                keys.push(cell.max);
            }
            word &= word - 1;
        }
    }
    Some(Bins { rank, origin, cells })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions(mut keys: Vec<u64>, pack: Packing, bins: Option<&Bins<u64>>) -> Vec<(u64, i64, i64, usize)> {
        keys.sort_unstable();
        let mut out = Vec::new();
        let mut i = 0;
        while i < keys.len() {
            let rank = keys[i].person(pack);
            let start = keys[i].ts(pack);
            let mut end = start;
            let mut count = 0;
            while i < keys.len() && keys[i].person(pack) == rank && keys[i].ts(pack) - end <= super::super::GAP {
                end = keys[i].ts(pack);
                count += match bins {
                    Some(b) if b.rank == rank => b.weight(keys[i], pack),
                    _ => 1,
                };
                i += 1;
            }
            out.push((rank, start, end, count));
        }
        out
    }

    #[test]
    fn extrema_preserve_same_bin_bridges_gaps_and_counts() {
        let origin = -100_000;
        let span = 14 * 86_400_000 - 1;
        let pack = Packing::new(origin, origin + span + 1, 3).unwrap();
        let mut keys: Vec<_> = (0..5000)
            .map(|i| pack.encode(1, origin + if i == 0 { 59_999 } else { (i * 13) % 60_000 }))
            .collect();
        keys.extend([
            pack.encode(1, origin + 59_999 + 1_800_000),
            pack.encode(1, origin + 59_999 + 3_600_001),
            pack.encode(2, origin + 5),
        ]);
        keys.reverse();
        let expected = sessions(keys.clone(), pack, None);
        let bins = compress(&mut keys, pack, origin, span as u64).expect("heavy person must use bins");
        assert_eq!(bins.rank, 1);
        assert_eq!(sessions(keys, pack, Some(&bins)), expected);
        assert_eq!(expected.len(), 3);
    }

    #[test]
    fn enormous_window_and_sparse_people_fall_back() {
        let origin = 0;
        let span = MAX_BINS as i64 * MINUTE as i64;
        let pack = Packing::new(origin, span + 1, 2).unwrap();
        let mut keys = vec![pack.encode(1, 1); MIN_EVENTS];
        assert!(compress(&mut keys, pack, origin, span as u64).is_none());
        assert_eq!(keys.len(), MIN_EVENTS);
        let pack = Packing::new(0, 100_000, 2).unwrap();
        keys = vec![pack.encode(1, 1); MIN_EVENTS - 1];
        assert!(compress(&mut keys, pack, 0, 99_999).is_none());
        assert_eq!(keys.len(), MIN_EVENTS - 1);
    }

    #[test]
    fn shuffled_distributions_match_full_sort() {
        let origin = -86_400_000;
        let span = 2 * 86_400_000 - 1;
        let pack = Packing::new(origin, origin + span + 1, 3).unwrap();
        for seed in 1..=50u64 {
            let mut state = seed;
            let mut random = || {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                state
            };
            let mut keys: Vec<_> = (0..6000)
                .map(|_| {
                    let cluster = random() % 30;
                    let offset = random() % 120_000;
                    pack.encode(1, origin + (cluster * 2_000_001 + offset) as i64)
                })
                .collect();
            keys.extend((0..100).map(|_| pack.encode(2, origin + (random() % 86_400_000) as i64)));
            let expected = sessions(keys.clone(), pack, None);
            let bins = compress(&mut keys, pack, origin, span as u64).expect("hot rank must use bins");
            assert_eq!(sessions(keys, pack, Some(&bins)), expected, "seed {seed}");
        }
    }
}
