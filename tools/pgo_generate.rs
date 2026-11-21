//! Deterministic, synthetic profile inputs. No data, IDs, seeds or answers from
//! a competition dataset are used. Three different shapes exercise the word
//! parser, its strict fallbacks, links, clicks, purchases and output.
use std::env;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 ^ (self.0 >> 23)
    }
    fn pick(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn field(&mut self, value: &str) -> String {
        if self.pick(5) == 0 {
            format!("\"{value}\"")
        } else {
            value.to_owned()
        }
    }
}

fn writer(path: &Path, header: &str) -> BufWriter<File> {
    let mut f = BufWriter::new(File::create(path).unwrap());
    writeln!(f, "{header}").unwrap();
    f
}

fn id(prefix: char, n: u64, width: usize) -> String {
    format!("{prefix}{n:0width$}", width = width - 1)
}

fn main() {
    let args: Vec<_> = env::args().collect();
    assert_eq!(args.len(), 4, "pgo_generate OUTPUT LAYOUT EVENTS");
    let root = Path::new(&args[1]);
    let layout: usize = args[2].parse().unwrap();
    let count: u64 = args[3].parse().unwrap();
    assert!(layout < 3 && count >= 100);
    for d in ["events", "clicks", "links"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    // Three independent calendar layouts. All timestamps
    // are in-window; raw epoch, offset and quoted forms vary independently.
    let (year, month, start_epoch): (u32, u32, i64) = [
        (2025, 6, 1748736000),
        (2026, 10, 1790812800),
        (2028, 2, 1832976000),
    ][layout];
    fs::write(root.join("manifest.json"), format!(
        "{{\"window_start\":\"{year:04}-{month:02}-01T00:00:00Z\",\"window_end\":\"{year:04}-{month:02}-15T00:00:00Z\"}}"
    )).unwrap();
    let mut products = writer(&root.join("products.csv"), "product_id,category");
    for n in 0..64 {
        writeln!(products, "p{n:04},cat{}", n % 8).unwrap();
    }
    let mut campaigns = writer(&root.join("campaigns.csv"), "campaign_id,channel");
    for n in 0..16 {
        writeln!(campaigns, "c_{n},channel{}", n % 4).unwrap();
    }
    let mut rng = Rng(0xfebc_57ac_28d3_4b9d ^ layout as u64);
    let mut ev = writer(
        &root.join("events/part.csv"),
        "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents",
    );
    let mut reordered = writer(
        &root.join("events/reordered.csv"),
        "user_id,event_id,ts,ingest_ts,event_type,product_id,price_cents",
    );
    let people = (count / 24).max(32);
    for n in 0..count {
        let width = 7 + (rng.pick(3) as usize);
        let eid = id('e', n, width);
        // A few frequently active people, the rest widely distributed.
        let person = if rng.pick(12) == 0 {
            rng.pick(8)
        } else {
            rng.pick(people)
        };
        let uid = if n % 101 == 0 {
            format!("u{person}")
        } else {
            id('u', person, width)
        };
        let day = rng.pick(14);
        let sec = rng.pick(86400);
        let civil = format!(
            "{year:04}-{month:02}-{:02}T{:02}:{:02}:{:02}",
            day + 1,
            sec / 3600,
            sec / 60 % 60,
            sec % 60
        );
        let ingest = format!("{civil}Z");
        let ts = match rng.pick(10) {
            0 => format!("{civil}+00:00"),
            1..=3 => ((start_epoch + (day * 86400 + sec) as i64) * 1000).to_string(),
            _ => ingest.clone(),
        };
        let typ = if rng.pick(33) == 0 {
            "purchase"
        } else if rng.pick(5) == 0 {
            "cart"
        } else {
            "view"
        };
        let product = if n % 131 == 0 {
            "long-product-fallback".to_string()
        } else {
            format!("p{:04}", rng.pick(64))
        };
        let price = if typ == "purchase" {
            (100 + rng.pick(49900)).to_string()
        } else {
            String::new()
        };
        let vals = [eid, uid, ts, ingest, typ.into(), product, price];
        let quoted: Vec<_> = vals.iter().map(|s| rng.field(s)).collect();
        if n % 97 == 0 {
            writeln!(
                reordered,
                "{},{},{},{},{},{},{}",
                quoted[1], quoted[0], quoted[2], quoted[3], quoted[4], quoted[5], quoted[6]
            )
            .unwrap();
        } else {
            writeln!(ev, "{}", quoted.join(",")).unwrap();
        }
        // Identical copies exercise the exact claim/dedup path without
        // deriving any values from the datasets being evaluated.
        if n % 37 == 0 {
            writeln!(ev, "{}", quoted.join(",")).unwrap();
        }
    }
    let mut clicks = writer(
        &root.join("clicks/part.csv"),
        "click_id,user_id,campaign_id,ts",
    );
    for n in 0..count / 8 {
        let width = 7 + rng.pick(3) as usize;
        let person = rng.pick(people);
        let day = rng.pick(14);
        let sec = rng.pick(86400);
        let vals = [
            id('k', n, width),
            id('u', person, width),
            format!("c_{}", rng.pick(16)),
            format!(
                "{year:04}-{month:02}-{:02}T{:02}:{:02}:{:02}Z",
                day + 1,
                sec / 3600,
                sec / 60 % 60,
                sec % 60
            ),
        ];
        writeln!(
            clicks,
            "{}",
            vals.iter()
                .map(|v| rng.field(v))
                .collect::<Vec<_>>()
                .join(",")
        )
        .unwrap();
    }
    let mut links = writer(&root.join("links/part.csv"), "user_id_a,user_id_b");
    for n in 0..people / 4 {
        writeln!(links, "{},{}", id('u', n, 7), id('u', n, 8)).unwrap();
    }
}
