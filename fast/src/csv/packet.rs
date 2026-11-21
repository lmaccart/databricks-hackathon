//! Fixed-width event CSV: validate quote pairs in parallel, gather structural
//! positions, then transpose 16 rows into streams of short field boundaries.
use super::{Chunk, for_rows};

pub(crate) const LANES: usize = 16;
const QUOTED: u16 = 1 << 15;
const OFFSET: u16 = QUOTED - 1;

pub(in crate::csv) type Row<'a> = [&'a [u8]; 8];

pub(crate) enum Batch<'a, 'b> {
    Tile(Tile<'a, 'b>),
    Row(&'b Row<'a>),
}

pub(crate) struct Tile<'a, 'b> {
    data: &'a [u8],
    cols: &'b [[u16; LANES]; 8],
    idx: &'b [usize; 8],
    len: usize,
}

pub(crate) struct Column<'a, 'b> {
    data: &'a [u8],
    starts: &'b [u16; LANES],
    ends: &'b [u16; LANES],
}

impl<'a, 'b> Tile<'a, 'b> {
    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub(crate) fn column(&self, k: usize) -> Column<'a, 'b> {
        let i = self.idx[k];
        Column {
            data: self.data,
            starts: &self.cols[i],
            ends: &self.cols[i + 1],
        }
    }
}

impl<'a> Column<'a, '_> {
    #[inline(always)]
    pub(crate) fn get(&self, i: usize) -> &'a [u8] {
        let (start, end) = self.bounds(i);
        // Valid populated lanes have 0 <= start <= end <= data.len().
        &self.data[start..end]
    }

    #[inline(always)]
    pub(crate) fn is_empty(&self, i: usize) -> bool {
        let (start, end) = self.bounds(i);
        start == end
    }

    #[inline(always)]
    pub(crate) fn key(&self, i: usize, codec: &crate::Codec<'a>) -> u64 {
        let (start, end) = self.bounds(i);
        codec.key_at(self.data, start, end)
    }

    #[inline(always)]
    fn bounds(&self, i: usize) -> (usize, usize) {
        let a = self.starts[i];
        let q = a >> 15;
        let start = (a & OFFSET) + q;
        let end = (self.ends[i] & OFFSET) - 1 - q;
        (usize::from(start), usize::from(end))
    }
}

#[inline]
pub(crate) fn for_tiles<'a>(c: &Chunk<'a>, mut f: impl FnMut(Batch<'a, '_>)) {
    if c.nfields != 7 || c.ncols != 7 {
        return for_rows(c, |r| f(Batch::Row(r)));
    }
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2")
        && std::arch::is_x86_feature_detected!("bmi2")
        && std::arch::is_x86_feature_detected!("popcnt")
    {
        // SAFETY: all features were detected.
        return unsafe { scan_x86(c, &mut f) };
    }
    scan::<Portable, _>(c, &mut f);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi2,popcnt")]
unsafe fn scan_x86<'a, F: FnMut(Batch<'a, '_>)>(c: &Chunk<'a>, f: &mut F) {
    scan::<X86, _>(c, f);
}

#[inline(always)]
fn scan<'a, K: Kernel, F: FnMut(Batch<'a, '_>)>(c: &Chunk<'a>, f: &mut F) {
    let mut rows = [[0u16; 8]; LANES];
    let mut cursor = Cursor::<K>::new(c.data);
    let mut base = 0;
    let mut n = 0;
    while cursor.start < c.data.len() {
        let start = cursor.start;
        let (end, good, suspect) = cursor.line(base, &mut rows[n]);
        let too_wide = end + 1 - base > usize::from(OFFSET);
        if suspect || too_wide {
            flush(c, f, base, &rows, n);
            n = 0;
            // Recheck only this physical line; quote state never becomes
            // authority for a neighbouring line, even in a mixed word.
            let one = Chunk {
                data: &c.data[start..end],
                ..*c
            };
            for_rows(&one, |r| f(Batch::Row(r)));
            base = cursor.start;
        } else if good {
            n += 1;
            if n == LANES {
                flush(c, f, base, &rows, n);
                n = 0;
                base = cursor.start;
            }
        } else if n == 0 {
            base = cursor.start;
        }
    }
    flush(c, f, base, &rows, n);
}

#[inline(always)]
fn flush<'a, F: FnMut(Batch<'a, '_>)>(c: &Chunk<'a>, f: &mut F, base: usize, rows: &[[u16; 8]; LANES], len: usize) {
    if len == 0 {
        return;
    }
    let cols = transpose(rows);
    f(Batch::Tile(Tile {
        data: &c.data[base..],
        cols: &cols,
        idx: &c.idx,
        len,
    }));
}

#[inline(always)]
fn transpose(rows: &[[u16; 8]; LANES]) -> [[u16; LANES]; 8] {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: SSE2 is baseline; source and target are full tiles.
    return unsafe { transpose_sse2(rows) };
    #[cfg(not(target_arch = "x86_64"))]
    std::array::from_fn(|k| std::array::from_fn(|i| rows[i][k]))
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn transpose_sse2(rows: &[[u16; 8]; LANES]) -> [[u16; LANES]; 8] {
    use std::arch::x86_64::*;
    let mut cols = [[0; LANES]; 8];
    // SAFETY: each load/store addresses exactly eight u16s within one array.
    unsafe {
        for half in [0, 8] {
            let [a, b, c, d, e, f, g, h] = std::array::from_fn(|i| _mm_loadu_si128(rows[half + i].as_ptr().cast()));
            let (ab0, ab1) = (_mm_unpacklo_epi16(a, b), _mm_unpackhi_epi16(a, b));
            let (cd0, cd1) = (_mm_unpacklo_epi16(c, d), _mm_unpackhi_epi16(c, d));
            let (ef0, ef1) = (_mm_unpacklo_epi16(e, f), _mm_unpackhi_epi16(e, f));
            let (gh0, gh1) = (_mm_unpacklo_epi16(g, h), _mm_unpackhi_epi16(g, h));
            let top = [
                _mm_unpacklo_epi32(ab0, cd0),
                _mm_unpackhi_epi32(ab0, cd0),
                _mm_unpacklo_epi32(ab1, cd1),
                _mm_unpackhi_epi32(ab1, cd1),
            ];
            let bottom = [
                _mm_unpacklo_epi32(ef0, gh0),
                _mm_unpackhi_epi32(ef0, gh0),
                _mm_unpacklo_epi32(ef1, gh1),
                _mm_unpackhi_epi32(ef1, gh1),
            ];
            for (i, (lo, hi)) in top.into_iter().zip(bottom).enumerate() {
                _mm_storeu_si128(cols[2 * i].as_mut_ptr().add(half).cast(), _mm_unpacklo_epi64(lo, hi));
                _mm_storeu_si128(
                    cols[2 * i + 1].as_mut_ptr().add(half).cast(),
                    _mm_unpackhi_epi64(lo, hi),
                );
            }
        }
    }
    cols
}

struct Cursor<'a, K> {
    data: &'a [u8],
    start: usize,
    base: usize,
    delimiters: u64,
    newlines: u64,
    bad: u64,
    after: u64,
    start_quote: bool,
    prev_delimiter: bool,
    carry: bool,
    kernel: std::marker::PhantomData<K>,
}

impl<'a, K: Kernel> Cursor<'a, K> {
    #[inline(always)]
    fn new(data: &'a [u8]) -> Self {
        let mut cursor = Self {
            data,
            start: 0,
            base: 0,
            delimiters: 0,
            newlines: 0,
            bad: 0,
            after: 0,
            start_quote: data.first() == Some(&b'"'),
            prev_delimiter: true,
            carry: false,
            kernel: std::marker::PhantomData,
        };
        cursor.load();
        cursor
    }

    #[inline(always)]
    fn load(&mut self) {
        let rest = &self.data[self.base..];
        let (commas, newlines, quotes) = if rest.len() >= 64 {
            // SAFETY: there are 64 bytes and the caller selected an available kernel.
            unsafe { K::classify(rest.as_ptr()) }
        } else {
            tail::<K>(rest)
        };
        let d = commas | newlines;
        let opens = quotes & ((d << 1) | u64::from(self.prev_delimiter));
        let closes = quotes ^ opens;
        let next = rest.get(64).copied().unwrap_or(b'\n');
        let before = (d >> 1) | (u64::from(matches!(next, b',' | b'\n')) << 63);

        // Start one carry per opening quote. It ripples across ordinary
        // bytes and stops on the next structural byte, including a newline.
        // The stop set must equal the closers, which must abut a delimiter.
        // Seeds cannot collide: another opener needs an intervening delimiter.
        let stops = d | closes;
        let (sum, carry) = (!stops).overflowing_add(opens + u64::from(self.carry));
        self.bad = ((sum & stops) ^ closes) | (closes & !before);
        self.after = (quotes >> 1) | (u64::from(next == b'"') << 63);
        self.delimiters = d;
        self.newlines = newlines;
        self.prev_delimiter = (d >> 63) != 0;
        self.carry = carry;
    }

    #[inline(always)]
    fn line(&mut self, tile_base: usize, row: &mut [u16; 8]) -> (usize, bool, bool) {
        row[0] = (self.start - tile_base) as u16 | (u16::from(self.start_quote) << 15);
        let mut n = 1;
        let mut suspect = false;
        loop {
            let nl = self.newlines & self.newlines.wrapping_neg();
            let through = nl ^ nl.wrapping_sub(1); // all bits when no newline
            let mut d = self.delimiters & through;
            suspect |= self.bad & through != 0;

            let last = n + d.count_ones() as usize;
            if last > row.len() {
                n = 9;
            } else {
                // x86 packs quote bits in delimiter order. Other targets
                // test only selected bits. No quote byte itself is walked.
                let mut packed = K::pack(self.after, d);
                let origin = self.base.wrapping_sub(tile_base).wrapping_add(1);
                for pos in &mut row[n..last] {
                    let p = d.trailing_zeros() as usize;
                    let quoted = K::quote(self.after, &mut packed, p);
                    *pos = origin.wrapping_add(p) as u16 | quoted;
                    d &= d - 1;
                }
                n = last;
            }

            if nl != 0 {
                let p = nl.trailing_zeros() as usize;
                let end = self.base + p;
                self.start = end + 1;
                self.start_quote = self.after & nl != 0;
                self.newlines ^= nl;
                self.delimiters &= !through;
                self.bad &= !through;
                return (end, n == 8, suspect);
            }
            self.base += 64;
            self.load();
        }
    }
}

#[cold]
fn tail<K: Kernel>(rest: &[u8]) -> (u64, u64, u64) {
    let mut buf = [0u8; 64];
    buf[..rest.len()].copy_from_slice(rest);
    buf[rest.len()] = b'\n'; // synthetic delimiter for an unterminated last line
    // SAFETY: full buffer and same kernel as caller.
    unsafe { K::classify(buf.as_ptr()) }
}

trait Kernel {
    /// 64 readable bytes. Returns comma, newline and quote masks respectively.
    unsafe fn classify(p: *const u8) -> (u64, u64, u64);

    #[inline(always)]
    fn pack(_after: u64, _delimiters: u64) -> u64 {
        0
    }

    #[inline(always)]
    fn quote(after: u64, _packed: &mut u64, pos: usize) -> u16 {
        (((after >> pos) & 1) as u16) << 15
    }
}

struct Portable;

impl Kernel for Portable {
    #[inline(always)]
    unsafe fn classify(p: *const u8) -> (u64, u64, u64) {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: NEON is baseline on aarch64; 64 bytes are readable.
        return unsafe { neon(p) };
        #[cfg(target_arch = "x86_64")]
        // SAFETY: SSE2 is baseline on x86_64; 64 bytes are readable.
        return unsafe { sse2(p) };
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            // SAFETY: the caller supplies a whole readable block.
            let b = unsafe { &*p.cast::<[u8; 64]>() };
            let mut masks = (0, 0, 0);
            for (i, &byte) in b.iter().enumerate() {
                masks.0 |= u64::from(byte == b',') << i;
                masks.1 |= u64::from(byte == b'\n') << i;
                masks.2 |= u64::from(byte == b'"') << i;
            }
            masks
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn sse2(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    // SAFETY: SSE2 baseline and 64 readable bytes.
    unsafe {
        let lanes = [0, 16, 32, 48].map(|i| _mm_loadu_si128(p.add(i).cast()));
        let mask = |byte| {
            let v = _mm_set1_epi8(byte as i8);
            lanes.into_iter().enumerate().fold(0, |acc, (i, x)| {
                acc | (u64::from(_mm_movemask_epi8(_mm_cmpeq_epi8(x, v)) as u16) << (i * 16))
            })
        };
        (mask(b','), mask(b'\n'), mask(b'"'))
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn neon(p: *const u8) -> (u64, u64, u64) {
    use std::arch::aarch64::*;
    const BIT: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    // SAFETY: NEON baseline and 64 readable bytes.
    unsafe {
        let bit = vld1q_u8(BIT.as_ptr());
        let [a, b, c, d] = [0, 16, 32, 48].map(|i| vld1q_u8(p.add(i)));
        let mask = |byte| {
            let v = vdupq_n_u8(byte);
            let [a, b, c, d] = [a, b, c, d].map(|x| vandq_u8(vceqq_u8(x, v), bit));
            let packed = vpaddq_u8(vpaddq_u8(a, b), vpaddq_u8(c, d));
            vgetq_lane_u64::<0>(vreinterpretq_u64_u8(vpaddq_u8(packed, packed)))
        };
        (mask(b','), mask(b'\n'), mask(b'"'))
    }
}

#[cfg(target_arch = "x86_64")]
struct X86;

#[cfg(target_arch = "x86_64")]
impl Kernel for X86 {
    #[inline(always)]
    unsafe fn classify(p: *const u8) -> (u64, u64, u64) {
        use std::arch::x86_64::*;
        // SAFETY: for_tiles checks AVX2 and the caller supplies 64 bytes.
        unsafe {
            let a = _mm256_loadu_si256(p.cast());
            let b = _mm256_loadu_si256(p.add(32).cast());
            let mask = |byte| {
                let v = _mm256_set1_epi8(byte as i8);
                let lo = _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, v)) as u32;
                let hi = _mm256_movemask_epi8(_mm256_cmpeq_epi8(b, v)) as u32;
                u64::from(lo) | (u64::from(hi) << 32)
            };
            (mask(b','), mask(b'\n'), mask(b'"'))
        }
    }

    #[inline(always)]
    fn pack(after: u64, delimiters: u64) -> u64 {
        // SAFETY: for_tiles checks BMI2.
        unsafe { std::arch::x86_64::_pext_u64(after, delimiters) }
    }

    #[inline(always)]
    fn quote(_after: u64, packed: &mut u64, _pos: usize) -> u16 {
        let tag = (*packed as u16 & 1) << 15;
        *packed >>= 1;
        tag
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csv::{chunks_sized, tests::py_rows};
    use std::path::Path;

    #[test]
    fn packet_boundaries_and_physical_line_recovery() {
        let cols = ["a", "b", "c", "d", "e", "f", "g"];
        let good = b"\"\",b,\"c\",,\0,\"f\",g";
        let bad: &[&[u8]] = &[
            b"\",b,c,d,e,f,g",
            b"a\",b,c,d,e,f,g",
            b"\"a,b\",c,d,e,f,g",
            b"a,b,c,d,e,f,\"\n\"a,b,c,d,e,f,g",
            b"a,b,c,d,e,f,\"g\"\r",
            b"a,b,c,d,e,f,\"\"\"",
            b"a,b,c,d,e,f,\"\"\"\"",
            b"\"a\"x\",b,c,d,e,f,g",
            b"a,b,c,d,e,f",
            b"a,b,c,d,e,f,g,",
            b"",
        ];
        for shift in 0..128 {
            let mut input = b"g,a,f,e,b,d,c\n".to_vec();
            input.extend(std::iter::repeat_n(b'x', shift));
            input.extend(b",a,b,c,d,e,f\n");
            for i in 0..38 {
                input.extend(if i % 3 == 0 { bad[i / 3 % bad.len()] } else { good });
                input.push(b'\n');
            }
            input.extend(b"\""); // oversize field/tile, crossing many mask words
            input.extend(std::iter::repeat_n(b'q', 65_580));
            input.extend(b"\",a,b,c,d,e,f\n");
            input.extend(good); // EOF itself closes the last field
            let want = py_rows(&input, &cols);
            for chunk in [usize::MAX, 63, 900] {
                let mut actual: Vec<Vec<Vec<u8>>> = Vec::new();
                for c in chunks_sized(&input, &cols, Path::new("packet.csv"), chunk) {
                    for_tiles(&c, |batch| match batch {
                        Batch::Tile(tile) => {
                            for i in 0..tile.len() {
                                actual.push((0..7).map(|k| tile.column(k).get(i).to_vec()).collect());
                            }
                        }
                        Batch::Row(fields) => actual.push(fields[..7].iter().map(|x| x.to_vec()).collect()),
                    });
                }
                assert_eq!(actual, want, "shift {shift}, chunk {chunk}");
            }
        }
    }

    #[test]
    fn quoted_rows_use_full_tiles() {
        let row = b"\"a\",b,\"\",d,\"e\",f,\"g\"\n";
        let mut input = b"a,b,c,d,e,f,g\n".to_vec();
        input.extend(row.repeat(LANES * 2));
        let cols = ["a", "b", "c", "d", "e", "f", "g"];
        let mut batches = 0;
        for c in chunks_sized(&input, &cols, Path::new("packet.csv"), usize::MAX) {
            for_tiles(&c, |batch| {
                let Batch::Tile(tile) = batch else {
                    panic!("unexpected fallback")
                };
                assert_eq!(tile.len(), LANES);
                batches += 1;
            });
        }
        assert_eq!(batches, 2);
    }
}
