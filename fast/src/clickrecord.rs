//! A click keeps its complete ID even when all other fields share eight bytes.

use super::{Click, PCl, RankedClick};

#[derive(Clone, Copy)]
pub(super) struct ClickLayout {
    base: i64,
    shift: u32,
    rank_mask: u32,
}

impl ClickLayout {
    pub(super) fn new(base: i64, end: i64, ranks: usize, campaigns: usize) -> Option<Self> {
        u32::try_from(i128::from(end) - i128::from(base) - 1).ok()?;
        let bits = |n: usize| usize::BITS - n.saturating_sub(1).leading_zeros();
        let shift = bits(ranks);
        if shift + bits(campaigns) > 32 {
            return None;
        }
        Some(Self {
            base,
            shift,
            rank_mask: ((1u64 << shift) - 1) as u32,
        })
    }
}

pub(super) trait ClickData: Copy {
    type Layout: Copy;
    fn ts(self, layout: Self::Layout) -> i64;
    fn cid(self) -> u64;
    fn camp(self, layout: Self::Layout) -> u32;
}

pub(super) trait RangedClick: Copy {
    type Layout: Copy;
    type Local: ClickData<Layout = Self::Layout>;
    fn rank(self, layout: Self::Layout) -> u32;
    fn local(self) -> Self::Local;
}

#[derive(Clone, Copy)]
pub(super) struct SmallClick {
    cid: u64,
    offset: u32,
    group: u32,
}

impl SmallClick {
    #[inline]
    pub(super) fn encode(layout: ClickLayout, rank: u32, click: Click) -> Self {
        debug_assert!(rank <= layout.rank_mask);
        let group = (u64::from(click.camp) << layout.shift) | u64::from(rank);
        debug_assert!(group <= u64::from(u32::MAX));
        Self {
            cid: click.cid,
            offset: (click.ts - layout.base) as u32,
            group: group as u32,
        }
    }
}

impl ClickData for SmallClick {
    type Layout = ClickLayout;
    #[inline]
    fn ts(self, layout: ClickLayout) -> i64 {
        layout.base + i64::from(self.offset)
    }
    #[inline]
    fn cid(self) -> u64 {
        self.cid
    }
    #[inline]
    fn camp(self, layout: ClickLayout) -> u32 {
        (u64::from(self.group) >> layout.shift) as u32
    }
}

impl RangedClick for SmallClick {
    type Layout = ClickLayout;
    type Local = Self;
    #[inline]
    fn rank(self, layout: ClickLayout) -> u32 {
        self.group & layout.rank_mask
    }
    #[inline]
    fn local(self) -> Self {
        self
    }
}

impl ClickData for Click {
    type Layout = ();
    #[inline]
    fn ts(self, _: ()) -> i64 {
        self.ts
    }
    #[inline]
    fn cid(self) -> u64 {
        self.cid
    }
    #[inline]
    fn camp(self, _: ()) -> u32 {
        self.camp
    }
}

impl ClickData for PCl {
    type Layout = ();
    #[inline]
    fn ts(self, _: ()) -> i64 {
        self.click.ts
    }
    #[inline]
    fn cid(self) -> u64 {
        self.click.cid
    }
    #[inline]
    fn camp(self, _: ()) -> u32 {
        self.click.camp
    }
}

impl RangedClick for RankedClick {
    type Layout = ();
    type Local = Click;
    #[inline]
    fn rank(self, _: ()) -> u32 {
        self.rank
    }
    #[inline]
    fn local(self) -> Click {
        Click {
            ts: self.ts,
            cid: self.cid,
            camp: self.camp,
        }
    }
}

const _: () = assert!(std::mem::size_of::<SmallClick>() == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_layout_includes_pre_window_clicks_and_all_available_ranks() {
        let lo = -604_800_000;
        let end = lo + (1i64 << 32);
        let layout = ClickLayout::new(lo, end, 1 << 21, 2048).unwrap();
        for (rank, camp, id, ts) in [
            (0, 0, 0, lo),
            ((1 << 21) - 1, 2047, u64::MAX, end - 1),
            (121, 1241, 1u64 << 63, lo + 1),
        ] {
            let row = SmallClick::encode(layout, rank, Click { ts, cid: id, camp });
            assert_eq!(row.rank(layout), rank);
            assert_eq!((row.ts(layout), row.cid(), row.camp(layout)), (ts, id, camp));
        }
        assert!(ClickLayout::new(lo, end + 1, 1024, 7).is_none());
        assert!(ClickLayout::new(lo, end, (1 << 21) + 1, 2048).is_none());
        assert!(ClickLayout::new(lo, end, 1 << 21, 2049).is_none());
        assert!(ClickLayout::new(lo, lo, 1, 1).is_none());
        assert!(ClickLayout::new(i64::MIN + 1, i64::MAX, 1, 1).is_none());
    }
}
