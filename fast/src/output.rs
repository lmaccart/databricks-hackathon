//! Parquet output: one row group per file, PLAIN, uncompressed, no statistics.
//!
//! Rows are pushed straight into the parquet crate's typed column writers in
//! small blocks, so there is no intermediate Arrow array (and no second copy of
//! every column). Only the crate's own encoders, page framing, footer and
//! thrift code write bytes; nothing here touches the file format.
//!
//! Schema (rules.md, Outputs): strings are `BYTE_ARRAY (STRING)`, `day` is
//! `INT32 (DATE)`, counts are `INT64`, only `top_category` is nullable.
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
fn long(name: &str) -> Arc<Type> {
    field(name, Physical::INT64, None, false)
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
    key: &(impl Fn(crate::UserName, &mut Vec<u8>) + Sync),
    cats: &[Vec<u8>],
) {
    let per = shards.len().div_ceil(parts.max(1)).max(1);
    let sch = schema(vec![
        string("user_id", false),
        date("day"),
        long("sessions"),
        long("events"),
        long("revenue_cents"),
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
            fixed::<Int64Type, _>(&mut c, rows(), |r| r.sessions);
            close(c);
            let mut c = next(&mut rg);
            fixed::<Int64Type, _>(&mut c, rows(), |r| r.events);
            close(c);
            let mut c = next(&mut rg);
            fixed::<Int64Type, _>(&mut c, rows(), |r| r.revenue);
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
    let sch = schema(vec![
        string("campaign_id", false),
        string("channel", false),
        date("day"),
        long("sessions"),
        long("purchasing_sessions"),
        long("revenue_cents"),
        long("assisted_sessions"),
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
            fixed::<Int64Type, _>(&mut c, rows.iter(), |r| r.counts[q]);
            close(c);
        }
        rg.close().expect("close row group");
    }
    w.close().expect("close file");
}
