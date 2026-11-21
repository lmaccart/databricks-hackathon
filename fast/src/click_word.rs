//! Guarded canonical click grammar. Any miss retries exactly one physical line.
use crate::word::{id, quoted};
use crate::*;

#[inline(always)]
fn campaign<'a>(s: &'a [u8], sm: &Small) -> Option<(u32, &'a [u8])> {
    let (q, body) = quoted(s);
    let end = memchr::memchr3(b',', b'"', b'\n', body)?;
    let tail = if q {
        body.get(end..)?.strip_prefix(b"\",")?
    } else {
        body.get(end..)?.strip_prefix(b",")?
    };
    Some((sm.camp_idx.get(&body[..end])?, tail))
}

#[inline(always)]
fn timestamp<'a>(s: &'a [u8], clock: &ManifestClock) -> Option<(i64, &'a [u8])> {
    let (q, body) = quoted(s);
    let (ts, n) = if *body.get(4)? != b'-' {
        (ManifestClock::event_f3(body.first_chunk::<13>()?), 13)
    } else if *body.get(19)? == b'Z' {
        let seconds = clock.event_f1_seconds(body.first_chunk::<20>()?);
        (if seconds == BAD { BAD } else { seconds * 1000 }, 20)
    } else {
        (ManifestClock::event_f2(body.first_chunk::<25>()?), 25)
    };
    if ts == BAD {
        return None;
    }
    let rest = body.get(n..)?.strip_prefix(if q { b"\"\n" as &[u8] } else { b"\n" })?;
    Some((ts, rest))
}

#[inline(always)]
fn decode<'a>(
    s: &'a [u8],
    sm: &Small,
    (lo, we, clock): (i64, i64, &ManifestClock),
) -> Option<(PendingClick, &'a [u8])> {
    let (cid, s) = id::<b','>(s)?;
    let (uid, s) = id::<b','>(s)?;
    let (camp, s) = campaign(s, sm)?;
    let (ts, rest) = timestamp(s, clock)?;
    if ts < lo || ts >= we {
        return None;
    }
    Some((
        PendingClick {
            uid,
            hash: links::mix(uid),
            cid,
            ts,
            camp,
        },
        rest,
    ))
}

pub(super) fn for_clicks<'a>(
    c: &Chunk<'a>,
    sm: &Small,
    window: (i64, i64, &ManifestClock),
    codec: &Codec<'a>,
    mut emit: impl FnMut(PendingClick),
) {
    if !c.identity_order(4) {
        return for_rows(c, |r| {
            if let Some(row) = decode_click(r, sm, window, codec) {
                emit(row);
            }
        });
    }
    let mut rest = c.data;
    while !rest.is_empty() {
        if let Some((row, tail)) = decode(rest, sm, window) {
            emit(row);
            rest = tail;
        } else {
            rest = word::fallback::<4>(rest, |r| {
                if let Some(row) = decode_click(r, sm, window, codec) {
                    emit(row);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_rows_match_strict_csv_and_clock_for_mutations() {
        let path = Path::new("test.csv");
        let sm = read_small(
            b"product_id,category\n",
            b"campaign_id,channel\nc_1,ad\nc_20,other\nc_01,leading\nodd key,opaque\n",
            path,
            path,
        );
        let lo = parse_ts(b"2026-09-24T00:00:00Z");
        let we = parse_ts(b"2026-10-15T00:00:00Z");
        let clock = ManifestClock::new(lo, we);
        let window = (lo, we, &clock);
        let cols = ["click_id", "user_id", "campaign_id", "ts"];
        let following = b"click0007,user00007,c_20,2026-10-03T10:00:00Z\n";
        let check = |row: &[u8], after: &[u8], header: &[u8]| {
            let file = [header, row, after].concat();
            let codec = Codec::new();
            for c in chunks_of(&file, &cols, path) {
                let (mut want, mut got) = (vec![], vec![]);
                for_rows(&c, |r| want.extend(decode_click(r, &sm, window, &codec)));
                for_clicks(&c, &sm, window, &codec, |r| got.push(r));
                assert_eq!(got, want, "{row:?} {after:?} {header:?}");
            }
        };
        let header = b"click_id,user_id,campaign_id,ts\n";
        let after = [&b"\n"[..], following].concat();
        for row in [
            &b"click0007,user00007,c_1,2026-10-03T10:00:00Z"[..],
            b"\"click0008\",\"user00008\",\"c_20\",\"1791021600001\"",
            b"click0009,user00009,c_01,2026-10-03T10:30:00+00:30",
        ] {
            let input = [row, &after].concat();
            let (_, rest) = decode(&input, &sm, window).expect("fast click path");
            assert_eq!(rest, following);
            check(row, b"", header);
            for i in 0..row.len() {
                check(&row[..i], &after, header);
                for byte in 0..=255u8 {
                    let mut changed = row.to_vec();
                    changed[i] = byte;
                    check(&changed, &after, header);
                    if matches!(byte, b',' | b'"' | b'\n') {
                        changed.insert(i, byte);
                        check(&changed, &after, header);
                    }
                }
            }
        }
        for (header, row) in [
            (
                &b"ts,campaign_id,user_id,click_id\n"[..],
                &b"2026-10-03T10:00:00Z,c_1,user00007,click0007\n"[..],
            ),
            (
                b"click_id,user_id,campaign_id,ts,extra\n",
                b"click0007,user00007,c_1,2026-10-03T10:00:00Z,x\n",
            ),
            (
                b"click_id,user_id,campaign_id,ts\n",
                b"x,\"unicode \xc3\xa9\",odd key,0001791021600000",
            ),
        ] {
            check(row, b"", header);
        }
    }
}
