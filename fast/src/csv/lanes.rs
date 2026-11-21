//! Positional event/click hot path. Four independent physical rows are in
//! flight, with the same field decoded at all four cursors before moving on.
use super::{Chunk, Native, for_rows, scan};
use crate::{Codec, ManifestClock, parse_price, person};
use memchr::memchr;

const LANES: usize = 4;
const POSITIONS: [u8; 7] = [0, 1, 2, 3, 4, 5, 6];

pub(crate) struct Id<'a> {
    bytes: &'a [u8],
    inline: u64,
}

impl<'a> Id<'a> {
    #[inline(always)]
    pub(crate) fn borrowed(bytes: &'a [u8]) -> Self {
        Self { bytes, inline: 0 }
    }

    #[inline(always)]
    pub(crate) fn key(&self, codec: &Codec<'a>) -> u64 {
        if self.inline != 0 {
            self.inline
        } else {
            codec.key(self.bytes)
        }
    }

    #[inline(always)]
    pub(crate) fn empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

pub(crate) struct Event<'a> {
    pub id: Id<'a>,
    pub user: Id<'a>,
    pub ts: i64,
    pub ingest: i64,
    pub price: Option<u32>,
    pub product: &'a [u8],
}

pub(crate) struct Click<'a> {
    pub id: Id<'a>,
    pub user: Id<'a>,
    pub ts: i64,
    pub campaign: &'a [u8],
}

type Raw<'a> = [&'a [u8]; 8];

pub(crate) fn events<'a>(c: &Chunk<'a>, clock: &ManifestClock, mut f: impl FnMut(Option<&Raw<'a>>, Option<Event<'a>>)) {
    let Some(split) = parts::<7>(c) else {
        return for_rows(c, |r| f(Some(r), None));
    };
    dispatched::<7, _, _>(split, &mut f, |mut rows, f| {
        // Keep the four delimiter dependency chains independent. The only
        // values that survive the packet are typed or borrowed from the map.
        let ids = stage!(rows, id);
        let users = stage!(rows, id);
        let ts = stage!(rows, field);
        let ingest = stage!(rows, field).map(|s| clock.lane_ingest(s));
        let ts = std::array::from_fn::<_, LANES, _>(|i| clock.lane_ts(ts[i], ingest[i].map(|(_, d)| d)));
        let ty = stage!(rows, field).map(|s| match s {
            b"view" | b"cart" => 0,
            b"purchase" => 1,
            _ => 2,
        });
        let products = stage!(rows, field);
        let price = std::array::from_fn::<_, LANES, _>(|i| match ty[i] {
            0 => Some(0),
            1 => parse_price(rows[i].field()),
            _ => None,
        });
        for (((((row, id), user), ts), ingest), (price, product)) in rows
            .into_iter()
            .zip(ids)
            .zip(users)
            .zip(ts)
            .zip(ingest)
            .zip(price.into_iter().zip(products))
        {
            match ingest.filter(|_| row.valid) {
                Some((ingest, _)) => f(
                    None,
                    Some(Event {
                        id,
                        user,
                        ts,
                        ingest,
                        product,
                        price,
                    }),
                ),
                None => raw::<7>(row.original, &mut |r| f(Some(r), None)),
            }
        }
    });
}

pub(crate) fn clicks<'a>(c: &Chunk<'a>, clock: &ManifestClock, mut f: impl FnMut(Option<&Raw<'a>>, Option<Click<'a>>)) {
    let Some(split) = parts::<4>(c) else {
        return for_rows(c, |r| f(Some(r), None));
    };
    dispatched::<4, _, _>(split, &mut f, |mut rows, f| {
        let ids = stage!(rows, id);
        let users = stage!(rows, id);
        let campaigns = stage!(rows, field);
        let times = stage!(rows, field);
        for ((((row, id), user), campaign), ts) in rows.into_iter().zip(ids).zip(users).zip(campaigns).zip(times) {
            if row.valid {
                f(
                    None,
                    Some(Click {
                        id,
                        user,
                        campaign,
                        ts: clock.lane_ts(ts, None),
                    }),
                );
            } else {
                raw::<4>(row.original, &mut |r| f(Some(r), None));
            }
        }
    });
}

// Explicitly unroll each stage, irrespective of LLVM's loop-size heuristic.
macro_rules! stage {
    ($rows:ident, $method:ident) => {
        [
            $rows[0].$method(),
            $rows[1].$method(),
            $rows[2].$method(),
            $rows[3].$method(),
        ]
    };
}
use stage;

#[inline]
fn parts<'a, const N: usize>(c: &Chunk<'a>) -> Option<[&'a [u8]; LANES]> {
    // Baseline x86_64 and other architectures retain the precise scanner.
    #[cfg(target_arch = "x86_64")]
    if !(std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("popcnt")) {
        return None;
    }
    if !cfg!(any(target_arch = "aarch64", target_arch = "x86_64"))
        || c.nfields != N
        || c.ncols != N
        || c.idx[..N].iter().copied().ne(0..N)
        || c.data.len() < 1024
    {
        return None;
    }
    let mut cuts = [0; LANES + 1];
    let stride = c.data.len() / LANES;
    for i in 1..LANES {
        let start = (i * stride).max(cuts[i - 1]);
        cuts[i] = start + memchr(b'\n', &c.data[start..])? + 1;
    }
    cuts[LANES] = c.data.len();
    Some(std::array::from_fn(|i| &c.data[cuts[i]..cuts[i + 1]]))
}

#[cold]
#[inline(never)]
fn raw<'a, const N: usize>(data: &'a [u8], f: &mut impl FnMut(&Raw<'a>)) {
    scan::<Native, _>(data, &POSITIONS[..N], f);
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "avx2,popcnt"))]
unsafe fn packets<'a, const N: usize>(lanes: &mut [&'a [u8]; LANES], mut packet: impl FnMut([Row<'a>; LANES])) {
    while lanes.iter().all(|r| r.len() >= 96) {
        packet([
            take::<N>(&mut lanes[0]),
            take::<N>(&mut lanes[1]),
            take::<N>(&mut lanes[2]),
            take::<N>(&mut lanes[3]),
        ]);
    }
}

#[inline]
fn dispatched<'a, const N: usize, T, F: FnMut(Option<&Raw<'a>>, Option<T>)>(
    mut lanes: [&'a [u8]; LANES],
    f: &mut F,
    mut packet: impl FnMut([Row<'a>; LANES], &mut F),
) {
    // SAFETY: parts checked ISA support. All vector loads are bounded by lane.
    unsafe { packets::<N>(&mut lanes, |rows| packet(rows, f)) };
    for lane in lanes {
        raw::<N>(lane, &mut |r| f(Some(r), None));
    }
}

struct Row<'a> {
    original: &'a [u8],
    bytes: &'a [u8; 96],
    lo: u64,
    hi: u64,
    start: u8,
    valid: bool,
}

#[inline(always)]
fn take<'a, const N: usize>(lane: &mut &'a [u8]) -> Row<'a> {
    let p = lane.as_ptr();
    // SAFETY: packet loop requires 96 readable bytes in each lane. The
    // second block is read only when there is no LF in the first.
    let (d, q, l) = unsafe { masks::<true>(p) };
    let (d, q, l) = if l != 0 {
        (d as u128, q as u128, l as u128)
    } else {
        let (hi, hq, hl) = unsafe { masks::<false>(p.add(64)) };
        (
            d as u128 | (hi as u128) << 64,
            q as u128 | (hq as u128) << 64,
            (hl as u128) << 64,
        )
    };
    let (len, valid, d) = if l == 0 {
        (memchr(b'\n', lane).map_or(lane.len(), |i| i + 1), false, 0)
    } else {
        let e = l & l.wrapping_neg();
        let q = q & (e - 1);
        let d = (d & (e - 1)) | e;
        let open = q & ((d << 1) | 1);
        let close = q & (d >> 1);
        // An opening quote adds a carry across its field's non-delimiter
        // bits. It must stop on exactly the delimiter after its closing
        // quote. A one-byte `"` cannot be both an opener and a closer.
        let valid = (open & close == 0)
            & (q == (open | close))
            & ((!d).wrapping_add(open) & d == close << 1)
            & (d.count_ones() as usize == N)
            // Short-ID path steps through the two low delimiters only.
            & ((d as u64 & (d as u64).wrapping_sub(1)) != 0);
        (e.trailing_zeros() as usize + 1, valid, d)
    };
    let original = &lane[..len];
    // Enough readable lookahead for the short-ID load, but never lend that
    // lookahead to Codec; a spill slice contains only the syntax-checked ID.
    // SAFETY: caller entered the packet with at least 96 readable bytes.
    let bytes = unsafe { &*p.cast::<[u8; 96]>() };
    *lane = &lane[len..];
    if valid {
        Row {
            original,
            bytes,
            lo: d as u64,
            hi: (d >> 64) as u64,
            start: 0,
            valid,
        }
    } else {
        Row {
            original,
            bytes: &[0; 96],
            lo: (1 << N) - 1,
            hi: 0,
            start: 0,
            valid,
        }
    }
}

impl<'a> Row<'a> {
    #[inline(always)]
    fn front(&mut self) -> &'a [u8] {
        debug_assert_ne!(self.lo, 0);
        let end = self.lo.trailing_zeros() as u8;
        self.lo &= self.lo.wrapping_sub(1);
        self.slice(end)
    }

    #[inline(always)]
    fn field(&mut self) -> &'a [u8] {
        if self.lo != 0 {
            self.front()
        } else {
            let end = (self.hi.trailing_zeros() + 64) as u8;
            self.hi &= self.hi.wrapping_sub(1);
            self.slice(end)
        }
    }

    #[inline(always)]
    fn slice(&mut self, end: u8) -> &'a [u8] {
        let start = usize::from(self.start);
        self.start = end + 1;
        // SAFETY: all fields are visited in order, at most N calls, with
        // exactly N terminators within the buffer. Quoted fields have >=2 bytes.
        unsafe {
            let quote = usize::from(*self.bytes.get_unchecked(start) == b'"');
            self.bytes.get_unchecked(start + quote..usize::from(end) - quote)
        }
    }

    #[inline(always)]
    fn id(&mut self) -> Id<'a> {
        let bytes = self.front();
        let len = bytes.len();
        let mut inline = 0;
        if (1..=9).contains(&len) && bytes[len - 1] != 0 {
            // SAFETY: front uses only positions below 64; the 96-byte
            // lookahead therefore holds all eight bytes even for a short ID.
            let p = self.bytes.as_ptr();
            let at = bytes.as_ptr() as usize - p as usize;
            let w = u64::from_be(unsafe { p.add(at).cast::<u64>().read_unaligned() });
            let pad = 8 - len.min(8);
            let w = w & (u64::MAX << (pad * 8));
            let tail = if len == 9 { bytes[8] } else { 0 };
            inline = person::pack(w, tail).unwrap_or(0);
        }
        Id { bytes, inline }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn masks<const WIDE: bool>(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    let a = unsafe { _mm256_loadu_si256(p.cast()) };
    let both = |c: u8| {
        let t = _mm256_set1_epi8(c as i8);
        let lo = _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, t)) as u32 as u64;
        if WIDE {
            let b = unsafe { _mm256_loadu_si256(p.add(32).cast()) };
            lo | ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(b, t)) as u32 as u64) << 32)
        } else {
            lo
        }
    };
    (both(b','), both(b'"'), both(b'\n'))
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn masks<const WIDE: bool>(p: *const u8) -> (u64, u64, u64) {
    use std::arch::aarch64::*;
    const BIT: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    unsafe {
        let bit = vld1q_u8(BIT.as_ptr());
        let [a, b] = [0, 16].map(|i| vld1q_u8(p.add(i)));
        let all = |ch: u8| {
            let ch = vdupq_n_u8(ch);
            let ab = vpaddq_u8(vandq_u8(vceqq_u8(a, ch), bit), vandq_u8(vceqq_u8(b, ch), bit));
            if WIDE {
                let [c, d] = [32, 48].map(|i| vld1q_u8(p.add(i)));
                let cd = vpaddq_u8(vandq_u8(vceqq_u8(c, ch), bit), vandq_u8(vceqq_u8(d, ch), bit));
                let h = vpaddq_u8(ab, cd);
                vgetq_lane_u64::<0>(vreinterpretq_u64_u8(vpaddq_u8(h, h)))
            } else {
                let h = vpaddq_u8(ab, ab);
                u64::from(vgetq_lane_u32::<0>(vreinterpretq_u32_u8(vpaddq_u8(h, h))))
            }
        };
        (all(b','), all(b'"'), all(b'\n'))
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
unsafe fn masks<const WIDE: bool>(_: *const u8) -> (u64, u64, u64) {
    unreachable!("lane parser is disabled on this architecture")
}
