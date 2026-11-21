use super::*;
use crate::csv::chunks_of;
use std::path::Path;

const COLS: [&str; 7] = [
    "event_id",
    "user_id",
    "ts",
    "ingest_ts",
    "event_type",
    "product_id",
    "price_cents",
];
const HEADER: &[u8] = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n";

fn check(clock: &ManifestClock, body: &[u8], model: bool) {
    let mut data = HEADER.to_vec();
    data.extend_from_slice(body);
    let c = chunks_of(&data, &COLS, Path::new("test"))[0];
    let mut want = Vec::new();
    csv::for_rows(&c, |r| {
        if !r[0].is_empty() {
            want.push(Row::generic(r, clock).normalized());
        }
    });
    let mut got = Vec::new();
    let mut accept = |r: Row<'_>| {
        if let Some(expected) = want.get(got.len()) {
            assert_eq!(r.normalized(), *expected, "{body:?}");
        }
        got.push(r.normalized());
    };
    if model {
        scan::<Model, _>(&c, clock, &mut accept);
    } else {
        for_events(&c, clock, &mut accept);
    }
    assert_eq!(got, want, "{body:?}");
}

#[test]
fn strict_row_mutations() {
    let clock = ManifestClock::new(
        crate::parse_ts(b"2028-02-27T00:00:00Z"),
        crate::parse_ts(b"2028-03-02T00:00:00Z"),
    );
    for ts in [
        "2028-02-29T23:48:59Z",
        "1835480939000",
        "2028-02-29T21:48:59-02:00",
        "00000000000001835480939000",
    ] {
        for quotes in 0..128 {
            let mut row = Vec::new();
            for (i, f) in [
                "ev\0ent_42",
                "user_0123",
                ts,
                "2028-02-29T23:59:00Z",
                "purchase",
                "p1",
                "200",
            ]
            .iter()
            .enumerate()
            {
                if i != 0 {
                    row.push(b',');
                }
                if (quotes >> i) & 1 != 0 {
                    row.push(b'"');
                }
                row.extend_from_slice(f.as_bytes());
                if (quotes >> i) & 1 != 0 {
                    row.push(b'"');
                }
            }
            let check_row = |r: &[u8]| {
                let mut body = r.to_vec();
                body.extend_from_slice(
                    b"\n,broken,,,,,\nx,,0,2028-02-29T00:00:00Z,view,p,\nx,x,0,2028-02-29T00:00:00Z,view,p,",
                );
                check(&clock, &body, true);
                check(&clock, &body, false);
            };
            check_row(&row);
            for i in 0..row.len() {
                let mut touched = row.clone();
                for b in [b'"', b',', b'\n', b'\r', b'0', b'9', b'-', 0, 0x80, 0xff] {
                    touched[i] = b;
                    check_row(&touched);
                }
                touched.remove(i);
                check_row(&touched);
            }
        }
    }
}

#[test]
fn load_edges_and_fallback() {
    let clock = ManifestClock::new(0, 86_400_000);
    for size in 55..=260 {
        for end in [b"\nlast,last,0,1970-01-01T00:00:00Z,view,p,\n".as_slice(), b""] {
            let mut body = b"\"e\",\"a\",0000000000001,\"1970-01-01T00:00:00Z\", \"malformed\",p,".to_vec();
            body.resize(size, b'x');
            body.extend_from_slice(end);
            check(&clock, &body, true);
            check(&clock, &body, false);
            body[47] = b'-';
            check(&clock, &body, true);
            check(&clock, &body, false);
        }
    }
}

#[test]
fn typed_activation_on_both_halves() {
    let clock = ManifestClock::new(0, 86_400_000);
    // Shift the timestamp bytes progressively across the 64-byte boundary.
    // Long IDs must still be borrowed; the user ID and timestamps are typed.
    for len in 8..=66 {
        let mut input = HEADER.to_vec();
        let mut body = vec![b'a'; len];
        body.extend_from_slice(b",\"u\0ser_012\",0000000000001,\"1970-01-01T00:00:00Z\",view,p,\n");
        body.resize(body.len() + 128, b'\n');
        input.extend_from_slice(&body);
        let c = chunks_of(&input, &COLS, Path::new("test"))[0];
        let mut seen = false;
        let mut accept = |r: Row<'_>| {
            assert!(!seen);
            seen = true;
            assert!(matches!(r.event, Id::Inline(_)) == (len <= 9));
            assert!(matches!(r.user, Id::Inline(k) if Some(k) == crate::person::inline(b"u\0ser_012")));
            assert_eq!(r.ts, 1);
            assert_eq!(r.ingest, (0, 0));
        };
        scan::<Model, _>(&c, &clock, &mut accept);
        assert!(seen);
        #[cfg(any(target_arch = "x86_64", feature = "event-gather-model"))]
        if cfg!(feature = "event-gather-model") || native_available() {
            seen = false;
            for_events(&c, &clock, |r| {
                seen = true;
                assert!(matches!(r.user, Id::Inline(k) if Some(k) == crate::person::inline(b"u\0ser_012")));
                assert_eq!(r.ts, 1);
                assert_eq!(r.ingest, (0, 0));
            });
            assert!(seen);
        }
    }
}

#[cfg(any(target_arch = "x86_64", feature = "event-gather-model"))]
fn native_available() -> bool {
    #[cfg(all(target_arch = "x86_64", not(feature = "event-gather-model")))]
    {
        x86::available()
    }
    #[cfg(any(not(target_arch = "x86_64"), feature = "event-gather-model"))]
    false
}
