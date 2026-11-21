//! Parquet output: bounded row groups, PLAIN, uncompressed, no statistics.
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
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// Values handed to a column writer per call. Also the writer's own page-size
/// check interval, so a page never overshoots its limit by more than a block.
const BLOCK: usize = 4096;
const USER_ROW_GROUP: usize = 8 * 1024;

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

fn user_ids(cw: &mut SerializedColumnWriter<'_>, rows: &[UserRow], key: &(dyn Fn(u64, &mut Vec<u8>) + Sync)) {
    let mut spans = Vec::with_capacity(BLOCK);
    let mut values = Vec::with_capacity(BLOCK);
    for block in rows.chunks(BLOCK) {
        let mut arena = Vec::with_capacity(BLOCK * 12);
        spans.clear();
        let mut last = None;
        let mut span = (0, 0);
        for r in block {
            if last != Some(r.person) {
                let start = arena.len();
                key(r.person, &mut arena);
                assert!(std::str::from_utf8(&arena[start..]).is_ok(), "IDs must be UTF-8");
                span = (start, arena.len());
                last = Some(r.person);
            }
            spans.push(span);
        }
        let shared = Bytes::from(arena);
        values.clear();
        values.extend(spans.iter().map(|&(a, b)| ByteArray::from(shared.slice(a..b))));
        cw.typed::<ByteArrayType>()
            .write_batch(&values, None, None)
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

fn user_schema() -> TypePtr {
    schema(vec![
        string("user_id", false),
        date("day"),
        long("sessions"),
        long("events"),
        long("revenue_cents"),
        string("top_category", true),
    ])
}

/// One lane belongs to a Rayon worker. It retains at most one bounded batch
/// of user rows across shards; all files use the same physical schema.
pub struct UserWriter<'a> {
    dir: &'a Path,
    part: usize,
    key: &'a (dyn Fn(u64, &mut Vec<u8>) + Sync),
    cats: &'a [Vec<u8>],
    rows: Vec<UserRow>,
    writer: Option<SerializedFileWriter<File>>,
}

impl<'a> UserWriter<'a> {
    pub fn new(dir: &'a Path, part: usize, key: &'a (dyn Fn(u64, &mut Vec<u8>) + Sync), cats: &'a [Vec<u8>]) -> Self {
        Self {
            dir,
            part,
            key,
            cats,
            rows: Vec::new(),
            writer: None,
        }
    }

    pub fn push(&mut self, row: UserRow) {
        self.rows.push(row);
        if self.rows.len() == USER_ROW_GROUP {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let w = self
            .writer
            .get_or_insert_with(|| file(&self.dir.join(format!("part-{}.parquet", self.part)), user_schema()));
        let rows = || self.rows.iter();
        {
            let mut rg = w.next_row_group().expect("row group");
            let mut c = next(&mut rg);
            user_ids(&mut c, &self.rows, self.key);
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
                    out.extend_from_slice(&self.cats[t as usize]);
                    true
                }
                None => false,
            });
            close(c);
            rg.close().expect("close row group");
        }
        self.rows.clear();
    }

    pub fn finish(mut self) {
        self.flush();
        // Even an entirely empty input must produce a readable table.
        if self.part == 0 && self.writer.is_none() {
            self.writer = Some(file(&self.dir.join("part-0.parquet"), user_schema()));
        }
        if let Some(w) = self.writer {
            w.close().expect("close file");
        }
    }
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
