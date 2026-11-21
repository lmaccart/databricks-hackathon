//! Keep the 256 physical buckets: replace low owner-shard bits with the
//! manifest-relative slab index, then give the whole owner group its clicks.
use super::{DAY, NS};

#[derive(Clone, Copy)]
pub(super) struct Slabs {
    origin: i64,
    reciprocal: u64,
    bits: u32,
}

impl Slabs {
    pub fn new(ws: i64, we: i64, workers: usize) -> Self {
        let mut result = Self {
            origin: ws,
            reciprocal: 0,
            bits: 0,
        };
        // Very wide or empty windows keep the original whole-owner path.
        let span = we.wrapping_sub(ws) as u64;
        if we <= ws || span > u64::from(u32::MAX) {
            return result;
        }
        // Aim for ~2 days per slab, with at most eight slabs and enough
        // independent final jobs to keep four owner groups per worker.
        let groups = workers.saturating_mul(4).clamp(1, NS).next_power_of_two();
        let count = (span.div_ceil(2 * DAY as u64) as usize)
            .next_power_of_two()
            .min(8)
            .min(NS / groups);
        if count > 1 {
            let width = span.div_ceil(count as u64);
            // Floor(offset / width) via the high half of a multiply. The
            // upward rounding error is < offset / 2^64 < 1 / width, since
            // the validated offset and width both fit in 32 bits.
            result.reciprocal = (1u128 << 64).div_ceil(u128::from(width)) as u64;
            result.bits = count.trailing_zeros();
        }
        result
    }

    pub fn count(self) -> usize {
        1 << self.bits
    }

    #[inline]
    pub fn group(self, owner: usize) -> usize {
        owner >> self.bits
    }

    #[inline]
    pub fn partition(self, owner: usize, ts: i64) -> usize {
        let offset = ts.wrapping_sub(self.origin) as u64;
        let slab = (u128::from(offset) * u128::from(self.reciprocal)) >> 64;
        (self.group(owner) << self.bits) | slab as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_boundaries_and_every_owner_fit_the_claim_byte() {
        let start = -234;
        // Deliberately not a multiple of eight; the last slab is shorter.
        let span = 14 * DAY + 3;
        let s = Slabs::new(start, start + span, 8);
        assert_eq!(s.count(), 8);
        let width = (span + 7) / 8;
        for owner in 0..NS {
            for i in 0..8 {
                let edge = start + i * width;
                let last = (edge + width).min(start + span) - 1;
                assert_eq!(s.partition(owner, edge), (owner & !7) | i as usize);
                assert_eq!(s.partition(owner, last), (owner & !7) | i as usize);
            }
        }
        assert_eq!(Slabs::new(start, start + span, 16).count(), 4);
        assert_eq!(Slabs::new(start, start + DAY, 8).count(), 1);
    }

    #[test]
    fn huge_windows_retain_the_full_owner_shard() {
        let s = Slabs::new(i64::MIN + 1, i64::MAX, 8);
        assert_eq!(s.count(), 1);
        assert_eq!(s.partition(255, i64::MIN + 1), 255);
        assert_eq!(s.partition(153, i64::MAX - 1), 153);
    }
}
