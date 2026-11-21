//! Timestamps of rules.md: F1 `YYYY-MM-DDTHH:MM:SSZ`, F2 with a `±HH:MM` offset,
//! F3 epoch milliseconds (digits only, leading zeros allowed).
//!
//! F1/F2 have fixed byte positions, so each 8-byte word is checked against a
//! template in one SWAR step: XOR with the template leaves 0..=9 in a digit byte
//! and 0 in a literal byte, and one add flags any byte above its limit.

use crate::BAD;

const LO7: u64 = 0x7F7F_7F7F_7F7F_7F7F;
const HI: u64 = 0x8080_8080_8080_8080;
/// Per-byte addend: a byte passes if it is at most 9 (digit) or exactly 0 (literal).
const D: u8 = 0x7F - 9;
const L: u8 = 0x7F;

const fn word(b: &[u8; 8]) -> u64 {
    u64::from_le_bytes(*b)
}

const T_DATE: u64 = word(b"0000-00-"); // bytes 0..8   YYYY-MM-
const A_DATE: u64 = word(&[D, D, D, D, L, D, D, L]);
const T_TIME: u64 = word(b"00T00:00"); // bytes 8..16  DDTHH:MM
const A_TIME: u64 = word(&[D, D, L, D, D, L, D, D]);
const T_F1: u64 = word(b":00Z\0\0\0\0"); // bytes 16..20 :SSZ
const A_F1: u64 = word(&[L, D, D, L, 0, 0, 0, 0]);
const T_F2: u64 = word(&[b':', b'0', b'0', 0, b'0', b'0', b':', b'0']); // bytes 16..24 :SS±HH:M, sign apart
const A_F2: u64 = word(&[L, D, D, L, D, D, L, D]);

#[inline(always)]
fn fits(v: u64, add: u64) -> bool {
    ((((v & LO7) + add) | v) & HI) == 0
}

#[inline(always)]
fn load8(s: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(s[i..i + 8].try_into().unwrap())
}

/// Byte i of a template-XORed word: a digit value after `fits`.
#[inline(always)]
fn b(v: u64, i: u32) -> i64 {
    ((v >> (8 * i)) & 0xFF) as i64
}

/// Days since 1970-01-01, proleptic Gregorian (Hinnant), for years 0..=9999.
#[inline(always)]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y + 400 - (m <= 2) as i64; // one era later keeps every year positive
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era - 1) * 146_097 + doe - 719_468
}

/// F1 (`F2 == false`, 20 bytes) or F2 (25 bytes), or BAD.
#[inline(always)]
fn iso<const F2: bool, const SECONDS: bool>(s: &[u8]) -> i64 {
    let a = load8(s, 0) ^ T_DATE;
    let t = load8(s, 8) ^ T_TIME;
    let (z, ok, off_min) = if F2 {
        let sign = s[19];
        let z = load8(s, 16) ^ (T_F2 | (sign as u64) << 24);
        let last = s[24].wrapping_sub(b'0') as i64;
        let (oh, om) = (b(z, 4) * 10 + b(z, 5), b(z, 7) * 10 + last);
        let ok = fits(z, A_F2) & (last <= 9) & (sign == b'+' || sign == b'-') & (oh <= 23) & (om <= 59);
        (z, ok, if sign == b'-' { -(oh * 60 + om) } else { oh * 60 + om })
    } else {
        let z = u32::from_le_bytes(s[16..20].try_into().unwrap()) as u64 ^ T_F1;
        (z, fits(z, A_F1), 0)
    };
    if !(fits(a, A_DATE) & fits(t, A_TIME) & ok) {
        return BAD;
    }
    let y = b(a, 0) * 1000 + b(a, 1) * 100 + b(a, 2) * 10 + b(a, 3);
    let (mo, d) = (b(a, 5) * 10 + b(a, 6), b(t, 0) * 10 + b(t, 1));
    let (h, mi, se) = (b(t, 3) * 10 + b(t, 4), b(t, 6) * 10 + b(t, 7), b(z, 1) * 10 + b(z, 2));
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || se > 59 {
        return BAD;
    }
    const MD: [i64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays = if mo == 2 && leap { 29 } else { MD[mo as usize] };
    if d < 1 || d > mdays {
        return BAD;
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - off_min * 60;
    if SECONDS { secs } else { secs * 1000 }
}

/// Eight ASCII digits (first byte most significant) as a number, or None.
#[inline(always)]
fn eight(x: u64) -> Option<u64> {
    const F0: u64 = 0xF0F0_F0F0_F0F0_F0F0;
    if (x & F0) | ((x.wrapping_add(0x0606_0606_0606_0606) & F0) >> 4) != 0x3333_3333_3333_3333 {
        return None;
    }
    let v = x - 0x3030_3030_3030_3030;
    let v = v * 10 + (v >> 8); // byte 2k: two-digit pair k
    const M: u64 = 0x0000_00FF_0000_00FF;
    let v =
        ((v & M).wrapping_mul(100 + (1_000_000 << 32))).wrapping_add(((v >> 16) & M).wrapping_mul(1 + (10_000 << 32)));
    Some(v >> 32)
}

/// Up to 8 digits, or None.
#[inline(always)]
fn short(s: &[u8]) -> Option<u64> {
    let mut v = 0;
    for &c in s {
        let d = c.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        v = v * 10 + d as u64;
    }
    Some(v)
}

/// F3: digits only, any length. A value beyond i64 is outside every window: BAD.
#[inline(always)]
fn epoch_ms(s: &[u8]) -> i64 {
    if s.is_empty() {
        return BAD;
    }
    let mut s = s;
    if s.len() > 19 {
        s = &s[s.iter().position(|&c| c != b'0').unwrap_or(s.len())..];
        if s.len() > 19 {
            return BAD; // a non-digit or more than 19 significant digits: BAD either way
        }
    }
    let n = s.len();
    let v = match n {
        0..=8 => short(s),
        9..=16 => {
            // the first n-8 digits, left-padded with '0' to a full word
            let k = n - 8;
            let x = load8(s, 0);
            let hi = if k == 8 {
                x
            } else {
                (x << (8 * (8 - k))) | (0x3030_3030_3030_3030 >> (8 * k))
            };
            eight(hi).zip(eight(load8(s, k))).map(|(h, l)| h * 100_000_000 + l)
        }
        _ => {
            let k = n - 16; // 1..=3; the total stays below 10^19 < u64::MAX
            short(&s[..k])
                .zip(eight(load8(s, k)))
                .zip(eight(load8(s, k + 8)))
                .map(|((h, m), l)| (h * 100_000_000 + m) * 100_000_000 + l)
        }
    };
    match v {
        Some(v) if v <= i64::MAX as u64 => v as i64,
        _ => BAD,
    }
}

/// Epoch ms, or BAD. Mirrors reference/pipeline.py::parse_ts.
#[inline]
pub fn parse_ts(s: &[u8]) -> i64 {
    // s[4] == '-' rules out F3, so F1/F2 syntax decides alone
    match s.len() {
        20 if s[4] == b'-' => iso::<false, false>(s),
        25 if s[4] == b'-' => iso::<true, false>(s),
        _ => epoch_ms(s),
    }
}

/// `ingest_ts`, which rules.md guarantees is valid F1. A bad one sorts last
/// (i64::MAX), so such a copy can only lose the dedup or be late.
#[inline]
pub fn parse_ingest(s: &[u8]) -> i64 {
    match parse_ts(s) {
        BAD => i64::MAX,
        t => t,
    }
}

#[derive(Clone, Copy, Default)]
struct CivilDay {
    first: u64,
    last: u16,
    midnight_seconds: i64,
}

/// Immutable exact date lookup derived from the manifest. A miss always
/// falls back to the complete Gregorian parser, including for ingest claims.
pub struct ManifestClock {
    // The unchecked BCD nibbles are only an address; the stored date bytes
    // are authoritative, including for all non-ASCII input and colliding months.
    days: [CivilDay; 64],
    decimals: [(u64, i64); 256],
}

#[inline(always)]
fn day_slot(last: u16) -> usize {
    (((last & 3) << 4) | ((last >> 8) & 15)) as usize
}

#[inline(always)]
fn decimal_slot(prefix: u64) -> usize {
    (((prefix >> 20) & 0xf0) | ((prefix >> 32) & 0x0f)) as usize
}

impl ManifestClock {
    pub fn new(ws: i64, we: i64) -> Self {
        const DAY: i64 = 86_400_000;
        const LIMIT: i64 = 256;
        let first_valid = days_from_civil(0, 1, 1);
        let last_valid = days_from_civil(9999, 12, 31);
        let lo = ws
            .saturating_sub(8 * DAY)
            .div_euclid(DAY)
            .clamp(first_valid, last_valid);
        let hi = we
            .saturating_add(2 * DAY)
            .div_euclid(DAY)
            .clamp(first_valid, last_valid);
        // Ineligible entries have a tag impossible in the compared width.
        let mut clock = Self {
            days: [CivilDay {
                first: 0,
                last: 0,
                midnight_seconds: BAD,
            }; 64],
            decimals: [(u64::MAX, 0); 256],
        };
        let low = ws.saturating_sub(8 * DAY).max(1_000_000_000_000) / 100_000_000;
        let high = we.saturating_add(2 * DAY).min(9_999_999_999_999) / 100_000_000;
        if high >= low && high - low < 256 {
            for pre in low..=high {
                let mut v = [0u8; 8];
                v[..5].copy_from_slice(format!("{pre:05}").as_bytes());
                let tag = u64::from_le_bytes(v);
                clock.decimals[decimal_slot(tag)] = (tag, pre * 100_000_000);
            }
        }
        // Very wide windows get both ends, with all other dates using fallback.
        if hi - lo >= LIMIT {
            for day in lo..lo + LIMIT / 2 {
                clock.add_day(day);
            }
            for day in hi - LIMIT / 2 + 1..=hi {
                clock.add_day(day);
            }
        } else {
            for day in lo..=hi {
                clock.add_day(day);
            }
        }
        clock
    }

    fn add_day(&mut self, day: i64) {
        // Inverse proleptic Gregorian conversion, valid for years 0000..9999.
        let z = day + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = mp + if mp < 10 { 3 } else { -9 };
        let y = y + i64::from(m <= 2);
        let date = format!("{y:04}-{m:02}-{d:02}");
        let bytes = date.as_bytes();
        let last = u16::from_le_bytes([bytes[8], bytes[9]]);
        self.days[day_slot(last)] = CivilDay {
            first: load8(bytes, 0),
            last,
            midnight_seconds: day * 86_400,
        };
    }

    /// `s` has the F1 length and a date separator at byte 4.
    #[inline]
    fn iso_seconds<const OFFSET: bool>(&self, s: &[u8]) -> i64 {
        let last = u16::from_le_bytes([s[8], s[9]]);
        let entry = &self.days[day_slot(last)];
        // The caller checked s[4] == '-', so an uninitialized zero prefix cannot match.
        if entry.first != load8(s, 0) || entry.last != last {
            return iso::<OFFSET, true>(s);
        }
        let t = load8(s, 11) ^ word(b"00:00:00");
        if !(fits(t, word(&[D, D, L, D, D, L, D, D])) & (s[10] == b'T')) {
            return BAD;
        }
        // BCD-to-binary for all three byte pairs with one multiply-add.
        let pairs = t.wrapping_mul(256 * 10 + 1);
        let (h, m, sec) = (b(pairs, 1), b(pairs, 4), b(pairs, 7));
        if !(h < 24 && m < 60 && sec < 60) {
            return BAD;
        }
        let mut seconds = entry.midnight_seconds + h * 3600 + m * 60 + sec;
        if OFFSET {
            let sign = s[19];
            let z = load8(s, 16) ^ (T_F2 | u64::from(sign) << 24);
            let d = i64::from(s[24].wrapping_sub(b'0'));
            let (oh, om) = (b(z, 4) * 10 + b(z, 5), b(z, 7) * 10 + d);
            if !(fits(z, A_F2) & (d < 10) & (sign == b'+' || sign == b'-') & (oh < 24) & (om < 60)) {
                return BAD;
            }
            let offset = oh * 3600 + om * 60;
            seconds += if sign == b'-' { offset } else { -offset };
        } else if s[19] != b'Z' {
            return BAD;
        }
        seconds
    }

    #[inline]
    pub fn parse_ts(&self, s: &[u8]) -> i64 {
        if matches!(s.len(), 20 | 25) && s[4] == b'-' {
            let seconds = if s.len() == 20 {
                self.iso_seconds::<false>(s)
            } else {
                self.iso_seconds::<true>(s)
            };
            return if seconds == BAD { BAD } else { seconds * 1000 };
        }
        if s.len() == 13 {
            let prefix = load8(s, 0) & 0xff_ffff_ffff;
            let (tag, base) = self.decimals[decimal_slot(prefix)];
            if prefix == tag {
                return eight(load8(s, 5)).map_or(BAD, |n| base + n as i64);
            }
        }
        epoch_ms(s)
    }

    /// The F1 path supplies both exact milliseconds for the late check
    /// and seconds for the compact claim. Non-F1 input still uses the sidecar.
    #[inline]
    pub fn ingest_for_claim(&self, s: &[u8]) -> (i64, i64) {
        if s.len() == 20 && s[4] == b'-' {
            let seconds = self.iso_seconds::<false>(s);
            if seconds != BAD {
                return (seconds * 1000, seconds);
            }
            return (i64::MAX, BAD);
        }
        (parse_ingest(s), BAD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- literal transcription of reference/pipeline.py::parse_ts (Python ints -> i128)

    fn py_digits(b: &[u8]) -> bool {
        !b.is_empty() && b.iter().all(|&c| (0x30..=0x39).contains(&c))
    }

    fn py_int(b: &[u8]) -> i128 {
        b.iter().fold(0i128, |a, &c| a * 10 + (c - b'0') as i128)
    }

    fn py_days_from_civil(y: i128, m: i128, d: i128) -> i128 {
        let y = y - (m <= 2) as i128;
        let era = (if y >= 0 { y } else { y - 399 }).div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2).div_euclid(5) + d - 1;
        let doe = yoe * 365 + yoe.div_euclid(4) - yoe.div_euclid(100) + doy;
        era * 146097 + doe - 719468
    }

    fn py_parse_ts(s: &[u8]) -> Option<i128> {
        if s.is_empty() {
            return None;
        }
        if py_digits(s) {
            let z = s.iter().position(|&c| c != b'0').unwrap_or(s.len());
            let significant = &s[z..];
            return Some(if significant.len() <= 19 {
                py_int(significant)
            } else {
                10i128.pow(19)
            });
        }
        if s.len() != 20 && s.len() != 25 {
            return None;
        }
        if !(s[4] == b'-' && s[7] == b'-' && s[10] == b'T' && s[13] == b':' && s[16] == b':') {
            return None;
        }
        let parts = [&s[0..4], &s[5..7], &s[8..10], &s[11..13], &s[14..16], &s[17..19]];
        if !parts.iter().all(|p| py_digits(p)) {
            return None;
        }
        let [y, mo, d, h, mi, se] = parts.map(py_int);
        if !(1..=12).contains(&mo) || h > 23 || mi > 59 || se > 59 {
            return None;
        }
        const MD: [i128; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let mdays = if mo == 2 && leap { 29 } else { MD[mo as usize] };
        if !(1..=mdays).contains(&d) {
            return None;
        }
        let tail = &s[19..];
        let off_min = if tail == b"Z" {
            0
        } else if tail.len() == 6 && (tail[0] == b'+' || tail[0] == b'-') && tail[3] == b':' {
            let (oh, om) = (&tail[1..3], &tail[4..6]);
            if !(py_digits(oh) && py_digits(om)) {
                return None;
            }
            let (oh, om) = (py_int(oh), py_int(om));
            if oh > 23 || om > 59 {
                return None;
            }
            if tail[0] == b'-' { -(oh * 60 + om) } else { oh * 60 + om }
        } else {
            return None;
        };
        let secs = py_days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se;
        Some((secs - off_min * 60) * 1000)
    }

    /// The pipeline's view of the oracle: an instant that does not fit i64 is
    /// outside every window, which BAD also is.
    fn want(s: &[u8]) -> i64 {
        match py_parse_ts(s) {
            Some(v) if v <= i64::MAX as i128 => v as i64,
            _ => BAD,
        }
    }

    fn check(s: &[u8]) {
        assert_eq!(parse_ts(s), want(s), "{:?}", String::from_utf8_lossy(s));
        let ingest = if want(s) == BAD { i64::MAX } else { want(s) };
        assert_eq!(parse_ingest(s), ingest, "ingest {:?}", String::from_utf8_lossy(s));
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

    const BASES: [&str; 6] = [
        "2026-10-03T23:58:10Z",
        "2026-10-03T19:58:10-04:00",
        "2028-02-29T00:00:00+12:00",
        "1791071890000",
        "0001791071890000",
        "9223372036854775807",
    ];

    fn check_clock(clock: &ManifestClock, s: &[u8]) {
        let ms = want(s);
        let seconds = if ms != BAD && s.len() == 20 && s[4] == b'-' {
            ms / 1000
        } else {
            BAD
        };
        assert_eq!(clock.parse_ts(s), ms, "{s:?}");
        assert_eq!(
            clock.ingest_for_claim(s),
            (if ms == BAD { i64::MAX } else { ms }, seconds),
            "{s:?}"
        );
    }

    #[test]
    fn window_grammar_proves_bytes_on_both_sides_of_decimal_splices_and_zones() {
        let clock = ManifestClock::new(parse_ts(b"2028-02-25T00:00:00Z"), parse_ts(b"2028-03-06T00:00:00Z"));
        for base in [
            &b"2028-02-29T00:00:00-23:59"[..],
            &b"2028-03-01T23:59:59+00:00"[..],
            &b"1835395199999"[..],
            &b"1835400000000"[..],
        ] {
            for pos in 0..base.len() {
                let mut v = base.to_vec();
                for b in 0..=255 {
                    v[pos] = b;
                    check_clock(&clock, &v);
                }
            }
        }
        for n in [
            0i64,
            1,
            99_999_999,
            100_000_000,
            999_999_999_999,
            1_000_000_000_000,
            9_999_999_999_999,
        ] {
            check_clock(&clock, format!("{n:013}").as_bytes());
        }
    }

    #[test]
    fn clock_validates_every_second_and_byte_of_cached_dates() {
        let clock = ManifestClock::new(parse_ts(b"2028-01-20T00:00:00Z"), parse_ts(b"2028-04-10T00:00:00Z"));
        // Same day number but distinct YYYY-MM prefixes, including a miss.
        assert!(clock.days.iter().any(|v| v.midnight_seconds != BAD));
        for s in [
            "2028-01-01T05:06:07Z",
            "2028-02-01T05:06:07Z",
            "2028-03-01T05:06:07Z",
            "2028-04-01T05:06:07Z",
            "2028-05-01T05:06:07Z",
            "2028-05-01T05:60:07Z",
            "00000001791071890001",
            "2028-02-29T23:59:59-23:59",
        ] {
            check_clock(&clock, s.as_bytes());
        }
        for base in [*b"2028-02-29T12:34:56Z", *b"2028-03-01T00:00:00Z"] {
            for pos in 0..base.len() {
                for byte in 0..=255 {
                    let mut input = base;
                    input[pos] = byte;
                    check_clock(&clock, &input);
                }
            }
        }
        for second in 0..86_400 {
            let s = format!(
                "2028-02-29T{:02}:{:02}:{:02}Z",
                second / 3600,
                second / 60 % 60,
                second % 60
            );
            check_clock(&clock, s.as_bytes());
        }
    }

    #[test]
    fn clock_bounds_cache_on_huge_spans_and_matches_fallback() {
        for (ws, we) in [
            (i64::MIN + 1, i64::MAX),
            (parse_ts(b"0000-01-01T00:00:00Z"), parse_ts(b"9999-12-31T23:59:59Z")),
            (parse_ts(b"1969-12-30T00:00:00Z"), parse_ts(b"1970-01-02T00:00:00Z")),
        ] {
            let clock = ManifestClock::new(ws, we);
            for entry in clock.days.iter().filter(|e| e.midnight_seconds != BAD) {
                let mut s = *b"0000-00-00T00:00:00Z";
                s[..8].copy_from_slice(&entry.first.to_le_bytes());
                s[8..10].copy_from_slice(&entry.last.to_le_bytes());
                assert_eq!(entry.midnight_seconds * 1000, want(&s), "{s:?}");
                check_clock(&clock, &s);
            }
            for s in [
                "0000-02-29T23:59:59Z",
                "0000-02-29T23:59:60Z",
                "0001-03-01T00:00:00Z",
                "1969-12-31T23:59:59Z",
                "1970-01-01T00:00:00Z",
                "9999-12-31T23:59:59Z",
                "9999-12-31T23:60:59Z",
                "9999-12-31T23:59:59-23:59",
                "999999999999999999999",
                "9223372036854775807",
            ] {
                check_clock(&clock, s.as_bytes());
            }
        }
    }

    #[test]
    fn rules_examples() {
        let f1 = 1_791_071_890_000;
        assert_eq!(parse_ts(b"2026-10-03T23:58:10Z"), f1);
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), f1);
        assert_eq!(parse_ts(b"2026-10-04T05:28:10+05:30"), f1);
        assert_eq!(parse_ts(b"1791071890000"), f1);
        assert_eq!(parse_ts(b"0001791071890000"), f1);
        assert_eq!(parse_ts(b"2026-10-03T23:58:10-00:00"), f1);
        assert_eq!(parse_ts(b"86400000"), 86_400_000);
        for bad in [
            &b""[..],
            b"2026-10-03T23:58:10.000Z",
            b"2026-10-03T23:58:10z",
            b"2026-10-03t23:58:10Z",
            b"2026-10-03 23:58:10Z",
            b"2026-10-03T23:58:10",
            b"1791071890000.0",
            b"1.79e12",
            b"-5",
            b"2026-13-01T00:00:00Z",
            b"2026-02-30T00:00:00Z",
            b"2026-02-29T00:00:00Z",
            b"2026-10-03T24:00:00Z",
            b"2026-10-03T23:59:60Z",
            b"2026-10-03T23:58:10+24:00",
            b"2026-10-03T23:58:10+05:60",
            b"2026-10-03T23:58:10+0200",
            b"2026-10-03T23:58:10Z\r",
            "١٧٩١٠٧١٨٩٠٠٠٠".as_bytes(),
            b"+1791071890000",
            b" 2026-10-03T23:58:10Z",
            b"99999999999999999999999",
        ] {
            assert_eq!(parse_ts(bad), BAD, "{:?}", String::from_utf8_lossy(bad));
            check(bad);
        }
        for s in BASES {
            check(s.as_bytes());
        }
    }

    #[test]
    fn calendar_sweep() {
        let years = [
            0, 1, 4, 99, 100, 400, 1600, 1900, 1969, 1970, 2000, 2024, 2025, 2026, 2027, 2028, 2100, 2400, 9996, 9999,
        ];
        let times = [
            "00:00:00", "23:59:59", "24:00:00", "12:60:00", "12:00:60", "99:99:99", "19:58:10",
        ];
        let zones = [
            "Z", "+00:00", "-00:00", "+23:59", "-23:59", "+24:00", "-05:60", "+12:00", "-10:00", "+05:30", "+99:99",
        ];
        for y in years {
            for mo in 0..=13 {
                for d in 0..=32 {
                    for t in times {
                        for z in zones {
                            check(format!("{y:04}-{mo:02}-{d:02}T{t}{z}").as_bytes());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn single_byte_mutations() {
        for base in BASES {
            let base = base.as_bytes();
            for i in 0..base.len() {
                for b in 0..=255u8 {
                    let mut s = base.to_vec();
                    s[i] = b;
                    check(&s);
                    s.insert(i, b); // also one byte longer
                    check(&s);
                }
                let mut s = base.to_vec();
                s.remove(i);
                check(&s);
            }
            for n in 0..=base.len() + 2 {
                check(&base[..n.min(base.len())]);
                let mut s = base.to_vec();
                s.resize(n, b'0');
                check(&s);
            }
        }
    }

    #[test]
    fn f3_lengths_and_overflow() {
        check(b"9223372036854775807");
        check(b"9223372036854775808");
        check(b"09223372036854775807");
        check(b"09223372036854775808");
        check(b"18446744073709551615");
        check(b"18446744073709551616");
        check(b"9999999999999999999");
        check(b"10000000000000000000");
        for n in 0..=5001 {
            check(&vec![b'0'; n]);
        }
        let mut zeros = vec![b'0'; 5000];
        zeros.extend_from_slice(b"1791071890000");
        check(&zeros);
        zeros.push(b'x');
        check(&zeros);
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        for _ in 0..200_000 {
            let n = rng.below(26);
            let lead = if rng.below(4) == 0 { rng.below(25) } else { 0 };
            let mut s = vec![b'0'; lead];
            for _ in 0..n {
                s.push(b'0' + rng.below(10) as u8);
            }
            check(&s);
            if !s.is_empty() {
                let i = rng.below(s.len());
                s[i] = [b'/', b':', b' ', b'.', b'-', b'e', 0x80, 0xFF, b'\r'][rng.below(9)];
                check(&s);
            }
        }
    }

    #[test]
    fn random_iso_shapes() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        const ALPHA: &[u8] = b"0123456789000111222-:T+-Z0123456789tz \x80/";
        for _ in 0..1_000_000 {
            let base = BASES[rng.below(3)].as_bytes();
            let mut s = base.to_vec();
            for _ in 0..1 + rng.below(3) {
                let i = rng.below(s.len());
                s[i] = ALPHA[rng.below(ALPHA.len())];
            }
            check(&s);
        }
    }
}
