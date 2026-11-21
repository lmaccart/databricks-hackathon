//! The canonical header permits a fixed event grammar. A hit proves every
//! byte of the physical line; misses retry from the original line start.
use super::*;

use crate::word::{comma, id, quoted};

#[inline(always)]
fn fixed<const N: usize>(s: &[u8], q: bool) -> Option<(&[u8; N], &[u8])> {
    let body = s.first_chunk::<N>()?;
    Some((body, comma(&s[N..], q)?))
}

#[inline(always)]
fn fixed_ingest(s: &[u8]) -> Option<(&[u8; 20], &[u8])> {
    let (q, s) = quoted(s);
    fixed::<20>(s, q)
}

#[inline(always)]
fn timestamps<'a>(s: &'a [u8], clock: &ManifestClock) -> Option<(i64, i64, &'a [u8])> {
    let (q, b) = quoted(s);
    let (ts, rest) = if *b.get(4)? != b'-' {
        let (body, rest) = fixed::<13>(b, q)?;
        (ManifestClock::event_f3(body), rest)
    } else if *b.get(19)? == b'Z' {
        let (body, rest) = fixed::<20>(b, q)?;
        let (ingest, rest) = fixed_ingest(rest)?;
        // Preserve the strict retry: neither an invalid event nor an unusual
        // ingest may skip the ordinary decoder's duplicate claim.
        let (sec, seconds) = clock.event_f1_pair(body, ingest)?;
        return Some((sec * 1000, seconds, rest));
    } else {
        let (body, rest) = fixed::<25>(b, q)?;
        (ManifestClock::event_f2(body), rest)
    };
    if ts == BAD {
        return None;
    }
    let (body, rest) = fixed_ingest(rest)?;
    let seconds = clock.event_f1_seconds(body);
    (seconds != BAD).then_some((ts, seconds, rest))
}

#[inline(always)]
fn event_type(s: &[u8]) -> Option<(bool, &[u8])> {
    let (q, s) = quoted(s);
    let purchase = match s.first_chunk::<4>()? {
        b"view" | b"cart" => false,
        _ => return Some((true, comma(s.strip_prefix(b"purchase")?, q)?)),
    };
    Some((purchase, comma(&s[4..], q)?))
}

// NEON naturally supplies a nibble per byte. SSE supplies a single bit.
#[cfg(target_arch = "aarch64")]
const STEP: usize = 4;
#[cfg(not(target_arch = "aarch64"))]
const STEP: usize = 1;

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn structural(b: &[u8; 16]) -> u64 {
    use std::arch::aarch64::*;
    // SAFETY: 16 readable bytes and NEON is baseline.
    unsafe {
        let v = vld1q_u8(b.as_ptr());
        let x = vorrq_u8(
            vorrq_u8(vceqq_u8(v, vdupq_n_u8(b',')), vceqq_u8(v, vdupq_n_u8(b'"'))),
            vceqq_u8(v, vdupq_n_u8(b'\n')),
        );
        vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(x))))
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn structural(b: &[u8; 16]) -> u64 {
    use std::arch::x86_64::*;
    // SAFETY: 16 readable bytes and SSE2 is baseline.
    unsafe {
        let v = _mm_loadu_si128(b.as_ptr().cast());
        let x = _mm_or_si128(
            _mm_or_si128(
                _mm_cmpeq_epi8(v, _mm_set1_epi8(b',' as i8)),
                _mm_cmpeq_epi8(v, _mm_set1_epi8(b'"' as i8)),
            ),
            _mm_cmpeq_epi8(v, _mm_set1_epi8(b'\n' as i8)),
        );
        _mm_movemask_epi8(x) as u64
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[inline(always)]
fn structural(b: &[u8; 16]) -> u64 {
    b.iter()
        .enumerate()
        .fold(0, |m, (i, c)| m | (u64::from(matches!(c, b',' | b'"' | b'\n')) << i))
}

#[inline(always)]
fn next(m: u64, start: usize) -> usize {
    if start >= 16 {
        return 16;
    }
    start + (m >> (start * STEP)).trailing_zeros() as usize / STEP
}

type PriceCategory = (u32, u32);

/// One classification for both product and price, even when the price is
/// ignored. The first structural byte in each body must be its exact end.
#[inline(always)]
fn suffix<'a>(s: &'a [u8], purchase: bool, sm: &Small) -> Option<(Option<PriceCategory>, &'a [u8])> {
    let b = s.first_chunk::<16>()?;
    let mask = structural(b);
    let p = usize::from(b[0] == b'"');
    let end = next(mask, p);
    let comma = if p == 1 {
        if *b.get(end)? != b'"' {
            return None;
        }
        end + 1
    } else {
        end
    };
    if *b.get(comma)? != b',' {
        return None;
    }
    let p2 = comma + 1;
    let q2 = usize::from(*b.get(p2)? == b'"');
    let start2 = p2 + q2;
    let end2 = next(mask, start2);
    let nl = if q2 == 1 {
        if *b.get(end2)? != b'"' {
            return None;
        }
        end2 + 1
    } else {
        end2
    };
    if *b.get(nl)? != b'\n' {
        return None;
    }
    let payload = if end == p {
        None
    } else if purchase {
        parse_price(&b[start2..end2]).map(|price| (price, sm.product_cat.get(&b[p..end]).unwrap_or(sm.unknown_cat)))
    } else {
        Some((0, 0))
    };
    Some((payload, &s[nl + 1..]))
}

#[inline(always)]
fn decode<'a>(s: &'a [u8], sm: &Small, (ws, we, clock): (i64, i64, &ManifestClock)) -> Option<(Pending, &'a [u8])> {
    let (eid, s) = id::<b','>(s)?;
    let (uid, s) = id::<b','>(s)?;
    let (ts, seconds, s) = timestamps(s, clock)?;
    let (purchase, s) = event_type(s)?;
    let (payload, rest) = suffix(s, purchase, sm)?;
    let mut e = Pending {
        claim_key: links::mix(eid),
        ts,
        claim_order: seconds as u64, // same exact F1 seconds encoding as Claim::order
        ..Default::default()
    };
    // The accepted forms (four-digit civil year or 13 digits) make the
    // millisecond difference safe in i64, including pre-epoch ingest.
    if (ws..we).contains(&ts)
        && seconds * 1000 - ts <= DAY
        && let Some((price, cat)) = payload
    {
        e.uid = uid;
        e.hash = links::mix(uid);
        e.price = price;
        e.cat = cat;
    }
    Some((e, rest))
}

#[inline]
pub(super) fn for_events<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    codec: &Codec<'a>,
    window: (i64, i64, &ManifestClock),
    spill: &mut Vec<i64>,
    mut emit: impl FnMut(Pending),
) {
    if !c.identity_order(7) {
        return for_rows(c, |r| {
            if let Some(e) = decode_fields(r, sm, codec, window, spill) {
                emit(e);
            }
        });
    }
    let mut rest = c.data;
    while !rest.is_empty() {
        if let Some((e, tail)) = decode(rest, sm, window) {
            emit(e);
            rest = tail;
        } else {
            rest = crate::word::fallback::<7>(rest, |r| {
                if let Some(e) = decode_fields(r, sm, codec, window, spill) {
                    emit(e);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_ids_match_codec_for_every_accepted_byte_and_boundary() {
        for len in 7..=9 {
            for pos in 0..len {
                for byte in b'-'..=127 {
                    for quoted in [false, true] {
                        let mut value = vec![b'x'; len];
                        value[pos] = byte;
                        let mut input = Vec::new();
                        if quoted {
                            input.push(b'"');
                        }
                        input.extend_from_slice(&value);
                        if quoted {
                            input.push(b'"');
                        }
                        input.extend_from_slice(b",following-field");
                        let (key, rest) = id::<b','>(&input).expect("ASCII ID must use the fast path");
                        assert_eq!(Some(key), person::inline(&value), "{value:?}");
                        assert_eq!(rest, b"following-field");
                    }
                }
            }
        }
    }

    #[test]
    fn word_rows_match_strict_decoder_under_byte_mutations() {
        let path = Path::new("fixture.csv");
        let sm = read_small(
            b"product_id,category\nitem-7,shoes\n",
            b"campaign_id,channel\n",
            path,
            path,
        );
        let ws = parse_ts(b"2026-10-01T00:00:00Z");
        let we = parse_ts(b"2026-10-15T00:00:00Z");
        let clock = ManifestClock::new(ws, we);
        let window = (ws, we, &clock);
        let cols = [
            "event_id",
            "user_id",
            "ts",
            "ingest_ts",
            "event_type",
            "product_id",
            "price_cents",
        ];
        let header = cols.join(",") + "\n";
        let following = b"next99999,user99999,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,view,item-7,\n";
        let check = |row: &[u8], after: &[u8]| {
            let file = [header.as_bytes(), row, after].concat();
            let codec = Codec::new();
            for c in chunks_of(&file, &cols, path) {
                let (mut want, mut got) = (vec![], vec![]);
                let (mut want_spill, mut got_spill) = (vec![], vec![]);
                for_rows(&c, |r| {
                    want.extend(decode_fields(r, &sm, &codec, window, &mut want_spill))
                });
                for_events(&c, &sm, &codec, window, &mut got_spill, |e| got.push(e));
                assert_eq!(got, want, "{row:?}, {after:?}");
                assert_eq!(got_spill, want_spill);
            }
        };
        let followed = [&b"\n"[..], following].concat();
        for row in [
            &b"opaque7,user0007,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,view,item-7,\r"[..],
            b"\"opaque08\",\"user00008\",\"1791021600001\",\"2026-10-03T10:00:00Z\",\"cart\",\"item-7\",\"\"",
            b"opaque009,\"USER_07\",\"2026-10-03T10:30:00+00:30\",\"2026-10-03T10:00:00Z\",\"purchase\",\"item-7\",\"0123\"",
        ] {
            let file = [row, &followed].concat();
            let (event, rest) = decode(&file, &sm, window).unwrap_or_else(|| panic!("must exercise the word path: {row:?}"));
            assert_ne!(event.uid, 0);
            assert_eq!(rest, following);
            check(row, b"");
            for i in 0..row.len() {
                check(&row[..i], &followed);
                for byte in 0..=u8::MAX {
                    let mut changed = row.to_vec();
                    changed[i] = byte;
                    check(&changed, &followed);
                    if matches!(byte, b',' | b'"' | b'\n') {
                        changed.insert(i, byte);
                        check(&changed, &followed);
                    }
                }
            }
        }
    }
}
