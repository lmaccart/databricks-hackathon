//! Optimistic fixed-order CSV decoder. Each attempt owns a single row, with no
//! structural positions carried from one row to the next. For events, split
//! after the first two fields so the rest usually fits a single 64-bit mask.

use memchr::memchr;
use std::arch::x86_64::*;

pub(super) fn supported() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
        && std::arch::is_x86_feature_detected!("bmi1")
        && std::arch::is_x86_feature_detected!("bmi2")
}

#[target_feature(enable = "avx2,bmi1,bmi2")]
#[inline]
unsafe fn masks(p: *const u8) -> (u32, u32, u32) {
    // SAFETY: caller supplies 32 readable bytes.
    let v = unsafe { _mm256_loadu_si256(p.cast()) };
    let eq = |b: i8| _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b))) as u32;
    (eq(b',' as i8), eq(b'"' as i8), eq(b'\n' as i8))
}

/// Prove whole-field quotes and return one bit per quoted field in field order.
/// `d` includes the last delimiter, `q` excludes everything from that delimiter on.
#[target_feature(enable = "bmi2")]
#[inline]
fn quotes(d: u64, q: u64) -> Option<u64> {
    let ends = _pext_u64(q << 1, d);
    let starts = (_pext_u64(q >> 1, d) << 1) | (q & 1);
    // A quote must occupy exactly one boundary: both would mean a lone quote
    // in a one-byte field. Extracting through the same delimiter mask aligns
    // the two boundary masks by field (including empty fields).
    (ends == starts && q & !(((d << 1) | 1) ^ (d >> 1)) == 0).then_some(ends)
}

/// A bounded suffix must have exactly four commas (events) or three (clicks),
/// followed by its first LF. Neither this proof nor the prefix loads inspect
/// past the available input slice, even on a final unterminated line.
#[target_feature(enable = "avx2,bmi1,bmi2")]
#[inline]
unsafe fn suffix<'a, const N: usize>(data: &'a [u8], row: &mut [&'a [u8]; 8]) -> Option<usize> {
    if data.len() < 32 {
        return None;
    }
    // SAFETY: each load has a local length check; the caller dispatched AVX2.
    let (cm, qt, nl) = unsafe { masks(data.as_ptr()) };
    let (cm, qt, nl) = if data.len() >= 64 {
        let (hi_cm, hi_qt, hi_nl) = unsafe { masks(data.as_ptr().add(32)) };
        (
            cm as u64 | ((hi_cm as u64) << 32),
            qt as u64 | ((hi_qt as u64) << 32),
            nl as u64 | ((hi_nl as u64) << 32),
        )
    } else {
        (cm as u64, qt as u64, nl as u64)
    };
    let e = nl.trailing_zeros();
    if e == 64 {
        return None;
    }
    let cm = _bzhi_u64(cm, e);
    let q = quotes(cm | (1 << e), _bzhi_u64(qt, e))?;
    let m1 = _blsr_u64(cm);
    let m2 = _blsr_u64(m1);
    let m3 = _blsr_u64(m2);
    // BLSR clears the lowest set bit; the residual checks prove the exact shape
    // before any fields are sliced, including adjacent commas and empty fields.
    let (a, b, c, d) = (
        cm.trailing_zeros(),
        m1.trailing_zeros(),
        m2.trailing_zeros(),
        m3.trailing_zeros(),
    );
    if N == 7 {
        if m3 == 0 || _blsr_u64(m3) != 0 {
            return None;
        }
        // SAFETY: four ordered commas before LF, and `quotes` proved each strip.
        unsafe {
            row[2] = value(data, 0, a, q);
            row[3] = value(data, a + 1, b, q >> 1);
            row[4] = value(data, b + 1, c, q >> 2);
            row[5] = value(data, c + 1, d, q >> 3);
            row[6] = value(data, d + 1, e, q >> 4);
        }
    } else {
        if m2 == 0 || m3 != 0 {
            return None;
        }
        // SAFETY: three ordered commas before LF, and `quotes` proved each strip.
        unsafe {
            row[0] = value(data, 0, a, q);
            row[1] = value(data, a + 1, b, q >> 1);
            row[2] = value(data, b + 1, c, q >> 2);
            row[3] = value(data, c + 1, e, q >> 3);
        }
    }
    Some(e as usize + 1)
}

#[inline(always)]
unsafe fn value(data: &[u8], start: u32, end: u32, quoted: u64) -> &[u8] {
    let q = (quoted & 1) as u32;
    // SAFETY: the caller proved the delimiter positions and both quote boundaries.
    unsafe { data.get_unchecked((start + q) as usize..(end - q) as usize) }
}

#[target_feature(enable = "avx2,bmi1,bmi2")]
#[inline]
unsafe fn attempt<'a, const N: usize>(data: &'a [u8], row: &mut [&'a [u8]; 8]) -> Option<usize> {
    if N == 4 {
        return unsafe { suffix::<N>(data, row) };
    }
    if data.len() < 32 {
        return None;
    }
    // SAFETY: 32 bytes remain, and this function has the required features.
    let (cm, qt, nl) = unsafe { masks(data.as_ptr()) };
    let a = cm.trailing_zeros();
    let b = _blsr_u32(cm).trailing_zeros();
    if b >= nl.trailing_zeros() {
        return None;
    }
    let q = quotes((1 << a) | (1 << b), _bzhi_u32(qt, b) as u64)?;
    let head = b as usize + 1;
    // SAFETY: two ordered commas within the first vector, with both strips proved.
    unsafe {
        row[0] = value(data, 0, a, q);
        row[1] = value(data, a + 1, b, q >> 1);
        suffix::<N>(data.get_unchecked(head..), row).map(|tail| head + tail)
    }
}

#[cold]
fn fallback<'a, const N: usize>(data: &'a [u8], row: &mut [&'a [u8]; 8]) -> (usize, bool) {
    let end = memchr(b'\n', data).unwrap_or(data.len());
    let len = end + usize::from(end != data.len());
    let mut fields = data[..end].split(|&b| b == b',');
    for dst in &mut row[..N] {
        let Some(field) = fields.next() else {
            return (len, false);
        };
        *dst = if memchr(b'"', field).is_none() {
            field
        } else if let [b'"', inner @ .., b'"'] = field
            && memchr(b'"', inner).is_none()
        {
            inner
        } else {
            return (len, false);
        };
    }
    (len, fields.next().is_none())
}

#[target_feature(enable = "avx2,bmi1,bmi2")]
pub(super) unsafe fn scan<'a, const N: usize, F: FnMut(&[&'a [u8]; 8])>(mut data: &'a [u8], f: &mut F) {
    let mut row = [&[] as &[u8]; 8];
    while !data.is_empty() {
        // SAFETY: this function required the same runtime features.
        let step = unsafe { attempt::<N>(data, &mut row) };
        let n = if let Some(n) = step {
            f(&row);
            n
        } else {
            let (n, ok) = fallback::<N>(data, &mut row);
            if ok {
                f(&row);
            }
            n
        };
        // SAFETY: both decoders return a byte count within the slice.
        data = unsafe { data.get_unchecked(n..) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_and_misses_are_not_just_fallback_parity() {
        if !supported() {
            return;
        }
        let ts = "t".repeat(23);
        for width in 0..=31 {
            let id = "x".repeat(width);
            // Trailing bytes belong to subsequent rows and must not affect the
            // masks. Place a closing quote and LF on both sides of a vector edge.
            let click = format!("\"{id}\",,c,\"{ts}\"\n,,,,\"{}\n", "x".repeat(64));
            let mut row = [&[] as &[u8]; 8];
            let n = unsafe { attempt::<4>(click.as_bytes(), &mut row) };
            assert_eq!(n, click.find('\n').map(|p| p + 1));
            assert_eq!(&row[..4], &[id.as_bytes(), b"", b"c", ts.as_bytes()]);
        }
        let mut row = [&[] as &[u8]; 8];
        let event = b"\"event_id\",\"user_id\",1791071890000,\"2026-10-03T23:58:10Z\",view,p,\"\"\n\
                      ,,,,,,,,,,,\"\"\"\"\"\"\"\"\"\n";
        assert_eq!(
            unsafe { attempt::<7>(event, &mut row) },
            memchr(b'\n', event).map(|p| p + 1)
        );
        assert_eq!(row[3], b"2026-10-03T23:58:10Z");
        for data in [b"a,b,c,d\n" as &[u8], b"a,b,c,\"\"", b"\"a,b,c,d\n", b"a,b,c,d,e\n"] {
            assert_eq!(unsafe { attempt::<4>(data, &mut row) }, None);
        }
        for data in [
            b"a,b,c,\"\n" as &[u8],
            b"a,b,c,\"\"\"\n",
            b"\"a\" ,b,c,d\n",
            b"a,b,c,d,e\n",
            b"a,b,c\n",
        ] {
            let mut padded = data.to_vec();
            padded.extend([b','; 64]);
            assert_eq!(unsafe { attempt::<4>(&padded, &mut [&[]; 8]) }, None);
        }
    }
}
