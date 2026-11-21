//! The canonical header permits a fixed event grammar. A hit proves every
//! byte of the physical line; misses retry from the original line start.
use super::*;

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGH: u64 = ONES << 7;
const FLOOR: u64 = ONES * b'-' as u64;
const PACKET: usize = 128;

// Callers prove a maximum offset for each phase in `decode`. Unaligned
// array references have alignment 1, including when backed by an mmap.
#[inline(always)]
unsafe fn bytes<const N: usize>(b: &[u8; PACKET], at: usize) -> &[u8; N] {
    debug_assert!(at <= PACKET - N);
    // SAFETY: the caller guarantees `at + N <= PACKET`.
    unsafe { &*b.as_ptr().add(at).cast() }
}

#[inline(always)]
fn boundary(pair: u16, q: usize) -> bool {
    // For unquoted fields the high byte belongs to the next field.
    let quoted = q != 0;
    let mask = if quoted { u16::MAX } else { 0xff };
    let expected = if quoted {
        u16::from_le_bytes(*b"\",")
    } else {
        u16::from(b',')
    };
    (pair & mask) == expected
}

/// The tight ID path uses the same load to prove ASCII and construct the
/// inline token. `'-'..=127` excludes all three CSV structural bytes and NUL;
/// IDs with any other spelling are still handled by the ordinary codec.
/// Caller supplies up to 12 readable bytes at `at`.
#[inline(always)]
unsafe fn id(b: &[u8; PACKET], at: usize) -> Option<(u64, usize, u64)> {
    // SAFETY: one opening byte, followed by at most 11 bytes examined.
    let q = usize::from(unsafe { bytes::<1>(b, at) }[0] == b'"');
    let at = at + q;
    let b11 = unsafe { bytes::<11>(b, at) };
    let w = u64::from_le_bytes(b11[..8].try_into().unwrap());
    let end = if q != 0 { b'"' } else { b',' };
    let tail = b11[8];
    if boundary(u16::from_le_bytes([b11[9], b11[10]]), q) && (b'-'..=127).contains(&tail) {
        return Some((
            person::pack_inline(w.swap_bytes(), tail),
            at + 10 + q,
            w.wrapping_sub(FLOOR) | w,
        ));
    }
    // Search the two shorter boundary positions. The subtract-based
    // zero mask can have borrow artifacts after a match, never before it.
    let v = u64::from(u16::from_le_bytes(b11[7..9].try_into().unwrap())) ^ (ONES * u64::from(end));
    let hits = v.wrapping_sub(ONES) & !v & (HIGH & 0x0000_ffff);
    let n = 7 + (hits.trailing_zeros() as usize / 8);
    if n > 8 {
        return None;
    }
    if q != 0 && b11[n + 1] != b',' {
        return None;
    }
    let active = if n == 7 { 0x00ff_ffff_ffff_ffff } else { u64::MAX };
    // If any byte underflows, the first offending byte either raises its
    // high bit on subtract or already had its high bit set.
    Some((
        person::pack_inline((w & active).swap_bytes(), 0),
        at + n + q + 1,
        (w.wrapping_sub(FLOOR) | w) & active,
    ))
}

/// Caller supplies at least 51 bytes (28 event + 23 ingest).
#[inline(always)]
unsafe fn timestamps(b: &[u8; PACKET], at: usize, clock: &ManifestClock) -> Option<(i64, i64, usize)> {
    // SAFETY: the longest event timestamp advances 28 bytes; the ingest
    // timestamp reads at most another 23 bytes.
    unsafe {
        let q = usize::from(bytes::<1>(b, at)[0] == b'"');
        let at = at + q;
        let head = u64::from_le_bytes(*bytes::<8>(b, at));
        let (len, gate, tail) = if (head >> 32) as u8 != b'-' {
            (13, u16::from_le_bytes(*bytes::<2>(b, at + 13)), 0)
        } else {
            // One load supplies the F1 parser's final second digit and Z,
            // the F1 comma gate, and the choice between F1 and F2.
            let tail = u32::from_le_bytes(*bytes::<4>(b, at + 18));
            if (tail >> 8) as u8 == b'Z' {
                (20, (tail >> 16) as u16, tail as u16)
            } else {
                (25, u16::from_le_bytes(*bytes::<2>(b, at + 25)), 0)
            }
        };
        if !boundary(gate, q) {
            return None;
        }
        let ingest = at + len + q + 1;
        let iq = usize::from(bytes::<1>(b, ingest)[0] == b'"');
        let ingest = ingest + iq;
        let itail = u32::from_le_bytes(*bytes::<4>(b, ingest + 18));
        if !boundary((itail >> 16) as u16, iq) {
            return None;
        }
        let ts = match len {
            13 => ManifestClock::event_f3(head, u64::from_le_bytes(*bytes::<8>(b, at + 5))),
            20 => clock.event_f1_seconds(bytes::<20>(b, at), head, tail),
            _ => ManifestClock::event_f2(bytes::<25>(b, at)),
        };
        let ibody = bytes::<20>(b, ingest);
        let seconds = clock.event_f1_seconds(ibody, u64::from_le_bytes(*ibody.first_chunk().unwrap()), itail as u16);
        if (ts == BAD) | (seconds == BAD) {
            return None;
        }
        let ts = ts * if len == 20 { 1000 } else { 1 };
        Some((ts, seconds, ingest + 21 + iq))
    }
}

/// Caller supplies at least 11 bytes.
#[inline(always)]
unsafe fn event_type(b: &[u8; PACKET], at: usize) -> Option<(bool, usize)> {
    // SAFETY: at most one opening quote, eight value bytes, and two
    // boundary bytes, ending within the caller's 11-byte span.
    let q = usize::from(unsafe { bytes::<1>(b, at) }[0] == b'"');
    let at = at + q;
    let body = unsafe { bytes::<10>(b, at) };
    const VIEW: u32 = u32::from_le_bytes(*b"view");
    const CART: u32 = u32::from_le_bytes(*b"cart");
    let head = u64::from_le_bytes(*body.first_chunk::<8>().unwrap());
    if matches!(head as u32, VIEW | CART) {
        return boundary((head >> 32) as u16, q).then_some((false, at + 5 + q));
    }
    (head == u64::from_le_bytes(*b"purchase") && boundary(u16::from_le_bytes([body[8], body[9]]), q))
        .then_some((true, at + 9 + q))
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
fn suffix(b: &[u8; 16], purchase: bool, sm: &Small) -> Option<(Option<PriceCategory>, usize)> {
    let mask = structural(b);
    let p = usize::from(b[0] == b'"');
    let end = next(mask, p);
    if end + p >= 15 {
        return None;
    }
    // SAFETY: end <= 14 and the next field's first byte (p2) is <= 15.
    let pair = u16::from_le_bytes(unsafe { *b.as_ptr().add(end).cast::<[u8; 2]>() });
    if !boundary(pair, p) {
        return None;
    }
    let p2 = end + p + 1;
    let q2 = usize::from(unsafe { *b.get_unchecked(p2) } == b'"');
    let start2 = p2 + q2;
    let end2 = next(mask, start2);
    let nl = end2 + q2;
    if nl >= 16 {
        return None;
    }
    // SAFETY: end2 <= nl < 16. When there is no opening quote the
    // first structural byte must be LF; otherwise it must be the quote.
    if unsafe { (*b.get_unchecked(nl) != b'\n') | (q2 != 0 && *b.get_unchecked(end2) != b'"') } {
        return None;
    }
    let payload = if end == p {
        None
    } else if purchase {
        priced(&b[start2..end2], &b[p..end], sm)
    } else {
        Some((0, 0))
    };
    Some((payload, nl + 1))
}

#[cold]
#[inline(never)]
fn priced(price: &[u8], product: &[u8], sm: &Small) -> Option<PriceCategory> {
    parse_price(price).map(|price| (price, sm.product_cat.get(product).unwrap_or(sm.unknown_cat)))
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
                        let mut packet = [0; PACKET];
                        packet[..input.len()].copy_from_slice(&input);
                        // SAFETY: a full packet is available from offset 0.
                        let (key, rest, bad) = unsafe { id(&packet, 0) }.expect("ASCII ID must use the fast path");
                        assert_eq!(bad & HIGH, 0);
                        assert_eq!(Some(key), person::inline(&value), "{value:?}");
                        assert_eq!(&input[rest..], b"following-field");
                    }
                }
            }
        }
    }

    #[test]
    fn packet_edges_match_strict_decoder_without_padding() {
        let path = Path::new("packet.csv");
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
        // Max cursor advance: both IDs, both timestamps and event type
        // take their longest quoted form; the tail fills all 16 bytes.
        let first = b"\"opaque009\",\"USER_0007\",\"2026-10-03T10:30:00+00:30\",\"2026-10-03T10:00:00Z\",\"purchase\",\"item-7\",\"0002\"\n";
        assert_eq!(first.len(), 102);
        let last = b"opaque07,user0007,1791021600001,2026-10-03T10:00:00Z,view,item-7,";
        let check = |body: &[u8]| {
            let file = [header.as_bytes(), body].concat();
            let codec = Codec::new();
            let [chunk] = &chunks_of(&file, &cols, path)[..] else {
                panic!()
            };
            // Simulate a chunk ending at the first newline with
            // further mapped bytes belonging to another chunk.
            for end in [memchr::memchr(b'\n', body).map_or(body.len(), |p| p + 1), body.len()] {
                let mut c = *chunk;
                c.data = &chunk.data[..end];
                let (mut want, mut got) = (vec![], vec![]);
                let (mut want_spill, mut got_spill) = (vec![], vec![]);
                for_rows(&c, |r| {
                    want.extend(decode_fields(r, &sm, &codec, window, &mut want_spill))
                });
                for_events(&c, &sm, &codec, window, &mut got_spill, |e| got.push(e));
                assert_eq!(got, want, "{end}: {body:?}");
                assert_eq!(got_spill, want_spill);
            }
        };
        for n in [first.len() - 1, first.len()] {
            check(&first[..n]);
            check(&[first, &first[..n]].concat());
        }
        for n in [127, 128, 129, 255, 256, 257] {
            for final_newline in [false, true] {
                for first in [&first[..], &first[..first.len() - 1], &[]] {
                    let mut body = [first, last].concat();
                    body.resize(n, b'x');
                    if final_newline {
                        body[n - 1] = b'\n';
                    }
                    check(&body);
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
            let (event, rest) = decode(file.first_chunk().unwrap(), &sm, window)
                .unwrap_or_else(|| panic!("must exercise the word path: {row:?}"));
            assert_ne!(event.uid, 0);
            assert_eq!(&file[rest..], following);
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

#[inline(always)]
fn decode(b: &[u8; PACKET], sm: &Small, (ws, we, clock): (i64, i64, &ManifestClock)) -> Option<(Pending, usize)> {
    // SAFETY: both IDs advance at most 12 bytes each, timestamps 51
    // together, type 11, and the suffix reads 16. Their sum is 102, so
    // even a malformed candidate never requires bytes outside this array.
    let (eid, uid, ts, seconds, payload, end) = unsafe {
        let (eid, at, eid_bad) = id(b, 0)?;
        let (uid, at, uid_bad) = id(b, at)?;
        if (eid_bad | uid_bad) & HIGH != 0 {
            return None;
        }
        let (ts, seconds, at) = timestamps(b, at, clock)?;
        let (purchase, at) = event_type(b, at)?;
        let (payload, n) = suffix(bytes::<16>(b, at), purchase, sm)?;
        (eid, uid, ts, seconds, payload, at + n)
    };
    let claim_key = links::mix(eid);
    let claim_order = seconds as u64; // same exact F1 seconds encoding as Claim::order
    // The accepted forms (four-digit civil year or 13 digits) make the
    // millisecond difference safe in i64, including pre-epoch ingest.
    if let Some((price, cat)) = payload {
        if (ws..we).contains(&ts) && seconds * 1000 - ts <= DAY {
            let e = Pending {
                claim_key,
                claim_order,
                ts,
                uid,
                hash: links::mix(uid),
                price,
                cat,
            };
            return Some((e, end));
        }
    }
    Some((
        Pending {
            claim_key,
            claim_order,
            ts,
            ..Default::default()
        },
        end,
    ))
}

#[cold]
#[inline(never)]
fn fallback<'a>(
    s: &'a [u8],
    sm: &Small,
    codec: &Codec<'a>,
    window: (i64, i64, &ManifestClock),
    spill: &mut Vec<i64>,
) -> (Option<Pending>, &'a [u8]) {
    let end = memchr::memchr(b'\n', s).map_or(s.len(), |i| i + 1);
    let (line, rest) = s.split_at(end);
    let mut row = None;
    csv::identity_row::<7>(line, &mut |r| row = decode_fields(r, sm, codec, window, spill));
    (row, rest)
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
    while let Some(packet) = rest.first_chunk::<PACKET>() {
        if let Some((e, n)) = decode(packet, sm, window) {
            emit(e);
            // SAFETY: decode consumes at most 102 bytes, within this packet.
            rest = unsafe { rest.get_unchecked(n..) };
        } else {
            let (e, tail) = fallback(rest, sm, codec, window, spill);
            if let Some(e) = e {
                emit(e);
            }
            rest = tail;
        }
    }
    if !rest.is_empty() {
        // No packet overlaps a chunk boundary. The regular line decoder
        // handles the short tail, including a final row without LF.
        let mut tail = *c;
        tail.data = rest;
        for_rows(&tail, |r| {
            if let Some(e) = decode_fields(r, sm, codec, window, spill) {
                emit(e);
            }
        });
    }
}
