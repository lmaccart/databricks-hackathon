//! Shared guards for canonical, one-physical-line parsing.
use crate::{csv, person};

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGH: u64 = ONES << 7;
const FLOOR: u64 = ONES * b'-' as u64;

#[inline(always)]
pub(super) fn quoted(s: &[u8]) -> (bool, &[u8]) {
    match s.strip_prefix(b"\"") {
        Some(body) => (true, body),
        None => (false, s),
    }
}

#[inline(always)]
pub(super) fn comma(s: &[u8], q: bool) -> Option<&[u8]> {
    s.strip_prefix(if q { b"\"," as &[u8] } else { b"," })
}

/// The tight ID path uses the same load to prove ASCII and construct the
/// inline token. `'-'..=127` excludes all three CSV structural bytes and NUL;
/// IDs with any other spelling are still handled by the ordinary codec.
#[inline(always)]
pub(super) fn id<const END: u8>(s: &[u8]) -> Option<(u64, &[u8])> {
    let (q, b) = quoted(s);
    let b11 = b.first_chunk::<11>()?;
    let w = u64::from_le_bytes(b11[..8].try_into().unwrap());
    let end = if q { b'"' } else { END };
    let tail = b11[8];
    if b11[9] == end && (b'-'..=127).contains(&tail) {
        if ((w.wrapping_sub(FLOOR) | w) & HIGH) != 0 || (q && b11[10] != END) {
            return None;
        }
        return Some((person::pack_inline(w.swap_bytes(), tail), &b[10 + usize::from(q)..]));
    }
    // Search the two shorter boundary positions. The subtract-based
    // zero mask can have borrow artifacts after a match, never before it.
    let v = u64::from(u16::from_le_bytes(b11[7..9].try_into().unwrap())) ^ (ONES * u64::from(end));
    let hits = v.wrapping_sub(ONES) & !v & (HIGH & 0x0000_ffff);
    let n = 7 + (hits.trailing_zeros() as usize / 8);
    if n > 8 {
        return None;
    }
    let more = n + usize::from(q) + 1;
    if q && b11[n + 1] != END {
        return None;
    }
    let active = if n == 7 { 0x00ff_ffff_ffff_ffff } else { u64::MAX };
    // If any byte underflows, the first offending byte either raises its
    // high bit on subtract or already had its high bit set.
    if ((w.wrapping_sub(FLOOR) | w) & HIGH & active) != 0 {
        return None;
    }
    Some((person::pack_inline((w & active).swap_bytes(), 0), &b[more..]))
}

/// Retry only the original physical line, including its newline if present.
#[cold]
#[inline(never)]
pub(super) fn fallback<'a, const N: usize>(s: &'a [u8], mut f: impl FnMut(&[&'a [u8]; 8])) -> &'a [u8] {
    let end = memchr::memchr(b'\n', s).map_or(s.len(), |i| i + 1);
    let (line, rest) = s.split_at(end);
    csv::identity_row::<N>(line, &mut f);
    rest
}
