//! Fixed-width, in-order event/click layouts. Keep the general scanner for
//! arbitrary column maps and as the exact recovery path for irregular input.

use std::arch::{is_x86_feature_detected, x86_64::*};
use std::mem::MaybeUninit;
use std::sync::OnceLock;

pub(super) fn supported() -> bool {
    is_x86_feature_detected!("avx2")
        && is_x86_feature_detected!("avx512f")
        && is_x86_feature_detected!("avx512bw")
        && is_x86_feature_detected!("avx512vl")
        && is_x86_feature_detected!("avx512vbmi2")
        && is_x86_feature_detected!("pclmulqdq")
        && is_x86_feature_detected!("popcnt")
}

pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| supported() && std::env::var_os("SS_DISABLE_AVX512_CSV").is_none())
}

const BATCH: usize = 256; // the largest local offset fits in a packed byte
const OFFSETS: [u8; BATCH] = {
    let mut offsets = [0; BATCH];
    let mut i = 0;
    while i < BATCH {
        offsets[i] = i as u8;
        i += 1;
    }
    offsets
};

#[derive(Default)]
struct QuoteState {
    prev_sep: bool,
    inside: bool,
    bad: bool,
}

impl QuoteState {
    #[inline]
    #[target_feature(enable = "pclmulqdq")]
    fn advance(&mut self, sep: u64, quotes: u64, next_sep: bool, last_bit: u32) {
        // Quotes may occur only immediately after/before separators. Treat
        // the beginning of the window as after a newline.
        let starts = (sep << 1) | u64::from(self.prev_sep);
        let closes = (sep >> 1) | (u64::from(next_sep) << last_bit);
        // Multiplying by all ones in GF(2) yields the inclusive prefix XOR.
        // No comma/newline may occur inside quotes, even when a wrap spans
        // blocks. This also rejects a single quote in an otherwise empty field.
        let parity = _mm_cvtsi128_si64(_mm_clmulepi64_si128::<0>(
            _mm_cvtsi64_si128(quotes as i64),
            _mm_set1_epi32(-1),
        )) as u64
            ^ 0u64.wrapping_sub(u64::from(self.inside));
        self.bad |= (quotes & !(starts | closes)) | (sep & parity) != 0;
        self.prev_sep = (sep >> last_bit) & 1 != 0;
        self.inside = (parity >> last_bit) & 1 != 0;
    }
}

#[cold]
#[inline(never)]
#[target_feature(enable = "avx2")]
fn recover<'a, const N: usize, F: FnMut(&[&'a [u8]; 8])>(data: &'a [u8], f: &mut F) {
    // SAFETY: this function also requires AVX2.
    unsafe { super::scan_avx2(data, &[0, 1, 2, 3, 4, 5, 6][..N], f) };
}

/// Each window starts on a line boundary. Two compact lists let us validate a
/// row by comparing its Nth separator with the next newline, not by walking
/// separator bits or loading each one to tell commas and newlines apart.
///
/// SAFETY: all target features must be available (see `supported`). N must
/// be 4 or 7 and all columns must be selected in file order.
#[target_feature(enable = "avx2,avx512f,avx512bw,avx512vl,avx512vbmi2,pclmulqdq,popcnt")]
pub(super) unsafe fn scan<'a, const N: usize, const COUNT: bool, F: FnMut(&[&'a [u8]; 8])>(
    mut data: &'a [u8],
    f: &mut F,
) -> usize {
    let mut separators = [MaybeUninit::<u8>::uninit(); BATCH];
    let mut newlines = [MaybeUninit::<u8>::uninit(); BATCH];
    let mut fast_rows = 0;
    while data.len() >= 32 {
        let len = data.len().min(BATCH);
        let wide = len / 64 * 64;
        let mut ns = 0;
        let mut nl = 0;
        let mut quote = QuoteState {
            prev_sep: true,
            ..QuoteState::default()
        };
        let next_sep = |p| matches!(data.get(p), Some(b',' | b'\n'));
        for base in (0..wide).step_by(64) {
            // SAFETY: there are 64 readable bytes starting at base in both
            // arrays. Each compress writes at most 64 bytes and the existing
            // counts are at most base.
            unsafe {
                let v = _mm512_loadu_si512(data.as_ptr().add(base).cast());
                let positions = _mm512_loadu_si512(OFFSETS.as_ptr().add(base).cast());
                let lines = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'\n' as i8));
                let sep = lines | _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b',' as i8));
                let q = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'"' as i8));
                _mm512_mask_compressstoreu_epi8(separators.as_mut_ptr().add(ns).cast(), sep, positions);
                _mm512_mask_compressstoreu_epi8(newlines.as_mut_ptr().add(nl).cast(), lines, positions);
                ns += sep.count_ones() as usize;
                nl += lines.count_ones() as usize;
                quote.advance(sep, q, next_sep(base + 64), 63);
            }
        }
        if len - wide >= 32 {
            // SAFETY: as above, with 32 readable/writable bytes.
            unsafe {
                let v = _mm256_loadu_si256(data.as_ptr().add(wide).cast());
                let positions = _mm256_loadu_si256(OFFSETS.as_ptr().add(wide).cast());
                let lines = _mm256_cmpeq_epi8_mask(v, _mm256_set1_epi8(b'\n' as i8));
                let sep = lines | _mm256_cmpeq_epi8_mask(v, _mm256_set1_epi8(b',' as i8));
                let q = _mm256_cmpeq_epi8_mask(v, _mm256_set1_epi8(b'"' as i8));
                _mm256_mask_compressstoreu_epi8(separators.as_mut_ptr().add(ns).cast(), sep, positions);
                _mm256_mask_compressstoreu_epi8(newlines.as_mut_ptr().add(nl).cast(), lines, positions);
                ns += sep.count_ones() as usize;
                nl += lines.count_ones() as usize;
                quote.advance(sep as u64, q as u64, next_sep(wide + 32), 31);
            }
        }

        // SAFETY: the stores initialized exactly the prefixes of lengths ns and nl.
        let ends = unsafe { std::slice::from_raw_parts(separators.as_ptr().cast::<u8>(), ns) };
        let lines = unsafe { std::slice::from_raw_parts(newlines.as_ptr().cast::<u8>(), nl) };
        let Some(&last) = lines.last() else {
            // No complete line in the window. Recover just this line, however
            // long; a final unterminated line also uses the original scanner.
            let end = memchr::memchr(b'\n', data).map_or(data.len(), |p| p + 1);
            recover::<N, F>(&data[..end], f);
            data = &data[end..];
            continue;
        };
        let end = usize::from(last) + 1;
        let mut start = 0;
        if !quote.bad {
            for (group, &line_end) in ends.chunks_exact(N).zip(lines) {
                if group[N - 1] != line_end {
                    break;
                }
                let mut row = [&[][..]; 8];
                for (field, &field_end) in row[..N].iter_mut().zip(group) {
                    let field_end = usize::from(field_end);
                    // SAFETY: the sorted separators are in the loaded prefix,
                    // and the matching newline proves exactly N fields. At
                    // `start` a quote belongs to a validated pair, so the two
                    // adjustments cannot make the slice negative in length.
                    *field = unsafe {
                        let wrapped = usize::from(*data.get_unchecked(start) == b'"');
                        data.get_unchecked(start + wrapped..field_end - wrapped)
                    };
                    start = field_end + 1;
                }
                f(&row);
                if COUNT {
                    fast_rows += 1;
                }
            }
        }
        if start != end {
            recover::<N, F>(&data[start..end], f);
        }
        data = &data[end..];
    }
    if !data.is_empty() {
        recover::<N, F>(data, f);
    }
    fast_rows
}
