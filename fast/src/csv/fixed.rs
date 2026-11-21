//! Mask-space validation and direct field extraction for identity-mapped
//! event/click headers. Column data is never copied.

use super::Chunk;

#[inline]
pub(super) fn identity<const N: usize>(c: &Chunk) -> bool {
    c.ncols == N && c.nfields == N && c.idx[..N].iter().copied().eq(0..N)
}

#[derive(Clone, Copy, Default)]
pub(super) struct Mask {
    sep: u64,
    nl: u64,
    quote: u64,
}

pub(super) trait Classify {
    /// SAFETY: `p` must point to 64 readable bytes.
    unsafe fn mask(p: *const u8) -> Mask;

    #[cold]
    #[inline(never)]
    fn tail(rest: &[u8]) -> Mask {
        let mut buf = [0u8; 64];
        buf[..rest.len()].copy_from_slice(rest);
        buf[rest.len()] = b'\n'; // synthetic separator for an unterminated last line
        // SAFETY: `buf` holds 64 bytes.
        unsafe { Self::mask(buf.as_ptr()) }
    }
}

#[cfg(target_arch = "x86_64")]
struct Avx2;

#[cfg(target_arch = "x86_64")]
impl Classify for Avx2 {
    #[inline(always)]
    unsafe fn mask(p: *const u8) -> Mask {
        // SAFETY: `p` has 64 readable bytes, and dispatch checked for AVX2.
        unsafe { avx2_mask(p) }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn avx2_mask(p: *const u8) -> Mask {
    use std::arch::x86_64::*;
    // SAFETY: both 32-byte unaligned loads are within the caller's readable block.
    unsafe {
        let cm = _mm256_set1_epi8(b',' as i8);
        let nl = _mm256_set1_epi8(b'\n' as i8);
        let qt = _mm256_set1_epi8(b'"' as i8);
        let lo = _mm256_loadu_si256(p.cast());
        let hi = _mm256_loadu_si256(p.add(32).cast());
        let pair = |lo, hi| (_mm256_movemask_epi8(lo) as u32 as u64) | ((_mm256_movemask_epi8(hi) as u32 as u64) << 32);
        let nl = pair(_mm256_cmpeq_epi8(lo, nl), _mm256_cmpeq_epi8(hi, nl));
        Mask {
            sep: nl | pair(_mm256_cmpeq_epi8(lo, cm), _mm256_cmpeq_epi8(hi, cm)),
            nl,
            quote: pair(_mm256_cmpeq_epi8(lo, qt), _mm256_cmpeq_epi8(hi, qt)),
        }
    }
}

#[cfg(test)]
pub(super) struct Scalar;

#[cfg(test)]
impl Classify for Scalar {
    unsafe fn mask(p: *const u8) -> Mask {
        let mut m = Mask::default();
        // SAFETY: caller supplies 64 readable bytes.
        for (i, &b) in unsafe { std::slice::from_raw_parts(p, 64) }.iter().enumerate() {
            match b {
                b'\n' => m.nl |= 1 << i,
                b',' => m.sep |= 1 << i,
                b'"' => m.quote |= 1 << i,
                _ => {}
            }
        }
        m.sep |= m.nl;
        m
    }
}

struct Block {
    sep: u64,
    nl: u64,
    bad: u64,
    quoted: u64,
}

// The previous byte may introduce or terminate a quoted field. `borrow`
// says the current field has an opening quote, even across empty masks.
struct Carry {
    sep: u64,
    close: u64,
    borrow: u64,
}

impl Carry {
    #[inline(always)]
    fn block<C: Classify>(&mut self, data: &[u8], base: usize) -> Block {
        let Mask { sep, nl, quote } = if data.len() - base >= 64 {
            // SAFETY: there are at least 64 readable bytes at `base`.
            unsafe { C::mask(data.as_ptr().add(base)) }
        } else {
            C::tail(&data[base..])
        };

        // Only a quote immediately after a separator (or at chunk start) can
        // open a field. All other quotes MUST immediately precede a separator.
        let open = quote & ((sep << 1) | self.sep);
        let close = quote ^ open;
        let after_close = (close << 1) | self.close;

        // Subtracting an opening bit borrows through that field's zero bits
        // and clears its first separator. An outstanding borrow is another
        // subtraction at bit 0; it and an opening quote at bit 0 are mutually
        // exclusive (one requires a preceding separator, the other forbids it).
        let (diff, borrow) = sep.overflowing_sub(open | self.borrow);
        let expected = sep & !diff;
        self.sep = sep >> 63;
        self.close = close >> 63;
        self.borrow = borrow as u64;

        // A close not followed by a separator marks the very next byte bad,
        // hence still the same row. At a separator, a close must be present
        // iff the field opened. A singleton quote is an open, never a close.
        Block {
            sep,
            nl,
            bad: expected ^ after_close,
            quoted: expected,
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi1,popcnt")]
pub(super) unsafe fn scan_avx2<'a, const N: usize, F: FnMut(&[&'a [u8]; 8])>(data: &'a [u8], f: &mut F) {
    scan::<N, Avx2, F>(data, f);
}

#[inline(always)]
pub(super) fn scan<'a, const N: usize, C: Classify, F: FnMut(&[&'a [u8]; 8])>(data: &'a [u8], f: &mut F) {
    let mut carry = Carry {
        sep: 1,
        close: 0,
        borrow: 0,
    };
    let mut row = [&[][..]; 8];
    let mut from = 0;
    let mut k = 0;
    let mut base = 0;
    while base <= data.len() {
        let Block {
            mut sep,
            mut nl,
            mut bad,
            quoted,
        } = carry.block::<C>(data, base);
        while sep | bad != 0 {
            let end_bit = nl & nl.wrapping_neg();
            // If no newline is available yet, the complete block belongs to
            // this line. A field can extend over any number of empty blocks.
            let before = end_bit.wrapping_sub(1);
            let through = before | end_bit;
            let commas = sep & before;
            if k + (commas.count_ones() as usize) < N && bad & through == 0 {
                // SAFETY: comma bits are in bounds and in order. The count
                // check bounds writes to the first N - 1 slots. Quote status
                // is final at each separator, even across block boundaries.
                unsafe { take(data, &mut row, &mut k, &mut from, commas, quoted, base) };
            } else {
                k = N; // malformed row, including across block boundaries
            }
            if end_bit == 0 {
                break;
            }
            let end = base + end_bit.trailing_zeros() as usize;
            if k == N - 1 {
                let q = (quoted & end_bit != 0) as usize;
                // SAFETY: `end` is the newline or EOF, and `q` a validated
                // quote pair. N is 4 or 7. N - 1 commas also excludes empty lines.
                unsafe { *row.get_unchecked_mut(N - 1) = value(data, from, end, q) };
                f(&row);
            }
            from = end + 1;
            k = 0;
            nl ^= end_bit;
            sep &= !through;
            bad &= !through;
        }
        base += 64;
    }
}

/// SAFETY: all bits point to commas of the line in `data` at or after `from`;
/// those fields have valid quotes; there is room for `bits.count_ones()` slices.
#[inline(always)]
unsafe fn take<'a>(
    data: &'a [u8],
    row: &mut [&'a [u8]; 8],
    k: &mut usize,
    from: &mut usize,
    mut bits: u64,
    quoted: u64,
    base: usize,
) {
    while bits != 0 {
        let at = bits.trailing_zeros();
        let to = base + at as usize;
        let quoted = ((quoted >> at) & 1) as usize;
        bits &= bits - 1;
        // SAFETY: per caller, this is a bounded, validated field and row slot.
        unsafe { *row.get_unchecked_mut(*k) = value(data, *from, to, quoted) };
        *from = to + 1;
        *k += 1;
    }
}

/// SAFETY: `from <= to <= data.len()`, and `quoted` is 1 for a validated
/// quote pair, 0 otherwise.
#[inline(always)]
unsafe fn value(data: &[u8], from: usize, to: usize, quoted: usize) -> &[u8] {
    // SAFETY: a quoted field has at least two bytes, an unquoted field uses
    // the given bounds unchanged.
    unsafe { data.get_unchecked(from + quoted..to - quoted) }
}
