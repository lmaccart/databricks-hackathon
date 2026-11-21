//! Uncompressed Parquet output, with modest file counts and bounded user groups.
//!
//! Required fixed-width values go straight to typed writers. For grouped
//! user strings and sparse cents, splice encoded chunks into the same file
//! writer; the crate still owns page framing, row groups and file metadata.
//!
//! Schema (rules.md, Outputs): strings are `BYTE_ARRAY (STRING)`, `day` is
//! `INT32 (DATE)`, counts are `INT64`, only `top_category` is nullable.
//! Every string is checked to be UTF-8 before it reaches a page; NUL bytes
//! and non-ASCII text are legal and byte-exact.
use crate::{Map, links, person::Codec, rs};
use bytes::Bytes;
use parquet::basic::{ConvertedType, Encoding, LogicalType, Repetition, Type as Physical};
use parquet::column::page::{CompressedPage, Page, PageWriter};
use parquet::column::writer::ColumnCloseResult;
use parquet::data_type::{ByteArray, ByteArrayType, DataType, Int32Type, Int64Type};
use parquet::file::metadata::ColumnChunkMetaData;
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterPropertiesPtr};
use parquet::file::writer::{
    SerializedColumnWriter, SerializedFileWriter, SerializedPageWriter, SerializedRowGroupWriter, TrackedWrite,
};
use parquet::schema::types::{ColumnDescPtr, SchemaDescriptor, Type, TypePtr};
use rayon::prelude::*;
use std::fs::File;
use std::io::Write;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

/// Values handed to a column writer per call. Also the writer's own page-size
/// check interval, so a page never overshoots its limit by more than a block.
const BLOCK: usize = 4096;
const GROUP_ROWS: usize = 16 * 1024;
const DICT_BYTES: usize = 1024 * 1024;
const NULL_CAT: u32 = u32::MAX;

#[derive(Default)]
struct NarrowCounts {
    small: Vec<u8>,
    // 255 is the escape; entries are appended in row order.
    exceptions: Vec<(usize, i64)>,
}

impl NarrowCounts {
    #[inline]
    fn push(&mut self, value: i64) {
        if (0..255).contains(&value) {
            self.small.push(value as u8);
        } else {
            self.exceptions.push((self.small.len(), value));
            self.small.push(255);
        }
    }

    fn iter_range(&self, range: Range<usize>) -> impl Iterator<Item = i64> + Clone + '_ {
        let offset = self.exceptions.partition_point(|&(row, _)| row < range.start);
        let mut exceptions = self.exceptions[offset..].iter();
        self.small[range].iter().map(move |&value| {
            if value == 255 {
                exceptions.next().expect("count exception").1
            } else {
                i64::from(value)
            }
        })
    }
}

#[derive(Default)]
pub struct UserColumns {
    day: Vec<i32>,
    sessions: NarrowCounts,
    events: NarrowCounts,
    revenue: Vec<i64>,
    category: Vec<u32>,
    people: Vec<(u64, usize)>, // exact person token, exclusive end row
}

impl UserColumns {
    #[inline]
    pub fn push_day(&mut self, day: i32, sessions: i64, events: i64, revenue: i64, top: Option<u32>) {
        self.day.push(day);
        self.sessions.push(sessions);
        self.events.push(events);
        self.revenue.push(revenue);
        self.category.push(top.unwrap_or(NULL_CAT));
    }

    #[inline]
    pub fn finish_person(&mut self, name: u64) {
        self.people.push((name, self.day.len()));
    }
}

fn props() -> WriterPropertiesPtr {
    Arc::new(
        WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .set_offset_index_disabled(true)
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

// PLAIN BYTE_ARRAY has a four-byte length; page payload lengths are signed i32.
fn text_size(s: &str) -> usize {
    let n = 4 + s.len();
    assert!(n <= i32::MAX as usize, "string exceeds Parquet page limit");
    n
}

fn plain_text(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u32).to_le_bytes());
    b.extend_from_slice(s.as_bytes());
}

// RLE is a subset of the Parquet RLE/bit-packed hybrid. A run is an
// unsigned varint (length << 1) followed by the little-endian value.
fn rle(out: &mut Vec<u8>, n: usize, value: u32, width: usize) {
    debug_assert!(n > 0 && n <= GROUP_ROWS && width <= 4);
    let mut header = n << 1;
    while header >= 128 {
        out.push((header as u8) | 128);
        header >>= 7;
    }
    out.push(header as u8);
    out.extend_from_slice(&value.to_le_bytes()[..width]);
}

#[derive(Default)]
struct Runs {
    runs: Vec<(u32, usize)>,
}
impl Runs {
    fn push(&mut self, value: u32) {
        if let Some((last, n)) = self.runs.last_mut()
            && *last == value
        {
            *n += 1;
        } else {
            self.runs.push((value, 1));
        }
    }

    fn write(&self, out: &mut Vec<u8>, bits: u8) {
        for &(value, n) in &self.runs {
            rle(out, n, value, usize::from(bits.div_ceil(8)));
        }
    }
}

fn index_bits(entries: usize) -> u8 {
    (usize::BITS - entries.saturating_sub(1).leading_zeros()) as u8
}

// A bounded per-group dictionary preserves INT64 for all parts, even when a
// whale needs a full-width count. Beyond 256 distinct values, write PLAIN.
fn write_counts(
    rg: &mut SerializedRowGroupWriter<'_, File>,
    desc: ColumnDescPtr,
    rows: usize,
    values: impl Iterator<Item = i64> + Clone,
) {
    let mut small = [u16::MAX; 255];
    let mut other: Map<i64, u16> = Map::with_hasher(rs());
    let mut dict = Vec::new();
    let mut indices = Vec::with_capacity(rows);
    for value in values.clone() {
        let slot = if (0..255).contains(&value) {
            &mut small[value as usize]
        } else {
            other.entry(value).or_insert(u16::MAX)
        };
        if *slot == u16::MAX {
            if dict.len() == 256 * 8 {
                let mut plain = Vec::with_capacity(rows * 8);
                for v in values {
                    plain.extend_from_slice(&v.to_le_bytes());
                }
                append_page(rg, desc, rows, None, plain);
                return;
            }
            *slot = (dict.len() / 8) as u16;
            dict.extend_from_slice(&value.to_le_bytes());
        }
        indices.push(*slot as u8);
    }
    let entries = dict.len() / 8;
    let bits = index_bits(entries);
    let mut buf = vec![bits];
    if bits == 0 {
        rle(&mut buf, rows, 0, 0);
    } else {
        // A single bit-packed hybrid run, padded to a multiple of eight.
        let groups = rows.div_ceil(8);
        let mut header = (groups << 1) | 1;
        while header >= 128 {
            buf.push((header as u8) | 128);
            header >>= 7;
        }
        buf.push(header as u8);
        let mut word = 0u32;
        let mut pending = 0;
        for index in indices.into_iter().chain(std::iter::repeat_n(0, groups * 8 - rows)) {
            word |= (index as u32) << pending;
            pending += bits;
            while pending >= 8 {
                buf.push(word as u8);
                word >>= 8;
                pending -= 8;
            }
        }
        debug_assert_eq!(pending, 0);
    }
    append_page(rg, desc, rows, Some((dict, entries)), buf);
}

/// Write one already-encoded data page and its optional dictionary. Nullable
/// columns prefix the data with V1 definition levels and their byte length.
/// SerializedFileWriter takes care of rebasing offsets.
fn append_page(
    rg: &mut SerializedRowGroupWriter<'_, File>,
    desc: ColumnDescPtr,
    rows: usize,
    dict: Option<(Vec<u8>, usize)>,
    buf: Vec<u8>,
) {
    let mut encoded = TrackedWrite::new(Vec::new());
    let mut pages = SerializedPageWriter::new(&mut encoded);
    let dictionary = dict.is_some();
    if let Some((bytes, entries)) = dict {
        let size = bytes.len();
        pages
            .write_page(CompressedPage::new(
                Page::DictionaryPage {
                    buf: Bytes::from(bytes),
                    num_values: entries as u32,
                    encoding: Encoding::PLAIN,
                    is_sorted: false,
                },
                size,
            ))
            .expect("write dictionary page");
    }
    let encoding = if dictionary {
        Encoding::RLE_DICTIONARY
    } else {
        Encoding::PLAIN
    };
    let size = buf.len();
    let data = pages
        .write_page(CompressedPage::new(
            Page::DataPage {
                buf: Bytes::from(buf),
                num_values: rows as u32,
                encoding,
                def_level_encoding: Encoding::RLE,
                rep_level_encoding: Encoding::RLE,
                statistics: None,
            },
            size,
        ))
        .expect("write data page");
    let encoded = Bytes::from(encoded.into_inner().expect("column pages"));
    let mut encodings = vec![Encoding::PLAIN, Encoding::RLE];
    if dictionary {
        encodings.push(encoding);
    }
    let size = encoded.len() as i64;
    let metadata = ColumnChunkMetaData::builder(desc)
        .set_encodings(encodings)
        .set_num_values(rows as i64)
        .set_dictionary_page_offset(dictionary.then_some(0))
        .set_data_page_offset(data.offset as i64)
        .set_total_compressed_size(size)
        .set_total_uncompressed_size(size)
        .build()
        .expect("column metadata");
    rg.append_column(
        &encoded,
        ColumnCloseResult {
            bytes_written: size as u64,
            rows_written: rows as u64,
            metadata,
            bloom_filter: None,
            column_index: None,
            offset_index: None,
        },
    )
    .expect("append column");
}

#[derive(Default)]
struct UserGroup<'a> {
    rows: usize,
    spans: Vec<(&'a UserColumns, Range<usize>)>,
    names: Vec<u8>, // each disjoint run contributes one PLAIN name
    name_runs: Vec<usize>,
}

// Most category dimensions fit in a page even if every category is used.
// Only a larger dimension needs to track its per-group byte budget as rows
// are added. Like the person dictionary, a single wide value gets its own
// budget and can still be referenced by arbitrarily many rows.
#[derive(Default)]
struct CategoryBudget {
    seen: Map<u32, ()>,
    bytes: usize,
}

impl CategoryBudget {
    fn fit(&mut self, ids: &[u32], cats: &[&str]) -> usize {
        for (i, &id) in ids.iter().enumerate() {
            if id == NULL_CAT || self.seen.contains_key(&id) {
                continue;
            }
            let n = text_size(cats[id as usize]);
            if self.bytes != 0 && self.bytes + n > DICT_BYTES {
                return i;
            }
            self.bytes += n;
            self.seen.insert(id, ());
        }
        ids.len()
    }

    fn reset(&mut self) {
        self.seen.clear();
        self.bytes = 0;
    }
}

impl<'a> UserGroup<'a> {
    fn span(&mut self, shard: &'a UserColumns, range: Range<usize>) {
        if !range.is_empty() {
            self.spans.push((shard, range));
        }
    }

    fn column<T: DataType>(&self, rg: &mut SerializedRowGroupWriter<'_, File>, get: impl Fn(&UserColumns) -> &[T::T]) {
        let mut col = next(rg);
        for (s, r) in &self.spans {
            col.typed::<T>()
                .write_batch(&get(s)[r.clone()], None, None)
                .expect("write column");
        }
        close(col);
    }

    fn revenue(&self, rg: &mut SerializedRowGroupWriter<'_, File>, desc: ColumnDescPtr) {
        let values = || {
            self.spans
                .iter()
                .flat_map(|(s, r)| s.revenue[r.clone()].iter().copied())
        };
        let limit = self.rows / 8;
        let nonzero = values().filter(|&v| v != 0).take(limit + 1).count();
        if nonzero > limit {
            self.column::<Int64Type>(rg, |s| &s.revenue);
            return;
        }
        if nonzero == 0 {
            let mut buf = vec![0]; // one dictionary entry, so zero index bits
            rle(&mut buf, self.rows, 0, 0);
            append_page(rg, desc, self.rows, Some((vec![0; 8], 1)), buf);
            return;
        }
        let mut seen: Map<i64, u32> = Map::with_hasher(rs());
        let mut dict = 0i64.to_le_bytes().to_vec();
        let mut runs = Runs::default();
        for v in values() {
            let id = if v == 0 {
                0
            } else {
                *seen.entry(v).or_insert_with(|| {
                    let i = dict.len() / 8;
                    dict.extend_from_slice(&v.to_le_bytes());
                    i as u32
                })
            };
            runs.push(id);
        }
        let entries = dict.len() / 8;
        let bits = index_bits(entries);
        let mut buf = vec![bits];
        runs.write(&mut buf, bits);
        append_page(rg, desc, self.rows, Some((dict, entries)), buf);
    }

    fn categories(&self, rg: &mut SerializedRowGroupWriter<'_, File>, desc: ColumnDescPtr, names: &[&str]) {
        let ids = || {
            self.spans
                .iter()
                .flat_map(|(s, r)| s.category[r.clone()].iter().copied())
        };
        let mut levels = Runs::default();
        let mut indices = Runs::default();
        let mut seen = Map::with_hasher(rs());
        let mut dict = Vec::new();
        let mut entries = 0usize;
        for id in ids() {
            levels.push(u32::from(id != NULL_CAT));
            if id == NULL_CAT {
                continue;
            }
            let index = seen.entry(id).or_insert(entries as u32);
            if *index == entries as u32 {
                plain_text(&mut dict, names[id as usize]);
                entries += 1;
            }
            indices.push(*index);
        }
        let mut buf = vec![0; 4];
        levels.write(&mut buf, 1);
        let len = (buf.len() - 4) as u32;
        buf[..4].copy_from_slice(&len.to_le_bytes());
        let dict = if entries != 0 {
            let bits = index_bits(entries);
            buf.push(bits);
            indices.write(&mut buf, bits);
            Some((dict, entries))
        } else {
            None
        };
        append_page(rg, desc, self.rows, dict, buf);
    }

    fn write(&mut self, w: &mut SerializedFileWriter<File>, sch: &SchemaDescriptor, cats: &[&str]) {
        if self.rows == 0 {
            return;
        }
        let mut rg = w.next_row_group().expect("row group");
        let entries = self.name_runs.len();
        if entries == self.rows {
            append_page(&mut rg, sch.column(0), self.rows, None, std::mem::take(&mut self.names));
        } else {
            let bits = index_bits(entries);
            let width = usize::from(bits.div_ceil(8));
            let mut buf = Vec::with_capacity(1 + entries * (width + 1));
            buf.push(bits);
            for (index, &n) in self.name_runs.iter().enumerate() {
                rle(&mut buf, n, index as u32, width);
            }
            append_page(
                &mut rg,
                sch.column(0),
                self.rows,
                Some((std::mem::take(&mut self.names), entries)),
                buf,
            );
        }
        self.column::<Int32Type>(&mut rg, |s| &s.day);
        write_counts(
            &mut rg,
            sch.column(2),
            self.rows,
            self.spans.iter().flat_map(|(s, r)| s.sessions.iter_range(r.clone())),
        );
        write_counts(
            &mut rg,
            sch.column(3),
            self.rows,
            self.spans.iter().flat_map(|(s, r)| s.events.iter_range(r.clone())),
        );
        self.revenue(&mut rg, sch.column(4));
        self.categories(&mut rg, sch.column(5), cats);
        rg.close().expect("close row group");
        self.rows = 0;
        self.spans.clear();
        self.names.clear();
        self.name_runs.clear();
    }
}

/// `user_daily`: shards are grouped into at most `parts` files, one worker each.
pub fn user_daily(dir: &Path, shards: &[&UserColumns], parts: usize, codec: &Codec<'_>, cats: &[&str]) {
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
    let descr = SchemaDescriptor::new(sch.clone());
    let wide_categories = cats
        .iter()
        .try_fold(0, |total, s| {
            let n = total + 4 + s.len();
            (n <= DICT_BYTES).then_some(n)
        })
        .is_none();
    let groups: Vec<&[&UserColumns]> = if shards.is_empty() {
        vec![&[][..]]
    } else {
        shards.chunks(per).collect()
    };
    groups.par_iter().enumerate().for_each(|(i, group)| {
        if i > 0 && group.iter().all(|s| s.day.is_empty()) {
            return;
        }
        let mut w = file(&dir.join(format!("part-{i}.parquet")), sch.clone());

        let mut batch = UserGroup::default();
        let mut budget = wide_categories.then(CategoryBudget::default);
        for &s in *group {
            let (mut pos, mut start) = (0, 0);
            for &(token, end) in &s.people {
                let mut scratch = [0; 9];
                let name =
                    std::str::from_utf8(codec.bytes(links::unmix(token), &mut scratch)).expect("IDs must be UTF-8");
                let bytes = text_size(name);
                while pos < end {
                    if batch.rows == GROUP_ROWS || (batch.rows > 0 && batch.names.len() + bytes > DICT_BYTES) {
                        batch.span(s, start..pos);
                        batch.write(&mut w, &descr, cats);
                        if let Some(b) = &mut budget {
                            b.reset();
                        }
                        start = pos;
                    }
                    let want = (end - pos).min(GROUP_ROWS - batch.rows);
                    let n = budget
                        .as_mut()
                        .map_or(want, |b| b.fit(&s.category[pos..pos + want], cats));
                    if n > 0 {
                        plain_text(&mut batch.names, name);
                        batch.name_runs.push(n);
                        batch.rows += n;
                        pos += n;
                    }
                    if n < want {
                        batch.span(s, start..pos);
                        batch.write(&mut w, &descr, cats);
                        budget.as_mut().unwrap().reset();
                        start = pos;
                    }
                }
            }
            debug_assert_eq!(pos, s.day.len());
            batch.span(s, start..pos);
        }
        batch.write(&mut w, &descr, cats);
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

#[cfg(test)]
mod count_tests {
    use super::*;
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;

    #[test]
    fn narrow_counts_and_encoded_pages_round_trip_boundaries() {
        let path = std::env::var_os("COUNT_TEST_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(format!("target/count-test-{}.parquet", std::process::id())));
        let sch = schema(vec![long("counts")]);
        let desc = SchemaDescriptor::new(sch.clone());
        let mut writer = file(&path, sch);
        let mut expected = Vec::new();
        // Every dictionary bit width, a partial final group of eight, a plain
        // fallback, and signed i64 outliers without generating billions of events.
        let mut groups = Vec::new();
        for cardinality in [1, 2, 3, 5, 9, 17, 33, 65, 129, 256, 257] {
            groups.push(
                (0..cardinality)
                    .map(|v| v as i64)
                    .cycle()
                    .take(cardinality * 3 + 1)
                    .collect::<Vec<_>>(),
            );
        }
        groups.push(vec![i64::MIN, -1, 0, 254, 255, 256, i32::MAX as i64, i64::MAX]);
        groups.push((0..GROUP_ROWS).map(|v| (v % 3) as i64).collect());
        for values in groups {
            let mut counts = NarrowCounts::default();
            for &v in &values {
                counts.push(v);
            }
            assert_eq!(counts.iter_range(0..values.len()).collect::<Vec<_>>(), values);
            assert_eq!(
                counts.iter_range(1..values.len() - 1).collect::<Vec<_>>(),
                values[1..values.len() - 1]
            );
            let mut rg = writer.next_row_group().unwrap();
            write_counts(
                &mut rg,
                desc.column(0),
                values.len(),
                counts.iter_range(0..values.len()),
            );
            rg.close().unwrap();
            expected.extend(values);
        }
        writer.close().unwrap();
        let reader = SerializedFileReader::new(File::open(&path).unwrap()).unwrap();
        assert!(
            !reader
                .metadata()
                .row_group(10)
                .column(0)
                .encodings()
                .any(|e| e == Encoding::RLE_DICTIONARY),
            "257 distinct values must use the bounded fallback"
        );
        assert!(
            reader
                .metadata()
                .row_group(11)
                .column(0)
                .encodings()
                .any(|e| e == Encoding::RLE_DICTIONARY),
            "full-width exceptions must also survive dictionary encoding"
        );
        let actual: Vec<_> = reader
            .get_row_iter(None)
            .unwrap()
            .map(|row| row.unwrap().get_long(0).unwrap())
            .collect();
        assert_eq!(actual, expected);
        if std::env::var_os("COUNT_TEST_OUT").is_none() {
            std::fs::remove_file(path).unwrap();
        }
    }
}
