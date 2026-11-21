//! Direct PLAIN Parquet pages, framed and described by the parquet crate.
//! The only nullable column uses bit-packed definition levels; IDs are UTF-8
//! bytes without normalization. Row groups and pages are bounded independently.
use crate::UserRow;
use bytes::Bytes;
use parquet::basic::{ConvertedType, Encoding, LogicalType, Repetition, Type as Physical};
use parquet::column::page::{CompressedPage, Page, PageWriter};
use parquet::file::metadata::{
    ColumnChunkMetaData, FileMetaData, ParquetMetaData, ParquetMetaDataWriter, RowGroupMetaData,
};
use parquet::file::writer::{SerializedPageWriter, TrackedWrite};
use parquet::schema::types::{SchemaDescriptor, Type};
use rayon::prelude::*;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

const GROUP_ROWS: usize = 65_536;
const PAGE_BYTES: usize = 1024 * 1024;
const LEVEL_RESERVE: usize = 4 + 3 + GROUP_ROWS.div_ceil(8);

fn field(name: &str, physical: Physical, logical: Option<LogicalType>, optional: bool) -> Arc<Type> {
    let converted = match &logical {
        Some(LogicalType::String) => ConvertedType::UTF8,
        Some(LogicalType::Date) => ConvertedType::DATE,
        _ => ConvertedType::NONE,
    };
    Arc::new(
        Type::primitive_type_builder(name, physical)
            .with_repetition(if optional {
                Repetition::OPTIONAL
            } else {
                Repetition::REQUIRED
            })
            .with_logical_type(logical)
            .with_converted_type(converted)
            .build()
            .expect("column type"),
    )
}
fn string(name: &str, optional: bool) -> Arc<Type> {
    field(name, Physical::BYTE_ARRAY, Some(LogicalType::String), optional)
}
fn date() -> Arc<Type> {
    field("day", Physical::INT32, Some(LogicalType::Date), false)
}
fn integer(name: &str, narrow: bool) -> Arc<Type> {
    field(
        name,
        if narrow { Physical::INT32 } else { Physical::INT64 },
        None,
        false,
    )
}
fn schema(fields: Vec<Arc<Type>>) -> Arc<SchemaDescriptor> {
    Arc::new(SchemaDescriptor::new(Arc::new(
        Type::group_type_builder("schema")
            .with_fields(fields)
            .build()
            .expect("schema"),
    )))
}
fn int(out: &mut Vec<u8>, value: i64, narrow: bool) {
    if narrow {
        out.extend_from_slice(&i32::try_from(value).expect("integer width").to_le_bytes());
    } else {
        out.extend_from_slice(&value.to_le_bytes());
    }
}
fn text(out: &mut Vec<u8>, put: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    put(out);
    let len = out.len() - start - 4;
    assert!(std::str::from_utf8(&out[start + 4..]).is_ok(), "IDs must be UTF-8");
    out[start..start + 4].copy_from_slice(&u32::try_from(len).expect("Parquet string too long").to_le_bytes());
}
fn varint(mut n: usize, out: &mut [u8; 3]) -> usize {
    let mut i = 0;
    loop {
        out[i] = (n as u8 & 127) | if n >= 128 { 128 } else { 0 };
        i += 1;
        n >>= 7;
        if n == 0 {
            return i;
        }
    }
}

struct Output {
    sink: TrackedWrite<File>,
    schema: Arc<SchemaDescriptor>,
    groups: Vec<RowGroupMetaData>,
    buf: Vec<u8>,
}
impl Output {
    fn new(path: &Path, schema: Arc<SchemaDescriptor>) -> Self {
        let mut sink = TrackedWrite::new(File::create(path).expect("create parquet"));
        sink.write_all(b"PAR1").expect("magic");
        Self {
            sink,
            schema,
            groups: vec![],
            buf: Vec::new(),
        }
    }
    fn column<R>(
        &mut self,
        index: usize,
        rows: &[R],
        optional: bool,
        mut put: impl FnMut(&R, &mut Vec<u8>) -> bool,
    ) -> ColumnChunkMetaData {
        let start = self.sink.bytes_written();
        let mut cursor = 0;
        while cursor < rows.len() {
            self.buf.clear();
            if optional {
                self.buf.resize(LEVEL_RESERVE, 0);
            }
            let mut n = 0;
            while cursor < rows.len() && n < GROUP_ROWS && (n == 0 || self.buf.len() < PAGE_BYTES) {
                let before = self.buf.len();
                let present = put(&rows[cursor], &mut self.buf);
                assert!(optional || present, "null in required column");
                // A long field can cross the page limit. Retry it on a fresh
                // page rather than letting an otherwise valid page overflow.
                if self.buf.len() > i32::MAX as usize && n != 0 {
                    self.buf.truncate(before);
                    break;
                }
                if optional && present {
                    self.buf[7 + n / 8] |= 1 << (n % 8);
                }
                n += 1;
                cursor += 1;
            }
            if optional {
                // Parquet V1: four-byte level-stream length, then one hybrid
                // bit-packed run (padded to a multiple of eight values).
                let level_bytes = n.div_ceil(8);
                let mut header = [0u8; 3];
                let h = varint(level_bytes * 2 + 1, &mut header);
                let prefix = 4 + h + level_bytes;
                self.buf.copy_within(7..7 + level_bytes, 4 + h);
                self.buf[4..4 + h].copy_from_slice(&header[..h]);
                self.buf[..4].copy_from_slice(&(u32::try_from(h + level_bytes).unwrap()).to_le_bytes());
                self.buf.copy_within(LEVEL_RESERVE.., prefix);
                self.buf.truncate(prefix + self.buf.len() - LEVEL_RESERVE);
            }
            let data = Bytes::from(std::mem::take(&mut self.buf));
            let size = data.len();
            assert!(size <= i32::MAX as usize, "Parquet page exceeds format limit");
            let page = Page::DataPage {
                buf: data.clone(),
                num_values: n as u32,
                encoding: Encoding::PLAIN,
                def_level_encoding: Encoding::RLE,
                rep_level_encoding: Encoding::RLE,
                statistics: None,
            };
            SerializedPageWriter::new(&mut self.sink)
                .write_page(CompressedPage::new(page, size))
                .expect("write page");
            self.buf = Vec::from(data.try_into_mut().expect("page buffer exclusively owned"));
        }
        let size = i64::try_from(self.sink.bytes_written() - start).expect("column size");
        ColumnChunkMetaData::builder(self.schema.column(index))
            .set_encodings(vec![Encoding::PLAIN, Encoding::RLE])
            .set_num_values(i64::try_from(rows.len()).expect("row count"))
            .set_data_page_offset(i64::try_from(start).expect("file offset"))
            .set_total_compressed_size(size)
            .set_total_uncompressed_size(size)
            .build()
            .expect("column metadata")
    }
    fn group(&mut self, rows: usize, columns: Vec<ColumnChunkMetaData>) {
        let bytes = columns.iter().map(|c| c.uncompressed_size()).sum();
        self.groups.push(
            RowGroupMetaData::builder(self.schema.clone())
                .set_column_metadata(columns)
                .set_total_byte_size(bytes)
                .set_num_rows(i64::try_from(rows).expect("row count"))
                .build()
                .expect("row group"),
        );
    }
    fn finish(mut self) {
        let rows = self.groups.iter().map(|g| g.num_rows()).sum();
        let metadata = ParquetMetaData::new(
            FileMetaData::new(1, rows, Some("shopping_sessions".into()), None, self.schema, None),
            self.groups,
        );
        // The metadata writer owns an internal buffered writer and does not
        // expose its final flush error. Buffer the footer in memory, then
        // explicitly write and flush the real file so disk errors propagate.
        let mut footer = Vec::new();
        ParquetMetaDataWriter::new(&mut footer, &metadata)
            .finish()
            .expect("footer");
        assert!(footer.len() - 8 <= u32::MAX as usize, "Parquet footer too large");
        self.sink.write_all(&footer).expect("write footer");
        self.sink.flush().expect("flush parquet");
    }
}

pub fn user_daily(
    dir: &Path,
    shards: &[&[UserRow]],
    parts: usize,
    key: &(impl Fn(u64, &mut Vec<u8>) + Sync),
    cats: &[Vec<u8>],
) {
    let mut narrow = [true; 3];
    for r in shards.iter().flat_map(|s| s.iter()) {
        for (ok, value) in narrow.iter_mut().zip([r.sessions, r.events, r.revenue]) {
            *ok &= i32::try_from(value).is_ok();
        }
    }
    let sch = schema(vec![
        string("user_id", false),
        date(),
        integer("sessions", narrow[0]),
        integer("events", narrow[1]),
        integer("revenue_cents", narrow[2]),
        string("top_category", true),
    ]);
    let per = shards.len().div_ceil(parts.max(1)).max(1);
    let groups: Vec<&[&[UserRow]]> = if shards.is_empty() {
        vec![&[]]
    } else {
        shards.chunks(per).collect()
    };
    groups.par_iter().enumerate().for_each(|(i, shards)| {
        if i > 0 && shards.iter().all(|s| s.is_empty()) {
            return;
        }
        let mut out = Output::new(&dir.join(format!("part-{i}.parquet")), sch.clone());
        let mut iter = shards.iter().flat_map(|s| s.iter());
        let mut batch = Vec::with_capacity(GROUP_ROWS);
        loop {
            batch.clear();
            batch.extend(iter.by_ref().take(GROUP_ROWS));
            if batch.is_empty() {
                break;
            }
            let mut cols = Vec::with_capacity(6);
            cols.push(out.column(0, &batch, false, |r, b| {
                text(b, |b| key(r.person, b));
                true
            }));
            cols.push(out.column(1, &batch, false, |r, b| {
                b.extend_from_slice(&r.day.to_le_bytes());
                true
            }));
            cols.push(out.column(2, &batch, false, |r, b| {
                int(b, r.sessions, narrow[0]);
                true
            }));
            cols.push(out.column(3, &batch, false, |r, b| {
                int(b, r.events, narrow[1]);
                true
            }));
            cols.push(out.column(4, &batch, false, |r, b| {
                int(b, r.revenue, narrow[2]);
                true
            }));
            cols.push(out.column(5, &batch, true, |r, b| match r.top {
                Some(t) => {
                    text(b, |b| b.extend_from_slice(&cats[t as usize]));
                    true
                }
                None => false,
            }));
            out.group(batch.len(), cols);
        }
        out.finish();
    });
}

pub struct CampRow<'a> {
    pub campaign: &'a [u8],
    pub channel: &'a [u8],
    pub day: i32,
    pub counts: [i64; 4],
}
pub fn campaign_daily(path: &Path, rows: &[CampRow<'_>]) {
    let mut narrow = [true; 4];
    for r in rows {
        for (ok, value) in narrow.iter_mut().zip(r.counts) {
            *ok &= i32::try_from(value).is_ok();
        }
    }
    let sch = schema(vec![
        string("campaign_id", false),
        string("channel", false),
        date(),
        integer("sessions", narrow[0]),
        integer("purchasing_sessions", narrow[1]),
        integer("revenue_cents", narrow[2]),
        integer("assisted_sessions", narrow[3]),
    ]);
    let mut out = Output::new(path, sch);
    for batch in rows.chunks(GROUP_ROWS) {
        let mut cols = Vec::with_capacity(7);
        cols.push(out.column(0, batch, false, |r, b| {
            text(b, |b| b.extend_from_slice(r.campaign));
            true
        }));
        cols.push(out.column(1, batch, false, |r, b| {
            text(b, |b| b.extend_from_slice(r.channel));
            true
        }));
        cols.push(out.column(2, batch, false, |r, b| {
            b.extend_from_slice(&r.day.to_le_bytes());
            true
        }));
        for q in 0..4 {
            cols.push(out.column(3 + q, batch, false, |r, b| {
                int(b, r.counts[q], narrow[q]);
                true
            }));
        }
        out.group(batch.len(), cols);
    }
    out.finish();
}
