//! CSV dialect of rules.md: only `\n` ends a line, `\r` is data, a quote may
//! only wrap a whole field, every comma separates fields, and an empty line or
//! a line with the wrong number of fields is malformed.
//!
//! A chunk is read in windows of a few KB, twice, so the second pass and the
//! row consumer find the bytes in L1:
//! 1. A SIMD classifier turns each 64-byte block into a comma, a newline and a
//!    quote bitmask. Comma and newline bits are flattened into position lists;
//!    quote bits stay masks, with a running count per block.
//! 2. Rows end at newline positions. A row of `n` fields is well formed iff it
//!    is non-empty, holds exactly `n - 1` commas, and every quote in it is the
//!    first or last byte of a field of length >= 2 ("wrapped"). Those boundary
//!    bytes are distinct and are quotes, so the row's quote count (two prefix
//!    lookups) equals twice its wrapped fields iff there is no other quote
//!    anywhere in the row, in a selected column or not.
//!
//! Neither pass walks quote bits or branches on whether a field is quoted.
//! With the header in the expected order, a 2-, 4- or 7-column row is checked
//! and split at once from its comma positions. Lines longer than a window and
//! a last line without `\n` take a scalar path with the same rules.

use memchr::memchr;
use std::path::Path;

/// Bytes classified per pass: the window, its position lists and the fields
/// the consumer reads stay in L1. A multiple of 64.
const WINDOW: usize = 8 << 10;
/// Row slot for fields no caller reads (the ninth entry of the row buffer).
const SKIP: u8 = 8;
/// After the last comma of a window; larger than any position in it.
const END: u32 = u32::MAX;

type Row<'a> = [&'a [u8]; 8];

/// A field's value, or None if a quote appears anywhere but around the whole field.
fn unquote(f: &[u8]) -> Option<&[u8]> {
    if memchr(b'"', f).is_none() {
        return Some(f);
    }
    let n = f.len();
    if n >= 2 && f[0] == b'"' && f[n - 1] == b'"' && memchr(b'"', &f[1..n - 1]).is_none() {
        Some(&f[1..n - 1])
    } else {
        None
    }
}

/// reference/pipeline.py names header columns `_unquote(h) or h`.
fn header_name(h: &[u8]) -> &[u8] {
    match unquote(h) {
        Some(v) if !v.is_empty() => v,
        _ => h,
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

pub fn chunks_of<'a>(file: &'a [u8], cols: &[&str], path: &Path) -> Vec<Chunk<'a>> {
    chunks_sized(file, cols, path, crate::CHUNK)
}

fn chunks_sized<'a>(file: &'a [u8], cols: &[&str], path: &Path, size: usize) -> Vec<Chunk<'a>> {
    assert!(cols.len() <= 8, "at most 8 selected columns");
    let mut out = Vec::new();
    if file.is_empty() {
        return out;
    }
    let hdr_end = memchr(b'\n', file).unwrap_or(file.len());
    let header: Vec<&[u8]> = file[..hdr_end]
        .split(|&c| c == b',')
        .map(header_name)
        .collect();
    let mut idx = [0usize; 8];
    for (k, c) in cols.iter().enumerate() {
        idx[k] = header
            .iter()
            .position(|h| *h == c.as_bytes())
            .unwrap_or_else(|| panic!("{}: header lacks column {c}", path.display()));
        assert!(!idx[..k].contains(&idx[k]), "column {c} selected twice");
    }
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
        out.push(Chunk {
            data: &body[start..end],
            nfields: header.len(),
            ncols: cols.len(),
            idx,
        });
        start = end;
    }
    out
}

/// Call f with the selected columns of every well-formed line of a chunk.
pub fn for_rows<'a>(c: &Chunk<'a>, mut f: impl FnMut(&Row<'a>)) {
    dispatch(c, WINDOW, &mut f);
}

/// The classifier `for_rows` uses on this CPU.
pub fn kernel() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        return "avx2";
    }
    std::any::type_name::<Native>()
        .rsplit("::")
        .next()
        .unwrap_or("?")
}

#[inline(always)]
fn dispatch<'a, F: FnMut(&Row<'a>)>(c: &Chunk<'a>, window: usize, f: &mut F) {
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        // SAFETY: the CPU has every feature scan_avx2 enables.
        return unsafe { scan_avx2(c, window, f) };
    }
    scan::<Native, F>(c, window, f);
}

#[cfg(target_arch = "x86_64")]
fn has_avx2() -> bool {
    is_x86_feature_detected!("avx2")
        && is_x86_feature_detected!("bmi1")
        && is_x86_feature_detected!("popcnt")
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi1,popcnt")]
unsafe fn scan_avx2<'a, F: FnMut(&Row<'a>)>(c: &Chunk<'a>, window: usize, f: &mut F) {
    scan::<Avx2, F>(c, window, f);
}

/// Specialized only by field count, for a header that is exactly the
/// selected columns in order; any other header takes the general row path.
#[inline(always)]
fn scan<'a, C: Classify, F: FnMut(&Row<'a>)>(c: &Chunk<'a>, window: usize, f: &mut F) {
    debug_assert!(window >= 64 && window.is_multiple_of(64) && window < END as usize);
    let mut slot = vec![SKIP; c.nfields];
    for k in 0..c.ncols {
        slot[c.idx[k]] = k as u8;
    }
    let in_order = c.ncols == c.nfields && slot.iter().enumerate().all(|(j, &k)| k as usize == j);
    match (in_order, c.nfields) {
        (true, 7) => windows::<C, F, 7>(c.data, window, &slot, f),
        (true, 4) => windows::<C, F, 4>(c.data, window, &slot, f),
        (true, 2) => windows::<C, F, 2>(c.data, window, &slot, f),
        _ => windows::<C, F, 0>(c.data, window, &slot, f),
    }
}

/// `N >= 2`: rows of exactly N fields in slot order; 0: the general path.
#[inline(always)]
fn windows<'a, C: Classify, F: FnMut(&Row<'a>), const N: usize>(
    data: &'a [u8],
    window: usize,
    slot: &[u8],
    f: &mut F,
) {
    // A small chunk (a small file) needs no full-size lists.
    let window = window.min(data.len().next_multiple_of(64));
    let mut lists = Lists::new(window);
    let mut ws = 0;
    while ws < data.len() {
        let w = &data[ws..data.len().min(ws + window)];
        let lines = lists.classify::<C>(w);
        if lines == 0 {
            // A line longer than a window, or a last line without `\n`.
            let end = ws + w.len();
            let le = memchr(b'\n', &data[end..]).map_or(data.len(), |p| end + p);
            scalar_line(&data[ws..le], slot, f);
            ws = le + 1;
        } else {
            lists.rows::<F, N>(w, lines, slot, f);
            // The partial line after the last newline starts the next window.
            ws += lists.newlines[lines - 1] as usize + 1;
        }
    }
}

/// One window's structure. Positions are offsets into the window.
struct Lists {
    commas: Vec<u32>,
    newlines: Vec<u32>,
    /// Per 64-byte block: its quote mask, and the quotes in earlier blocks.
    quotes: Vec<u64>,
    before: Vec<u32>,
}

impl Lists {
    fn new(window: usize) -> Self {
        // flatten() writes past the count; rows() reads 8 ENDs past the last comma.
        Lists {
            commas: vec![END; window + 64],
            newlines: vec![0; window + 64],
            quotes: vec![0; window / 64],
            before: vec![0; window / 64],
        }
    }

    /// Classify `w` (at most one window); returns how many newlines it has.
    #[inline(always)]
    fn classify<C: Classify>(&mut self, w: &[u8]) -> usize {
        let blocks = w.len().div_ceil(64);
        let quotes = &mut self.quotes[..blocks];
        let before = &mut self.before[..blocks];
        let (commas, newlines) = (self.commas.as_mut_ptr(), self.newlines.as_mut_ptr());
        let (mut nc, mut nn, mut q) = (0, 0, 0u32);
        for b in 0..blocks {
            let at = 64 * b;
            let [comma, newline, quote] = if w.len() - at >= 64 {
                // SAFETY: 64 readable bytes at `at`.
                unsafe { C::masks(w.as_ptr().add(at)) }
            } else {
                tail::<C>(&w[at..])
            };
            quotes[b] = quote;
            before[b] = q;
            q += quote.count_ones();
            // SAFETY: both lists hold window + 64 entries. Blocks before this
            // one added at most `at` positions, and this one writes at most 64.
            unsafe {
                nc = flatten::<8>(commas, nc, comma, at as u32);
                nn = flatten::<4>(newlines, nn, newline, at as u32);
            }
        }
        self.commas[nc..nc + 8].fill(END);
        nn
    }

    /// Emit the well-formed rows among the first `lines` lines of window `w`.
    #[inline(always)]
    fn rows<'a, F: FnMut(&Row<'a>), const N: usize>(
        &self,
        w: &'a [u8],
        lines: usize,
        slot: &[u8],
        f: &mut F,
    ) {
        let commas = &self.commas[..];
        let mut c = 0; // first comma at or after the row start
        let mut rs = 0u32; // row start
        let mut qs = 0u32; // quotes before rs
        let mut row: [&'a [u8]; 9] = [&[]; 9];
        for &le in &self.newlines[..lines] {
            let b = (le / 64) as usize;
            // SAFETY: le < w.len(), so its block was classified.
            let qe = unsafe {
                self.before.get_unchecked(b)
                    + (self.quotes.get_unchecked(b) & ((1u64 << (le % 64)) - 1)).count_ones()
            };
            if N >= 2 {
                // SAFETY: c never passes the commas before le; ENDs pad N past them.
                let (a, z) = unsafe {
                    (
                        *commas.get_unchecked(c + N - 2),
                        *commas.get_unchecked(c + N - 1),
                    )
                };
                if (a < le) & (z > le) {
                    // Exactly N - 1 commas in the row: its N fields end at them and at le.
                    let mut wrapped = 0;
                    let mut s = rs;
                    for (k, out) in row.iter_mut().enumerate().take(N) {
                        // SAFETY: k < N - 1 indexes one of those commas.
                        let e = if k + 1 < N {
                            unsafe { *commas.get_unchecked(c + k) }
                        } else {
                            le
                        };
                        let (v, q) = field(w, s, e);
                        *out = v;
                        wrapped += q;
                        s = e + 1;
                    }
                    if qe - qs == 2 * wrapped {
                        f(row.first_chunk().unwrap());
                    }
                    c += N - 1;
                } else {
                    while commas[c] < le {
                        c += 1;
                    }
                }
            } else {
                let (mut k, mut s, mut wrapped) = (0, rs, 0);
                loop {
                    let next = commas[c];
                    let e = next.min(le);
                    let (v, q) = field(w, s, e);
                    wrapped += q;
                    if let Some(&j) = slot.get(k) {
                        row[j as usize] = v;
                    }
                    k += 1;
                    if next > le {
                        break;
                    }
                    c += 1;
                    s = e + 1;
                }
                if k == slot.len() && le > rs && qe - qs == 2 * wrapped {
                    f(row.first_chunk().unwrap());
                }
            }
            rs = le + 1;
            qs = qe;
        }
    }
}

/// Field `w[s..e]` (e is its delimiter) without wrapping quotes, and 1 if it had them.
#[inline(always)]
fn field(w: &[u8], s: u32, e: u32) -> (&[u8], u32) {
    let (s, e) = (s as usize, e as usize);
    // SAFETY: s <= e < w.len(). An empty field probes its delimiter twice.
    let (first, last) = unsafe { (*w.get_unchecked(s), *w.get_unchecked(e.max(s + 1) - 1)) };
    let q = ((e - s >= 2) & (first == b'"') & (last == b'"')) as usize;
    // SAFETY: q = 1 only when e - s >= 2.
    (unsafe { w.get_unchecked(s + q..e - q) }, q as u32)
}

/// Write `base + i` for each set bit i of m at out[n..]; returns n + its count.
/// The first K are written unconditionally, so a block costs no count branch.
///
/// SAFETY: out must hold n + 64 entries.
#[inline(always)]
unsafe fn flatten<const K: usize>(out: *mut u32, n: usize, mut m: u64, base: u32) -> usize {
    let count = m.count_ones() as usize;
    // SAFETY: at most max(K, count) <= 64 writes from out + n.
    unsafe {
        let o = out.add(n);
        for i in 0..K {
            o.add(i).write(base + m.trailing_zeros());
            m &= m.wrapping_sub(1);
        }
        let mut i = K;
        while m != 0 {
            o.add(i).write(base + m.trailing_zeros());
            m &= m - 1;
            i += 1;
        }
    }
    n + count
}

/// One line by the literal rule: lines longer than a window, last line without `\n`.
#[cold]
fn scalar_line<'a, F: FnMut(&Row<'a>)>(line: &'a [u8], slot: &[u8], f: &mut F) {
    if line.is_empty() {
        return;
    }
    let mut row: [&'a [u8]; 9] = [&[]; 9];
    let mut k = 0;
    for v in line.split(|&b| b == b',') {
        let (Some(&j), Some(v)) = (slot.get(k), unquote(v)) else {
            return;
        };
        row[j as usize] = v;
        k += 1;
    }
    if k == slot.len() {
        f(row.first_chunk().unwrap());
    }
}

// ------------------------------------------------------------------ classifiers

trait Classify {
    /// Bit i of each mask is set iff byte i of the 64 at p is `,` / `\n` / `"`.
    ///
    /// SAFETY: 64 bytes at p must be readable.
    unsafe fn masks(p: *const u8) -> [u64; 3];
}

#[cold]
fn tail<C: Classify>(rest: &[u8]) -> [u64; 3] {
    let mut buf = [0u8; 64]; // zero is not structural
    buf[..rest.len()].copy_from_slice(rest);
    // SAFETY: buf has 64 bytes.
    unsafe { C::masks(buf.as_ptr()) }
}

/// Portable fallback: exact zero-byte detection on eight 64-bit words.
#[allow(dead_code)] // the native kernel elsewhere; tested on every target
struct Swar;

impl Classify for Swar {
    #[inline(always)]
    unsafe fn masks(p: *const u8) -> [u64; 3] {
        const LO: u64 = 0x7F7F_7F7F_7F7F_7F7F;
        const ONES: u64 = 0x0101_0101_0101_0101;
        // 0x80 in exactly the zero bytes of x: no carry crosses a byte.
        let zero = |x: u64| !(((x & LO) + LO) | x | LO);
        // the top bit of each byte, gathered into the top byte
        let gather = |h: u64| (h >> 7).wrapping_mul(0x0102_0408_1020_4080) >> 56;
        let mut m = [0u64; 3];
        for i in 0..8 {
            // SAFETY: the caller guarantees 64 readable bytes.
            let w = u64::from_le_bytes(unsafe { p.add(8 * i).cast::<[u8; 8]>().read_unaligned() });
            for (m, ch) in m.iter_mut().zip(*b",\n\"") {
                *m |= gather(zero(w ^ (ONES * ch as u64))) << (8 * i);
            }
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
    unsafe fn masks(p: *const u8) -> [u64; 3] {
        use std::arch::x86_64::*;
        // SAFETY: the caller guarantees 64 readable bytes; SSE2 is baseline.
        unsafe {
            let ch = [
                _mm_set1_epi8(b',' as i8),
                _mm_set1_epi8(b'\n' as i8),
                _mm_set1_epi8(b'"' as i8),
            ];
            let mut m = [0u64; 3];
            for i in 0..4 {
                let v = _mm_loadu_si128(p.add(16 * i).cast());
                for (m, ch) in m.iter_mut().zip(ch) {
                    *m |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, ch)) as u16 as u64) << (16 * i);
                }
            }
            m
        }
    }
}

/// Reached only through scan_avx2, after runtime detection.
#[cfg(target_arch = "x86_64")]
struct Avx2;

#[cfg(target_arch = "x86_64")]
impl Classify for Avx2 {
    #[inline(always)]
    unsafe fn masks(p: *const u8) -> [u64; 3] {
        // SAFETY: the caller guarantees 64 readable bytes and AVX2.
        unsafe { avx2_masks(p) }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn avx2_masks(p: *const u8) -> [u64; 3] {
    use std::arch::x86_64::*;
    // SAFETY: the caller guarantees 64 readable bytes. No closures: they
    // would not inherit this function's target features.
    unsafe {
        let lo = _mm256_loadu_si256(p.cast());
        let hi = _mm256_loadu_si256(p.add(32).cast());
        let (c, n, q) = (
            _mm256_set1_epi8(b',' as i8),
            _mm256_set1_epi8(b'\n' as i8),
            _mm256_set1_epi8(b'"' as i8),
        );
        let cl = _mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, c)) as u32 as u64;
        let ch = _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, c)) as u32 as u64;
        let nl = _mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, n)) as u32 as u64;
        let nh = _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, n)) as u32 as u64;
        let ql = _mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, q)) as u32 as u64;
        let qh = _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, q)) as u32 as u64;
        [cl | ch << 32, nl | nh << 32, ql | qh << 32]
    }
}

/// NEON is part of the aarch64 baseline.
#[cfg(target_arch = "aarch64")]
struct Neon;

#[cfg(target_arch = "aarch64")]
impl Classify for Neon {
    #[inline(always)]
    unsafe fn masks(p: *const u8) -> [u64; 3] {
        use std::arch::aarch64::*;
        const BIT: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        // SAFETY: the caller guarantees 64 readable bytes; NEON is baseline.
        unsafe {
            let bit = vld1q_u8(BIT.as_ptr());
            let v = [
                vld1q_u8(p),
                vld1q_u8(p.add(16)),
                vld1q_u8(p.add(32)),
                vld1q_u8(p.add(48)),
            ];
            // Each matching byte keeps its bit weight; pairwise adds fold the
            // 64 bytes into 8 (two folds here, the third below).
            let fold = |ch: u8| {
                let h = v.map(|v| vandq_u8(vceqq_u8(v, vdupq_n_u8(ch)), bit));
                vpaddq_u8(vpaddq_u8(h[0], h[1]), vpaddq_u8(h[2], h[3]))
            };
            let cn = vreinterpretq_u64_u8(vpaddq_u8(fold(b','), fold(b'\n')));
            let q = fold(b'"');
            let qq = vreinterpretq_u64_u8(vpaddq_u8(q, q));
            [
                vgetq_lane_u64::<0>(cn),
                vgetq_lane_u64::<1>(cn),
                vgetq_lane_u64::<0>(qq),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Rows = Vec<Vec<Vec<u8>>>;

    #[derive(Clone, Copy, Debug)]
    enum Kernel {
        Swar,
        Native,
        #[cfg(target_arch = "x86_64")]
        Avx2,
    }

    fn kernels() -> Vec<Kernel> {
        #[allow(unused_mut)]
        let mut k = vec![Kernel::Swar, Kernel::Native];
        #[cfg(target_arch = "x86_64")]
        if has_avx2() {
            k.push(Kernel::Avx2);
        }
        k
    }

    fn masks(k: Kernel, block: &[u8; 64]) -> [u64; 3] {
        // SAFETY: 64 bytes; Avx2 is listed only when detected.
        unsafe {
            match k {
                Kernel::Swar => Swar::masks(block.as_ptr()),
                Kernel::Native => Native::masks(block.as_ptr()),
                #[cfg(target_arch = "x86_64")]
                Kernel::Avx2 => avx2_masks(block.as_ptr()),
            }
        }
    }

    fn run(k: Kernel, c: &Chunk, window: usize, out: &mut Rows) {
        let ncols = c.ncols;
        let mut push = |r: &Row| out.push(r[..ncols].iter().map(|v| v.to_vec()).collect());
        match k {
            Kernel::Swar => scan::<Swar, _>(c, window, &mut push),
            Kernel::Native => scan::<Native, _>(c, window, &mut push),
            // SAFETY: listed only when detected.
            #[cfg(target_arch = "x86_64")]
            Kernel::Avx2 => unsafe { scan_avx2(c, window, &mut push) },
        }
    }

    /// reference/pipeline.py read literally: split the whole file at `\n`,
    /// then each line at `,`, then unquote each field.
    fn model(file: &[u8], cols: &[&str]) -> Rows {
        let mut lines: Vec<&[u8]> = file.split(|&b| b == b'\n').collect();
        if file.ends_with(b"\n") {
            lines.pop();
        }
        if file.is_empty() || lines.is_empty() {
            return Vec::new();
        }
        let header: Vec<&[u8]> = lines[0].split(|&b| b == b',').map(header_name).collect();
        let idx: Vec<usize> = cols
            .iter()
            .map(|c| header.iter().position(|h| *h == c.as_bytes()).unwrap())
            .collect();
        let unq = |p: &[u8]| -> Option<Vec<u8>> {
            let n = p.len();
            match p.iter().filter(|&&b| b == b'"').count() {
                0 => Some(p.to_vec()),
                2 if n >= 2 && p[0] == b'"' && p[n - 1] == b'"' => Some(p[1..n - 1].to_vec()),
                _ => None,
            }
        };
        let mut rows = Vec::new();
        for line in &lines[1..] {
            let parts: Vec<&[u8]> = line.split(|&b| b == b',').collect();
            if line.is_empty() || parts.len() != header.len() {
                continue;
            }
            if let Some(vals) = parts.iter().map(|p| unq(p)).collect::<Option<Vec<_>>>() {
                rows.push(idx.iter().map(|&i| vals[i].clone()).collect());
            }
        }
        rows
    }

    const WINDOWS: [usize; 4] = [64, 128, 192, WINDOW];

    /// Every kernel, window and chunk size agrees with the model; returns it.
    fn check_with(file: &[u8], cols: &[&str], windows: &[usize], sizes: &[usize]) -> Rows {
        let want = model(file, cols);
        for k in kernels() {
            for &window in windows {
                for &size in sizes {
                    let mut got = Vec::new();
                    for c in chunks_sized(file, cols, Path::new("t.csv"), size) {
                        run(k, &c, window, &mut got);
                    }
                    assert_eq!(
                        got,
                        want,
                        "{k:?} window {window} chunk {size} cols {cols:?} file {:?}",
                        String::from_utf8_lossy(file)
                    );
                }
            }
        }
        want
    }

    fn check(file: &[u8], cols: &[&str]) -> Rows {
        check_with(file, cols, &WINDOWS, &[1, 50, usize::MAX])
    }

    fn rows(v: &[&[&str]]) -> Rows {
        v.iter()
            .map(|r| r.iter().map(|f| f.as_bytes().to_vec()).collect())
            .collect()
    }

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

    const COLS7: [&str; 7] = [
        "event_id",
        "user_id",
        "ts",
        "ingest_ts",
        "event_type",
        "product_id",
        "price_cents",
    ];

    #[test]
    fn kernels_classify_every_byte_value() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..4096 {
            let mut block = [0u8; 64];
            for b in &mut block {
                *b = match rng.below(4) {
                    0 => b",\n\"\r"[rng.below(4)],
                    _ => rng.below(256) as u8,
                };
            }
            if round < 256 {
                block = [round as u8; 64];
            }
            let want = b",\n\"".map(|ch| {
                block
                    .iter()
                    .enumerate()
                    .fold(0u64, |m, (i, &b)| m | ((b == ch) as u64) << i)
            });
            for k in kernels() {
                assert_eq!(masks(k, &block), want, "{k:?} {block:?}");
            }
        }
    }

    #[test]
    fn dialect_examples_from_rules() {
        let body = concat!(
            "x,y\n",
            "\"x\",\"y\"\n",
            "\"\",\"\"\n",
            ",\n",
            "\n",
            "x\n",
            "x,y,z\n",
            "\"x,y\"\n",
            "x,\"\n",
            "x,\"\"\"\n",
            "x,\"\"\"\"\n",
            "\"x\"y,z\n",
            "x,y\r\n",
            "x,\"y\"\r\n",
            "x,\"y\r\"\n",
            " x , \"y \"\n", // a space before the quote: not a whole-field wrap
            " x ,\"y \"\n",
            "\"x\",y\n",
        );
        let want = rows(&[
            &["x", "y"],
            &["x", "y"],
            &["", ""],
            &["", ""],
            &["x", "y\r"],
            &["x", "y\r"],
            &[" x ", "y "],
            &["x", "y"],
        ]);
        let file = format!("a,b\n{body}");
        assert_eq!(check(file.as_bytes(), &["a", "b"]), want);
        // the same lines on the general path: reordered selection
        let swapped: Rows = want
            .iter()
            .map(|r| vec![r[1].clone(), r[0].clone()])
            .collect();
        assert_eq!(check(file.as_bytes(), &["b", "a"]), swapped);
    }

    #[test]
    fn stray_quotes_never_balance_across_fields_rows_or_columns() {
        // Each line holds as many quotes as a well-formed one could, or pairs
        // a missing boundary quote with an extra quote elsewhere.
        let bad = [
            "a\"b,\"c,d",      // two lone quotes in two fields
            "a,\",\"",         // a quote alone in each of two fields
            "\"a,b\",c",       // a wrap spanning a comma
            "\"a,\"b\",c",     // an unopened field next to a wrapped one
            "\"a\",\"b\"\",c", // a wrapped field with a doubled end quote
            "\"\"\",\"\",c",   // three quotes, then a valid empty wrap
            "a,b,\"c\"\"\"",   // four quotes in the last field
            "\"a\"\"b\",c,d",  // escaped quote inside a wrap is not a dialect
        ];
        let good = ["\"a\",\"b\",\"c\"", "\"\",b,\"\"", "a,\"b\",c"];
        let mut file = String::from("p,q,r\n");
        for (i, b) in bad.iter().enumerate() {
            file.push_str(b);
            file.push('\n');
            file.push_str(good[i % good.len()]);
            file.push('\n');
        }
        let got = check(file.as_bytes(), &["p", "q", "r"]);
        assert_eq!(
            got.len(),
            bad.len(),
            "every bad line dropped, every good kept"
        );

        // Two neighbours with one stray quote each; their sum is even.
        let pair = "x,y\n\"a,b\na,b\"\n\"x\",\"y\"\n";
        assert_eq!(
            check(format!("a,b\n{pair}").as_bytes(), &["a", "b"]),
            rows(&[&["x", "y"], &["x", "y"]])
        );

        // A stray quote in a column nobody reads still drops the line.
        let extra = "a,b\"c,d\na,\"b\",d\na,\"\",d\na,\"b\"\",d\n\"\"\"\",b,d\n";
        assert_eq!(
            check(format!("id,extra,v\n{extra}").as_bytes(), &["id", "v"]),
            rows(&[&["a", "d"], &["a", "d"]])
        );

        // The same in every position of a 7- and a 4-column row.
        for n in [7, 4] {
            let cols = &COLS7[..n];
            let mut file = cols.join(",") + "\n";
            for k in 0..n {
                for bad in ["\"v", "v\"", "\"", "\"\"\"", "\"v\"v\"", "v\"\"v"] {
                    let line: Vec<&str> =
                        (0..n).map(|j| if j == k { bad } else { "\"w\"" }).collect();
                    file += &line.join(",");
                    file += "\n";
                }
            }
            file += &vec!["\"w\""; n].join(",");
            assert_eq!(
                check(file.as_bytes(), cols),
                vec![vec![b"w".to_vec(); n]],
                "{n} columns"
            );
        }
    }

    #[test]
    fn rows_straddle_blocks_windows_and_chunks() {
        let lines = [
            "e1,u1,2026-10-03T23:58:10Z,2026-10-03T23:58:11Z,view,p1,",
            "\"e2\",\"u2\",\"1791071890000\",\"2026-10-03T23:58:11Z\",\"purchase\",\"p2\",\"0250\"",
            "e3,u3,t,i,cart,p3,\"\"",
            "",
            "e4,u4,t,i,view,p4",
            "e5,u5,t,i,view,p5,1,extra",
            "e6,u\"6,t,i,view,p6,1",
            "e7,u7,t,i,view,p7,1\r",
            "\"e8\",u8,t,i,view,p8,\"1\"",
            ",,,,,,",
        ];
        for n in [7, 4, 2, 3] {
            let cols = &COLS7[..n];
            for perm in [false, true] {
                let mut header: Vec<&str> = cols.to_vec();
                if perm {
                    header.reverse();
                }
                let body: String = lines
                    .iter()
                    .map(|l| {
                        let f: Vec<&str> = l.split(',').collect();
                        // keep the malformed shapes: an extra or missing field stays one
                        let keep = if f.len() == 7 {
                            &f[..n]
                        } else if f.len() < 7 {
                            &f[..(n - 1).min(f.len())]
                        } else {
                            &f[..n + 1]
                        };
                        keep.join(",") + "\n"
                    })
                    .collect();
                let mut total = 0;
                for pad in 0..=130 {
                    let mut file = header.join(",") + "\n";
                    file += &format!("{},{}\n", "z".repeat(pad), vec!["q"; n - 1].join(","));
                    file += &body;
                    for end in [false, true] {
                        let f = if end {
                            file.trim_end_matches('\n').to_string()
                        } else {
                            file.clone()
                        };
                        total += check_with(f.as_bytes(), cols, &WINDOWS, &[usize::MAX]).len();
                        check_with(f.as_bytes(), cols, &[64], &[1]);
                    }
                }
                assert!(total > 0);
            }
        }
    }

    #[test]
    fn lines_longer_than_a_window() {
        let cols = ["a", "b"];
        for len in [
            62,
            63,
            64,
            65,
            127,
            128,
            129,
            191,
            192,
            193,
            300,
            WINDOW - 1,
            WINDOW,
            WINDOW + 1,
            3 * WINDOW,
        ] {
            let v = "v".repeat(len);
            let cases = [
                (format!("{v},x"), Some([v.as_str(), "x"])),
                (format!("\"{v}\",x"), Some([v.as_str(), "x"])),
                (format!("{v}\",x"), None),
                (format!("x,{v},y"), None),
                (v.clone(), None),
                (format!("x,\"{v}"), None),
            ];
            for (line, want) in &cases {
                for tail in ["\n", "\nx,\"y\"\n", ""] {
                    let file = format!("a,b\n{line}{tail}");
                    let got = check(file.as_bytes(), &cols);
                    let mut expect: Rows = want
                        .iter()
                        .map(|r| r.iter().map(|f| f.as_bytes().to_vec()).collect())
                        .collect();
                    if tail.len() > 1 {
                        expect.push(vec![b"x".to_vec(), b"y".to_vec()]);
                    }
                    assert_eq!(got, expect, "len {len} tail {tail:?}");
                }
            }
        }
    }

    #[test]
    fn end_of_input() {
        let cols = ["a", "b"];
        let cases: [(&str, &[&[&str]]); 13] = [
            ("a,b", &[]),
            ("a,b\n", &[]),
            ("a,b\n\n", &[]),
            ("a,b\n\n\n", &[]),
            ("a,b\nx,y", &[&["x", "y"]]),
            ("a,b\nx,", &[&["x", ""]]),
            ("a,b\nx,\"\"", &[&["x", ""]]),
            ("a,b\nx,\"", &[]),
            ("a,b\n\"x\",\"y\"", &[&["x", "y"]]),
            ("a,b\nx,y\r", &[&["x", "y\r"]]),
            ("a,b\nx,y\n,", &[&["x", "y"], &["", ""]]),
            ("a,b\nx,y\nz", &[&["x", "y"]]),
            ("a,b\n,\n", &[&["", ""]]),
        ];
        for (file, want) in cases {
            assert_eq!(check(file.as_bytes(), &cols), rows(want), "{file:?}");
        }
        // last byte at every offset around block and window ends
        for len in 1..=200 {
            let row = format!("{},y", "x".repeat(len));
            for end in ["", "\n", "\n\n", ","] {
                let file = format!("a,b\n{row}{end}");
                let want: &[&str] = if end == "," {
                    &[]
                } else {
                    &[row.split(',').next().unwrap(), "y"]
                };
                let got = check_with(file.as_bytes(), &cols, &WINDOWS, &[usize::MAX]);
                assert_eq!(
                    got,
                    if want.is_empty() {
                        vec![]
                    } else {
                        rows(&[want])
                    },
                    "{file:?}"
                );
            }
        }
    }

    #[test]
    fn header_maps_columns_by_name() {
        let file = b"\"v\",extra,\"\",id\n1,x,y,a\n\"2\",x,y,\"b\"\n3,x\"y,z,c\n";
        assert_eq!(check(file, &["id", "v"]), rows(&[&["a", "1"], &["b", "2"]]));
        let shuffled =
            "price_cents,ts,event_id,product_id,user_id,event_type,ingest_ts\n9,t,e,p,u,view,i\n";
        assert_eq!(
            check(shuffled.as_bytes(), &COLS7),
            rows(&[&["e", "u", "t", "i", "view", "p", "9"]])
        );
        // a selected subset of a wider header, in order: the general path
        let wide = "a,b,c\n1,2,3\n1,2\n";
        assert_eq!(check(wide.as_bytes(), &["a", "b"]), rows(&[&["1", "2"]]));
    }

    #[test]
    fn exhaustive_short_bodies() {
        const ALPHABET: [u8; 4] = *b"a,\"\n";
        let headers: [(&str, &[&str]); 6] = [
            ("a", &["a"]),
            ("a,b", &["a", "b"]),
            ("a,b,c", &["a", "b", "c"]),
            ("a,b,c", &["c", "a"]),
            ("a,b,c,d", &["a", "b", "c", "d"]),
            ("a,b,c,d,e,f,g", &["a", "b", "c", "d", "e", "f", "g"]),
        ];
        for len in 0..=7u32 {
            for code in 0..4usize.pow(len) {
                let body: Vec<u8> = (0..len)
                    .map(|i| ALPHABET[code / 4usize.pow(i) % 4])
                    .collect();
                for (h, cols) in headers {
                    let mut file = format!("{h}\n").into_bytes();
                    file.extend_from_slice(&body);
                    check_with(&file, cols, &[64], &[usize::MAX]);
                }
            }
        }
    }

    #[test]
    fn random_dirty_files() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let atoms: [&str; 12] = [
            "u",
            "p123",
            "",
            "2026-10-03T19:58:10-04:00",
            "1791071890000",
            " x ",
            "\r",
            "\"",
            "\"\"",
            "view",
            "a\"b",
            "\"\"\"",
        ];
        for round in 0..400 {
            let n = [2, 4, 7, 3, 1][round % 5];
            let cols: Vec<&str> = COLS7[..n.min(7)].to_vec();
            let mut header = cols.clone();
            if round % 3 == 1 {
                header.reverse();
            }
            if round % 7 == 2 {
                header.push("extra");
            }
            let mut file = header.join(",") + "\n";
            for _ in 0..rng.below(120) {
                let fields = match rng.below(40) {
                    0 => header.len() + 1,
                    1 => header.len().saturating_sub(1),
                    2 => 0,
                    _ => header.len(),
                };
                let mut line: Vec<String> = Vec::new();
                for _ in 0..fields {
                    let mut v = if rng.below(50) == 0 {
                        "w".repeat(rng.below(300))
                    } else {
                        let pool = if rng.below(8) == 0 { atoms.len() } else { 6 };
                        atoms[rng.below(pool)].to_string()
                    };
                    if rng.below(5) == 0 {
                        v = format!("\"{v}\"");
                    }
                    line.push(v);
                }
                file += &line.join(",");
                file += "\n";
            }
            if rng.below(4) == 0 {
                file.pop();
            }
            check_with(file.as_bytes(), &cols, &WINDOWS, &[1, 97, usize::MAX]);
        }
    }
}
