//! Pre-flight, **metadata-only** data-size estimator + peak-working-set model
//! (architecture §4 data plane, §11 scheduler).
//!
//! The estimator answers a single question the [`crate::planner`] needs *before*
//! a query runs: roughly how much RAM will this query need on this machine? It
//! does so **without a full scan** — reading only file/object metadata
//! (Parquet footers, Delta `_delta_log` add-action stats, Iceberg manifest
//! entries, CSV/JSON object size + a small sample) and applying **projection
//! pushdown** (only the referenced columns) and **predicate pushdown** (prune
//! row-groups/files whose min/max stats can't satisfy the filter).
//!
//! ## Two stages
//! 1. [`estimate_scan`] turns per-format [metadata] into a [`ScanEstimate`]:
//!    estimated *scanned uncompressed bytes* + row counts for the projected
//!    columns over the surviving row-groups/files.
//! 2. [`estimate_working_set`] turns a [`ScanEstimate`] + a [`QueryShape`]
//!    (which blocking operators the plan has) into a [`WorkingSetEstimate`]:
//!    the **estimated peak working-set memory**. This is deliberately *not* the
//!    raw input size — DuckDB streams scans and spills, so steady-state RAM is
//!    driven by **blocking operators**: high-cardinality `GROUP BY` hash
//!    tables, hash-join build sides, and sorts/windows that must buffer rows.
//!
//! ## What is real vs. synthetic (honest scope)
//! * **CSV / JSON**: [`csv_metadata`] / `ndjson_metadata` read a real local file
//!   (size via `stat`, columns + average row width from a bounded byte sample).
//!   Fully exercised on real fixtures with no engine.
//! * **Delta**: [`delta_metadata`] parses a real `_delta_log` directory's JSON
//!   commit log (`add` action `size` + `stats`, `metaData` schema). Pure Rust.
//! * **Parquet**: the estimator core consumes [`ParquetMetadata`] structs that
//!   mirror DuckDB's `parquet_metadata()` output (`row_group_num_rows`,
//!   `num_values`, `total_uncompressed_size`, `stats_min_value`/`stats_max_value`
//!   per column-chunk). The real probe that fills them by running
//!   `parquet_metadata()` / `parquet_file_metadata()` lives behind the
//!   `duckdb-engine` feature ([`crate::duckdb_engine`]); where that engine can't
//!   be built (broken host C++ toolchain) the core is unit-tested against
//!   synthetic metadata that matches the documented column semantics.
//! * **Iceberg**: same shape as Delta via [`IcebergMetadata`]; reading real
//!   Avro manifests needs the `iceberg` extension/engine (caveat, not faked).
//! * **`EXPLAIN` cardinality**: [`parse_explain_cardinality`] extracts the
//!   `EC: n` estimates from an `EXPLAIN` plan (pure parser, unit-tested); the
//!   engine-backed probe that produces the plan text is behind `duckdb-engine`.

use std::collections::BTreeMap;

use p2p_proto::{ResultSet, Value};

// ---------------------------------------------------------------------------
// Projection + predicates
// ---------------------------------------------------------------------------

/// Which columns a query actually reads (projection pushdown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    /// All columns (`SELECT *`).
    All,
    /// Only these columns are scanned.
    Columns(Vec<String>),
}

impl Projection {
    /// Build a projection from referenced column names.
    pub fn columns<I, S>(cols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Projection::Columns(cols.into_iter().map(Into::into).collect())
    }

    /// Whether `col` is read by this projection.
    fn includes(&self, col: &str) -> bool {
        match self {
            Projection::All => true,
            Projection::Columns(cs) => cs.iter().any(|c| c == col),
        }
    }

    /// Number of projected columns given the table's full column count.
    fn projected_count(&self, total_columns: usize) -> usize {
        match self {
            Projection::All => total_columns.max(1),
            Projection::Columns(cs) => cs.len().max(1),
        }
    }
}

/// A comparison operator usable for min/max-stats row-group/file pruning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A single pushed-down predicate `column <op> value` (numeric). Predicates on
/// non-numeric columns (no `value`) simply don't prune.
#[derive(Debug, Clone)]
pub struct Predicate {
    pub column: String,
    pub op: Cmp,
    pub value: f64,
}

impl Predicate {
    pub fn new(column: impl Into<String>, op: Cmp, value: f64) -> Self {
        Self {
            column: column.into(),
            op,
            value,
        }
    }

    /// Can a chunk with stats `[min,max]` possibly contain a row satisfying this
    /// predicate? Returns `true` (keep) when unknown or when the range overlaps.
    fn overlaps(&self, min: Option<f64>, max: Option<f64>) -> bool {
        let (min, max) = match (min, max) {
            (Some(a), Some(b)) => (a, b),
            // No stats ⇒ cannot prune.
            _ => return true,
        };
        match self.op {
            Cmp::Eq => min <= self.value && self.value <= max,
            Cmp::Ne => true, // a range can't be excluded by a single != value
            Cmp::Lt => min < self.value,
            Cmp::Le => min <= self.value,
            Cmp::Gt => max > self.value,
            Cmp::Ge => max >= self.value,
        }
    }

    /// Residual selectivity for a *surviving* chunk (rough heuristic, used only
    /// for output-cardinality estimation, never for pruning correctness).
    fn selectivity(&self, p: &EstimateParams) -> f64 {
        match self.op {
            Cmp::Eq => p.eq_selectivity,
            Cmp::Ne => p.ne_selectivity,
            Cmp::Lt | Cmp::Le | Cmp::Gt | Cmp::Ge => p.range_selectivity,
        }
    }
}

// ---------------------------------------------------------------------------
// Tunable heuristics (documented, not magic constants scattered in code)
// ---------------------------------------------------------------------------

/// Estimator heuristics. These are estimation *assumptions* (distinct from the
/// operational `[planner]` config); every field is documented and overridable.
#[derive(Debug, Clone, PartialEq)]
pub struct EstimateParams {
    /// Assumed uncompressed/compressed ratio when only on-disk (compressed)
    /// sizes are known (Delta/Iceberg expose file `size`/`file_size_in_bytes`,
    /// not per-column uncompressed sizes like Parquet footers do).
    pub columnar_decompression_ratio: f64,
    /// Default selectivity of an equality predicate on a surviving chunk.
    pub eq_selectivity: f64,
    /// Default selectivity of a range predicate on a surviving chunk.
    pub range_selectivity: f64,
    /// Default selectivity of a `!=` predicate.
    pub ne_selectivity: f64,
    /// Fallback average row width (bytes) for text formats when a sample is
    /// unavailable.
    pub text_row_width_fallback: u64,
    /// Bounded streaming scan buffer (bytes). DuckDB streams scans morsel by
    /// morsel, so the scan contributes only a bounded buffer to peak RAM, never
    /// the whole input. The estimate uses `min(scanned_bytes, this)`.
    pub scan_buffer_cap_bytes: u64,
    /// Per-group overhead (bytes) for a `GROUP BY` hash-table entry beyond the
    /// key + aggregate-state widths (pointer, hash, slot slack).
    pub group_entry_overhead_bytes: u64,
    /// Assumed local scan+process throughput (bytes/ms) for the runtime
    /// estimate that feeds the latency budget.
    pub throughput_bytes_per_ms: u64,
}

impl Default for EstimateParams {
    fn default() -> Self {
        Self {
            // Parquet/columnar data commonly compresses ~3–5x; 4x is a neutral
            // default and is only used for Delta/Iceberg (Parquet uses exact
            // per-column uncompressed sizes from the footer).
            columnar_decompression_ratio: 4.0,
            eq_selectivity: 0.10,
            range_selectivity: 0.30,
            ne_selectivity: 0.90,
            text_row_width_fallback: 128,
            scan_buffer_cap_bytes: 16 * 1024 * 1024,
            group_entry_overhead_bytes: 32,
            // ~500 MB/s steady-state local scan/aggregate throughput.
            throughput_bytes_per_ms: 500_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-format metadata inputs
// ---------------------------------------------------------------------------

/// One Parquet column-chunk's metadata (mirrors a `parquet_metadata()` row).
#[derive(Debug, Clone)]
pub struct ColumnChunkMeta {
    /// `path_in_schema` — the column name.
    pub name: String,
    /// `total_uncompressed_size` — the in-memory footprint when fully decoded
    /// (what working-set sizing cares about, not the on-disk compressed size).
    pub total_uncompressed_size: u64,
    /// `num_values` in this chunk.
    pub num_values: u64,
    /// `stats_min_value` parsed to f64 (None if absent/non-numeric).
    pub min: Option<f64>,
    /// `stats_max_value` parsed to f64.
    pub max: Option<f64>,
}

/// One Parquet row-group (`row_group_id`).
#[derive(Debug, Clone)]
pub struct RowGroupMeta {
    pub num_rows: u64,
    pub columns: Vec<ColumnChunkMeta>,
}

impl RowGroupMeta {
    fn column(&self, name: &str) -> Option<&ColumnChunkMeta> {
        self.columns.iter().find(|c| c.name == name)
    }
}

/// Parquet file (or glob) metadata: a list of row-groups.
#[derive(Debug, Clone, Default)]
pub struct ParquetMetadata {
    pub row_groups: Vec<RowGroupMeta>,
}

/// Build [`ParquetMetadata`] from the rows of a DuckDB `parquet_metadata()`
/// query. Expected (case-insensitive) columns — a subset of the documented
/// `parquet_metadata` schema — looked up by name so column order is irrelevant:
/// `row_group_id`, `row_group_num_rows`, `path_in_schema`, `num_values`,
/// `total_uncompressed_size`, `stats_min_value`, `stats_max_value`.
///
/// Pure (no engine) so it is unit-tested directly against a synthetic
/// `ResultSet` that mirrors real `parquet_metadata()` output; the engine-backed
/// probe that produces that `ResultSet` lives behind the `duckdb-engine`
/// feature.
pub fn parquet_metadata_from_resultset(rs: &ResultSet) -> ParquetMetadata {
    let col = |name: &str| rs.columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let (Some(i_rg), Some(i_rows), Some(i_path), Some(i_vals), Some(i_size)) = (
        col("row_group_id"),
        col("row_group_num_rows"),
        col("path_in_schema"),
        col("num_values"),
        col("total_uncompressed_size"),
    ) else {
        return ParquetMetadata::default();
    };
    let i_min = col("stats_min_value");
    let i_max = col("stats_max_value");

    let mut groups: Vec<RowGroupMeta> = Vec::new();
    // row_group_id -> index into `groups` (preserves first-seen order).
    let mut index: BTreeMap<i64, usize> = BTreeMap::new();
    for row in &rs.rows {
        let rg_id = value_as_i64(row.get(i_rg)).unwrap_or(0);
        let num_rows = value_as_u64(row.get(i_rows)).unwrap_or(0);
        let gi = *index.entry(rg_id).or_insert_with(|| {
            groups.push(RowGroupMeta {
                num_rows,
                columns: Vec::new(),
            });
            groups.len() - 1
        });
        groups[gi].columns.push(ColumnChunkMeta {
            name: value_as_text(row.get(i_path)).unwrap_or_default(),
            total_uncompressed_size: value_as_u64(row.get(i_size)).unwrap_or(0),
            num_values: value_as_u64(row.get(i_vals)).unwrap_or(0),
            min: i_min.and_then(|i| value_as_f64(row.get(i))),
            max: i_max.and_then(|i| value_as_f64(row.get(i))),
        });
    }
    ParquetMetadata { row_groups: groups }
}

fn value_as_i64(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Int(i) => Some(*i),
        Value::Float(f) => Some(*f as i64),
        Value::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn value_as_u64(v: Option<&Value>) -> Option<u64> {
    value_as_i64(v).and_then(|i| u64::try_from(i).ok())
}

fn value_as_f64(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn value_as_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Text(s) => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        _ => None,
    }
}

/// Per-file metadata for a Delta or Iceberg data file (`add` action / manifest
/// entry): on-disk size + record count + per-column numeric min/max bounds.
#[derive(Debug, Clone)]
pub struct DataFileMeta {
    /// On-disk (compressed) bytes — Delta `size` / Iceberg `file_size_in_bytes`.
    pub size_bytes: u64,
    /// Delta `numRecords` / Iceberg `record_count`.
    pub record_count: u64,
    /// Per-column numeric `(min,max)` bounds for pruning (`minValues`/`maxValues`
    /// for Delta, `lower_bounds`/`upper_bounds` for Iceberg).
    pub bounds: BTreeMap<String, (Option<f64>, Option<f64>)>,
}

/// Delta / Iceberg table metadata: full schema columns + per-file entries.
#[derive(Debug, Clone, Default)]
pub struct TableFilesMetadata {
    /// All column names in the table schema (for the projection ratio).
    pub all_columns: Vec<String>,
    pub files: Vec<DataFileMeta>,
}

// Aliases so call sites read naturally.
pub type DeltaMetadata = TableFilesMetadata;
pub type IcebergMetadata = TableFilesMetadata;

/// CSV / JSON object metadata: total bytes + a bounded sample for average row
/// width and column count.
#[derive(Debug, Clone)]
pub struct TextMetadata {
    /// Total object size in bytes (storage `HEAD`/`stat`/list).
    pub object_bytes: u64,
    /// Bytes consumed by the sample.
    pub sample_bytes: u64,
    /// Rows observed in the sample.
    pub sample_rows: u64,
    /// Total columns detected (header / first object) for the projection ratio.
    pub total_columns: usize,
}

// ---------------------------------------------------------------------------
// Scan estimate
// ---------------------------------------------------------------------------

/// Result of stage 1: estimated bytes/rows scanned for the projected columns
/// over the row-groups/files that survive predicate pruning.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanEstimate {
    /// Estimated **uncompressed** bytes of the projected columns over surviving
    /// row-groups/files (the in-memory footprint a full materialization would
    /// take — but the scan itself streams, see [`WorkingSetEstimate`]).
    pub scanned_uncompressed_bytes: u64,
    /// Rows in the surviving row-groups/files (pre residual-predicate).
    pub total_rows: u64,
    /// Estimated rows emitted after residual predicate selectivity.
    pub estimated_output_rows: u64,
    /// Average projected row width (bytes/row).
    pub avg_row_width_bytes: u64,
    /// Total row-groups/files considered.
    pub units_total: usize,
    /// Row-groups/files that survived pruning (were "scanned").
    pub units_scanned: usize,
    /// Number of projected columns.
    pub projected_columns: usize,
}

impl ScanEstimate {
    fn finalize(
        scanned_bytes: u64,
        total_rows: u64,
        surviving_selectivity: f64,
        projected_columns: usize,
        units_total: usize,
        units_scanned: usize,
    ) -> Self {
        let avg_row_width_bytes = if total_rows > 0 {
            scanned_bytes / total_rows
        } else {
            0
        };
        let estimated_output_rows = ((total_rows as f64) * surviving_selectivity).round() as u64;
        Self {
            scanned_uncompressed_bytes: scanned_bytes,
            total_rows,
            estimated_output_rows,
            avg_row_width_bytes,
            units_total,
            units_scanned,
            projected_columns,
        }
    }
}

/// Combined residual selectivity of a predicate list (multiplicative, clamped).
fn combined_selectivity(predicates: &[Predicate], params: &EstimateParams) -> f64 {
    let mut s = 1.0_f64;
    for p in predicates {
        s *= p.selectivity(params);
    }
    s.clamp(1e-6, 1.0)
}

/// Estimate a Parquet scan from footer metadata, with projection + predicate
/// (min/max) pushdown applied per row-group.
pub fn estimate_parquet(
    meta: &ParquetMetadata,
    projection: &Projection,
    predicates: &[Predicate],
    params: &EstimateParams,
) -> ScanEstimate {
    let mut scanned_bytes = 0u64;
    let mut total_rows = 0u64;
    let mut units_scanned = 0usize;
    let mut projected_cols = 0usize;

    for rg in &meta.row_groups {
        // Row-group pruning: drop the group if ANY predicate's stats exclude it.
        let pruned = predicates.iter().any(|pred| {
            rg.column(&pred.column)
                .map(|c| !pred.overlaps(c.min, c.max))
                .unwrap_or(false)
        });
        if pruned {
            continue;
        }
        units_scanned += 1;
        total_rows += rg.num_rows;
        let mut cols_here = 0usize;
        for c in &rg.columns {
            if projection.includes(&c.name) {
                scanned_bytes += c.total_uncompressed_size;
                cols_here += 1;
            }
        }
        projected_cols = projected_cols.max(cols_here);
    }

    ScanEstimate::finalize(
        scanned_bytes,
        total_rows,
        combined_selectivity(predicates, params),
        projected_cols,
        meta.row_groups.len(),
        units_scanned,
    )
}

/// Estimate a Delta/Iceberg scan from per-file `size` + `stats`/bounds, applying
/// projection (as a column ratio over the schema) and file pruning via bounds.
pub fn estimate_table_files(
    meta: &TableFilesMetadata,
    projection: &Projection,
    predicates: &[Predicate],
    params: &EstimateParams,
) -> ScanEstimate {
    let total_columns = meta.all_columns.len().max(1);
    let projected = projection.projected_count(total_columns);
    let projection_ratio = (projected as f64 / total_columns as f64).min(1.0);

    let mut scanned_bytes = 0u64;
    let mut total_rows = 0u64;
    let mut units_scanned = 0usize;

    for f in &meta.files {
        let pruned = predicates.iter().any(|pred| {
            f.bounds
                .get(&pred.column)
                .map(|(min, max)| !pred.overlaps(*min, *max))
                .unwrap_or(false)
        });
        if pruned {
            continue;
        }
        units_scanned += 1;
        total_rows += f.record_count;
        // Only on-disk (compressed) size is known; approximate the projected
        // uncompressed footprint = compressed * decompression_ratio * ratio.
        let uncompressed = (f.size_bytes as f64) * params.columnar_decompression_ratio;
        scanned_bytes += (uncompressed * projection_ratio).round() as u64;
    }

    ScanEstimate::finalize(
        scanned_bytes,
        total_rows,
        combined_selectivity(predicates, params),
        projected,
        meta.files.len(),
        units_scanned,
    )
}

/// Estimate a CSV/JSON scan from object size + a sample-derived average row
/// width. Text formats are row-oriented so a scan reads the whole object, but
/// only the projected columns occupy the working row.
pub fn estimate_text(
    meta: &TextMetadata,
    projection: &Projection,
    predicates: &[Predicate],
    params: &EstimateParams,
) -> ScanEstimate {
    // Extrapolate row count from the sampled bytes/row *ratio* (don't truncate
    // the average row width to an integer first — that biases the count high).
    let total_rows = if meta.sample_rows > 0 && meta.sample_bytes > 0 {
        ((meta.object_bytes as u128 * meta.sample_rows as u128) / meta.sample_bytes as u128) as u64
    } else if params.text_row_width_fallback > 0 {
        meta.object_bytes / params.text_row_width_fallback
    } else {
        0
    };

    let total_columns = meta.total_columns.max(1);
    let projected = projection.projected_count(total_columns);
    let projection_ratio = (projected as f64 / total_columns as f64).min(1.0);
    // Working bytes = whole object scanned, but the in-memory row keeps only the
    // projected columns.
    let scanned_bytes = ((meta.object_bytes as f64) * projection_ratio).round() as u64;

    ScanEstimate::finalize(
        scanned_bytes,
        total_rows,
        combined_selectivity(predicates, params),
        projected,
        1,
        1,
    )
}

// ---------------------------------------------------------------------------
// Stage 2: working-set memory model
// ---------------------------------------------------------------------------

/// Description of the blocking operators in the plan that drive peak RAM. Build
/// it from query analysis or an `EXPLAIN` cardinality probe. The streaming scan
/// + non-blocking projection/filter contribute only a bounded buffer.
#[derive(Debug, Clone, Default)]
pub struct QueryShape {
    /// Plan has a (blocking) hash `GROUP BY` / `DISTINCT`.
    pub group_by: bool,
    /// Estimated number of distinct groups (NDV of the grouping keys). The hash
    /// table holds one entry per group, so this is the dominant term for
    /// high-cardinality aggregations.
    pub distinct_groups: u64,
    /// Bytes per group entry (key width + aggregate-state width).
    pub group_payload_bytes: u64,
    /// Plan has a hash join whose **build** side must be fully materialized.
    pub hash_join: bool,
    /// Rows on the hash-join build side.
    pub join_build_rows: u64,
    /// Bytes per build-side row.
    pub join_build_row_width: u64,
    /// Plan has a sort / window that buffers rows.
    pub sort_or_window: bool,
    /// Rows buffered by the sort/window.
    pub sort_rows: u64,
    /// Bytes per buffered row.
    pub sort_row_width: u64,
}

impl QueryShape {
    /// A fully streaming plan (scan + filter + projection, no blocking ops). Its
    /// peak working set is just the bounded scan buffer.
    pub fn streaming() -> Self {
        Self::default()
    }

    /// Convenience: a simple `GROUP BY` whose key+state width defaults to the
    /// scan's average row width when `group_payload_bytes` is 0.
    pub fn group_by(distinct_groups: u64) -> Self {
        Self {
            group_by: true,
            distinct_groups,
            ..Self::default()
        }
    }
}

/// Result of stage 2: the estimated peak working-set memory + a breakdown of
/// the contributing blocking operators (so callers can explain a decision).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkingSetEstimate {
    pub scanned_uncompressed_bytes: u64,
    pub estimated_rows: u64,
    /// Bounded streaming-scan buffer.
    pub scan_buffer_bytes: u64,
    /// `GROUP BY` hash-table memory.
    pub group_by_bytes: u64,
    /// Hash-join build-side memory.
    pub join_build_bytes: u64,
    /// Sort/window buffer memory.
    pub sort_bytes: u64,
    /// The headline number: estimated peak RAM the query needs.
    pub peak_working_set_bytes: u64,
    /// Estimated local runtime (ms) from scanned bytes / assumed throughput.
    pub estimated_runtime_ms: u64,
}

/// Stage 2: translate a [`ScanEstimate`] + [`QueryShape`] into a peak
/// working-set memory estimate. Peak RAM = bounded scan buffer + the sum of the
/// blocking operators' state, **not** the raw input size.
pub fn estimate_working_set(
    scan: &ScanEstimate,
    shape: &QueryShape,
    params: &EstimateParams,
) -> WorkingSetEstimate {
    let scan_buffer_bytes = scan
        .scanned_uncompressed_bytes
        .min(params.scan_buffer_cap_bytes);

    let group_by_bytes = if shape.group_by {
        let payload = if shape.group_payload_bytes > 0 {
            shape.group_payload_bytes
        } else {
            // Fall back to the projected row width as the per-group key+state.
            scan.avg_row_width_bytes.max(1)
        };
        shape
            .distinct_groups
            .saturating_mul(payload.saturating_add(params.group_entry_overhead_bytes))
    } else {
        0
    };

    let join_build_bytes = if shape.hash_join {
        let w = if shape.join_build_row_width > 0 {
            shape.join_build_row_width
        } else {
            scan.avg_row_width_bytes.max(1)
        };
        shape.join_build_rows.saturating_mul(w)
    } else {
        0
    };

    let sort_bytes = if shape.sort_or_window {
        let w = if shape.sort_row_width > 0 {
            shape.sort_row_width
        } else {
            scan.avg_row_width_bytes.max(1)
        };
        shape.sort_rows.saturating_mul(w)
    } else {
        0
    };

    let peak_working_set_bytes = scan_buffer_bytes
        .saturating_add(group_by_bytes)
        .saturating_add(join_build_bytes)
        .saturating_add(sort_bytes);

    let estimated_runtime_ms = if params.throughput_bytes_per_ms > 0 {
        scan.scanned_uncompressed_bytes / params.throughput_bytes_per_ms
    } else {
        0
    };

    WorkingSetEstimate {
        scanned_uncompressed_bytes: scan.scanned_uncompressed_bytes,
        estimated_rows: scan.estimated_output_rows,
        scan_buffer_bytes,
        group_by_bytes,
        join_build_bytes,
        sort_bytes,
        peak_working_set_bytes,
        estimated_runtime_ms,
    }
}

// ---------------------------------------------------------------------------
// CSV / JSON readers (pure Rust — no engine)
// ---------------------------------------------------------------------------

/// Errors from reading object metadata for the estimate.
#[derive(Debug, thiserror::Error)]
pub enum EstimateError {
    #[error("io error reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error("could not parse metadata: {0}")]
    Parse(String),
}

/// Read CSV metadata from a local file: total size + a bounded byte sample to
/// derive the average row width and the column count (header). `delimiter` is
/// usually `,`. Reads at most `sample_limit_bytes` (a "HEAD"-style probe).
pub fn csv_metadata(
    path: &std::path::Path,
    delimiter: u8,
    sample_limit_bytes: usize,
) -> Result<TextMetadata, EstimateError> {
    use std::io::Read;
    let object_bytes = std::fs::metadata(path)
        .map_err(|e| EstimateError::Io(path.display().to_string(), e))?
        .len();
    let mut f =
        std::fs::File::open(path).map_err(|e| EstimateError::Io(path.display().to_string(), e))?;
    let mut buf = vec![0u8; sample_limit_bytes.min(object_bytes as usize)];
    let n = f
        .read(&mut buf)
        .map_err(|e| EstimateError::Io(path.display().to_string(), e))?;
    buf.truncate(n);

    // Count complete lines in the sample (newline-terminated).
    let newline_positions: Vec<usize> = buf
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .map(|(i, _)| i)
        .collect();
    let last_complete = newline_positions.last().copied();
    let total_lines = newline_positions.len() as u64;

    // First line = header → column count; data rows = lines after the header.
    let header_end = newline_positions.first().copied().unwrap_or(buf.len());
    let header = &buf[..header_end];
    let total_columns = if header.is_empty() {
        1
    } else {
        header.iter().filter(|b| **b == delimiter).count() + 1
    };

    // Sample rows = data rows (exclude the header line).
    let sample_rows = total_lines.saturating_sub(1);
    // Sample bytes = bytes covering the complete sampled data rows (excluding
    // the header), so avg width reflects data rows only.
    let sample_bytes = match (last_complete, sample_rows) {
        (Some(end), r) if r > 0 => (end as u64).saturating_sub(header_end as u64 + 1),
        _ => 0,
    };

    Ok(TextMetadata {
        object_bytes,
        sample_bytes,
        sample_rows,
        total_columns,
    })
}

/// Read newline-delimited JSON metadata from a local file: total size + a
/// bounded sample for the average row width and the key count of the first
/// object.
pub fn ndjson_metadata(
    path: &std::path::Path,
    sample_limit_bytes: usize,
) -> Result<TextMetadata, EstimateError> {
    use std::io::Read;
    let object_bytes = std::fs::metadata(path)
        .map_err(|e| EstimateError::Io(path.display().to_string(), e))?
        .len();
    let mut f =
        std::fs::File::open(path).map_err(|e| EstimateError::Io(path.display().to_string(), e))?;
    let mut buf = vec![0u8; sample_limit_bytes.min(object_bytes as usize)];
    let n = f
        .read(&mut buf)
        .map_err(|e| EstimateError::Io(path.display().to_string(), e))?;
    buf.truncate(n);

    let newline_positions: Vec<usize> = buf
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .map(|(i, _)| i)
        .collect();
    let sample_rows = newline_positions.len() as u64;
    let sample_bytes = newline_positions.last().map(|i| *i as u64 + 1).unwrap_or(0);

    // Column count = number of top-level keys in the first JSON object.
    let total_columns = buf
        .iter()
        .position(|b| *b == b'\n')
        .map(|end| &buf[..end])
        .and_then(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .and_then(|v| v.as_object().map(|o| o.len()))
        .unwrap_or(1)
        .max(1);

    Ok(TextMetadata {
        object_bytes,
        sample_bytes,
        sample_rows,
        total_columns,
    })
}

// ---------------------------------------------------------------------------
// Delta `_delta_log` reader (pure Rust JSON — no engine)
// ---------------------------------------------------------------------------

/// Parse a Delta table's `_delta_log` directory: accumulate `add` actions
/// (their `size`, and `stats` → `numRecords` + `minValues`/`maxValues`) and the
/// latest `metaData` schema (for the column list). Reads only the JSON commit
/// log — no data files are scanned.
pub fn delta_metadata(table_dir: &std::path::Path) -> Result<DeltaMetadata, EstimateError> {
    let log_dir = table_dir.join("_delta_log");
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&log_dir)
        .map_err(|e| EstimateError::Io(log_dir.display().to_string(), e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    entries.sort();

    let mut out = DeltaMetadata::default();
    let mut removed: std::collections::BTreeSet<String> = Default::default();
    let mut added_paths: BTreeMap<String, DataFileMeta> = BTreeMap::new();

    for path in entries {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| EstimateError::Io(path.display().to_string(), e))?;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let v: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(meta) = v.get("metaData") {
                if let Some(schema) = meta.get("schemaString").and_then(|s| s.as_str()) {
                    if let Ok(cols) = parse_delta_schema_columns(schema) {
                        out.all_columns = cols;
                    }
                }
            }
            if let Some(remove) = v.get("remove") {
                if let Some(p) = remove.get("path").and_then(|s| s.as_str()) {
                    removed.insert(p.to_string());
                }
            }
            if let Some(add) = v.get("add") {
                let p = add
                    .get("path")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let size = add.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                let (num_records, bounds) = parse_delta_stats(add.get("stats"));
                added_paths.insert(
                    p,
                    DataFileMeta {
                        size_bytes: size,
                        record_count: num_records,
                        bounds,
                    },
                );
            }
        }
    }

    for (p, f) in added_paths {
        if !removed.contains(&p) {
            out.files.push(f);
        }
    }
    Ok(out)
}

/// Extract column names from a Delta `schemaString` (a JSON struct schema).
fn parse_delta_schema_columns(schema: &str) -> Result<Vec<String>, EstimateError> {
    let v: serde_json::Value =
        serde_json::from_str(schema).map_err(|e| EstimateError::Parse(e.to_string()))?;
    let fields = v
        .get("fields")
        .and_then(|f| f.as_array())
        .ok_or_else(|| EstimateError::Parse("delta schema has no fields".into()))?;
    Ok(fields
        .iter()
        .filter_map(|f| f.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect())
}

/// Parse a Delta `add.stats` JSON string: `numRecords` + numeric
/// `minValues`/`maxValues` per column.
fn parse_delta_stats(
    stats: Option<&serde_json::Value>,
) -> (u64, BTreeMap<String, (Option<f64>, Option<f64>)>) {
    let mut bounds: BTreeMap<String, (Option<f64>, Option<f64>)> = BTreeMap::new();
    let stats = match stats {
        Some(s) => s,
        None => return (0, bounds),
    };
    // `stats` is usually a JSON-encoded string; tolerate an inline object too.
    let parsed: serde_json::Value = match stats {
        serde_json::Value::String(s) => match serde_json::from_str(s) {
            Ok(v) => v,
            Err(_) => return (0, bounds),
        },
        other => other.clone(),
    };
    let num_records = parsed
        .get("numRecords")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let mins = parsed.get("minValues").and_then(|m| m.as_object());
    let maxs = parsed.get("maxValues").and_then(|m| m.as_object());
    if let Some(mins) = mins {
        for (k, v) in mins {
            bounds.entry(k.clone()).or_insert((None, None)).0 = json_as_f64(v);
        }
    }
    if let Some(maxs) = maxs {
        for (k, v) in maxs {
            bounds.entry(k.clone()).or_insert((None, None)).1 = json_as_f64(v);
        }
    }
    (num_records, bounds)
}

fn json_as_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// EXPLAIN cardinality parser (pure — engine probe is behind duckdb-engine)
// ---------------------------------------------------------------------------

/// Extract the maximum estimated cardinality (`EC: n`) from a DuckDB `EXPLAIN`
/// plan. DuckDB annotates operators with `EC: <rows>`; the root/top operator's
/// cardinality is the query's estimated output-row count. Returns the max EC
/// seen (a robust proxy for the heaviest operator's row count).
pub fn parse_explain_cardinality(explain_text: &str) -> Option<u64> {
    fn parse_grouped_digits(rest: &str) -> Option<u64> {
        let digits: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '_')
            .filter(|c| c.is_ascii_digit())
            .collect();
        digits.parse::<u64>().ok()
    }

    let mut max_ec: Option<u64> = None;
    let mut bump = |v: u64| max_ec = Some(max_ec.map_or(v, |m: u64| m.max(v)));
    for raw in explain_text.split(['\n', '|']) {
        let line = raw.trim();
        // Legacy DuckDB annotation: `EC: <n>`.
        if let Some(idx) = line.find("EC:") {
            if let Some(v) = parse_grouped_digits(line[idx + 3..].trim_start()) {
                bump(v);
            }
        }
        // Modern DuckDB physical-plan annotation: `~<n> rows` (with grouped
        // thousands, e.g. `~5,000 rows`).
        if let Some(idx) = line.find('~') {
            let rest = line[idx + 1..].trim_start();
            if rest
                .trim_start_matches(|c: char| c.is_ascii_digit() || c == ',' || c == '_')
                .trim_start()
                .starts_with("row")
            {
                if let Some(v) = parse_grouped_digits(rest) {
                    bump(v);
                }
            }
        }
    }
    max_ec
}

// ---------------------------------------------------------------------------
// Conservative SQL source classification (requester pre-flight router)
// ---------------------------------------------------------------------------

/// Does this SQL reference any **external data source** — a relation (`FROM`/
/// `JOIN`), a data-reader table function (`read_csv`/`read_parquet`/`*_scan`/…),
/// a file/object literal, or `COPY`/`ATTACH` — as opposed to a pure in-memory
/// computation (`SELECT 1 + 1`, scalar functions, `SELECT now()`)?
///
/// This is the conservative gate the requester's pre-flight router uses to keep
/// ONLY confirmed pure-in-memory queries on the free local path. It is biased to
/// return `true` ("assume a source") whenever unsure: the locked-down local
/// engine cannot read external data, and a non-resource local failure does NOT
/// fail over to the grid, so a false "no source" would turn a working grid query
/// into a hard error. A false "has source" merely forgoes the local optimization
/// (the query routes remote, exactly today's behavior) — routing-only, never
/// correctness. Returns `false` only when NO source marker is present at all.
pub fn has_data_source(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    // Any of these markers ⇒ treat the query as touching external data. Each only
    // ADDS conservatism (more → remote), so over-matching (e.g. a column named
    // `read_count`, or `.csv` inside a string literal) is safe by construction.
    const MARKERS: &[&str] = &[
        " from ", "\tfrom ", "\nfrom ", "(from ", ")from ", "\rfrom ", "\nfrom\t",
        " join ", "\njoin ", "\tjoin ", "(join ",
        "read_", "scan_", "parquet_", "_scan(", "glob(", "query_table(",
        "copy ", "attach ", "delta_", "iceberg_",
        ".parquet", ".csv", ".json", ".ndjson", "://",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_data_source_is_conservative() {
        // Pure in-memory ⇒ no source (eligible for the free local path).
        assert!(!has_data_source("SELECT 1 + 1"));
        assert!(!has_data_source("SELECT now()"));
        assert!(!has_data_source("select 42 as x, upper('hi')"));
        // Any relation / reader / file / COPY / ATTACH ⇒ treated as a source.
        assert!(has_data_source("SELECT * FROM t"));
        assert!(has_data_source("select a\nfrom range(100)"));
        assert!(has_data_source("SELECT * FROM read_csv_auto('x.csv')"));
        assert!(has_data_source("SELECT * FROM read_parquet('s3://b/k.parquet')"));
        assert!(has_data_source("SELECT a JOIN b ON a.x=b.x"));
        assert!(has_data_source("COPY t TO 'out.csv'"));
        assert!(has_data_source("ATTACH 'db.duckdb'"));
        assert!(has_data_source("SELECT * FROM delta_scan('/t')"));
    }

    fn chunk(
        name: &str,
        bytes: u64,
        vals: u64,
        min: Option<f64>,
        max: Option<f64>,
    ) -> ColumnChunkMeta {
        ColumnChunkMeta {
            name: name.into(),
            total_uncompressed_size: bytes,
            num_values: vals,
            min,
            max,
        }
    }

    fn two_rowgroup_parquet() -> ParquetMetadata {
        // Each row-group: 1000 rows; columns a (id) and b (payload).
        ParquetMetadata {
            row_groups: vec![
                RowGroupMeta {
                    num_rows: 1000,
                    columns: vec![
                        chunk("a", 8_000, 1000, Some(0.0), Some(999.0)),
                        chunk("b", 40_000, 1000, None, None),
                    ],
                },
                RowGroupMeta {
                    num_rows: 1000,
                    columns: vec![
                        chunk("a", 8_000, 1000, Some(1000.0), Some(1999.0)),
                        chunk("b", 40_000, 1000, None, None),
                    ],
                },
            ],
        }
    }

    #[test]
    fn parquet_projection_sums_only_projected_columns() {
        let meta = two_rowgroup_parquet();
        let p = EstimateParams::default();
        // Project only "a": 2 row-groups * 8000 bytes = 16000.
        let est = estimate_parquet(&meta, &Projection::columns(["a"]), &[], &p);
        assert_eq!(est.scanned_uncompressed_bytes, 16_000);
        assert_eq!(est.total_rows, 2000);
        assert_eq!(est.units_scanned, 2);
        // Project all: (8000+40000)*2 = 96000.
        let all = estimate_parquet(&meta, &Projection::All, &[], &p);
        assert_eq!(all.scanned_uncompressed_bytes, 96_000);
    }

    #[test]
    fn parquet_predicate_prunes_rowgroup_via_minmax() {
        let meta = two_rowgroup_parquet();
        let p = EstimateParams::default();
        // a > 1500 → only the second row-group (max 1999) survives.
        let est = estimate_parquet(
            &meta,
            &Projection::columns(["a"]),
            &[Predicate::new("a", Cmp::Gt, 1500.0)],
            &p,
        );
        assert_eq!(est.units_scanned, 1);
        assert_eq!(est.scanned_uncompressed_bytes, 8_000);
        assert_eq!(est.total_rows, 1000);
        // a > 5000 → both pruned.
        let none = estimate_parquet(
            &meta,
            &Projection::columns(["a"]),
            &[Predicate::new("a", Cmp::Gt, 5000.0)],
            &p,
        );
        assert_eq!(none.units_scanned, 0);
        assert_eq!(none.scanned_uncompressed_bytes, 0);
    }

    #[test]
    fn working_set_high_cardinality_group_by_dominates() {
        let meta = two_rowgroup_parquet();
        let p = EstimateParams::default();
        let scan = estimate_parquet(&meta, &Projection::columns(["a"]), &[], &p);
        // Streaming: peak is just the bounded scan buffer (<= cap, and here <=
        // scanned bytes 16000).
        let streaming = estimate_working_set(&scan, &QueryShape::streaming(), &p);
        assert_eq!(streaming.group_by_bytes, 0);
        assert_eq!(
            streaming.peak_working_set_bytes,
            scan.scanned_uncompressed_bytes
        );

        // High-cardinality GROUP BY: 2000 distinct groups * (8-byte width + 32
        // overhead) = 80000, which dominates the 16000 scan buffer.
        let shape = QueryShape {
            group_by: true,
            distinct_groups: 2000,
            group_payload_bytes: 8,
            ..QueryShape::default()
        };
        let gb = estimate_working_set(&scan, &shape, &p);
        assert_eq!(gb.group_by_bytes, 2000 * (8 + 32));
        assert!(gb.peak_working_set_bytes > streaming.peak_working_set_bytes);
    }

    #[test]
    fn working_set_join_and_sort_add_up() {
        let scan = ScanEstimate {
            scanned_uncompressed_bytes: 1_000_000,
            total_rows: 10_000,
            estimated_output_rows: 10_000,
            avg_row_width_bytes: 100,
            units_total: 1,
            units_scanned: 1,
            projected_columns: 3,
        };
        let p = EstimateParams::default();
        let shape = QueryShape {
            hash_join: true,
            join_build_rows: 5_000,
            join_build_row_width: 50,
            sort_or_window: true,
            sort_rows: 10_000,
            sort_row_width: 100,
            ..QueryShape::default()
        };
        let ws = estimate_working_set(&scan, &shape, &p);
        assert_eq!(ws.join_build_bytes, 5_000 * 50);
        assert_eq!(ws.sort_bytes, 10_000 * 100);
        // scan buffer capped at 16 MiB but scanned is 1 MB → buffer = 1 MB.
        assert_eq!(ws.scan_buffer_bytes, 1_000_000);
        assert_eq!(
            ws.peak_working_set_bytes,
            1_000_000 + 5_000 * 50 + 10_000 * 100
        );
    }

    #[test]
    fn table_files_projection_ratio_and_pruning() {
        let mut f1_bounds = BTreeMap::new();
        f1_bounds.insert("ts".to_string(), (Some(0.0), Some(100.0)));
        let mut f2_bounds = BTreeMap::new();
        f2_bounds.insert("ts".to_string(), (Some(200.0), Some(300.0)));
        let meta = TableFilesMetadata {
            all_columns: vec!["ts".into(), "a".into(), "b".into(), "c".into()],
            files: vec![
                DataFileMeta {
                    size_bytes: 1_000_000,
                    record_count: 10_000,
                    bounds: f1_bounds,
                },
                DataFileMeta {
                    size_bytes: 1_000_000,
                    record_count: 10_000,
                    bounds: f2_bounds,
                },
            ],
        };
        let p = EstimateParams::default();
        // Project 1 of 4 columns, filter ts < 150 → only file 1 survives.
        let est = estimate_table_files(
            &meta,
            &Projection::columns(["ts"]),
            &[Predicate::new("ts", Cmp::Lt, 150.0)],
            &p,
        );
        assert_eq!(est.units_scanned, 1);
        assert_eq!(est.total_rows, 10_000);
        // uncompressed = 1_000_000 * 4.0 * (1/4) = 1_000_000.
        assert_eq!(est.scanned_uncompressed_bytes, 1_000_000);
    }

    #[test]
    fn parquet_metadata_from_resultset_groups_by_row_group() {
        // Mirrors `SELECT row_group_id, row_group_num_rows, path_in_schema,
        // num_values, total_uncompressed_size, stats_min_value, stats_max_value
        // FROM parquet_metadata(...)` — two row-groups, two columns each.
        let columns = vec![
            "row_group_id".into(),
            "row_group_num_rows".into(),
            "path_in_schema".into(),
            "num_values".into(),
            "total_uncompressed_size".into(),
            "stats_min_value".into(),
            "stats_max_value".into(),
        ];
        let row = |rg: i64, n: i64, name: &str, vals: i64, size: i64, mn: &str, mx: &str| {
            vec![
                Value::Int(rg),
                Value::Int(n),
                Value::Text(name.into()),
                Value::Int(vals),
                Value::Int(size),
                Value::Text(mn.into()),
                Value::Text(mx.into()),
            ]
        };
        let rs = ResultSet::new(
            columns,
            vec![
                row(0, 1000, "a", 1000, 8000, "0", "999"),
                row(0, 1000, "b", 1000, 40000, "x", "z"),
                row(1, 500, "a", 500, 4000, "1000", "1499"),
                row(1, 500, "b", 500, 20000, "x", "z"),
            ],
        );
        let meta = parquet_metadata_from_resultset(&rs);
        assert_eq!(meta.row_groups.len(), 2);
        assert_eq!(meta.row_groups[0].num_rows, 1000);
        assert_eq!(meta.row_groups[1].num_rows, 500);
        // numeric stats on "a" parsed; non-numeric stats on "b" → None.
        assert_eq!(meta.row_groups[0].columns[0].min, Some(0.0));
        assert_eq!(meta.row_groups[0].columns[0].max, Some(999.0));
        assert_eq!(meta.row_groups[0].columns[1].min, None);

        // Estimate over it: project "a", filter a > 1200 → only RG1 survives.
        let p = EstimateParams::default();
        let est = estimate_parquet(
            &meta,
            &Projection::columns(["a"]),
            &[Predicate::new("a", Cmp::Gt, 1200.0)],
            &p,
        );
        assert_eq!(est.units_scanned, 1);
        assert_eq!(est.scanned_uncompressed_bytes, 4000);
        assert_eq!(est.total_rows, 500);
    }

    #[test]
    fn explain_cardinality_picks_max_ec() {
        let plan = "\
┌─────────────┐
│  HASH_GROUP_BY  │
│   EC: 1200   │
└─────────────┘
┌─────────────┐
│   SEQ_SCAN   │
│  EC: 50000   │
└─────────────┘";
        assert_eq!(parse_explain_cardinality(plan), Some(50_000));
        assert_eq!(parse_explain_cardinality("no cardinality here"), None);
    }

    #[test]
    fn explain_cardinality_parses_modern_tilde_rows() {
        // Modern DuckDB physical plans annotate operators with `~<n> rows`
        // (grouped thousands) instead of the legacy `EC: n`.
        let plan = "\
┌───────────────────────────┐
│       HASH_GROUP_BY       │
│        ~5,000 rows        │
└─────────────┬─────────────┘
┌─────────────┴─────────────┐
│           RANGE           │
│        ~10,000 rows       │
└───────────────────────────┘";
        assert_eq!(parse_explain_cardinality(plan), Some(10_000));
    }
}
