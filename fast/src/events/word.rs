//! The canonical header permits a fixed event grammar. A hit proves every
//! byte of the physical line; misses retry from the original line start.
use super::*;

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGH: u64 = ONES << 7;
const FLOOR: u64 = ONES * b'-' as u64;

#[inline(always)]
fn quoted(s: &[u8]) -> (bool, &[u8]) {
    match s.strip_prefix(b"\"") {
        Some(body) => (true, body),
        None => (false, s),
    }
}

/// The tight ID path uses the same load to prove ASCII and construct the
/// inline token. `'-'..=127` excludes all three CSV structural bytes and NUL;
/// IDs with any other spelling are still handled by the ordinary codec.
#[inline(always)]
fn id(s: &[u8]) -> Option<(u64, &[u8])> {
    let (q, b) = quoted(s);
    let b11 = b.first_chunk::<11>()?;
    let w = u64::from_le_bytes(b11[..8].try_into().unwrap());
    let end = if q { b'"' } else { b',' };
    let tail = b11[8];
    if b11[9] == end && (b'-'..=127).contains(&tail) {
        if ((w.wrapping_sub(FLOOR) | w) & HIGH) != 0 || (q && b11[10] != b',') {
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
    if q && b11[n + 1] != b',' {
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

#[inline(always)]
fn ingest(s: &[u8], clock: &ManifestClock) -> Option<(i64, usize)> {
    let q = s.ends_with(b"\"");
    let body = &s[..s.len() - usize::from(q)];
    let (before, ingest) = body.split_last_chunk::<20>()?;
    let seconds = clock.event_f1_seconds(ingest);
    let s = before.strip_suffix(if q { b",\"" as &[u8] } else { b"," })?;
    (seconds != BAD).then_some((seconds, s.len()))
}

#[inline(always)]
fn event_time(s: &[u8], clock: &ManifestClock) -> Option<i64> {
    let (q, b) = quoted(s);
    let b = if q { b.strip_suffix(b"\"")? } else { b };
    let ts = match b.len() {
        20 => {
            let seconds = clock.event_f1_seconds(b.first_chunk::<20>()?);
            if seconds == BAD {
                return None;
            }
            seconds * 1000
        }
        13 => ManifestClock::event_f3(b.first_chunk::<13>()?),
        25 => ManifestClock::event_f2(b.first_chunk::<25>()?),
        _ => return None,
    };
    (ts != BAD).then_some(ts)
}

#[inline(always)]
fn event_type(s: &[u8]) -> Option<(bool, usize)> {
    // Match the preceding comma too: suffix matches alone would accept
    // e.g. preview or a quote at only one end.
    let word = u64::from_le_bytes(*s.last_chunk::<8>()?);
    let n = match (word >> 24).to_le_bytes() {
        [b',', b'v', b'i', b'e', b'w', ..] | [b',', b'c', b'a', b'r', b't', ..] => 5,
        _ => match (word >> 8).to_le_bytes() {
            [b',', b'"', b'v', b'i', b'e', b'w', b'"', _] | [b',', b'"', b'c', b'a', b'r', b't', b'"', _] => 7,
            _ if s.ends_with(b",purchase") => 9,
            _ if s.ends_with(b",\"purchase\"") => 11,
            _ => return None,
        },
    };
    Some((n > 7, s.len() - n))
}

// NEON naturally supplies a nibble per byte. SSE supplies a single bit.
#[cfg(target_arch = "aarch64")]
const STEP: usize = 4;
#[cfg(not(target_arch = "aarch64"))]
const STEP: usize = 1;

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn structural(b: &[u8; 16]) -> (u64, u64) {
    use std::arch::aarch64::*;
    // SAFETY: 16 readable bytes and NEON is baseline.
    unsafe {
        let v = vld1q_u8(b.as_ptr());
        let bits = |x| {
            vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(x)))) & 0x1111_1111_1111_1111
        };
        (bits(vceqq_u8(v, vdupq_n_u8(b','))), bits(vceqq_u8(v, vdupq_n_u8(b'"'))))
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn structural(b: &[u8; 16]) -> (u64, u64) {
    use std::arch::x86_64::*;
    // SAFETY: 16 readable bytes and SSE2 is baseline.
    unsafe {
        let v = _mm_loadu_si128(b.as_ptr().cast());
        let bits = |byte| _mm_movemask_epi8(_mm_cmpeq_epi8(v, _mm_set1_epi8(byte as i8))) as u64;
        (bits(b','), bits(b'"'))
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[inline(always)]
fn structural(b: &[u8; 16]) -> (u64, u64) {
    b.iter().enumerate().fold((0, 0), |(m, q), (i, &c)| {
        (m | (u64::from(c == b',') << i), q | (u64::from(c == b'"') << i))
    })
}

#[inline(always)]
fn last_bit(m: u64) -> usize {
    // A missing match wraps to usize::MAX, never a lane.
    63usize.wrapping_sub(m.leading_zeros() as usize)
}

/// Every comma separates fields, even inside malformed quotes. The final
/// two commas delimit product and price; their quotes must still be proved
/// before deciding whether to ignore the price.
#[inline(always)]
fn suffix(s: &[u8]) -> Option<(usize, bool, &[u8], &[u8])> {
    let b = s.last_chunk::<16>()?;
    let (commas, quotes) = structural(b);
    let last = last_bit(commas);
    let rest = commas & !(1 << (last & 63));
    if rest == 0 {
        return None;
    }
    let prev = last_bit(rest);
    let p = prev / STEP;
    let mut product = &b[p + 1..last / STEP];
    let mut price = &b[last / STEP + 1..];
    let tail_quotes = quotes & (u64::MAX << prev);
    if tail_quotes != 0 {
        // At most an opening and closing quote can occur in each field.
        // Check the full mask, not just whether the endpoints are quotes.
        let (prod, cost) = match_quoted_tail(b, tail_quotes, prev, last)?;
        product = prod;
        price = cost;
    }
    let (purchase, before_type) = event_type(&s[..s.len() - 16 + p])?;
    Some((before_type, purchase, product, price))
}

#[inline(always)]
fn match_quoted_tail(b: &[u8; 16], quotes: u64, prev: usize, last: usize) -> Option<(&[u8], &[u8])> {
    let hi = (15 * STEP) as u32;
    let close_prod = last.wrapping_sub(STEP);
    let qp = (quotes >> close_prod) & 1;
    let qc = (quotes >> hi) & 1;
    let prod_mask = (qp << (prev + STEP)) | (qp << close_prod);
    let cost_mask = (qc << ((last + STEP) & 63)) | (qc << hi);
    if quotes != prod_mask | cost_mask {
        return None;
    }
    let (qp, qc) = (qp as usize, qc as usize);
    Some((
        b.get(prev / STEP + 1 + qp..last / STEP - qp)?,
        b.get(last / STEP + 1 + qc..16 - qc)?,
    ))
}

#[inline(always)]
fn decode(s: &[u8], sm: &Small, (ws, we, clock): (i64, i64, &ManifestClock)) -> Option<Pending> {
    let row_len = s.len();
    let (before_type, purchase, product, raw_price) = suffix(s)?;
    let (seconds, ts_end) = ingest(&s[..before_type], clock)?;
    let (eid, s) = id(s)?;
    let (uid, s) = id(s)?;
    // Decode exactly the gap between the UID and ingest proofs.
    let ts = event_time(s.get(..ts_end.checked_sub(row_len - s.len())?)?, clock)?;
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
        && !product.is_empty()
        && let Some(price) = if purchase { parse_price(raw_price) } else { Some(0) }
    {
        e.uid = uid;
        e.hash = links::mix(uid);
        e.price = price;
        e.cat = if purchase {
            sm.product_cat.get(product).unwrap_or(sm.unknown_cat)
        } else {
            0
        };
    }
    Some(e)
}

#[cold]
#[inline(never)]
fn fallback<'a>(
    line: &'a [u8],
    sm: &Small,
    codec: &Codec<'a>,
    window: (i64, i64, &ManifestClock),
    spill: &mut Vec<i64>,
) -> Option<Pending> {
    let mut row = None;
    csv::identity_row::<7>(line, &mut |r| row = decode_fields(r, sm, codec, window, spill));
    row
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
    csv::physical_lines(c.data, |line| {
        if let Some(e) = decode(line, sm, window).or_else(|| fallback(line, sm, codec, window, spill)) {
            emit(e);
        }
    });
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
                        let (key, rest) = id(&input).expect("ASCII ID must use the fast path");
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
            let event = decode(row, &sm, window).unwrap_or_else(|| panic!("must exercise the word path: {row:?}"));
            assert_ne!(event.uid, 0);
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
