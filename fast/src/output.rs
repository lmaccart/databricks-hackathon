//! Parquet output: one row group per file, PLAIN, uncompressed, no statistics.
//!
//! Rows are pushed straight into the parquet crate's typed column writers in
//! small blocks, so there is no intermediate Arrow array (and no second copy of
//! every column). Only the crate's own encoders, page framing, footer and
//! thrift code write bytes; nothing here touches the file format.
//!
//! Schema (rules.md, Outputs): strings are `BYTE_ARRAY (STRING)`, `day` is
//! `INT32 (DATE)`, integers are table-wide `INT32` or `INT64`, only
//! `top_category` is nullable.
//! Every string is checked to be UTF-8 before it reaches a page; NUL bytes
//! and non-ASCII text are legal and byte-exact.
use crate::UserRow;
use bytes::Bytes;
use parquet::basic::{ConvertedType, LogicalType, Repetition, Type as Physical};
use parquet::data_type::{ByteArray, ByteArrayType, DataType, Int32Type, Int64Type};
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterPropertiesPtr};
use parquet::file::writer::{SerializedColumnWriter, SerializedFileWriter, SerializedRowGroupWriter};
use parquet::schema::types::{Type, TypePtr};
use rayon::prelude::*;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// Values handed to a column writer per call. Also the writer's own page-size
/// check interval, so a page never overshoots its limit by more than a block.
const BLOCK: usize = 4096;

fn props() -> WriterPropertiesPtr {
    Arc::new(
        WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .set_write_batch_size(BLOCK)
            .set_data_page_row_count_limit(usize::MAX)
            .build(),
    )
}

fn field(name: &str, physical: Physical, logical: Option<LogicalType>, optional: bool) -> Arc<Type> {
    let rep = if optional {
        Repetition::OPTIONAL
    } else {
        Repetition::REQUIRED
    };
    let converted = match &logical {
        Some(LogicalType::String) => ConvertedType::UTF8,
        Some(LogicalType::Date) => ConvertedType::DATE,
        _ => ConvertedType::NONE,
    };
    Arc::new(
        Type::primitive_type_builder(name, physical)
            .with_repetition(rep)
            .with_logical_type(logical)
            .with_converted_type(converted)
            .build()
            .expect("column type"),
    )
}

fn schema(fields: Vec<Arc<Type>>) -> TypePtr {
    Arc::new(
        Type::group_type_builder("schema")
            .with_fields(fields)
            .build()
            .expect("schema"),
    )
}

fn string(name: &str, optional: bool) -> Arc<Type> {
    field(name, Physical::BYTE_ARRAY, Some(LogicalType::String), optional)
}
fn date(name: &str) -> Arc<Type> {
    field(name, Physical::INT32, Some(LogicalType::Date), false)
}
fn count(name: &str, use_i32: bool) -> Arc<Type> {
    field(
        name,
        if use_i32 { Physical::INT32 } else { Physical::INT64 },
        None,
        false,
    )
}

#[inline]
fn fits_i32(value: i64) -> bool {
    i32::try_from(value).is_ok()
}

/// Fixed-width column: `value` is called once per row, in order.
fn fixed<'a, T: DataType, R: 'a>(
    cw: &mut SerializedColumnWriter<'_>,
    mut rows: impl Iterator<Item = &'a R>,
    value: impl Fn(&R) -> T::T,
) where
    T::T: Copy,
{
    let mut buf: Vec<T::T> = Vec::with_capacity(BLOCK);
    loop {
        buf.clear();
        buf.extend(rows.by_ref().take(BLOCK).map(&value));
        if buf.is_empty() {
            return;
        }
        cw.typed::<T>().write_batch(&buf, None, None).expect("write column");
    }
}

fn fixed_count<'a, R: 'a>(
    cw: &mut SerializedColumnWriter<'_>,
    rows: impl Iterator<Item = &'a R>,
    use_i32: bool,
    value: impl Fn(&R) -> i64,
) {
    if use_i32 {
        fixed::<Int32Type, _>(cw, rows, |r| value(r) as i32);
    } else {
        fixed::<Int64Type, _>(cw, rows, value);
    }
}

/// String column: `put` appends the row's bytes to the arena and returns
/// false for NULL (appending nothing). A block's values are views into one
/// shared allocation, so the only per-value cost is a reference-count bump.
fn strings<'a, R: 'a>(
    cw: &mut SerializedColumnWriter<'_>,
    mut rows: impl Iterator<Item = &'a R>,
    nullable: bool,
    mut put: impl FnMut(&R, &mut Vec<u8>) -> bool,
) {
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(BLOCK);
    let mut defs: Vec<i16> = Vec::with_capacity(BLOCK);
    let mut vals: Vec<ByteArray> = Vec::with_capacity(BLOCK);
    loop {
        let mut arena: Vec<u8> = Vec::with_capacity(BLOCK * 12);
        spans.clear();
        defs.clear();
        for r in rows.by_ref().take(BLOCK) {
            let start = arena.len();
            let present = put(r, &mut arena);
            if present {
                assert!(std::str::from_utf8(&arena[start..]).is_ok(), "IDs must be UTF-8");
                spans.push((start, arena.len()));
            }
            defs.push(present as i16);
        }
        if defs.is_empty() {
            return;
        }
        let shared = Bytes::from(arena);
        vals.clear();
        vals.extend(spans.iter().map(|&(a, b)| ByteArray::from(shared.slice(a..b))));
        let levels = nullable.then_some(&defs[..]);
        cw.typed::<ByteArrayType>()
            .write_batch(&vals, levels, None)
            .expect("write column");
    }
}

fn next<'a, W: Write + Send>(rg: &'a mut SerializedRowGroupWriter<'_, W>) -> SerializedColumnWriter<'a> {
    rg.next_column().expect("column").expect("schema has more columns")
}

fn close(cw: SerializedColumnWriter<'_>) {
    cw.close().expect("close column");
}

fn file(path: &Path, schema: TypePtr) -> SerializedFileWriter<File> {
    SerializedFileWriter::new(File::create(path).expect("create parquet"), schema, props()).expect("writer")
}

/// `user_daily`: shards are grouped into at most `parts` files, one thread each.
/// `key` appends a person's name (byte-wise smallest of their IDs).
pub fn user_daily(
    dir: &Path,
    shards: &[&[UserRow]],
    parts: usize,
    key: &(impl Fn(u64, &mut Vec<u8>) + Sync),
    cats: &[Vec<u8>],
) {
    let per = shards.len().div_ceil(parts.max(1)).max(1);
    // Select one physical integer width for the whole table so parts agree.
    let mut narrow = [true; 3];
    for r in shards.iter().flat_map(|s| s.iter()) {
        narrow[0] &= fits_i32(r.sessions);
        narrow[1] &= fits_i32(r.events);
        narrow[2] &= fits_i32(r.revenue);
    }
    let sch = schema(vec![
        string("user_id", false),
        date("day"),
        count("sessions", narrow[0]),
        count("events", narrow[1]),
        count("revenue_cents", narrow[2]),
        string("top_category", true),
    ]);
    // part-0 always exists so the table is never an empty folder.
    let groups: Vec<&[&[UserRow]]> = if shards.is_empty() {
        vec![&[][..]]
    } else {
        shards.chunks(per).collect()
    };
    groups.par_iter().enumerate().for_each(|(i, group)| {
        let n: usize = group.iter().map(|s| s.len()).sum();
        if n == 0 && i > 0 {
            return;
        }
        let rows = || group.iter().flat_map(|s| s.iter());
        let mut w = file(&dir.join(format!("part-{i}.parquet")), sch.clone());
        if n > 0 {
            let mut rg = w.next_row_group().expect("row group");
            let mut c = next(&mut rg);
            strings(&mut c, rows(), false, |r, out| {
                key(r.person, out);
                true
            });
            close(c);
            let mut c = next(&mut rg);
            fixed::<Int32Type, _>(&mut c, rows(), |r| r.day);
            close(c);
            let mut c = next(&mut rg);
            fixed_count(&mut c, rows(), narrow[0], |r| r.sessions);
            close(c);
            let mut c = next(&mut rg);
            fixed_count(&mut c, rows(), narrow[1], |r| r.events);
            close(c);
            let mut c = next(&mut rg);
            fixed_count(&mut c, rows(), narrow[2], |r| r.revenue);
            close(c);
            let mut c = next(&mut rg);
            strings(&mut c, rows(), true, |r, out| match r.top {
                Some(t) => {
                    out.extend_from_slice(&cats[t as usize]);
                    true
                }
                None => false,
            });
            close(c);
            rg.close().expect("close row group");
        }
        w.close().expect("close file");
    });
}

/// One `campaign_daily` row.
pub struct CampRow<'a> {
    pub campaign: &'a [u8],
    pub channel: &'a [u8],
    pub day: i32,
    pub counts: [i64; 4],
}

pub fn campaign_daily(path: &Path, rows: &[CampRow<'_>]) {
    // All output files for this table share these table-wide physical widths.
    let mut narrow = [true; 4];
    for row in rows {
        for q in 0..4 {
            narrow[q] &= fits_i32(row.counts[q]);
        }
    }
    let sch = schema(vec![
        string("campaign_id", false),
        string("channel", false),
        date("day"),
        count("sessions", narrow[0]),
        count("purchasing_sessions", narrow[1]),
        count("revenue_cents", narrow[2]),
        count("assisted_sessions", narrow[3]),
    ]);
    let mut w = file(path, sch);
    if !rows.is_empty() {
        let mut rg = w.next_row_group().expect("row group");
        let mut c = next(&mut rg);
        strings(&mut c, rows.iter(), false, |r, out| {
            out.extend_from_slice(r.campaign);
            true
        });
        close(c);
        let mut c = next(&mut rg);
        strings(&mut c, rows.iter(), false, |r, out| {
            out.extend_from_slice(r.channel);
            true
        });
        close(c);
        let mut c = next(&mut rg);
        fixed::<Int32Type, _>(&mut c, rows.iter(), |r| r.day);
        close(c);
        for q in 0..4 {
            let mut c = next(&mut rg);
            fixed_count(&mut c, rows.iter(), narrow[q], |r| r.counts[q]);
            close(c);
        }
        rg.close().expect("close row group");
    }
    w.close().expect("close file");
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::basic::{Compression, ConvertedType, Encoding, Repetition, Type as Physical};
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir() -> std::path::PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "shopping-sessions-output-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn table_wide_widths_and_plain_page_metadata() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let id_tail = "x".repeat(260);
        let rows: Vec<_> = (0..9000)
            .map(|i| UserRow {
                person: i,
                day: 0,
                sessions: if i == 0 { i64::MAX } else { 1 },
                events: i as i64 + 1,
                revenue: if i == 0 { i64::MAX } else { i as i64 },
                top: (i % 3 != 0).then_some(0),
            })
            .collect();
        let key = |person: u64, out: &mut Vec<u8>| {
            out.extend_from_slice(b"\0");
            out.extend_from_slice("雪🛒".as_bytes());
            out.extend_from_slice(id_tail.as_bytes());
            out.extend_from_slice(person.to_string().as_bytes());
        };
        let cats = ["カテゴリ🎁".as_bytes().to_vec()];
        user_daily(&dir, &[&rows[..4500], &rows[4500..]], 2, &key, &cats);

        for part in 0..2 {
            let reader =
                SerializedFileReader::new(File::open(dir.join(format!("part-{part}.parquet"))).unwrap()).unwrap();
            assert_eq!(reader.num_row_groups(), 1);
            let rg = reader.get_row_group(0).unwrap();
            assert_eq!(rg.metadata().num_rows(), 4500);
            let schema = rg.metadata().schema_descr();
            assert_eq!(schema.column(0).physical_type(), Physical::BYTE_ARRAY);
            assert_eq!(schema.column(0).converted_type(), ConvertedType::UTF8);
            assert_eq!(schema.column(1).physical_type(), Physical::INT32);
            assert_eq!(schema.column(1).converted_type(), ConvertedType::DATE);
            assert_eq!(schema.column(2).physical_type(), Physical::INT64);
            assert_eq!(schema.column(3).physical_type(), Physical::INT32);
            assert_eq!(schema.column(4).physical_type(), Physical::INT64);
            assert_eq!(schema.column(5).get_basic_info().repetition(), Repetition::OPTIONAL);
            for col in 0..rg.num_columns() {
                let meta = rg.metadata().column(col);
                assert_eq!(meta.compression(), Compression::UNCOMPRESSED);
                assert!(meta.statistics().is_none());
            }
            if part == 1 {
                let mut pages = rg.get_column_page_reader(0).unwrap();
                let mut page_count = 0;
                while let Some(page) = pages.get_next_page().unwrap() {
                    assert_eq!(page.encoding(), Encoding::PLAIN);
                    assert!(page.buffer().len() <= 2 * 1024 * 1024);
                    page_count += 1;
                }
                assert!(page_count > 1);
            }
        }

        let camp_path = dir.join("campaign.parquet");
        campaign_daily(
            &camp_path,
            &[
                CampRow {
                    campaign: "c_\0é".as_bytes(),
                    channel: "social🫶".as_bytes(),
                    day: 0,
                    counts: [1, 1, i64::MAX, 0],
                },
                CampRow {
                    campaign: b"c_2",
                    channel: b"search",
                    day: 1,
                    counts: [2, 1, 3, 1],
                },
            ],
        );
        let camp = SerializedFileReader::new(File::open(camp_path).unwrap()).unwrap();
        let camp_rg = camp.get_row_group(0).unwrap();
        let schema = camp_rg.metadata().schema_descr();
        assert_eq!(schema.column(3).physical_type(), Physical::INT32);
        assert_eq!(schema.column(4).physical_type(), Physical::INT32);
        assert_eq!(schema.column(5).physical_type(), Physical::INT64);
        assert_eq!(schema.column(6).physical_type(), Physical::INT32);

        let empty = temp_dir();
        std::fs::create_dir_all(&empty).unwrap();
        user_daily(&empty, &[], 1, &|_, _| {}, &[]);
        campaign_daily(&empty.join("campaign.parquet"), &[]);
        for name in ["part-0.parquet", "campaign.parquet"] {
            let reader = SerializedFileReader::new(File::open(empty.join(name)).unwrap()).unwrap();
            assert_eq!(reader.num_row_groups(), 0);
        }
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(empty).unwrap();
    }
}
