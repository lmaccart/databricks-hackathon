//! Private to a group of person shards: no atomics and no hashing on the
//! bounded dense path. Wide windows/large catalogues use exact sparse keys.
use super::{DAY, Map, ORGANIC, ckey, rs};

#[derive(Clone, Copy)]
pub(super) struct Shape {
    day0: i32,
    width: usize,
    cells: usize,
}

impl Shape {
    pub(super) fn new(ws: i64, we: i64, ncamp: usize, groups: usize) -> Self {
        let lo = ws.div_euclid(DAY);
        let hi = we.saturating_sub(1).div_euclid(DAY);
        let width = ncamp + 1; // organic
        let product = (hi - lo + 1).max(0) as u128 * width as u128;
        // Bound ALL live dense accumulators together, not just one shard's.
        let limit = (96 << 20) / size_of::<[i64; 4]>() / (groups + 1);
        let cells = if product <= limit as u128 && i32::try_from(lo).is_ok() && i32::try_from(hi).is_ok() {
            product as usize
        } else {
            0
        };
        Self {
            day0: lo as i32,
            width,
            cells,
        }
    }
}

pub(super) struct Counts {
    shape: Shape,
    cells: Vec<[i64; 4]>,
    sparse: Map<u64, [i64; 4]>,
}

impl Counts {
    pub(super) fn new(shape: Shape) -> Self {
        Self {
            shape,
            cells: vec![[0; 4]; shape.cells],
            sparse: Map::with_hasher(rs()),
        }
    }

    #[inline]
    pub(super) fn at(&mut self, camp: u32, day: i32) -> &mut [i64; 4] {
        if self.cells.is_empty() {
            self.sparse.entry(ckey(camp, day)).or_default()
        } else {
            let c = if camp == ORGANIC {
                self.shape.width - 1
            } else {
                camp as usize
            };
            &mut self.cells[(day - self.shape.day0) as usize * self.shape.width + c]
        }
    }

    pub(super) fn merge(&mut self, other: &Self) {
        if self.cells.is_empty() {
            for (&k, v) in &other.sparse {
                let row = self.sparse.entry(k).or_default();
                for j in 0..4 {
                    row[j] += v[j];
                }
            }
        } else {
            for (row, v) in self.cells.iter_mut().zip(&other.cells) {
                for j in 0..4 {
                    row[j] += v[j];
                }
            }
        }
    }

    pub(super) fn rows(self) -> Vec<(u64, [i64; 4])> {
        if self.cells.is_empty() {
            return self.sparse.into_iter().collect();
        }
        let Shape { day0, width, .. } = self.shape;
        self.cells
            .into_iter()
            .enumerate()
            .filter_map(|(i, v)| {
                if v[0] == 0 && v[3] == 0 {
                    return None;
                }
                let c = i % width;
                let camp = if c + 1 == width { ORGANIC } else { c as u32 };
                Some((ckey(camp, day0 + (i / width) as i32), v))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dense_and_wide_sparse_emit_identical_assist_only_rows() {
        let build = |shape| {
            let mut t = Counts::new(shape);
            let mut other = Counts::new(shape);
            *other.at(ORGANIC, -1) = [1, 0, 0, 0];
            t.at(1, 1)[3] += 2;
            other.at(1, 1)[3] += 3;
            t.merge(&other);
            let mut rows = t.rows();
            rows.sort_unstable();
            rows
        };
        let dense = Shape::new(-DAY, 2 * DAY, 2, 4);
        let sparse = Shape::new(-10000 * DAY, 10000 * DAY, 1000, 32);
        assert!(dense.cells > 0 && sparse.cells == 0);
        assert_eq!(build(dense), build(sparse));
    }
}
