use super::*;
use std::arch::x86_64::*;
use std::mem::transmute;

pub(super) fn available() -> bool {
    is_x86_feature_detected!("avx512vbmi")
        && is_x86_feature_detected!("avx512bw")
        && is_x86_feature_detected!("bmi2")
        && is_x86_feature_detected!("popcnt")
}

#[target_feature(enable = "avx512vbmi,avx512bw,bmi2,popcnt")]
pub(super) unsafe fn scan_native<'a, F: FnMut(Row<'a>)>(c: &Chunk<'a>, clock: &ManifestClock, f: &mut F) {
    scan::<Native, F>(c, clock, f);
}

struct Native;

impl Kernel for Native {
    type Block = (__m512i, __m512i);

    #[inline]
    fn load(data: &[u8; 128]) -> (Self::Block, u128, u128, u128) {
        // SAFETY: Native is private to the feature-checked kernel.
        unsafe { load(data) }
    }

    #[inline]
    fn deposit(bits: u64, mask: u64) -> u64 {
        // SAFETY: Native is private to the feature-checked kernel.
        unsafe { _pdep_u64(bits, mask) }
    }

    #[inline]
    fn extract(bits: u64, mask: u64) -> u64 {
        // SAFETY: Native is private to the feature-checked kernel.
        unsafe { _pext_u64(bits, mask) }
    }

    #[inline]
    fn gather(block: Self::Block, starts: [u8; 4], keep: u64, f3: bool) -> Gathered {
        // SAFETY: Native is private to the feature-checked kernel.
        unsafe { gather(block, starts, keep, f3) }
    }
}

#[inline]
#[target_feature(enable = "avx512vbmi,avx512bw,bmi2,popcnt")]
unsafe fn load(data: &[u8; 128]) -> ((__m512i, __m512i), u128, u128, u128) {
    // SAFETY: both reads are wholly inside the 128-byte array reference.
    let (a, b) = unsafe {
        (
            _mm512_loadu_si512(data.as_ptr().cast()),
            _mm512_loadu_si512(data.as_ptr().add(64).cast()),
        )
    };
    let mask = |byte| {
        let v = _mm512_set1_epi8(byte);
        u128::from(_mm512_cmpeq_epi8_mask(a, v)) | (u128::from(_mm512_cmpeq_epi8_mask(b, v)) << 64)
    };
    ((a, b), mask(b'\n' as i8), mask(b',' as i8), mask(b'"' as i8))
}

#[inline]
#[target_feature(enable = "avx512vbmi,avx512bw,bmi2,popcnt")]
unsafe fn gather((a, b): (__m512i, __m512i), [e, u, i, t]: [u8; 4], keep: u64, f3: bool) -> Gathered {
    // Repeat the four starts in each 128-bit lane, then distribute them
    // to the corresponding bytes of the canonical row with one shuffle.
    let bases = _mm512_set1_epi32(i32::from_le_bytes([e, u, i, t]));
    // SAFETY: equal-size conversion; every integer bit pattern is valid.
    let shuffle = unsafe { transmute::<[u64; 8], __m512i>(BASES) };
    let starts = _mm512_shuffle_epi8(bases, shuffle);
    let layout = if f3 { F3_LAYOUT } else { LAYOUT };
    // SAFETY: equal-size conversion; every integer bit pattern is valid.
    let offsets = unsafe { transmute::<[u64; 8], __m512i>(layout) };
    let perm = _mm512_add_epi8(starts, offsets);
    let mut row = _mm512_maskz_permutex2var_epi8(keep, a, perm, b);
    if f3 {
        row = _mm512_mask_mov_epi8(row, F3_PAD, _mm512_set1_epi8(b'0' as i8));
    }
    let d = _mm512_sub_epi8(row, _mm512_set1_epi8(b'0' as i8));
    let digit_mask = _mm512_cmple_epu8_mask(d, _mm512_set1_epi8(9));
    let syntax_mask = _mm512_cmpeq_epi8_mask(row, _mm512_set1_epi64(SYNTAX as i64));
    let pairs = _mm512_maddubs_epi16(d, _mm512_set1_epi16(0x010a));
    // SAFETY: equal-size conversions; every integer bit pattern is valid.
    let mut words = unsafe { transmute::<__m512i, [u64; 8]>(row) };
    let mut terms = unsafe { transmute::<__m512i, [u64; 8]>(pairs) };
    if f3 {
        let quads = _mm512_madd_epi16(pairs, _mm512_set1_epi32(0x0001_0064));
        let values = unsafe { transmute::<__m512i, [u64; 8]>(quads) };
        words[3] = values[3];
        terms[5] = values[5];
    }
    Gathered {
        words,
        pairs: [terms[4], terms[5]],
        digit_mask,
        syntax_mask,
    }
}
