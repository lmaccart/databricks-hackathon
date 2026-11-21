//! Optimistic decoder for events with the identity header. A successful decode
//! proves the entire physical line, not just the fields that affect a payload.
use super::*;

const ONES: u64 = 0x0101_0101_0101_0101;

#[inline(always)]
fn marks(w: u64) -> u64 {
    // Borrow can mark a subsequent byte but never changes the FIRST hit.
    let zero = |x: u64| x.wrapping_sub(ONES) & !x & (ONES << 7);
    zero(w ^ (ONES * b',' as u64)) | zero(w ^ (ONES * b'"' as u64)) | zero(w ^ (ONES * b'\n' as u64))
}

/// Bounded scan for small opaque fields. Neither a quote nor a comma hides an
/// LF, including when the next load straddles the end of the physical line.
#[inline(always)]
fn short_field<const END: u8>(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let quoted = data.first() == Some(&b'"');
    let s = if quoted { &data[1..] } else { data };
    let first = marks(u64::from_le_bytes(*s.first_chunk::<8>()?));
    let i = if first != 0 {
        first.trailing_zeros() as usize / 8
    } else {
        let second = marks(u64::from_le_bytes(*s.get(8..)?.first_chunk::<8>()?));
        if second == 0 {
            return None;
        }
        8 + second.trailing_zeros() as usize / 8
    };
    let rest = s.get(i..)?;
    if quoted && rest.first() != Some(&b'"') {
        return None;
    }
    Some((
        &s[..i],
        rest.strip_prefix(if quoted { &[b'"', END][..] } else { &[END][..] })?,
    ))
}

/// Exact suffix for a field whose body is proved by its own grammar.
#[inline(always)]
fn fixed_ts<const N: usize>(s: &[u8], quoted: bool) -> Option<(i64, &[u8])> {
    let (body, delim) = s.split_at_checked(N)?;
    let rest = if quoted {
        delim.strip_prefix(b"\",")?
    } else {
        delim.strip_prefix(b",")?
    };
    // The timestamp validator proves every byte; don't scan inside this field.
    valid_ts(body, rest)
}

#[inline(always)]
fn valid_ts<'a>(body: &[u8], rest: &'a [u8]) -> Option<(i64, &'a [u8])> {
    let ts = parse_ts(body);
    // BAD proves nothing about structure; use the whole-line fallback.
    (ts != BAD).then_some((ts, rest))
}

#[inline(always)]
fn timestamp<const INGEST: bool>(data: &[u8]) -> Option<(i64, &[u8])> {
    let quoted = data.first() == Some(&b'"');
    let s = if quoted { &data[1..] } else { data };
    if INGEST {
        fixed_ts::<20>(s, quoted)
    } else if s.get(4) == Some(&b'-') {
        match s.get(19)? {
            b'Z' => fixed_ts::<20>(s, quoted),
            b'+' | b'-' => fixed_ts::<25>(s, quoted),
            _ => None,
        }
    } else if s.get(13) == Some(&if quoted { b'"' } else { b',' }) {
        // Common-width F3, guarded by the exact suffix and the digit validator.
        fixed_ts::<13>(s, quoted)
    } else {
        let (field, rest) = short_field::<b','>(data)?;
        valid_ts(field, rest)
    }
}

#[inline(always)]
fn event_type(s: &[u8]) -> Option<(bool, &[u8])> {
    let quoted = s.first() == Some(&b'"');
    let s = if quoted { &s[1..] } else { s };
    let (purchase, rest) = if let Some(rest) = s.strip_prefix(b"view").or_else(|| s.strip_prefix(b"cart")) {
        (false, rest)
    } else {
        (true, s.strip_prefix(b"purchase")?)
    };
    Some((
        purchase,
        rest.strip_prefix(if quoted { &b"\","[..] } else { &b","[..] })?,
    ))
}

#[inline(always)]
fn price_field(s: &[u8]) -> Option<(&[u8], &[u8])> {
    if let Some(rest) = s.strip_prefix(b"\n") {
        Some((&[], rest))
    } else if let Some(rest) = s.strip_prefix(b"\"\"\n") {
        Some((&[], rest))
    } else {
        short_field::<b'\n'>(s)
    }
}

#[inline(always)]
fn decode<'a>(data: &'a [u8], sm: &Small, codec: &Codec<'a>, ws: i64, we: i64) -> Option<(Pending, &'a [u8])> {
    let (eid, rest) = short_field::<b','>(data)?;
    let (uid, rest) = short_field::<b','>(rest)?;
    let (ts, rest) = timestamp::<false>(rest)?;
    let (ingest, rest) = timestamp::<true>(rest)?;
    let (purchase, rest) = event_type(rest)?;
    let (product, rest) = short_field::<b','>(rest)?;
    let (raw_price, rest) = price_field(rest)?;
    let mut next = Pending {
        ts,
        ingest,
        ..Pending::default()
    };
    if !eid.is_empty() {
        next.id = codec.key(eid);
        if eligible(uid, product, ts, ingest, ws, we) {
            if let Some(price) = if purchase { parse_price(raw_price) } else { Some(0) } {
                next.set_payload(codec.key(uid), product, price, sm);
            }
        }
    }
    Some((next, rest))
}

#[cold]
#[inline(never)]
fn fallback<'a>(data: &'a [u8], sm: &Small, codec: &Codec<'a>, ws: i64, we: i64) -> (Pending, &'a [u8]) {
    let end = memchr::memchr(b'\n', data).map_or(data.len(), |i| i + 1);
    let (line, rest) = data.split_at(end);
    let mut next = Pending::default();
    csv::identity_row::<7>(line, &mut |r| next = decode_fields(r, sm, codec, ws, we));
    (next, rest)
}

#[inline]
pub(super) fn for_events<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    codec: &Codec<'a>,
    ws: i64,
    we: i64,
    mut emit: impl FnMut(Pending),
) {
    if !c.identity_order(7) {
        return for_rows(c, |r| emit(decode_fields(r, sm, codec, ws, we)));
    }
    let mut rest = c.data;
    while !rest.is_empty() {
        let (next, tail) = match decode(rest, sm, codec, ws, we) {
            Some(row) => row,
            None => fallback(rest, sm, codec, ws, we),
        };
        emit(next);
        rest = tail;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: [&str; 7] = [
        "event_id",
        "user_id",
        "ts",
        "ingest_ts",
        "event_type",
        "product_id",
        "price_cents",
    ];

    fn tuple(e: Pending) -> (u64, u64, u64, i64, i64, u32, u32) {
        (e.id, e.uid, e.hash, e.ts, e.ingest, e.price, e.cat)
    }

    #[test]
    fn mutation_and_truncation_match_full_event_decoder() {
        let path = Path::new("fixture.csv");
        let sm = read_small(b"product_id,category\np1,shoes\n", b"campaign_id,channel\n", path, path);
        let lo = parse_ts(b"2026-10-01T00:00:00Z");
        let hi = parse_ts(b"2026-10-15T00:00:00Z");
        let header = COLS.join(",") + "\n";
        let next_row = b"next,user,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,view,p1,\n";
        let check = |row: &[u8], terminator: &[u8]| {
            let file = [header.as_bytes(), row, terminator].concat();
            let codec = Codec::new();
            for c in chunks_of(&file, &COLS, path) {
                let mut want = vec![];
                let mut got = vec![];
                for_rows(&c, |r| {
                    let e = decode_fields(r, &sm, &codec, lo, hi);
                    if e.id != 0 {
                        want.push(tuple(e));
                    }
                });
                for_events(&c, &sm, &codec, lo, hi, |e| {
                    if e.id != 0 {
                        got.push(tuple(e));
                    }
                });
                assert_eq!(got, want, "{row:?}, {terminator:?}");
            }
        };

        let common = [
            &b"id,user,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,view,p1,"[..],
            b"\"id1234567\",\"u12345678\",\"2026-10-03T10:30:00+00:30\",\"2026-10-03T10:00:00Z\",\"purchase\",\"p1\",\"000250\"",
            b"a\0b,user,1791021600000,2026-10-03T10:00:00Z,cart,p1,\r",
            b"ident,usr,01791021600000,2026-10-03T10:00:00Z,view,p1,\"\"",
        ];
        let followed = [&b"\n"[..], next_row].concat();
        for row in common {
            let file = [row, b"\n", next_row].concat();
            let codec = Codec::new();
            let (e, rest) = decode(&file, &sm, &codec, lo, hi).expect("common row must hit the typed path");
            assert_ne!(e.uid, 0);
            assert_eq!(rest, next_row);
            check(row, b""); // EOF can end a row, including just after a closing quote.
            for i in 0..row.len() {
                check(&row[..i], b"");
                check(&row[..i], &followed);
                for b in [b',', b'\n', b'\r', b'"', b'\0', b' ', b'0', b'@', 0x80] {
                    let mut changed = row.to_vec();
                    changed[i] = b;
                    check(&changed, b"\n");
                    changed.insert(i, b);
                    check(&changed, b"\n");
                }
            }
        }
    }
}
