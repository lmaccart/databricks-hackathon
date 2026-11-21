use ahash::RandomState;
use std::collections::HashMap;

use super::{DAY, ORGANIC};

const DENSE_BUDGET: usize = 32 << 20;

#[derive(Clone, Copy)]
struct CreditTriple {
    cell: u32,
    counts: [i64; 3],
}

/// Per-fold campaign totals. Assists are dense because every active campaign
/// is touched per session; credited campaigns are sparse and use a direct
/// cell-to-triple index instead of hashing each session.
pub(super) struct DenseCampaigns {
    campaigns: usize,
    day_min: i32,
    days: usize,
    assists: Vec<i64>,
    credit_slot: Vec<u32>,
    credits: Vec<CreditTriple>,
}

impl DenseCampaigns {
    pub(super) fn new(campaigns: usize, day_min: i32, days: usize) -> Option<Self> {
        let cells = campaigns.checked_add(1)?.checked_mul(days)?;
        if days == 0 || cells > u32::MAX as usize {
            return None;
        }
        Some(Self {
            campaigns,
            day_min,
            days,
            assists: vec![0; cells],
            credit_slot: vec![u32::MAX; cells],
            credits: Vec::new(),
        })
    }

    #[inline]
    fn cell(&self, campaign: u32, day: i32) -> usize {
        let offset = (day as i64 - self.day_min as i64) as usize;
        debug_assert!(offset < self.days);
        let index = if campaign == ORGANIC {
            self.campaigns
        } else {
            debug_assert!((campaign as usize) < self.campaigns);
            campaign as usize
        };
        index * self.days + offset
    }

    #[inline]
    pub(super) fn add_credit(&mut self, campaign: u32, day: i32, purchasing: bool, revenue: i64) {
        let cell = self.cell(campaign, day);
        let slot = self.credit_slot[cell];
        let index = if slot == u32::MAX {
            let index = u32::try_from(self.credits.len()).expect("campaign/day credit count");
            self.credit_slot[cell] = index;
            self.credits.push(CreditTriple {
                cell: cell as u32,
                counts: [0; 3],
            });
            index as usize
        } else {
            slot as usize
        };
        let counts = &mut self.credits[index].counts;
        counts[0] += 1;
        counts[1] += purchasing as i64;
        counts[2] += revenue;
    }

    #[inline]
    pub(super) fn add_assist(&mut self, campaign: u32, day: i32) {
        debug_assert!(campaign < self.campaigns as u32);
        let cell = self.cell(campaign, day);
        self.assists[cell] += 1;
    }

    pub(super) fn merge(&mut self, other: &Self) {
        assert_eq!(
            (self.campaigns, self.day_min, self.days),
            (other.campaigns, other.day_min, other.days)
        );
        for (left, right) in self.assists.iter_mut().zip(&other.assists) {
            *left += right;
        }
        for credit in &other.credits {
            let cell = credit.cell as usize;
            let slot = self.credit_slot[cell];
            let index = if slot == u32::MAX {
                let index = u32::try_from(self.credits.len()).expect("campaign/day credit count");
                self.credit_slot[cell] = index;
                self.credits.push(CreditTriple {
                    cell: credit.cell,
                    counts: [0; 3],
                });
                index as usize
            } else {
                slot as usize
            };
            for i in 0..3 {
                self.credits[index].counts[i] += credit.counts[i];
            }
        }
    }

    pub(super) fn rows(&self) -> Vec<(u32, i32, [i64; 4])> {
        let mut rows = Vec::new();
        for cell in 0..self.assists.len() {
            let assisted = self.assists[cell];
            let slot = self.credit_slot[cell];
            if assisted == 0 && slot == u32::MAX {
                continue;
            }
            let mut counts = [0; 4];
            if slot != u32::MAX {
                counts[..3].copy_from_slice(&self.credits[slot as usize].counts);
            }
            counts[3] = assisted;
            let campaign = cell / self.days;
            rows.push((
                if campaign == self.campaigns {
                    ORGANIC
                } else {
                    campaign as u32
                },
                self.day_min + (cell % self.days) as i32,
                counts,
            ));
        }
        rows
    }
}

pub(super) struct MapCampaigns(HashMap<u64, [i64; 4], RandomState>);

impl MapCampaigns {
    pub(super) fn new() -> Self {
        Self(HashMap::with_hasher(super::rs()))
    }

    pub(super) fn add_credit(&mut self, campaign: u32, day: i32, purchasing: bool, revenue: i64) {
        let counts = self.0.entry(ckey(campaign, day)).or_insert([0; 4]);
        counts[0] += 1;
        counts[1] += purchasing as i64;
        counts[2] += revenue;
    }

    pub(super) fn add_assist(&mut self, campaign: u32, day: i32) {
        self.0.entry(ckey(campaign, day)).or_insert([0; 4])[3] += 1;
    }

    pub(super) fn merge(&mut self, other: &Self) {
        for (&key, counts) in &other.0 {
            let total = self.0.entry(key).or_insert([0; 4]);
            for i in 0..4 {
                total[i] += counts[i];
            }
        }
    }

    pub(super) fn rows(&self) -> Vec<(u32, i32, [i64; 4])> {
        self.0
            .iter()
            .map(|(&key, &counts)| ((key >> 32) as u32, key as u32 as i32, counts))
            .collect()
    }
}

pub(super) trait CampaignSink {
    fn credit(&mut self, campaign: u32, day: i32, purchasing: bool, revenue: i64);
    fn assist(&mut self, campaign: u32, day: i32);
}

impl CampaignSink for DenseCampaigns {
    #[inline]
    fn credit(&mut self, campaign: u32, day: i32, purchasing: bool, revenue: i64) {
        self.add_credit(campaign, day, purchasing, revenue);
    }

    #[inline]
    fn assist(&mut self, campaign: u32, day: i32) {
        self.add_assist(campaign, day);
    }
}

impl CampaignSink for MapCampaigns {
    #[inline]
    fn credit(&mut self, campaign: u32, day: i32, purchasing: bool, revenue: i64) {
        self.add_credit(campaign, day, purchasing, revenue);
    }

    #[inline]
    fn assist(&mut self, campaign: u32, day: i32) {
        self.add_assist(campaign, day);
    }
}

#[inline]
fn ckey(campaign: u32, day: i32) -> u64 {
    ((campaign as u64) << 32) | day as u32 as u64
}

/// Bound all fold-local dense tables, including the final reduction target and
/// worst-case sparse credit-vector growth. Huge arbitrary windows stay hashed.
pub(super) fn dense_span(campaigns: usize, window_start: i64, window_end: i64, workers: usize) -> Option<(i32, usize)> {
    if window_start >= window_end {
        return None;
    }
    let first_day = window_start.div_euclid(DAY);
    let last_day = (window_end as i128 - 1).div_euclid(DAY as i128);
    let days: usize = (last_day - first_day as i128 + 1).try_into().ok()?;
    let first_day: i32 = first_day.try_into().ok()?;
    let cells = campaigns.checked_add(1)?.checked_mul(days)?;
    let estimated_bytes = cells.checked_mul(workers.max(1).checked_add(1)?)?.checked_mul(
        std::mem::size_of::<i64>() + std::mem::size_of::<u32>() + 2 * std::mem::size_of::<CreditTriple>(),
    )?;
    if cells == 0 || cells > u32::MAX as usize || estimated_bytes > DENSE_BUDGET {
        return None;
    }
    Some((first_day, days))
}

#[cfg(test)]
mod tests {
    use super::{DenseCampaigns, ORGANIC};

    #[test]
    fn dense_accumulator_keeps_assist_only_rows_and_merges_credit_triples() {
        let mut left = DenseCampaigns::new(2, 10, 3).unwrap();
        left.add_credit(0, 10, true, 75);
        left.add_assist(1, 11);

        let mut right = DenseCampaigns::new(2, 10, 3).unwrap();
        right.add_credit(0, 10, false, 0);
        right.add_assist(1, 11);
        right.add_credit(ORGANIC, 12, true, 21);
        left.merge(&right);

        assert_eq!(
            left.rows(),
            vec![
                (0, 10, [2, 1, 75, 0]),
                (1, 11, [0, 0, 0, 2]),
                (ORGANIC, 12, [1, 1, 21, 0]),
            ]
        );
    }

    #[test]
    fn empty_dense_accumulator_has_no_campaign_rows() {
        assert!(DenseCampaigns::new(0, -2, 4).unwrap().rows().is_empty());
    }

    #[test]
    fn dense_dimensions_are_checked_and_can_fall_back() {
        assert!(DenseCampaigns::new(usize::MAX, 0, 2).is_none());
        assert!(DenseCampaigns::new(2, 0, usize::MAX).is_none());
    }

    #[test]
    fn dense_span_uses_actual_manifest_dates_and_bounds_large_windows() {
        use super::dense_span;

        let day = super::DAY;
        assert_eq!(dense_span(2, day + 1, day * 2, 8), Some((1, 1)));
        assert!(dense_span(100_000, 0, day * 14, 8).is_none());
        assert!(dense_span(2, 0, i64::MAX, 8).is_none());
    }
}
