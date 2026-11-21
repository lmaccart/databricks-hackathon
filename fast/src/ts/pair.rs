//! Decode the two ISO clocks already present in a row. The CSV scanner still
//! owns quoting and row boundaries; every load here stays within its field.

use super::{parse_ingest, parse_ts};

type ParsePair = unsafe fn(&[u8], &[u8]) -> (i64, i64);

#[derive(Clone, Copy)]
pub struct EventClock(ParsePair);

impl EventClock {
    #[inline(always)]
    pub fn parse(self, ts: &[u8], ingest: &[u8]) -> (i64, i64) {
        // SAFETY: event_clock only installs kernels supported by the CPU.
        unsafe { (self.0)(ts, ingest) }
    }

    #[cfg(all(test, target_arch = "x86_64"))]
    pub(super) fn is_avx2(self) -> bool {
        std::ptr::fn_addr_eq(self.0, event_pair_avx2 as ParsePair)
    }
}

#[inline]
pub fn event_clock() -> EventClock {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        return EventClock(event_pair_avx2);
    }
    EventClock(scalar)
}

fn scalar(ts: &[u8], ingest: &[u8]) -> (i64, i64) {
    (parse_ts(ts), parse_ingest(ingest))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn event_pair_avx2(ts: &[u8], ingest: &[u8]) -> (i64, i64) {
    if ingest.len() == 20 && matches!(ts.len(), 20 | 25) {
        // SAFETY: lengths checked above; event_clock requires AVX2.
        if let Some(pair) = unsafe { try_pair_avx2(ts, ingest) } {
            return pair;
        }
        return rejected_iso(ts, ingest);
    }
    // F3 is common, not a cold branch. Leave its arbitrary-length spellings to
    // the existing word parser rather than staging or padding another lane.
    scalar(ts, ingest)
}

#[cfg(target_arch = "x86_64")]
#[cold]
fn rejected_iso(ts: &[u8], ingest: &[u8]) -> (i64, i64) {
    scalar(ts, ingest)
}

#[cfg(target_arch = "x86_64")]
use {super::iso_ms, crate::BAD, std::arch::x86_64::*};

/// `None` means the pair needs the scalar parser (possibly a padded F3).
/// SAFETY: CPU must support AVX2; inputs must be 20/25 bytes and 20 bytes.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
pub(super) unsafe fn try_pair_avx2(ts: &[u8], ingest: &[u8]) -> Option<(i64, i64)> {
    const HEAD: [u8; 16] = *b"0000-00-00T00:00";
    const HEAD_MAX: [u8; 16] = [9, 9, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9];
    const F1: [u8; 16] = *b":00Z\0\0\0\0\0\0\0\0\0\0\0\0";
    const F1_MAX: [u8; 16] = [0, 9, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    const F2_MAX: [u8; 16] = [0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 0, 0, 0, 0, 0, 0];
    const FROM_HEAD: [u8; 16] = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 128, 128, 128, 128];
    const FROM_TAIL: [u8; 16] = [
        128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 1, 2, 128, 128,
    ];
    debug_assert!(matches!(ts.len(), 20 | 25) && ingest.len() == 20);
    // SAFETY: length precondition bounds all loads, including the overlapping
    // F2 load of bytes 9..25. The unaligned loads do not read CSV delimiters.
    unsafe {
        let load = |a: &[u8; 16]| _mm_loadu_si128(a.as_ptr().cast());
        let join = |event, ingest| _mm256_set_m128i(ingest, event);
        let head = join(
            _mm_loadu_si128(ts.as_ptr().cast()),
            _mm_loadu_si128(ingest.as_ptr().cast()),
        );
        let f1_tail = |s: &[u8]| _mm_cvtsi32_si128(s.as_ptr().add(16).cast::<i32>().read_unaligned());
        let (tail, template, max) = if ts.len() == 25 {
            let template = match ts[19] {
                b'+' => b":00+00:00\0\0\0\0\0\0\0",
                b'-' => b":00-00:00\0\0\0\0\0\0\0",
                _ => return None,
            };
            let tail = _mm_srli_si128::<7>(_mm_loadu_si128(ts.as_ptr().add(9).cast()));
            (tail, load(template), load(&F2_MAX))
        } else {
            (f1_tail(ts), load(&F1), load(&F1_MAX))
        };
        let h = _mm256_xor_si256(head, _mm256_broadcastsi128_si256(load(&HEAD)));
        let t = _mm256_xor_si256(join(tail, f1_tail(ingest)), join(template, load(&F1)));
        // XOR with '0' is 0..9 only for ASCII digits; literals must XOR to 0.
        let excess = _mm256_or_si256(
            _mm256_subs_epu8(h, _mm256_broadcastsi128_si256(load(&HEAD_MAX))),
            _mm256_subs_epu8(t, join(max, load(&F1_MAX))),
        );
        if _mm256_testz_si256(excess, excess) == 0 {
            return None;
        }

        let digits = _mm256_or_si256(
            _mm256_shuffle_epi8(h, _mm256_broadcastsi128_si256(load(&FROM_HEAD))),
            _mm256_shuffle_epi8(t, _mm256_broadcastsi128_si256(load(&FROM_TAIL))),
        );
        let pairs = _mm256_maddubs_epi16(digits, _mm256_set1_epi16(0x010a));
        // Per lane, form [year, month << 8 | day, hour << 8 | minute, second].
        let coeff = _mm_setr_epi16(100, 1, 256, 1, 256, 1, 1, 0);
        let fields = _mm256_madd_epi16(pairs, _mm256_broadcastsi128_si256(coeff));
        let finish = |v: __m128i, offset| {
            let y = _mm_cvtsi128_si32(v) as i64;
            let md = _mm_extract_epi32::<1>(v) as i64;
            let hm = _mm_extract_epi32::<2>(v) as i64;
            let sec = _mm_extract_epi32::<3>(v) as i64;
            iso_ms(y, md >> 8, md & 255, hm >> 8, hm & 255, sec, offset)
        };
        let offset = if ts.len() == 25 {
            // The tail's syntax has already passed the joint check.
            let hh = (ts[20] - b'0') as i64 * 10 + (ts[21] - b'0') as i64;
            let mm = (ts[23] - b'0') as i64 * 10 + (ts[24] - b'0') as i64;
            if hh > 23 || mm > 59 {
                return None;
            }
            let minutes = hh * 60 + mm;
            if ts[19] == b'-' { -minutes } else { minutes }
        } else {
            0
        };
        let ts = finish(_mm256_castsi256_si128(fields), offset);
        let ingest = finish(_mm256_extracti128_si256::<1>(fields), 0);
        (ts != BAD && ingest != BAD).then_some((ts, ingest))
    }
}
