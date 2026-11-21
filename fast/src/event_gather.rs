//! Canonical seven-column event rows: classify and gather from the same two
//! 64-byte registers. Usual IDs and timestamps never need a second mmap load.

use crate::{
    Codec, ManifestClock,
    csv::{self, Chunk},
};

#[derive(Clone, Copy)]
pub(super) enum Id<'a> {
    #[cfg(any(target_arch = "x86_64", test, feature = "event-gather-model"))]
    Inline(u64),
    Bytes(&'a [u8]),
}

impl<'a> Id<'a> {
    #[inline]
    pub fn key(self, codec: &Codec<'a>) -> u64 {
        match self {
            #[cfg(any(target_arch = "x86_64", test, feature = "event-gather-model"))]
            Self::Inline(key) => key,
            Self::Bytes(s) => codec.key(s),
        }
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        matches!(self, Self::Bytes([]))
    }
}

pub(super) struct Row<'a> {
    pub event: Id<'a>,
    pub user: Id<'a>,
    pub ts: i64,
    pub ingest: (i64, i64),
    pub kind: &'a [u8],
    pub product: &'a [u8],
    pub price: &'a [u8],
}

impl<'a> Row<'a> {
    #[inline]
    fn generic(r: &[&'a [u8]; 8], clock: &ManifestClock) -> Self {
        Self {
            event: Id::Bytes(r[0]),
            user: Id::Bytes(r[1]),
            ts: clock.parse_ts(r[2]),
            ingest: clock.ingest_for_claim(r[3]),
            kind: r[4],
            product: r[5],
            price: r[6],
        }
    }

    #[cfg(test)]
    fn normalized(&self) -> (Vec<u8>, Vec<u8>, i64, (i64, i64), Vec<u8>, Vec<u8>, Vec<u8>) {
        let decode = |id| match id {
            Id::Bytes(s) => s.to_vec(),
            Id::Inline(k) => Codec::new().bytes(k, &mut [0; 9]).to_vec(),
        };
        (
            decode(self.event),
            decode(self.user),
            self.ts,
            self.ingest,
            self.kind.to_vec(),
            self.product.to_vec(),
            self.price.to_vec(),
        )
    }
}

#[inline]
fn generic<'a>(c: &Chunk<'a>, clock: &ManifestClock, f: &mut impl FnMut(Row<'a>)) {
    csv::for_rows(c, |r| {
        if !r[0].is_empty() {
            f(Row::generic(r, clock));
        }
    });
}

#[inline]
pub(super) fn for_events<'a>(c: &Chunk<'a>, clock: &ManifestClock, mut f: impl FnMut(Row<'a>)) {
    #[cfg(any(target_arch = "x86_64", feature = "event-gather-model"))]
    if c.canonical_events() {
        #[cfg(feature = "event-gather-model")]
        return scan::<Model, _>(c, clock, &mut f);
        #[cfg(all(target_arch = "x86_64", not(feature = "event-gather-model")))]
        if x86::available() {
            // SAFETY: detected every required extension before entering the chunk.
            return unsafe { x86::scan_native(c, clock, &mut f) };
        }
    }
    generic(c, clock, &mut f);
}

#[cfg(any(target_arch = "x86_64", test, feature = "event-gather-model"))]
use kernel::*;

#[cfg(any(target_arch = "x86_64", test, feature = "event-gather-model"))]
mod kernel {
    use super::*;
    use memchr::memchr;

    pub(super) trait Kernel {
        type Block;
        fn load(data: &[u8; 128]) -> (Self::Block, u128, u128, u128);
        fn deposit(bits: u64, mask: u64) -> u64;
        fn extract(bits: u64, mask: u64) -> u64;
        fn gather(block: Self::Block, starts: [u8; 4], keep: u64, f3: bool) -> Gathered;
    }

    pub(super) struct Gathered {
        // Eight qwords in canonical order:
        // IDs[0..8] x2, date[0..8] x2, DDHHMMSS x2, T::Z + ID ninth bytes, T::Z.
        // F3 substitutes the high/low groups of eight digits for the event date / DDHHMMSS.
        pub words: [u64; 8],
        pub pairs: [u64; 2],
        pub digit_mask: u64,
        pub syntax_mask: u64,
    }

    const fn low(n: usize) -> u64 {
        if n >= 64 { !0 } else { (1 << n) - 1 }
    }

    pub(super) const F3_PAD: u64 = 7 << 24;
    // Byte indices into each copy of [event ID, user ID, ingest, event ts].
    pub(super) const BASES: [u64; 8] = [
        0,
        0x0101_0101_0101_0101,
        0x0202_0202_0202_0202,
        0x0303_0303_0303_0303,
        0x0202_0202_0202_0202,
        0x0303_0303_0303_0303,
        0x0000_0100_0202_0202,
        0x0303_0303_0303_0303,
    ];
    pub(super) const LAYOUT: [u64; 8] = [
        u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]),
        u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]),
        u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]),
        u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]),
        u64::from_le_bytes([8, 9, 11, 12, 14, 15, 17, 18]),
        u64::from_le_bytes([8, 9, 11, 12, 14, 15, 17, 18]),
        u64::from_le_bytes([10, 13, 16, 19, 8, 8, 0, 0]),
        u64::from_le_bytes([10, 13, 16, 19, 0, 0, 0, 0]),
    ];
    pub(super) const F3_LAYOUT: [u64; 8] = {
        let mut layout = LAYOUT;
        layout[3] = u64::from_le_bytes([0, 0, 0, 0, 1, 2, 3, 4]);
        layout[5] = u64::from_le_bytes([5, 6, 7, 8, 9, 10, 11, 12]);
        layout
    };
    pub(super) const SYNTAX: u64 = u64::from_le_bytes(*b"T::Z\0\0\0\0");

    #[inline(always)]
    fn extract128<K: Kernel>(bits: u128, mask: u128) -> u64 {
        let lo = mask as u64;
        // All calls select exactly seven bits, so the shift is never >= 64.
        K::extract(bits as u64, lo) | (K::extract((bits >> 64) as u64, (mask >> 64) as u64) << lo.count_ones())
    }

    #[inline(always)]
    fn select<K: Kernel>(commas: u128, nlo: usize, nth: usize) -> usize {
        if nth < nlo {
            K::deposit(1 << nth, commas as u64).trailing_zeros() as usize
        } else {
            64 + K::deposit(1 << (nth - nlo), (commas >> 64) as u64).trailing_zeros() as usize
        }
    }

    #[inline(always)]
    fn inline_id<K: Kernel>(lo: u64, ninth: u8, s: &[u8], a: usize, n: usize) -> Id<'_> {
        let hi = if n == 9 { ninth } else { 0 };
        let last = if n == 9 {
            ninth
        } else {
            (lo >> (8 * (n.wrapping_sub(1) & 7))) as u8
        };
        if (1..=9).contains(&n) && last != 0 && lo & 0x8080_8080_8080_8080 == 0 && hi < 128 {
            Id::Inline((K::extract(lo.swap_bytes(), 0x7f7f_7f7f_7f7f_7f7f) << 7) | u64::from(hi))
        } else {
            Id::Bytes(&s[a..a + n])
        }
    }

    #[inline(always)]
    fn f1(clock: &ManifestClock, g: &Gathered, event: usize) -> Option<i64> {
        if (g.digit_mask >> (32 + event * 8)) as u8 != 255 || (g.syntax_mask >> (48 + event * 8)) & 15 != 15 {
            return None;
        }
        clock.gathered_f1(g.words[2 + event], g.words[4 + event] as u16, g.pairs[event])
    }

    #[inline(always)]
    fn typed<'a, K: Kernel>(
        s: &'a [u8],
        clock: &ManifestClock,
        block: K::Block,
        end: usize,
        commas: u128,
        quotes: u128,
    ) -> Option<Row<'a>> {
        let nlo = (commas as u64).count_ones() as usize;
        if nlo + ((commas >> 64) as u64).count_ones() as usize != 6 {
            return None;
        }
        let wrapped = if quotes == 0 {
            0
        } else {
            let starts = (commas << 1) | 1;
            let ends = commas | (1 << end);
            let open = quotes & starts;
            let close = quotes & (ends >> 1);
            // No internal quotes and no single quote counted as both ends.
            if open & close != 0 || open | close != quotes {
                return None;
            }
            // Compress opening and closing endpoints to field indices. Matching
            // counts alone would accept an opening in one field and a close in another.
            let wrapped = extract128::<K>(open, starts);
            if wrapped != extract128::<K>(close << 1, ends) {
                return None;
            }
            wrapped
        };
        let [c0, c1, c2, c3, c4, c5] = [
            select::<K>(commas, nlo, 0),
            select::<K>(commas, nlo, 1),
            select::<K>(commas, nlo, 2),
            select::<K>(commas, nlo, 3),
            select::<K>(commas, nlo, 4),
            select::<K>(commas, nlo, 5),
        ];
        let q = |i: usize| ((wrapped >> i) & 1u64) as usize;
        let a = [
            q(0),
            c0 + 1 + q(1),
            c1 + 1 + q(2),
            c2 + 1 + q(3),
            c3 + 1 + q(4),
            c4 + 1 + q(5),
            c5 + 1 + q(6),
        ];
        let z = [
            c0 - q(0),
            c1 - q(1),
            c2 - q(2),
            c3 - q(3),
            c4 - q(4),
            c5 - q(5),
            end - q(6),
        ];
        let n0 = z[0] - a[0];
        if n0 == 0 {
            return None;
        }
        let n1 = z[1] - a[1];
        let n2 = z[2] - a[2];
        let n3 = z[3] - a[3];
        let f3 = n2 == 13;
        let keep = !0xffff | low(n0.min(8)) | (low(n1.min(8)) << 8);
        let keep = if f3 { keep & !F3_PAD } else { keep };
        let g = K::gather(block, [a[0] as u8, a[1] as u8, a[3] as u8, a[2] as u8], keep, f3);
        let ts = if f3 && g.digit_mask & (0xffu64 << 24 | 0xff << 40) == (0xff << 24 | 0xff << 40) {
            // The F3 kernel leaves two decimal quads in each of these words.
            let eight = |p: u64| (p as u32 as i64) * 10_000 + (p >> 32) as i64;
            eight(g.words[3]) * 100_000_000 + eight(g.pairs[1])
        } else if n2 == 20 {
            f1(clock, &g, 1).map_or_else(|| clock.parse_ts(&s[a[2]..z[2]]), |sec| sec * 1000)
        } else {
            clock.parse_ts(&s[a[2]..z[2]])
        };
        let ingest = if n3 == 20 { f1(clock, &g, 0) } else { None };
        let ingest = ingest.map_or_else(|| clock.ingest_for_claim(&s[a[3]..z[3]]), |sec| (sec * 1000, sec));
        Some(Row {
            event: inline_id::<K>(g.words[0], (g.words[6] >> 32) as u8, s, a[0], n0),
            user: inline_id::<K>(g.words[1], (g.words[6] >> 40) as u8, s, a[1], n1),
            ts,
            ingest,
            kind: &s[a[4]..z[4]],
            product: &s[a[5]..z[5]],
            price: &s[a[6]..z[6]],
        })
    }

    #[inline(always)]
    pub(super) fn scan<'a, K: Kernel, F: FnMut(Row<'a>)>(c: &Chunk<'a>, clock: &ManifestClock, f: &mut F) {
        let mut rest = c.data;
        while let Some(prefix) = rest.first_chunk::<128>() {
            let (regs, lf, commas, quotes) = K::load(prefix);
            if lf == 0 {
                // No newline in the registers. The existing scanner handles overlong
                // rows and final rows without LF; never load past the chunk boundary.
                let n = memchr(b'\n', &rest[128..]).map_or(rest.len(), |i| 129 + i);
                generic(&c.subchunk(&rest[..n]), clock, f);
                rest = &rest[n..];
                continue;
            }
            let end = lf.trailing_zeros() as usize;
            let before = (1u128 << end) - 1;
            if let Some(r) = typed::<K>(rest, clock, regs, end, commas & before, quotes & before) {
                f(r);
            }
            rest = &rest[end + 1..];
        }
        if !rest.is_empty() {
            generic(&c.subchunk(rest), clock, f);
        }
    }
}

#[cfg(any(test, feature = "event-gather-model"))]
mod model;
#[cfg(all(target_arch = "x86_64", not(feature = "event-gather-model")))]
mod x86;
#[cfg(any(test, feature = "event-gather-model"))]
use model::Model;
#[cfg(test)]
mod tests;
