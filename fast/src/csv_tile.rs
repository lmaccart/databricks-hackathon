//! Erase CSV syntax into L1, then parse fixed records from the resulting bytes.
use super::{Chunk, for_rows};
use crate::person::{Codec, inline_padded};
use memchr::{memchr, memrchr};

const TILE: usize = 4096;
const BLOCK: usize = 64;

pub(crate) trait Fields<'a> {
    fn bytes(&self, i: usize) -> &[u8];
    fn key(&self, i: usize, codec: &Codec<'a>) -> u64;
}

impl<'a> Fields<'a> for &[&'a [u8]; 8] {
    #[inline(always)]
    fn bytes(&self, i: usize) -> &[u8] {
        self[i]
    }

    #[inline(always)]
    fn key(&self, i: usize, codec: &Codec<'a>) -> u64 {
        codec.key(self[i])
    }
}

pub(crate) enum Row<'a, 't, const N: usize> {
    Raw(&'t [&'a [u8]; 8]),
    Compact(Compact<'a, 't, N>),
}

pub(crate) struct Compact<'a, 't, const N: usize> {
    raw: &'a [u8],
    tile: &'t Tile,
    first: usize,
    ends: &'t [u16; N],
    pext: bool,
}

impl<const N: usize> Compact<'_, '_, N> {
    #[inline(always)]
    fn span(&self, i: usize) -> (usize, usize) {
        let start = if i == 0 {
            self.first
        } else {
            usize::from(self.ends[i - 1]) + 1
        };
        (start, usize::from(self.ends[i]))
    }
}

impl<'a, const N: usize> Fields<'a> for Compact<'a, '_, N> {
    #[inline(always)]
    fn bytes(&self, i: usize) -> &[u8] {
        let (s, e) = self.span(i);
        // SAFETY: the emitter found these increasing separators in the tile.
        unsafe { self.tile.bytes.get_unchecked(s..e) }
    }

    #[inline(always)]
    fn key(&self, i: usize, codec: &Codec<'a>) -> u64 {
        let (s, e) = self.span(i);
        // SAFETY: the initialized tile has at least 8 readable bytes at s
        // even for fields touching the end of the compacted data.
        if let Some(key) = unsafe { inline_padded(self.tile.bytes.as_ptr().add(s), e - s, self.pext) } {
            key
        } else if s == e {
            0
        } else {
            self.spill(s, e - s, codec)
        }
    }
}

impl<'a, const N: usize> Compact<'a, '_, N> {
    #[cold]
    #[inline(never)]
    fn spill(&self, start: usize, len: usize, codec: &Codec<'a>) -> u64 {
        // An accepted field has no internal quotes, so only its first byte
        // needs select(). Recover a slice into the mmap, never into scratch.
        let block = self.tile.starts.partition_point(|&p| usize::from(p) <= start) - 1;
        let mut keep = self.tile.keep[block];
        for _ in usize::from(self.tile.starts[block])..start {
            keep &= keep - 1;
        }
        let lo = block * BLOCK + keep.trailing_zeros() as usize;
        codec.key(&self.raw[lo..lo + len])
    }
}

struct Tile {
    bytes: [u8; TILE + BLOCK],
    starts: [u16; TILE / BLOCK],
    keep: [u64; TILE / BLOCK],
}

#[inline]
pub(crate) fn for_fixed<'a, const N: usize>(c: &Chunk<'a>, mut f: impl FnMut(Row<'a, '_, N>)) {
    // Less common header shapes still validate every column, even skipped
    // columns. Small files (or tails) need no tile setup.
    if c.nfields != N || c.ncols != N || !c.idx[..N].iter().copied().eq(0..N) || c.data.len() <= TILE {
        return for_rows(c, |r| f(Row::Raw(r)));
    }
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::is_x86_feature_detected as has;
        if has!("pclmulqdq") && has!("popcnt") {
            if has!("avx512vbmi2") && has!("avx512bw") && has!("bmi2") {
                // SAFETY: checked all required target features.
                return unsafe { run_vbmi2(c, &mut f) };
            }
            if has!("avx2") {
                // SAFETY: checked all required target features.
                return unsafe { run_avx2(c, &mut f) };
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    return run::<Neon, N>(c, &mut f);
    #[cfg(not(target_arch = "aarch64"))]
    run::<Portable, N>(c, &mut f);
}

#[inline(always)]
fn run<'a, K: Kernel, const N: usize>(c: &Chunk<'a>, f: &mut impl FnMut(Row<'a, '_, N>)) {
    let mut tile = Tile {
        bytes: [0; TILE + BLOCK],
        starts: [0; TILE / BLOCK],
        keep: [0; TILE / BLOCK],
    };
    let mut ends = [0u16; TILE + BLOCK];
    let mut lines = [0u16; TILE + BLOCK];
    let mut data = c.data;
    while data.len() > TILE {
        let Some(last) = memrchr(b'\n', &data[..TILE]) else {
            // An arbitrarily long physical row, possibly also the EOF row.
            let n = memchr(b'\n', &data[TILE..]).map_or(data.len(), |p| TILE + p + 1);
            raw(c, &data[..n], f);
            data = &data[n..];
            continue;
        };
        // SAFETY: TILE + 1 bytes of original input, TILE + BLOCK writable
        // initialized scratch bytes; the compacted position never exceeds
        // the input position. The target-feature dispatcher guards K.
        if unsafe { erase::<K>(data.as_ptr(), &mut tile) } {
            let block = last / BLOCK;
            let len = usize::from(tile.starts[block])
                + (tile.keep[block] << (BLOCK - 1 - last % BLOCK)).count_ones() as usize;
            compact_rows::<K, N>(data, &tile, len, &mut ends, &mut lines, f);
        } else {
            // Bad quote syntax is local to physical lines. Re-read only this
            // tile (not the whole chunk) to retain exactly its valid rows.
            raw(c, &data[..=last], f);
        }
        data = &data[last + 1..];
    }
    raw(c, data, f);
}

#[cold]
fn raw<'a, const N: usize>(c: &Chunk<'a>, data: &'a [u8], f: &mut impl FnMut(Row<'a, '_, N>)) {
    for_rows(&Chunk { data, ..*c }, |r| f(Row::Raw(r)));
}

#[inline(always)]
unsafe fn erase<K: Kernel>(src: *const u8, tile: &mut Tile) -> bool {
    let mut pos = 0;
    let mut parity = 0;
    let mut left = 1;
    for i in 0..TILE / BLOCK {
        // SAFETY: erase's caller supplies a full tile and one-byte lookahead.
        let (v, sep, q, next) = unsafe {
            let p = src.add(i * BLOCK);
            let (v, sep, q) = K::prepare(p);
            (v, sep, q, p.add(BLOCK).read())
        };
        let right = u64::from(matches!(next, b',' | b'\n')) << 63;
        let edge = ((sep << 1) | left) | ((sep >> 1) | right);
        // Every quote must hug a separator (or start of tile/line), and
        // every separator must find even quote parity. Since there are at
        // most two distinct edge positions per field, this proves 0 or 2
        // quotes, at the exact two ends. No quote can hide a comma or LF.
        let inside = K::prefix(q) ^ parity;
        if ((q & !edge) | (sep & inside)) != 0 {
            return false;
        }
        left = sep >> 63;
        parity = (inside as i64 >> 63) as u64;
        let keep = !q;
        tile.starts[i] = pos as u16;
        tile.keep[i] = keep;
        // SAFETY: the source and output each have at least 64 bytes here.
        unsafe { K::pack(v, tile.bytes.as_mut_ptr().add(pos), keep) };
        pos += keep.count_ones() as usize;
    }
    true
}

#[inline(always)]
fn compact_rows<'a, K: Kernel, const N: usize>(
    raw: &'a [u8],
    tile: &Tile,
    len: usize,
    ends: &mut [u16; TILE + BLOCK],
    lines: &mut [u16; TILE + BLOCK],
    f: &mut impl FnMut(Row<'a, '_, N>),
) {
    let (mut used, mut nl) = (0, 0);
    for base in (0..len).step_by(BLOCK) {
        // SAFETY: initialized suffix permits full reads across len. A block
        // emits at most BLOCK positions; the buffers permit overlap/padding.
        unsafe {
            let (sep, lf) = K::delimiters(tile.bytes.as_ptr().add(base));
            let mask = u64::MAX >> BLOCK.saturating_sub(len - base);
            let sep = sep & mask;
            let mut lf = lf & mask;
            while lf != 0 {
                // Remember the LF's rank among the compact separators. This
                // gives the exact field count without scanning a bad row.
                let rank = (sep << (63 - lf.trailing_zeros())).count_ones() as usize;
                lines.as_mut_ptr().add(nl).write((used + rank) as u16);
                nl += 1;
                lf &= lf - 1;
            }
            used += K::positions(ends.as_mut_ptr().add(used), base as u16, sep);
        }
    }
    let (mut start, mut prev) = (0, 0);
    for &rank in &lines[..nl] {
        let rank = usize::from(rank);
        // SAFETY: each rank is 1-based and inside the written prefix of
        // ends. Consecutive ranks enclose exactly the current row's ends.
        let last = unsafe { *ends.get_unchecked(rank - 1) };
        if rank - prev == N {
            let e = unsafe { &*ends.as_ptr().add(prev).cast::<[u16; N]>() };
            f(Row::Compact(Compact {
                raw,
                tile,
                first: start,
                ends: e,
                pext: K::PEXT,
            }));
        }
        start = usize::from(last) + 1;
        prev = rank;
    }
}

trait Kernel {
    const PEXT: bool = false;
    type Block;
    // All byte pointers require a complete 64-byte readable/writable block.
    unsafe fn prepare(src: *const u8) -> (Self::Block, u64, u64);
    fn prefix(quotes: u64) -> u64;
    unsafe fn pack(v: Self::Block, dst: *mut u8, keep: u64);
    unsafe fn delimiters(src: *const u8) -> (u64, u64);
    // Destination has room for 64 u16s, including any overlapping store.
    #[inline(always)]
    unsafe fn positions(dst: *mut u16, base: u16, mask: u64) -> usize {
        // SAFETY: caller supplies space for every set bit.
        unsafe { positions_scalar(dst, base, mask) }
    }
}

#[inline(always)]
unsafe fn positions_scalar(dst: *mut u16, base: u16, mut mask: u64) -> usize {
    let mut n = 0;
    while mask != 0 {
        // SAFETY: no more than 64 elements, as required from caller.
        unsafe { dst.add(n).write(base + mask.trailing_zeros() as u16) };
        mask &= mask - 1;
        n += 1;
    }
    n
}

#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
struct Portable;

impl Kernel for Portable {
    type Block = *const u8;

    #[inline(always)]
    unsafe fn prepare(src: *const u8) -> (Self::Block, u64, u64) {
        let mut quotes = 0;
        let mut sep = 0;
        for i in 0..BLOCK {
            let b = unsafe { src.add(i).read() };
            quotes |= u64::from(b == b'"') << i;
            sep |= u64::from(matches!(b, b',' | b'\n')) << i;
        }
        (src, sep, quotes)
    }

    #[inline(always)]
    fn prefix(mut q: u64) -> u64 {
        q ^= q << 1;
        q ^= q << 2;
        q ^= q << 4;
        q ^= q << 8;
        q ^= q << 16;
        q ^ (q << 32)
    }

    #[inline(always)]
    unsafe fn pack(src: *const u8, dst: *mut u8, mut keep: u64) {
        let mut n = 0;
        while keep != 0 {
            unsafe { dst.add(n).write(src.add(keep.trailing_zeros() as usize).read()) };
            keep &= keep - 1;
            n += 1;
        }
    }

    #[inline(always)]
    unsafe fn delimiters(src: *const u8) -> (u64, u64) {
        let (mut comma, mut lf) = (0, 0);
        for i in 0..BLOCK {
            let b = unsafe { src.add(i).read() };
            comma |= u64::from(b == b',') << i;
            lf |= u64::from(b == b'\n') << i;
        }
        (comma | lf, lf)
    }
}

// Each shuffle uses two 8-byte LUT entries (2 KiB total, versus a 1 MiB
// 16-lane LUT). Store the halves with overlap instead of merging a variable
// number of bytes; subsequent stores overwrite only discarded lanes.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const PICK8: [u64; 256] = {
    let mut table = [0; 256];
    let mut mask = 0;
    while mask < 256 {
        let mut pack = 0;
        let mut n = 0;
        let mut b = 0;
        while b < 8 {
            if mask & (1 << b) != 0 {
                pack |= (b as u64) << (n * 8);
                n += 1;
            }
            b += 1;
        }
        table[mask] = pack;
        mask += 1;
    }
    table
};

#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::*;
    use std::arch::x86_64::*;

    #[target_feature(enable = "avx2,pclmulqdq,popcnt")]
    pub(super) unsafe fn run_avx2<'a, const N: usize>(c: &Chunk<'a>, f: &mut impl FnMut(Row<'a, '_, N>)) {
        run::<Avx2, N>(c, f);
    }

    #[target_feature(enable = "avx512bw,avx512vbmi2,pclmulqdq,popcnt,bmi2")]
    pub(super) unsafe fn run_vbmi2<'a, const N: usize>(c: &Chunk<'a>, f: &mut impl FnMut(Row<'a, '_, N>)) {
        run::<Vbmi2, N>(c, f);
    }

    #[inline(always)]
    unsafe fn prefix(q: u64) -> u64 {
        // Carry-less multiply by all ones computes each inclusive prefix XOR.
        unsafe {
            _mm_cvtsi128_si64(_mm_clmulepi64_si128::<0>(
                _mm_set_epi64x(0, q as i64),
                _mm_set1_epi64x(-1),
            )) as u64
        }
    }

    pub(super) struct Avx2;

    impl Kernel for Avx2 {
        type Block = [__m256i; 2];

        #[inline(always)]
        unsafe fn prepare(p: *const u8) -> (Self::Block, u64, u64) {
            unsafe {
                let v = [_mm256_loadu_si256(p.cast()), _mm256_loadu_si256(p.add(32).cast())];
                let (sep, _) = avx2_delim(v);
                let quotes = v.map(|x| _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, _mm256_set1_epi8(b'"' as i8))) as u32);
                (v, sep, u64::from(quotes[0]) | (u64::from(quotes[1]) << 32))
            }
        }

        #[inline(always)]
        fn prefix(q: u64) -> u64 {
            unsafe { prefix(q) }
        }

        #[inline(always)]
        unsafe fn pack([lo, hi]: Self::Block, mut dst: *mut u8, mut keep: u64) {
            unsafe {
                let lanes = [
                    _mm256_castsi256_si128(lo),
                    _mm256_extracti128_si256::<1>(lo),
                    _mm256_castsi256_si128(hi),
                    _mm256_extracti128_si256::<1>(hi),
                ];
                for v in lanes {
                    let m0 = keep as u8;
                    let m1 = (keep >> 8) as u8;
                    let index = _mm_set_epi64x(
                        (PICK8[usize::from(m1)] | 0x0808_0808_0808_0808) as i64,
                        PICK8[usize::from(m0)] as i64,
                    );
                    let sh = _mm_shuffle_epi8(v, index);
                    _mm_storel_epi64(dst.cast(), sh);
                    dst = dst.add(m0.count_ones() as usize);
                    _mm_storel_epi64(dst.cast(), _mm_srli_si128::<8>(sh));
                    dst = dst.add(m1.count_ones() as usize);
                    keep >>= 16;
                }
            }
        }

        #[inline(always)]
        unsafe fn delimiters(p: *const u8) -> (u64, u64) {
            unsafe { avx2_delim([_mm256_loadu_si256(p.cast()), _mm256_loadu_si256(p.add(32).cast())]) }
        }
    }

    #[inline(always)]
    unsafe fn avx2_delim(v: [__m256i; 2]) -> (u64, u64) {
        unsafe {
            let mut sep = 0;
            let mut lf = 0;
            for (i, x) in v.into_iter().enumerate() {
                let n = _mm256_cmpeq_epi8(x, _mm256_set1_epi8(b'\n' as i8));
                let d = _mm256_or_si256(n, _mm256_cmpeq_epi8(x, _mm256_set1_epi8(b',' as i8)));
                lf |= u64::from(_mm256_movemask_epi8(n) as u32) << (32 * i);
                sep |= u64::from(_mm256_movemask_epi8(d) as u32) << (32 * i);
            }
            (sep, lf)
        }
    }

    pub(super) struct Vbmi2;
    const INDEX: [u16; 32] = {
        let mut z = [0; 32];
        let mut i = 0;
        while i < 32 {
            z[i] = i as u16;
            i += 1;
        }
        z
    };

    impl Kernel for Vbmi2 {
        // Machines with VBMI2 and BMI2 have fast PEXT. In particular, do not
        // use it solely on the basis of BMI2 on early Zen.
        const PEXT: bool = true;
        type Block = __m512i;

        #[inline(always)]
        unsafe fn prepare(p: *const u8) -> (Self::Block, u64, u64) {
            unsafe {
                let v = _mm512_loadu_si512(p.cast());
                let (sep, _) = vbmi_delim(v);
                (v, sep, _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'"' as i8)))
            }
        }

        #[inline(always)]
        fn prefix(q: u64) -> u64 {
            unsafe { prefix(q) }
        }

        #[inline(always)]
        unsafe fn pack(v: Self::Block, dst: *mut u8, keep: u64) {
            // Register compress + ordinary store; avoid the more expensive
            // memory form of VPCOMPRESSB on some implementations.
            unsafe { _mm512_storeu_si512(dst.cast(), _mm512_maskz_compress_epi8(keep, v)) };
        }

        #[inline(always)]
        unsafe fn delimiters(p: *const u8) -> (u64, u64) {
            unsafe { vbmi_delim(_mm512_loadu_si512(p.cast())) }
        }

        #[inline(always)]
        unsafe fn positions(dst: *mut u16, base: u16, mask: u64) -> usize {
            unsafe {
                let v = _mm512_add_epi16(
                    _mm512_loadu_si512(INDEX.as_ptr().cast()),
                    _mm512_set1_epi16(base as i16),
                );
                let n = (mask as u32).count_ones() as usize;
                _mm512_storeu_si512(dst.cast(), _mm512_maskz_compress_epi16(mask as u32, v));
                _mm512_storeu_si512(
                    dst.add(n).cast(),
                    _mm512_maskz_compress_epi16((mask >> 32) as u32, _mm512_add_epi16(v, _mm512_set1_epi16(32))),
                );
                n + (mask >> 32).count_ones() as usize
            }
        }
    }

    #[inline(always)]
    unsafe fn vbmi_delim(v: __m512i) -> (u64, u64) {
        unsafe {
            let lf = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'\n' as i8));
            (lf | _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b',' as i8)), lf)
        }
    }
}

#[cfg(target_arch = "x86_64")]
use x86::{run_avx2, run_vbmi2};

#[cfg(target_arch = "aarch64")]
use neon::Neon;

#[cfg(target_arch = "aarch64")]
mod neon {
    use super::*;
    use std::arch::aarch64::*;

    pub(super) struct Neon;

    #[inline(always)]
    unsafe fn load(p: *const u8) -> [uint8x16_t; 4] {
        unsafe { std::array::from_fn(|i| vld1q_u8(p.add(16 * i))) }
    }

    #[inline(always)]
    unsafe fn mask(v: [uint8x16_t; 4]) -> u64 {
        const BITS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        unsafe {
            let b = vld1q_u8(BITS.as_ptr());
            let [a, b, c, d] = v.map(|x| vandq_u8(x, b));
            let x = vpaddq_u8(vpaddq_u8(a, b), vpaddq_u8(c, d));
            vgetq_lane_u64::<0>(vreinterpretq_u64_u8(vpaddq_u8(x, x)))
        }
    }

    impl Kernel for Neon {
        type Block = [uint8x16_t; 4];

        #[inline(always)]
        unsafe fn prepare(p: *const u8) -> (Self::Block, u64, u64) {
            unsafe {
                let v = load(p);
                let sep = mask(v.map(|x| vorrq_u8(vceqq_u8(x, vdupq_n_u8(b',')), vceqq_u8(x, vdupq_n_u8(b'\n')))));
                let q = mask(v.map(|x| vceqq_u8(x, vdupq_n_u8(b'"'))));
                (v, sep, q)
            }
        }

        #[inline(always)]
        fn prefix(q: u64) -> u64 {
            Portable::prefix(q)
        }

        #[inline(always)]
        unsafe fn pack(v: Self::Block, mut dst: *mut u8, mut keep: u64) {
            unsafe {
                for x in v {
                    let m0 = keep as u8;
                    let m1 = (keep >> 8) as u8;
                    let pick = PICK8[usize::from(m0)];
                    let pick1 = PICK8[usize::from(m1)] | 0x0808_0808_0808_0808;
                    let idx = vreinterpretq_u8_u64(vsetq_lane_u64::<1>(pick1, vdupq_n_u64(pick)));
                    let sh = vqtbl1q_u8(x, idx);
                    vst1_u8(dst, vget_low_u8(sh));
                    dst = dst.add(m0.count_ones() as usize);
                    vst1_u8(dst, vget_high_u8(sh));
                    dst = dst.add(m1.count_ones() as usize);
                    keep >>= 16;
                }
            }
        }

        #[inline(always)]
        unsafe fn delimiters(p: *const u8) -> (u64, u64) {
            unsafe {
                let v = load(p);
                let lf = v.map(|x| vceqq_u8(x, vdupq_n_u8(b'\n')));
                let comma = v.map(|x| vceqq_u8(x, vdupq_n_u8(b',')));
                let n = mask(lf);
                (mask(comma) | n, n)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check<'a>(r: Row<'a, '_, 4>, codec: &Codec<'a>, out: &mut Vec<Vec<Vec<u8>>>) {
        fn row<'a>(r: impl Fields<'a>, codec: &Codec<'a>) -> Vec<Vec<u8>> {
            (0..4)
                .map(|i| {
                    let bytes = r.bytes(i);
                    assert_eq!(codec.bytes(r.key(i, codec), &mut [0; 9]), bytes);
                    bytes.to_vec()
                })
                .collect()
        }
        out.push(match r {
            Row::Raw(r) => row(r, codec),
            Row::Compact(r) => row(r, codec),
        });
    }

    #[test]
    fn fixed_rows_match_original_scanner_at_block_and_tile_edges() {
        let mut body = Vec::new();
        // Position the first quote, the closing quote and the following
        // newline at every possible offset near tile and vector boundaries.
        for pad in 0..2 * BLOCK {
            body.clear();
            body.extend_from_slice(b"\"first\",x,y,");
            body.resize(TILE - 63 + pad, b'k');
            body.extend_from_slice(b"\n\"0123456789\x00\",,\xe9\x9b\xaa,\"\"\n");
            // Both malformed-shape recovery and the raw quote-validation
            // fallback, followed by a quote-dense, properly shaped region.
            body.extend_from_slice(b"x,y,z\nw,x,y,z,a\nx,y,z,\"x\"\r\n");
            for _ in 0..280 {
                body.extend_from_slice(b"\"a\x00b\",\"abcdefghij\",\"\x00\",\"\xc3\xa9\"\n");
                body.extend_from_slice(b"\"abcdefghi\",\"\",,\r\n");
            }
            body.extend_from_slice(b"\n\"a\",\"b\",\"c\",\"eof\"");
            let c = Chunk {
                data: &body,
                nfields: 4,
                ncols: 4,
                idx: [0, 1, 2, 3, 0, 0, 0, 0],
            };
            let mut expected = Vec::new();
            for_rows(&c, |r| {
                expected.push(r[..4].iter().map(|s| s.to_vec()).collect::<Vec<_>>())
            });
            let codec = Codec::new();
            let mut actual = Vec::new();
            run::<Portable, 4>(&c, &mut |r| check(r, &codec, &mut actual));
            assert_eq!(actual, expected, "portable offset {pad}");
            actual.clear();
            for_fixed::<4>(&c, |r| check(r, &codec, &mut actual));
            assert_eq!(actual, expected, "native offset {pad}");
            #[cfg(target_arch = "x86_64")]
            if std::arch::is_x86_feature_detected!("avx2")
                && std::arch::is_x86_feature_detected!("popcnt")
                && std::arch::is_x86_feature_detected!("pclmulqdq")
            {
                actual.clear();
                unsafe { run_avx2(&c, &mut |r| check(r, &codec, &mut actual)) };
                assert_eq!(actual, expected, "AVX2 offset {pad}");
            }
        }
    }
}
