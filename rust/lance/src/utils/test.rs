// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

use std::fmt::{Display, Formatter};
use std::ops::Range;
use std::sync::atomic::AtomicU16;
use std::sync::{Arc, Mutex};

use arrow_array::{RecordBatch, RecordBatchIterator};
use arrow_schema::Schema as ArrowSchema;
use bytes::Bytes;
use datafusion_physical_plan::ExecutionPlan;
use futures::stream::BoxStream;
use lance_arrow::RecordBatchExt;
use lance_core::datatypes::Schema;
use lance_datagen::{BatchCount, BatchGeneratorBuilder, ByteCount, RowCount};
use lance_file::version::LanceFileVersion;
use lance_io::object_store::WrappingObjectStore;
use lance_table::format::Fragment;
use object_store::path::Path;
use object_store::{
    GetOptions, GetRange, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOpts, PutOptions, PutPayload, PutResult, Result as OSResult, UploadPart,
};
use rand::prelude::SliceRandom;
use rand::{Rng, SeedableRng};
use tempfile::{tempdir, TempDir};

use crate::dataset::fragment::write::FragmentCreateBuilder;
use crate::dataset::transaction::Operation;
use crate::dataset::WriteParams;
use crate::Dataset;

mod throttle_store;

pub use throttle_store::ThrottledStoreWrapper;

/// A dataset generator that can generate random layouts. This is used to test
/// dataset operations are robust to different layouts.
///
/// "Layout" includes: How the fields are split across files within the same
/// fragment, the order of the field ids, and the order of fields across files.
pub struct TestDatasetGenerator {
    seed: Option<u64>,
    data: Vec<RecordBatch>,
    data_storage_version: LanceFileVersion,
}

impl TestDatasetGenerator {
    /// Create a new dataset generator with the given data.
    ///
    /// Each batch will become a separate fragment in the dataset.
    pub fn new(data: Vec<RecordBatch>, data_storage_version: LanceFileVersion) -> Self {
        assert!(!data.is_empty());
        Self {
            data,
            seed: None,
            data_storage_version,
        }
    }

    /// Set the seed for the random number generator.
    ///
    /// If not set, a random seed will be generated on each call to [`Self::make_hostile`].
    #[allow(dead_code)]
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Make a new dataset that has a "hostile" layout.
    ///
    /// For this to be effective, there should be at least two top-level columns.
    ///
    /// By "hostile", we mean that:
    /// 1. Top-level columns are randomly split into different files. If there
    ///    are multiple fragments, they do not all have the same arrangement of
    ///    fields in data files. There is an exception for single-column data,
    ///    which will always be in a single file.
    /// 2. The field ids are not in sorted order, and have at least one hole.
    /// 3. The order of fields across the data files is random, and not
    ///    consistent across fragments.
    ///
    pub async fn make_hostile(&self, uri: &str) -> Dataset {
        let seed = self.seed.unwrap_or_else(|| rand::thread_rng().gen());
        let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);
        let schema = self.make_schema(&mut rng);

        // If we only have one fragment, we should split it into two files. But
        // if we have multiple fragments, we can allow one of them to have a single
        // file. This prevents an infinite loop.
        let min_num_files = if self.data.len() > 1 { 1 } else { 2 };

        let mut fragments = Vec::with_capacity(self.data.len());
        let mut id = 0;

        for batch in &self.data {
            loop {
                let mut fragment = self
                    .make_fragment(uri, batch, &schema, &mut rng, min_num_files)
                    .await;

                let fields = field_structure(&fragment);
                let first_fields = fragments.first().map(field_structure);
                if let Some(first_fields) = first_fields {
                    if fields == first_fields && schema.fields.len() > 1 {
                        // The layout is the same as the first fragment, try again
                        // If there's only one field, then we can't expect a different
                        // layout, so there's an exception for that.
                        continue;
                    }
                }

                fragment.id = id;
                id += 1;
                fragments.push(fragment);
                break;
            }
        }

        let operation = Operation::Overwrite {
            fragments,
            schema,
            config_upsert_values: None,
        };

        Dataset::commit(
            uri,
            operation,
            None,
            Default::default(),
            None,
            Default::default(),
            false,
        )
        .await
        .unwrap()
    }

    fn make_schema(&self, rng: &mut impl Rng) -> Schema {
        let arrow_schema = self.data[0].schema();
        let mut schema = Schema::try_from(arrow_schema.as_ref()).unwrap();

        let field_ids = schema.fields_pre_order().map(|f| f.id).collect::<Vec<_>>();
        let mut new_ids = field_ids.clone();
        // Add a hole
        if new_ids.len() > 2 {
            let hole_pos = rng.gen_range(1..new_ids.len() - 1);
            for id in new_ids.iter_mut().skip(hole_pos) {
                *id += 1;
            }
        }
        // Randomize the order of ids
        loop {
            new_ids.shuffle(rng);
            // In case we accidentally shuffled to the same order
            if new_ids.len() == 1 || new_ids != field_ids {
                break;
            }
        }
        for (old_id, new_id) in field_ids.iter().zip(new_ids.iter()) {
            let field = schema.mut_field_by_id(*old_id).unwrap();
            field.id = *new_id;
        }

        schema
    }

    async fn make_fragment(
        &self,
        uri: &str,
        batch: &RecordBatch,
        schema: &Schema,
        rng: &mut impl Rng,
        min_num_files: usize,
    ) -> Fragment {
        // Choose a random number of files.
        let num_files = if batch.num_columns() == 1 {
            1
        } else {
            rng.gen_range(min_num_files..=batch.num_columns())
        };

        // Randomly assign top level fields to files.
        let column_names = batch
            .schema()
            .fields
            .iter()
            .map(|f| f.name().clone())
            .collect::<Vec<_>>();
        let mut file_assignments = (0..num_files)
            .cycle()
            .take(column_names.len())
            .collect::<Vec<_>>();
        file_assignments.shuffle(rng);

        // Write each as own fragment.
        let mut sub_fragments = Vec::with_capacity(num_files);
        for file_id in 0..num_files {
            let columns = column_names
                .iter()
                .zip(file_assignments.iter())
                .filter_map(|(name, &file)| {
                    if file == file_id {
                        Some(name.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            let file_schema = schema.project(&columns).unwrap();
            let file_arrow_schema = Arc::new(ArrowSchema::from(&file_schema));
            let data = batch.project_by_schema(file_arrow_schema.as_ref()).unwrap();
            let reader = RecordBatchIterator::new(vec![Ok(data)], file_arrow_schema.clone());
            let sub_frag = FragmentCreateBuilder::new(uri)
                .schema(&file_schema)
                .write_params(&WriteParams {
                    data_storage_version: Some(self.data_storage_version),
                    ..Default::default()
                })
                .write(reader, None)
                .await
                .unwrap();

            // The sub_fragment has it's own schema, with field ids that are local to
            // it. We need to remap the field ids to the global schema.

            sub_fragments.push(sub_frag);
        }

        // Combine the fragments into a single one.
        let mut files = sub_fragments
            .into_iter()
            .flat_map(|frag| frag.files.into_iter())
            .collect::<Vec<_>>();

        // Make sure the field id order is distinct from the schema.
        let schema_field_ids = schema.fields_pre_order().map(|f| f.id).collect::<Vec<_>>();
        if files
            .iter()
            .flat_map(|file| file.fields.iter().cloned())
            .collect::<Vec<_>>()
            == schema_field_ids
            && files.len() > 1
        {
            // Swap first two files
            files.swap(0, 1);
        }

        Fragment {
            id: 0,
            files,
            deletion_file: None,
            row_id_meta: None,
            physical_rows: Some(batch.num_rows()),
        }
    }
}

fn get_field_structure(dataset: &Dataset) -> Vec<Vec<Vec<i32>>> {
    dataset
        .get_fragments()
        .into_iter()
        .map(|frag| field_structure(frag.metadata()))
        .collect::<Vec<_>>()
}

fn field_structure(fragment: &Fragment) -> Vec<Vec<i32>> {
    fragment
        .files
        .iter()
        .map(|file| file.fields.clone())
        .collect::<Vec<_>>()
}

#[derive(Debug, Default)]
pub struct IoStats {
    pub read_iops: u64,
    pub read_bytes: u64,
    pub write_iops: u64,
    pub write_bytes: u64,
    /// Number of disjoint periods where at least one IO is in-flight.
    pub num_hops: u64,
    pub requests: Vec<IoRequestRecord>,
}

// These fields are "dead code" because we just use them right now to display
// in test failure messages through Debug. (The lint ignores Debug impls.)
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct IoRequestRecord {
    pub method: &'static str,
    pub path: Path,
    pub range: Option<Range<u64>>,
}

impl Display for IoStats {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#?}", self)
    }
}

#[derive(Debug)]
pub struct IoTrackingStore {
    target: Arc<dyn ObjectStore>,
    stats: Arc<Mutex<IoStats>>,
    active_requests: Arc<AtomicU16>,
}

impl Display for IoTrackingStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#?}", self)
    }
}

#[derive(Debug, Default, Clone)]
pub struct StatsHolder(Arc<Mutex<IoStats>>);

impl StatsHolder {
    pub fn incremental_stats(&self) -> IoStats {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl WrappingObjectStore for StatsHolder {
    fn wrap(&self, target: Arc<dyn ObjectStore>) -> Arc<dyn ObjectStore> {
        Arc::new(IoTrackingStore {
            target,
            stats: self.0.clone(),
            active_requests: Arc::new(AtomicU16::new(0)),
        })
    }
}

impl IoTrackingStore {
    pub fn new_wrapper() -> (Arc<dyn WrappingObjectStore>, Arc<Mutex<IoStats>>) {
        let stats = Arc::new(Mutex::new(IoStats::default()));
        (Arc::new(StatsHolder(stats.clone())), stats)
    }

    fn record_read(
        &self,
        method: &'static str,
        path: Path,
        num_bytes: u64,
        range: Option<Range<u64>>,
    ) {
        let mut stats = self.stats.lock().unwrap();
        stats.read_iops += 1;
        stats.read_bytes += num_bytes;
        stats.requests.push(IoRequestRecord {
            method,
            path,
            range,
        });
    }

    fn record_write(&self, num_bytes: u64) {
        let mut stats = self.stats.lock().unwrap();
        stats.write_iops += 1;
        stats.write_bytes += num_bytes;
    }

    fn hop_guard(&self) -> HopGuard {
        HopGuard::new(self.active_requests.clone(), self.stats.clone())
    }
}

#[async_trait::async_trait]
#[deny(clippy::missing_trait_methods)]
impl ObjectStore for IoTrackingStore {
    async fn put(&self, location: &Path, bytes: PutPayload) -> OSResult<PutResult> {
        let _guard = self.hop_guard();
        self.record_write(bytes.content_length() as u64);
        self.target.put(location, bytes).await
    }

    async fn put_opts(
        &self,
        location: &Path,
        bytes: PutPayload,
        opts: PutOptions,
    ) -> OSResult<PutResult> {
        let _guard = self.hop_guard();
        self.record_write(bytes.content_length() as u64);
        self.target.put_opts(location, bytes, opts).await
    }

    async fn put_multipart(&self, location: &Path) -> OSResult<Box<dyn MultipartUpload>> {
        let _guard = self.hop_guard();
        let target = self.target.put_multipart(location).await?;
        Ok(Box::new(IoTrackingMultipartUpload {
            target,
            stats: self.stats.clone(),
            _guard,
        }))
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOpts,
    ) -> OSResult<Box<dyn MultipartUpload>> {
        let _guard = self.hop_guard();
        let target = self.target.put_multipart_opts(location, opts).await?;
        Ok(Box::new(IoTrackingMultipartUpload {
            target,
            stats: self.stats.clone(),
            _guard,
        }))
    }

    async fn get(&self, location: &Path) -> OSResult<GetResult> {
        let _guard = self.hop_guard();
        let result = self.target.get(location).await;
        if let Ok(result) = &result {
            let num_bytes = result.range.end - result.range.start;
            self.record_read("get", location.to_owned(), num_bytes, None);
        }
        result
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> OSResult<GetResult> {
        let _guard = self.hop_guard();
        let range = match &options.range {
            Some(GetRange::Bounded(range)) => Some(range.clone()),
            _ => None, // TODO: fill in other options.
        };
        let result = self.target.get_opts(location, options).await;
        if let Ok(result) = &result {
            let num_bytes = result.range.end - result.range.start;

            self.record_read("get_opts", location.to_owned(), num_bytes, range);
        }
        result
    }

    async fn get_range(&self, location: &Path, range: Range<u64>) -> OSResult<Bytes> {
        let _guard = self.hop_guard();
        let result = self.target.get_range(location, range.clone()).await;
        if let Ok(result) = &result {
            self.record_read(
                "get_range",
                location.to_owned(),
                result.len() as u64,
                Some(range),
            );
        }
        result
    }

    async fn get_ranges(&self, location: &Path, ranges: &[Range<u64>]) -> OSResult<Vec<Bytes>> {
        let _guard = self.hop_guard();
        let result = self.target.get_ranges(location, ranges).await;
        if let Ok(result) = &result {
            self.record_read(
                "get_ranges",
                location.to_owned(),
                result.iter().map(|b| b.len() as u64).sum(),
                None,
            );
        }
        result
    }

    async fn head(&self, location: &Path) -> OSResult<ObjectMeta> {
        let _guard = self.hop_guard();
        self.record_read("head", location.to_owned(), 0, None);
        self.target.head(location).await
    }

    async fn delete(&self, location: &Path) -> OSResult<()> {
        let _guard = self.hop_guard();
        self.record_write(0);
        self.target.delete(location).await
    }

    fn delete_stream<'a>(
        &'a self,
        locations: BoxStream<'a, OSResult<Path>>,
    ) -> BoxStream<'a, OSResult<Path>> {
        self.target.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, OSResult<ObjectMeta>> {
        let _guard = self.hop_guard();
        self.record_read("list", prefix.cloned().unwrap_or_default(), 0, None);
        self.target.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, OSResult<ObjectMeta>> {
        self.record_read(
            "list_with_offset",
            prefix.cloned().unwrap_or_default(),
            0,
            None,
        );
        self.target.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> OSResult<ListResult> {
        let _guard = self.hop_guard();
        self.record_read(
            "list_with_delimiter",
            prefix.cloned().unwrap_or_default(),
            0,
            None,
        );
        self.target.list_with_delimiter(prefix).await
    }

    async fn copy(&self, from: &Path, to: &Path) -> OSResult<()> {
        let _guard = self.hop_guard();
        self.record_write(0);
        self.target.copy(from, to).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> OSResult<()> {
        let _guard = self.hop_guard();
        self.record_write(0);
        self.target.rename(from, to).await
    }

    async fn rename_if_not_exists(&self, from: &Path, to: &Path) -> OSResult<()> {
        let _guard = self.hop_guard();
        self.record_write(0);
        self.target.rename_if_not_exists(from, to).await
    }

    async fn copy_if_not_exists(&self, from: &Path, to: &Path) -> OSResult<()> {
        let _guard = self.hop_guard();
        self.record_write(0);
        self.target.copy_if_not_exists(from, to).await
    }
}

#[derive(Debug)]
struct IoTrackingMultipartUpload {
    target: Box<dyn MultipartUpload>,
    stats: Arc<Mutex<IoStats>>,
    _guard: HopGuard,
}

#[async_trait::async_trait]
impl MultipartUpload for IoTrackingMultipartUpload {
    async fn abort(&mut self) -> OSResult<()> {
        self.target.abort().await
    }

    async fn complete(&mut self) -> OSResult<PutResult> {
        self.target.complete().await
    }

    fn put_part(&mut self, payload: PutPayload) -> UploadPart {
        {
            let mut stats = self.stats.lock().unwrap();
            stats.write_iops += 1;
            stats.write_bytes += payload.content_length() as u64;
        }
        self.target.put_part(payload)
    }
}

#[derive(Debug)]
struct HopGuard {
    active_requests: Arc<AtomicU16>,
    stats: Arc<Mutex<IoStats>>,
}

impl HopGuard {
    fn new(active_requests: Arc<AtomicU16>, stats: Arc<Mutex<IoStats>>) -> Self {
        active_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self {
            active_requests,
            stats,
        }
    }
}

impl Drop for HopGuard {
    fn drop(&mut self) {
        if self
            .active_requests
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
            == 1
        {
            let mut stats = self.stats.lock().unwrap();
            stats.num_hops += 1;
        }
    }
}

pub struct FragmentCount(pub u32);

impl From<u32> for FragmentCount {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

pub struct FragmentRowCount(pub u32);

impl From<u32> for FragmentRowCount {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

#[async_trait::async_trait]
pub trait DatagenExt {
    async fn into_dataset(
        self,
        path: &str,
        frag_count: FragmentCount,
        rows_per_fragment: FragmentRowCount,
    ) -> crate::Result<Dataset>
    where
        Self: Sized,
    {
        let rows_per_fragment_val = rows_per_fragment.0;
        self.into_dataset_with_params(
            path,
            frag_count,
            rows_per_fragment,
            Some(WriteParams {
                max_rows_per_file: rows_per_fragment_val as usize,
                ..Default::default()
            }),
        )
        .await
    }

    async fn into_dataset_with_params(
        self,
        path: &str,
        frag_count: FragmentCount,
        rows_per_fragment: FragmentRowCount,
        write_params: Option<WriteParams>,
    ) -> crate::Result<Dataset>;

    async fn into_ram_dataset_with_params(
        self,
        frag_count: FragmentCount,
        rows_per_fragment: FragmentRowCount,
        write_params: Option<WriteParams>,
    ) -> crate::Result<Dataset>
    where
        Self: Sized,
    {
        self.into_dataset_with_params("memory://", frag_count, rows_per_fragment, write_params)
            .await
    }

    async fn into_ram_dataset(
        self,
        frag_count: FragmentCount,
        rows_per_fragment: FragmentRowCount,
    ) -> crate::Result<Dataset>
    where
        Self: Sized,
    {
        self.into_dataset("memory://", frag_count, rows_per_fragment)
            .await
    }
}

#[async_trait::async_trait]
impl DatagenExt for BatchGeneratorBuilder {
    async fn into_dataset_with_params(
        self,
        path: &str,
        frag_count: FragmentCount,
        rows_per_fragment: FragmentRowCount,
        write_params: Option<WriteParams>,
    ) -> lance_core::Result<Dataset> {
        let reader = self.into_reader_rows(
            RowCount::from(rows_per_fragment.0 as u64),
            BatchCount::from(frag_count.0),
        );
        Dataset::write(reader, path, write_params).await
    }
}

pub struct NoContextTestFixture {
    _tmp_dir: TempDir,
    pub dataset: Dataset,
}

impl NoContextTestFixture {
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime.block_on(async move {
            let tempdir = tempdir().unwrap();
            let tmppath = tempdir.path().to_str().unwrap();
            let dataset = lance_datagen::gen()
                .col(
                    "text",
                    lance_datagen::array::rand_utf8(ByteCount::from(10), false),
                )
                .into_dataset(tmppath, FragmentCount::from(4), FragmentRowCount::from(100))
                .await
                .unwrap();
            Self {
                dataset,
                _tmp_dir: tempdir,
            }
        })
    }
}

pub fn copy_dir_all(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    use std::fs;
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Copies a test dataset into a temporary directory, returning the tmpdir.
///
/// The `table_path` should be relative to `test_data/` at the root of the
/// repo.
pub fn copy_test_data_to_tmp(table_path: &str) -> std::io::Result<TempDir> {
    use std::path::PathBuf;

    let mut src = PathBuf::new();
    src.push(env!("CARGO_MANIFEST_DIR"));
    src.push("../../test_data");
    src.push(table_path);

    let test_dir = tempdir().unwrap();

    copy_dir_all(src.as_path(), test_dir.path())?;

    Ok(test_dir)
}

/// Trims whitespace from the start and end of each line in the string.
fn trim_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for line in s.lines() {
        let line = line.trim();
        if !line.is_empty() {
            result.push_str(line);
            result.push('\n');
        }
    }
    if !result.is_empty() {
        // Remove the last newline
        result.pop();
    }
    result
}

pub async fn assert_plan_node_equals(
    plan_node: Arc<dyn ExecutionPlan>,
    raw_expected: &str,
) -> lance_core::Result<()> {
    let raw_plan_desc = format!(
        "{}",
        datafusion::physical_plan::displayable(plan_node.as_ref()).indent(true)
    );
    let plan_desc = trim_whitespace(&raw_plan_desc);

    let expected = trim_whitespace(raw_expected);

    let to_match = expected.split("...").collect::<Vec<_>>();
    let num_pieces = to_match.len();
    let mut remainder = plan_desc.as_str().trim_end_matches('\n');
    for (i, piece) in to_match.into_iter().enumerate() {
        let res = match i {
            0 => remainder.starts_with(piece),
            _ if i == num_pieces - 1 => remainder.ends_with(piece),
            _ => remainder.contains(piece),
        };
        if !res {
            break;
        }
        let idx = remainder.find(piece).unwrap();
        remainder = &remainder[idx + piece.len()..];
    }
    if !remainder.is_empty() {
        panic!(
            "Expected plan to match:\nExpected: {}\nActual: {}",
            raw_expected, raw_plan_desc
        )
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use arrow_array::{ArrayRef, BooleanArray, Float64Array, Int32Array, StringArray, StructArray};
    use arrow_schema::{DataType, Field as ArrowField, Fields as ArrowFields};
    use rstest::rstest;

    #[rstest]
    #[test]
    fn test_make_schema(
        #[values(LanceFileVersion::Legacy, LanceFileVersion::Stable)]
        data_storage_version: LanceFileVersion,
    ) {
        let arrow_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("a", DataType::Int32, false),
            ArrowField::new(
                "b",
                DataType::Struct(
                    vec![
                        ArrowField::new("f1", DataType::Utf8, true),
                        ArrowField::new("f2", DataType::Boolean, false),
                    ]
                    .into(),
                ),
                true,
            ),
            ArrowField::new("c", DataType::Float64, false),
        ]));
        let data = vec![RecordBatch::new_empty(arrow_schema.clone())];

        let generator = TestDatasetGenerator::new(data, data_storage_version);
        let schema = generator.make_schema(&mut rand::thread_rng());

        let roundtripped_schema = ArrowSchema::from(&schema);
        assert_eq!(&roundtripped_schema, arrow_schema.as_ref());

        let field_ids = schema.fields_pre_order().map(|f| f.id).collect::<Vec<_>>();
        let mut sorted_ids = field_ids.clone();
        sorted_ids.sort_unstable();
        assert_ne!(field_ids, sorted_ids);

        let mut num_holes = 0;
        for w in sorted_ids.windows(2) {
            let prev = w[0];
            let next = w[1];
            if next - prev > 1 {
                num_holes += 1;
            }
        }
        assert!(num_holes > 0, "Expected at least one hole in the field ids");
    }

    #[rstest]
    #[tokio::test]
    async fn test_make_fragment(
        #[values(LanceFileVersion::Legacy, LanceFileVersion::Stable)]
        data_storage_version: LanceFileVersion,
    ) {
        let tmp_dir = tempfile::tempdir().unwrap();

        let struct_fields: ArrowFields = vec![
            ArrowField::new("f1", DataType::Utf8, true),
            ArrowField::new("f2", DataType::Boolean, false),
        ]
        .into();
        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("a", DataType::Int32, false),
            ArrowField::new("b", DataType::Struct(struct_fields.clone()), true),
            ArrowField::new("c", DataType::Float64, false),
        ]));
        let data = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![1, 2, 3])),
                Arc::new(StructArray::new(
                    struct_fields,
                    vec![
                        Arc::new(StringArray::from(vec!["foo", "bar", "baz"])) as ArrayRef,
                        Arc::new(BooleanArray::from(vec![true, false, true])),
                    ],
                    None,
                )),
                Arc::new(Float64Array::from(vec![1.1, 2.2, 3.3])),
            ],
        )
        .unwrap();

        let generator = TestDatasetGenerator::new(vec![data.clone()], data_storage_version);
        let mut rng = rand::thread_rng();
        for _ in 1..50 {
            let schema = generator.make_schema(&mut rng);
            let fragment = generator
                .make_fragment(
                    tmp_dir.path().to_str().unwrap(),
                    &data,
                    &schema,
                    &mut rng,
                    2,
                )
                .await;

            assert!(fragment.files.len() > 1, "Expected multiple files");

            let mut field_ids_frags = fragment
                .files
                .iter()
                .flat_map(|file| file.fields.iter())
                .cloned()
                .collect::<Vec<_>>();
            let mut field_ids = schema.fields_pre_order().map(|f| f.id).collect::<Vec<_>>();
            assert_ne!(field_ids_frags, field_ids);
            field_ids_frags.sort_unstable();
            field_ids.sort_unstable();
            assert_eq!(field_ids_frags, field_ids);
        }
    }

    #[rstest]
    #[tokio::test]
    async fn test_make_hostile(
        #[values(LanceFileVersion::Legacy, LanceFileVersion::Stable)]
        data_storage_version: LanceFileVersion,
    ) {
        let tmp_dir = tempfile::tempdir().unwrap();

        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("a", DataType::Int32, false),
            ArrowField::new("b", DataType::Int32, false),
            ArrowField::new("c", DataType::Float64, false),
        ]));
        let data = vec![
            RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(Int32Array::from(vec![1, 2, 3])),
                    Arc::new(Int32Array::from(vec![10, 20, 30])),
                    Arc::new(Float64Array::from(vec![1.1, 2.2, 3.3])),
                ],
            )
            .unwrap(),
            RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(Int32Array::from(vec![4, 5, 6])),
                    Arc::new(Int32Array::from(vec![40, 50, 60])),
                    Arc::new(Float64Array::from(vec![4.4, 5.5, 6.6])),
                ],
            )
            .unwrap(),
        ];

        let seed = 42;
        let generator = TestDatasetGenerator::new(data.clone(), data_storage_version).seed(seed);

        let path = tmp_dir.path().join("ds1");
        let dataset = generator.make_hostile(path.to_str().unwrap()).await;

        let path2 = tmp_dir.path().join("ds2");
        let dataset2 = generator.make_hostile(path2.to_str().unwrap()).await;

        // Given the same seed, should produce the same layout.
        assert_eq!(dataset.schema(), dataset2.schema());
        let field_structure_1 = get_field_structure(&dataset);
        let field_structure_2 = get_field_structure(&dataset2);
        assert_eq!(field_structure_1, field_structure_2);

        // Make sure we handle different numbers of columns
        for num_cols in 1..4 {
            let projection = (0..num_cols).collect::<Vec<_>>();
            let data = data
                .iter()
                .map(|rb| rb.project(&projection).unwrap())
                .collect::<Vec<RecordBatch>>();

            let generator = TestDatasetGenerator::new(data.clone(), data_storage_version);
            // Sample a few
            for i in 1..20 {
                let path = tmp_dir.path().join(format!("test_ds_{}_{}", num_cols, i));
                let dataset = generator.make_hostile(path.to_str().unwrap()).await;

                let field_structure = get_field_structure(&dataset);

                // The two fragments should have different layout.
                assert_eq!(field_structure.len(), 2);
                if num_cols > 1 {
                    assert_ne!(field_structure[0], field_structure[1]);
                }
            }
        }
    }
}
