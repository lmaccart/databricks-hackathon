//! CSV dialect of rules.md: only `\n` ends a line, `\r` is data, a quote may only
//! wrap a whole field, every comma separates fields, and an empty line or a
//! line with the wrong field count is malformed.
//!
//! One pass per chunk: a SIMD classifier turns each 64-byte block into a bitmask
//! of the three structural bytes `,` `"` `\n`, and a small state machine walks
//! the set bits. Because no value may hold a comma, quote or newline, line ends
//! are exactly the `\n` bytes and field ends exactly the commas, quotes or not;
//! a quote is valid only as the first and last byte of its field.

use memchr::memchr;
use std::marker::PhantomData;
use std::path::Path;

/// Output slot for fields no caller reads (the ninth entry of the row buffer).
const SKIP: u8 = 8;

/// reference/pipeline.py names header columns with `_unquote(h) or h`.
fn header_name(h: &[u8]) -> &[u8] {
    let n = h.len();
    if memchr(b'"', h).is_some() && n > 2 && h[0] == b'"' && h[n - 1] == b'"' && memchr(b'"', &h[1..n - 1]).is_none() {
        &h[1..n - 1]
    } else {
        h
    }
}

/// A slice of one CSV file's body (whole lines only) plus that file's column map.
#[derive(Clone, Copy)]
pub struct Chunk<'a> {
    pub data: &'a [u8],
    nfields: usize,
    ncols: usize,
    idx: [usize; 8],
}

impl Chunk<'_> {
    pub fn identity_order(&self, width: usize) -> bool {
        self.nfields == width && self.ncols == width && self.idx[..width].iter().copied().eq(0..width)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Header {
    nfields: usize,
    ncols: usize,
    idx: [usize; 8],
}

impl Header {
    pub(crate) fn chunk(self, data: &[u8]) -> Chunk<'_> {
        Chunk {
            data,
            nfields: self.nfields,
            ncols: self.ncols,
            idx: self.idx,
        }
    }
}

/// Decode one physical line after the caller has checked the identity header.
pub fn identity_row<'a, const N: usize>(line: &'a [u8], f: &mut impl FnMut(&[&'a [u8]; 8])) {
    const { assert!(N <= 8) };
    let slot: [u8; N] = std::array::from_fn(|i| i as u8);
    scan::<Native, _>(line, &slot, f);
}

pub fn chunks_of<'a>(file: &'a [u8], cols: &[&str], path: &Path) -> Vec<Chunk<'a>> {
    chunks_sized(file, cols, path, crate::CHUNK)
}

pub(crate) fn header(bytes: &[u8], cols: &[&str], path: &Path) -> Header {
    assert!(cols.len() <= 8, "at most 8 selected columns");
    let header: Vec<&[u8]> = bytes.split(|&c| c == b',').map(header_name).collect();
    let mut idx = [0usize; 8];
    for (k, c) in cols.iter().enumerate() {
        idx[k] = header
            .iter()
            .position(|h| *h == c.as_bytes())
            .unwrap_or_else(|| panic!("{}: header lacks column {c}", path.display()));
        assert!(!idx[..k].contains(&idx[k]), "column {c} selected twice");
    }
    Header {
        nfields: header.len(),
        ncols: cols.len(),
        idx,
    }
}

fn chunks_sized<'a>(file: &'a [u8], cols: &[&str], path: &Path, size: usize) -> Vec<Chunk<'a>> {
    let mut out = Vec::new();
    if file.is_empty() {
        return out;
    }
    let hdr_end = memchr(b'\n', file).unwrap_or(file.len());
    let header = header(&file[..hdr_end], cols, path);
    let body = if hdr_end < file.len() {
        &file[hdr_end + 1..]
    } else {
        &file[file.len()..]
    };
    let mut start = 0;
    while start < body.len() {
        let mut end = start.saturating_add(size).min(body.len());
        if end < body.len() {
            end = match memchr(b'\n', &body[end..]) {
                Some(p) => end + p + 1,
                None => body.len(),
            };
        }
        out.push(header.chunk(&body[start..end]));
        start = end;
    }
    out
}

/// Raw field index -> output slot (selected column k, or SKIP).
fn slots(c: &Chunk) -> Vec<u8> {
    let mut slot = vec![SKIP; c.nfields];
    for k in 0..c.ncols {
        slot[c.idx[k]] = k as u8;
    }
    slot
}

/// Call f with the selected columns of every well-formed line of a chunk.
#[inline]
pub fn for_rows<'a>(c: &Chunk<'a>, mut f: impl FnMut(&[&'a [u8]; 8])) {
    let slot = slots(c);
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU supports AVX2.
            return unsafe { scan_avx2(c.data, &slot, &mut f) };
        }
    }
    scan::<Native, _>(c.data, &slot, &mut f);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn scan_avx2<'a, F: FnMut(&[&'a [u8]; 8])>(data: &'a [u8], slot: &[u8], f: &mut F) {
    scan::<Avx2, F>(data, slot, f);
}

/// The row state machine. Every structural byte is consumed exactly once, so a
/// malformed line costs no more than a good one and never hides the next line.
#[inline(always)]
fn scan<'a, C: Classify, F: FnMut(&[&'a [u8]; 8])>(data: &'a [u8], slot: &[u8], f: &mut F) {
    let len = data.len();
    let nfields = slot.len();
    let mut cur = Cursor::<C>::new(data);
    let mut row: [&'a [u8]; 9] = [&[]; 9];
    let at = |p: usize| if p < len { data[p] } else { b'\n' }; // end of data ends the last line
    let mut ls = 0; // line start
    while ls < len {
        let mut fs = ls; // field start
        let mut nf = 0;
        let (end, ok) = loop {
            let mut p = cur.next();
            let mut c = at(p);
            let (vs, ve);
            if c == b'"' {
                // `"value"` followed directly by `,` / `\n` / end; nothing else may hold a quote
                if p != fs {
                    break (cur.skip_line(p, c), false);
                }
                let q = cur.next();
                let cq = at(q);
                if cq != b'"' {
                    break (cur.skip_line(q, cq), false);
                }
                let r = cur.next();
                let cr = at(r);
                if r != q + 1 || cr == b'"' {
                    break (cur.skip_line(r, cr), false);
                }
                (vs, ve, p, c) = (p + 1, q, r, cr);
            } else {
                (vs, ve) = (fs, p);
            }
            if nf == nfields {
                break (cur.skip_line(p, c), false); // too many fields
            }
            // SAFETY: vs <= ve <= len (cursor positions only grow and never pass len);
            // nf < nfields == slot.len(); every slot is <= SKIP < row.len().
            unsafe {
                *row.get_unchecked_mut(*slot.get_unchecked(nf) as usize) = data.get_unchecked(vs..ve);
            }
            nf += 1;
            if c == b'\n' {
                break (p, nf == nfields && p > ls); // p == ls: empty line
            }
            fs = p + 1;
        };
        if ok {
            f(row.first_chunk::<8>().unwrap());
        }
        ls = end + 1;
    }
}

/// Positions of structural bytes, in order; `data.len()` once exhausted.
struct Cursor<'a, C> {
    data: &'a [u8],
    base: usize,
    bits: u64,
    _c: PhantomData<C>,
}

impl<'a, C: Classify> Cursor<'a, C> {
    #[inline(always)]
    fn new(data: &'a [u8]) -> Self {
        let bits = if data.is_empty() { 0 } else { Self::block(data, 0) };
        Cursor {
            data,
            base: 0,
            bits,
            _c: PhantomData,
        }
    }

    #[inline(always)]
    fn block(data: &[u8], at: usize) -> u64 {
        if data.len() - at >= 64 {
            // SAFETY: 64 readable bytes at `at`.
            unsafe { C::mask(data.as_ptr().add(at)) }
        } else {
            Self::tail(&data[at..])
        }
    }

    #[cold]
    #[inline(never)]
    fn tail(rest: &[u8]) -> u64 {
        let mut buf = [0u8; 64]; // zero is not structural
        buf[..rest.len()].copy_from_slice(rest);
        // SAFETY: buf has 64 bytes.
        unsafe { C::mask(buf.as_ptr()) }
    }

    #[inline(always)]
    fn next(&mut self) -> usize {
        while self.bits == 0 {
            let nb = self.base + 64;
            if nb >= self.data.len() {
                return self.data.len();
            }
            self.base = nb;
            self.bits = Self::block(self.data, nb);
        }
        let p = self.base + self.bits.trailing_zeros() as usize;
        self.bits &= self.bits - 1;
        p
    }

    /// Consume the rest of a malformed line; returns its end (`\n` or len).
    #[inline(always)]
    fn skip_line(&mut self, mut p: usize, mut c: u8) -> usize {
        while c != b'\n' && p < self.data.len() {
            p = self.next();
            c = if p < self.data.len() { self.data[p] } else { b'\n' };
        }
        p
    }
}

// ------------------------------------------------------------------ classifiers

trait Classify {
    /// Bit i set iff byte i of the 64 at `p` is `,`, `"` or `\n`.
    /// SAFETY: 64 bytes at `p` must be readable.
    unsafe fn mask(p: *const u8) -> u64;
}

/// Portable fallback: exact zero-byte detection on eight 64-bit words.
#[allow(dead_code)] // the native kernel on x86_64 and aarch64; tested everywhere
struct Swar;

impl Classify for Swar {
    #[inline(always)]
    unsafe fn mask(p: *const u8) -> u64 {
        const LO: u64 = 0x7F7F_7F7F_7F7F_7F7F;
        const ONES: u64 = 0x0101_0101_0101_0101;
        // 0x80 in each zero byte, nothing else (no borrow between bytes)
        let zero = |x: u64| !(((x & LO) + LO) | x | LO);
        let mut m = 0;
        for i in 0..8 {
            // SAFETY: caller guarantees 64 readable bytes.
            let w = u64::from_le_bytes(unsafe { p.add(8 * i).cast::<[u8; 8]>().read_unaligned() });
            let hit = zero(w ^ (ONES * b',' as u64)) | zero(w ^ (ONES * b'"' as u64)) | zero(w ^ (ONES * b'\n' as u64));
            m |= ((hit >> 7).wrapping_mul(0x0102_0408_1020_4080) >> 56) << (8 * i);
        }
        m
    }
}

#[cfg(target_arch = "x86_64")]
type Native = Sse2;
#[cfg(target_arch = "aarch64")]
type Native = Neon;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
type Native = Swar;

/// SSE2 is part of the x86_64 baseline, so this never traps.
#[cfg(target_arch = "x86_64")]
struct Sse2;

#[cfg(target_arch = "x86_64")]
impl Classify for Sse2 {
    #[inline(always)]
    unsafe fn mask(p: *const u8) -> u64 {
        use std::arch::x86_64::*;
        // SAFETY: caller guarantees 64 readable bytes; SSE2 is baseline on x86_64.
        unsafe {
            let (cm, qt, nl) = (
                _mm_set1_epi8(b',' as i8),
                _mm_set1_epi8(b'"' as i8),
                _mm_set1_epi8(b'\n' as i8),
            );
            let mut m = 0u64;
            for i in 0..4 {
                let v = _mm_loadu_si128(p.add(16 * i).cast());
                let hit = _mm_or_si128(
                    _mm_or_si128(_mm_cmpeq_epi8(v, cm), _mm_cmpeq_epi8(v, qt)),
                    _mm_cmpeq_epi8(v, nl),
                );
                m |= (_mm_movemask_epi8(hit) as u16 as u64) << (16 * i);
            }
            m
        }
    }
}

/// Only reached through `scan_avx2`, after runtime detection.
#[cfg(target_arch = "x86_64")]
struct Avx2;

#[cfg(target_arch = "x86_64")]
impl Classify for Avx2 {
    #[inline(always)]
    unsafe fn mask(p: *const u8) -> u64 {
        // SAFETY: caller guarantees 64 readable bytes and AVX2 support.
        unsafe { avx2_mask(p) }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn avx2_mask(p: *const u8) -> u64 {
    use std::arch::x86_64::*;
    // SAFETY: caller guarantees 64 readable bytes.
    unsafe {
        let (cm, qt, nl) = (
            _mm256_set1_epi8(b',' as i8),
            _mm256_set1_epi8(b'"' as i8),
            _mm256_set1_epi8(b'\n' as i8),
        );
        let lo = _mm256_loadu_si256(p.cast());
        let hi = _mm256_loadu_si256(p.add(32).cast());
        let lo = _mm256_or_si256(
            _mm256_or_si256(_mm256_cmpeq_epi8(lo, cm), _mm256_cmpeq_epi8(lo, qt)),
            _mm256_cmpeq_epi8(lo, nl),
        );
        let hi = _mm256_or_si256(
            _mm256_or_si256(_mm256_cmpeq_epi8(hi, cm), _mm256_cmpeq_epi8(hi, qt)),
            _mm256_cmpeq_epi8(hi, nl),
        );
        (_mm256_movemask_epi8(lo) as u32 as u64) | ((_mm256_movemask_epi8(hi) as u32 as u64) << 32)
    }
}

/// NEON is part of the aarch64 baseline.
#[cfg(target_arch = "aarch64")]
struct Neon;

#[cfg(target_arch = "aarch64")]
impl Classify for Neon {
    #[inline(always)]
    unsafe fn mask(p: *const u8) -> u64 {
        use std::arch::aarch64::*;
        const BIT: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        // SAFETY: caller guarantees 64 readable bytes; NEON is baseline on aarch64.
        unsafe {
            let bit = vld1q_u8(BIT.as_ptr());
            let (cm, qt, nl) = (vdupq_n_u8(b','), vdupq_n_u8(b'"'), vdupq_n_u8(b'\n'));
            let mut h = [vdupq_n_u8(0); 4];
            for (i, h) in h.iter_mut().enumerate() {
                let v = vld1q_u8(p.add(16 * i));
                *h = vandq_u8(
                    vorrq_u8(vorrq_u8(vceqq_u8(v, cm), vceqq_u8(v, qt)), vceqq_u8(v, nl)),
                    bit,
                );
            }
            // pairwise adds fold each 8-byte group into the byte holding its 8 bits
            let s = vpaddq_u8(vpaddq_u8(h[0], h[1]), vpaddq_u8(h[2], h[3]));
            vgetq_lane_u64::<0>(vreinterpretq_u64_u8(vpaddq_u8(s, s)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Rows = Vec<Vec<Vec<u8>>>;

    // ---- literal transcription of reference/pipeline.py (read_csv_rows, parse_line, _unquote)

    fn py_unquote(f: &[u8]) -> Option<&[u8]> {
        if !f.contains(&b'"') {
            return Some(f);
        }
        if f.len() >= 2 && f[0] == b'"' && f[f.len() - 1] == b'"' && !f[1..f.len() - 1].contains(&b'"') {
            return Some(&f[1..f.len() - 1]);
        }
        None
    }

    fn py_parse_line(line: &[u8], nfields: usize) -> Option<Vec<&[u8]>> {
        if line.is_empty() {
            return None;
        }
        let parts: Vec<&[u8]> = line.split(|&c| c == b',').collect();
        if parts.len() != nfields {
            return None;
        }
        parts.into_iter().map(py_unquote).collect()
    }

    fn py_rows(data: &[u8], cols: &[&str]) -> Rows {
        if data.is_empty() {
            return vec![];
        }
        let mut lines: Vec<&[u8]> = data.split(|&c| c == b'\n').collect();
        if data.ends_with(b"\n") {
            lines.pop();
        }
        let header: Vec<&[u8]> = lines[0]
            .split(|&c| c == b',')
            .map(|h| match py_unquote(h) {
                Some(v) if !v.is_empty() => v,
                _ => h,
            })
            .collect();
        let idx: Vec<usize> = cols
            .iter()
            .map(|c| header.iter().position(|h| *h == c.as_bytes()).expect("column"))
            .collect();
        let mut out = vec![];
        for line in &lines[1..] {
            if let Some(vals) = py_parse_line(line, header.len()) {
                out.push(idx.iter().map(|&i| vals[i].to_vec()).collect());
            }
        }
        out
    }

    /// Kernels this CPU can run: dispatching entry, SWAR, native SIMD, AVX2.
    fn kernels() -> Vec<usize> {
        #[allow(unused_mut)]
        let mut k = vec![0, 1, 2];
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            k.push(3);
        }
        k
    }

    fn kernel_rows(data: &[u8], cols: &[&str], size: usize, kernel: usize) -> Rows {
        let mut out: Rows = vec![];
        for c in chunks_sized(data, cols, Path::new("test.csv"), size) {
            let mut f = |r: &[&[u8]; 8]| out.push(r[..cols.len()].iter().map(|v| v.to_vec()).collect());
            let slot = slots(&c);
            match kernel {
                0 => for_rows(&c, &mut f),
                1 => scan::<Swar, _>(c.data, &slot, &mut f),
                2 => scan::<Native, _>(c.data, &slot, &mut f),
                #[cfg(target_arch = "x86_64")]
                3 => unsafe { scan_avx2(c.data, &slot, &mut f) },
                _ => unreachable!(),
            }
        }
        out
    }

    fn fast_rows(data: &[u8], cols: &[&str], size: usize) -> Rows {
        let rows = kernel_rows(data, cols, size, 0);
        for k in kernels() {
            assert_eq!(
                kernel_rows(data, cols, size, k),
                rows,
                "kernel {k} {:?}",
                String::from_utf8_lossy(data)
            );
        }
        rows
    }

    fn check(data: &[u8], cols: &[&str]) {
        let want = py_rows(data, cols);
        for size in [usize::MAX, 1, 2, 3, 63, 64, 65, 200] {
            let got = fast_rows(data, cols, size);
            assert_eq!(
                got,
                want,
                "file {:?} cols {cols:?} chunk {size}",
                String::from_utf8_lossy(data)
            );
        }
    }

    fn scalar_mask(b: &[u8]) -> u64 {
        (0..64)
            .filter(|&i| matches!(b[i], b',' | b'"' | b'\n'))
            .fold(0, |m, i| m | 1 << i)
    }

    #[test]
    fn classifiers_agree() {
        let mut rng = Rng(0xA076_1D64_78BD_642F);
        let mut buf = [0u8; 64];
        for round in 0..200_000 {
            for b in buf.iter_mut() {
                *b = match round % 3 {
                    0 => rng.next() as u8, // every byte value, incl. 0x80+ and neighbours of , " \n
                    1 => b",\"\n\r\x0b\x2b\x2d\x21\x23\x09\xac\xa2\x8a"[rng.below(13)],
                    _ => {
                        if rng.below(8) == 0 {
                            b",\"\n"[rng.below(3)]
                        } else {
                            b'a'
                        }
                    }
                };
            }
            let want = scalar_mask(&buf);
            unsafe {
                assert_eq!(Swar::mask(buf.as_ptr()), want, "swar {buf:?}");
                assert_eq!(Native::mask(buf.as_ptr()), want, "native {buf:?}");
                #[cfg(target_arch = "x86_64")]
                if std::arch::is_x86_feature_detected!("avx2") {
                    assert_eq!(Avx2::mask(buf.as_ptr()), want, "avx2 {buf:?}");
                }
            }
        }
    }

    /// Deterministic xorshift; tests stay reproducible without extra crates.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn rules_examples() {
        let cols = ["a", "b", "c"];
        check(b"a,b,c\n\"u1\",,\"\"\n", &cols);
        check(b"a,b,c\nu\"1,a,b\n\"u1,a,b\n\"\"\"\",a,b\na,b\n\na,b,1000\r\n", &cols);
        check(b"a,b,c\r\nx,y,z\r\n", &["a", "b"]); // header's last name is `c\r`
        check(b"a,b,c\n u1 ,\" u1\",\"u1 \"\n", &cols);
        check(b"a,b,c\nx,y,z", &cols); // no final newline
        check(b"a,b,c\nx,y,z\n\n", &cols); // trailing empty line
        check(b"a,b,c\n\"x\",\"y\",\"z\"", &cols); // closing quote at end of file
        check(b"a,b,c\n\"x\",\"y\",\"", &cols); // lone quote at end of file
        check(b"a,b,c", &cols); // header only, no newline
        check(b"a,b,c\n", &cols);
    }

    #[test]
    fn exhaustive_short_bodies() {
        const ALPHA: &[u8] = b"a,\"\n\r";
        let headers: [(&[u8], &[&str]); 4] = [
            (b"x", &["x"]),
            (b"x,y", &["x", "y"]),
            (b"x,y", &["y"]),
            (b"x,\"y\",z", &["z", "x", "y"]),
        ];
        let mut body = Vec::new();
        for len in 0..=7u32 {
            for mut code in 0..ALPHA.len().pow(len) {
                body.clear();
                for _ in 0..len {
                    body.push(ALPHA[code % ALPHA.len()]);
                    code /= ALPHA.len();
                }
                for (h, cols) in headers {
                    let mut file = h.to_vec();
                    file.push(b'\n');
                    file.extend_from_slice(&body);
                    let want = py_rows(&file, cols);
                    assert_eq!(
                        fast_rows(&file, cols, usize::MAX),
                        want,
                        "{:?}",
                        String::from_utf8_lossy(&file)
                    );
                    assert_eq!(fast_rows(&file, cols, 1), want, "{:?}", String::from_utf8_lossy(&file));
                }
            }
        }
    }

    #[test]
    fn header_mapping() {
        // reordered, quoted names, first of duplicate names wins, columns we do not read
        check(
            b"c,\"b\",a,b\n1,2,3,4\n\"1\",\"2\",\"3\",\"4\"\n1,2,3\n",
            &["a", "b", "c"],
        );
        check(
            b"a,b,c,d,e,f,g,h,i,j\n0,1,2,3,4,5,6,7,8,9\n0,1,2,3,4,5,6,7,8\n",
            &["j", "a", "e"],
        );
        check(b"z0,z1,z2,z3,z4,z5,z6,z7,z8,a\n0,1,2,3,4,5,6,7,8,9\n", &["a"]);
        check(b"\"\",a\n1,2\n", &["a"]);
        let ev = [
            "event_id",
            "user_id",
            "ts",
            "ingest_ts",
            "event_type",
            "product_id",
            "price_cents",
        ];
        check(
            b"price_cents,product_id,event_type,ingest_ts,ts,user_id,event_id\n7,p,view,i,t,u,e\n\"7\",p,view,i,t,u,\"e\"\n",
            &ev,
        );
    }

    #[test]
    fn every_block_offset() {
        // Put each structural byte of a line on every offset of a 64-byte block,
        // with and without the final newline and with fields longer than a block.
        let long = "q".repeat(70);
        let lines = [
            "e1,\"u1\",\"\",x".to_string(),
            format!("\"{long}\",{long},\"{long}\",\"\""),
            format!("{long}\",a,b,c"),
            format!("\"{long},a,b,c"),
            format!("a,b,c,\"{long}\"\r"),
            "a,b,c".to_string(),
            "a,b,c,d,e".to_string(),
            "\"a\"\"\",b,c,d".to_string(),
        ];
        let cols = ["w", "x", "y", "z"];
        for pad in 0..140 {
            for (i, line) in lines.iter().enumerate() {
                let mut file = b"w,x,y,z\n".to_vec();
                file.extend_from_slice("p".repeat(pad).as_bytes());
                file.extend_from_slice(b",1,2,3\n");
                file.extend_from_slice(line.as_bytes());
                check(&file, &cols);
                file.push(b'\n');
                file.extend_from_slice(lines[(i + 1) % lines.len()].as_bytes());
                file.push(b'\n');
                check(&file, &cols);
            }
        }
    }

    #[test]
    fn random_bodies() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        // weighted alphabet: mostly data, plenty of separators and quotes, some high bytes
        const ALPHA: &[u8] = b"aaaaaab01 ,,,,\"\"\"\n\n\r\x00\x80\xff\xe2";
        let headers: [(&[u8], &[&str]); 3] = [
            (b"a,b,c,d,e,f,g", &["a", "b", "c", "d", "e", "f", "g"]),
            (b"g,\"f\",e,d,c,b,a", &["a", "b", "c", "d", "e", "f", "g"]),
            (b"a,b", &["b", "a"]),
        ];
        for _ in 0..20_000 {
            let (h, cols) = headers[rng.below(headers.len())];
            let mut file = h.to_vec();
            file.push(b'\n');
            let n = rng.below(600);
            for _ in 0..n {
                file.push(ALPHA[rng.below(ALPHA.len())]);
            }
            check(&file, cols);
        }
        // mostly well-formed lines with random dirt, so the valid path is exercised heavily
        for _ in 0..5_000 {
            let mut file = b"a,b,c,d,e,f,g\n".to_vec();
            for _ in 0..rng.below(20) {
                let nf = if rng.below(10) == 0 { rng.below(9) } else { 7 };
                for k in 0..nf {
                    if k > 0 {
                        file.push(b',');
                    }
                    let q = rng.below(5) == 0;
                    if q {
                        file.push(b'"');
                    }
                    for _ in 0..rng.below(30) {
                        file.push(if rng.below(200) == 0 {
                            b'"'
                        } else {
                            b'a' + rng.below(26) as u8
                        });
                    }
                    if q && rng.below(30) != 0 {
                        file.push(b'"');
                    }
                }
                if rng.below(20) == 0 {
                    file.push(b'\r');
                }
                file.push(b'\n');
            }
            if rng.below(2) == 0 {
                file.pop();
            }
            check(&file, &["g", "c", "a"]);
        }
    }
}
