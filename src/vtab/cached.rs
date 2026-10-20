// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::error::{Error, Result, sqlite_ok};
use crate::function::{FunctionArg, FunctionContext, build_args};
use libsqlite3_sys as ffi;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use super::column_helpers;
use super::ffi::{
    c_string_to_string, call_cached_modify_hook, callback_status, finish_cursor_unit,
    finish_vtab_unit, like_constraint_has_usable_prefix, parse_col_used, quote_identifier,
    resolve_constraint_specs, set_vtab_error, sqlite_mprintf_string, sqlite3_vtab_nochange,
    unsupported_delete, unsupported_insert, unsupported_update,
};
use super::index::InsertFn;
use super::index_key;
use super::index_key::{
    KeyBound, KeyOperand, exact_key, key_operand, key_satisfies, lower_bound, upper_bound,
};
use super::{
    ColumnType, ConstraintOp, ConstraintRequest, FILTER_NONE, GeneratorConstraintArg, ModifyHook,
    RowIterator, TransactionHooks, TransactionLifecycle, TransactionState, WriteCaps,
    WriteSurfaceRegistry, apply_update_columns, begin_vtab_callback, connect_write_surface,
    destroy_write_surface, table_write_counter, vtab_callback_failure,
};

const CACHED_ROWID_SCAN: c_int = -3;
const CACHED_COUNT_ONLY_SCAN: c_int = -1;
// An equality index lookup uses idxNum = CACHED_INDEX_BASE + index_pos.
const CACHED_INDEX_BASE: c_int = 10_000;

// Range (>=, >, <=, <, BETWEEN) pushdown on an indexed column. Encoded as
//   idxNum = CACHED_RANGE_BASE + index_pos * CACHED_RANGE_STRIDE + range_flags
// where range_flags is a 4-bit mask. CACHED_RANGE_BASE sits ABOVE the equality
// index space (`CACHED_INDEX_BASE + index_pos`) so the two never collide;
// `index_pos * CACHED_RANGE_STRIDE` reserves 16 flag values per index. A range
// plan always sets idxNum >= CACHED_RANGE_BASE > 0, so the equality encoding is
// unchanged and a full scan still reports "VIRTUAL TABLE INDEX 0". argv order is
// [low?, high?] (low first when both bounds are present, matching how
// xBestIndex assigns argvIndex). Mirrors the C++ RANGE_BASE encoding; only the
// numeric base differs, tracking this port's larger CACHED_INDEX_BASE.
const CACHED_RANGE_BASE: c_int = 20_000;
const CACHED_RANGE_STRIDE: c_int = 16; // 4 flag bits per index slot
const RANGE_HAS_LOW: c_int = 0x1; // a lower bound (>= or >) is present
const RANGE_LOW_STRICT: c_int = 0x2; // lower bound is strict (>, not >=)
const RANGE_HAS_HIGH: c_int = 0x4; // an upper bound (<= or <) is present
const RANGE_HIGH_STRICT: c_int = 0x8; // upper bound is strict (<, not <=)
const RANGE_FLAG_MASK: c_int =
    RANGE_HAS_LOW | RANGE_LOW_STRICT | RANGE_HAS_HIGH | RANGE_HIGH_STRICT;

// Multi-column/operator constraint filters use a disjoint high range, tested in
// xFilter BEFORE the range decode (whose encoding is open ended). Mirrors the C++
// CACHED_CONSTRAINT_FILTER_BASE.
const CACHED_CONSTRAINT_FILTER_BASE: c_int = 1_000_000;
// The range space `CACHED_RANGE_BASE + index_pos * CACHED_RANGE_STRIDE` would
// reach the constraint-filter space at this many indexes, and the equality space
// `CACHED_INDEX_BASE + index_pos` would reach the range space earlier still;
// build() rejects a table past the lower bound instead of mis-dispatching a plan.
const CACHED_MAX_INDEXES: usize = {
    let range = (CACHED_CONSTRAINT_FILTER_BASE - CACHED_RANGE_BASE) / CACHED_RANGE_STRIDE;
    let equality = CACHED_RANGE_BASE - CACHED_INDEX_BASE;
    (if range < equality { range } else { equality }) as usize
};
const _: () = assert!(CACHED_CONSTRAINT_FILTER_BASE > CACHED_RANGE_BASE);
const _: () = assert!(CACHED_RANGE_BASE > CACHED_INDEX_BASE);

type CachedGetter<Row> = dyn for<'a> Fn(&mut FunctionContext<'a>, &Row) + 'static;
type CachedSetter<Row> = dyn for<'a> Fn(&mut Row, &FunctionArg<'a>) -> bool + 'static;
type CachedBuilder<Row> = dyn Fn(&mut Vec<Row>) + 'static;
type CachedProjectionBuilder<Row> = dyn Fn(&mut Vec<Row>, u64) + 'static;
type StatefulCachedBuilder<Row> = dyn Fn(&TransactionState, &mut Vec<Row>) + 'static;
pub(crate) type CachedCount = dyn Fn() -> usize + 'static;
pub(crate) type CachedFilterFactory =
    dyn for<'a> Fn(&FunctionArg<'a>) -> Box<dyn RowIterator> + 'static;
type CachedIndexKey<Row> = dyn Fn(&Row) -> i64 + 'static;
type CachedRowid<Row> = dyn Fn(&Row) -> i64 + 'static;
type CachedDelete<Row> = dyn Fn(&Row) -> bool + 'static;
type CachedLookup<Row> = dyn Fn(i64) -> Option<Row> + 'static;
type CachedPopulator<Row> = dyn for<'a> Fn(&[FunctionArg<'a>]) -> Option<Row> + 'static;
type CachedConstraintFactory =
    dyn for<'a> Fn(&[GeneratorConstraintArg<'a>]) -> Box<dyn RowIterator> + 'static;

struct CachedColumnDef<Row> {
    name: String,
    column_type: ColumnType,
    getter: Box<CachedGetter<Row>>,
    setter: Option<Box<CachedSetter<Row>>>,
}

struct CachedFilterDef {
    column_index: usize,
    op: c_int,
    omit: bool,
    filter_id: c_int,
    estimated_cost: f64,
    estimated_rows: i64,
    factory: Box<CachedFilterFactory>,
}

struct CachedIndexDef<Row> {
    column_index: usize,
    key: Box<CachedIndexKey<Row>>,
}

struct CachedConstraintSpec {
    column_index: usize,
    op: ConstraintOp,
    required: bool,
}

/// A multi-column/operator pushdown (for example an interval containment
/// `p.value >= i.start AND p.value < i.end`) that a single-column filter cannot
/// express. xBestIndex picks the cheapest spec set whose required constraints are
/// all present and encodes the chosen spec positions in idxStr. Mirrors the C++
/// `CachedConstraintFilterDef`.
struct CachedConstraintFilterDef {
    specs: Vec<CachedConstraintSpec>,
    filter_id: c_int,
    estimated_cost: f64,
    estimated_rows: i64,
    /// `Some`: factory-backed and exact (`constraint_filter`). `None`: evaluated
    /// in memory over the table's rows through its `index_on` keys
    /// (`constraint_cache_filter`).
    factory: Option<Box<CachedConstraintFactory>>,
}

/// A per-index sorted view used ONLY for range (>=, >, <=, <, BETWEEN)
/// pushdown. The hash `indexes` stay the sole structure for the hot equality
/// path; this parallel structure is built LAZILY on the first range query for an
/// index and then cached alongside the hash index (invalidated on the same
/// rebuilds). It is a stable-sorted vector of `(key, row-position)` pairs so a
/// binary search can carve out the matching window; duplicate keys sort together
/// (in row order), so a multi-match key is a contiguous run. `built` gates a
/// one-time construction. Mirrors the C++ `detail::SortedIndex`.
#[derive(Default)]
struct SortedIndex {
    entries: Vec<(i64, usize)>, // sorted by key ascending, stable in row order
    built: bool,
}

impl SortedIndex {
    /// Build (once) the sorted `(key, row)` view from a data slice + its key
    /// extractor. A stable sort keeps a deterministic per-key order (matching
    /// the data order for equal keys), so the window a range carves is
    /// reproducible. Mirrors the C++ `detail::build_sorted_index`.
    fn build<Row>(&mut self, data: &[Row], key: &CachedIndexKey<Row>) {
        self.entries.clear();
        self.entries.reserve(data.len());
        for (row, item) in data.iter().enumerate() {
            self.entries.push((key(item), row));
        }
        // stable sort by key: equal keys keep their original row order.
        self.entries.sort_by_key(|(k, _)| *k);
        self.built = true;
    }

    /// Carve the matching window honoring the range flags + bounds and append
    /// the matching row positions to `out`. `low`/`high` are only read when the
    /// corresponding `RANGE_HAS_*` flag is set. Mirrors the C++
    /// `detail::collect_range_matches` (`partition_point` replaces
    /// `lower_bound`/`upper_bound`).
    fn collect_matches(&self, range_flags: c_int, low: i64, high: i64, out: &mut Vec<usize>) {
        // begin index: first entry included by the low bound.
        let begin = if range_flags & RANGE_HAS_LOW != 0 {
            if range_flags & RANGE_LOW_STRICT != 0 {
                // > low : first key strictly greater than low.
                self.entries.partition_point(|(k, _)| *k <= low)
            } else {
                // >= low : first key not less than low.
                self.entries.partition_point(|(k, _)| *k < low)
            }
        } else {
            0
        };
        // end index: one past the last entry included by the high bound.
        let end = if range_flags & RANGE_HAS_HIGH != 0 {
            if range_flags & RANGE_HIGH_STRICT != 0 {
                // < high : first key not less than high.
                self.entries.partition_point(|(k, _)| *k < high)
            } else {
                // <= high : one past the last key not greater than high.
                self.entries.partition_point(|(k, _)| *k <= high)
            }
        } else {
            self.entries.len()
        };
        if begin < end {
            out.extend(self.entries[begin..end].iter().map(|(_, row)| *row));
        }
    }
}

/// Decode a range idxNum into `(index_pos, range_flags)`, or `None` if it is not
/// a range plan. Keeps the encoding math in one place for xFilter.
fn decode_range_idx(idx_num: c_int) -> Option<(usize, c_int)> {
    if idx_num < CACHED_RANGE_BASE {
        return None;
    }
    let encoded = idx_num - CACHED_RANGE_BASE;
    let index_pos = (encoded / CACHED_RANGE_STRIDE) as usize;
    let range_flags = encoded & RANGE_FLAG_MASK;
    Some((index_pos, range_flags))
}

struct SharedCache<Row> {
    data: Vec<Row>,
    indexes: Vec<HashMap<i64, Vec<usize>>>,
    /// Parallel sorted views for range pushdown, one slot per hash index. Built
    /// on demand (first range query), cleared whenever `indexes` is.
    sorted_indexes: Vec<SortedIndex>,
    built: bool,
    /// True when `data` is retained as a snapshot of an in-progress scan-driven
    /// mutation even though `built` is false. While set, the cursor read paths
    /// and positional update/delete reconstruction keep using `data` without a
    /// rebuild, so a multi-row `UPDATE`/`DELETE` visits every row consistently.
    mutation_snapshot: bool,
}

impl<Row> Default for SharedCache<Row> {
    fn default() -> Self {
        Self {
            data: Vec::new(),
            indexes: Vec::new(),
            sorted_indexes: Vec::new(),
            built: false,
            mutation_snapshot: false,
        }
    }
}

struct CachedInner<Row> {
    name: String,
    estimate_rows: Option<Box<CachedCount>>,
    row_count: Option<Box<CachedCount>>,
    cache_builder: Option<Box<CachedBuilder<Row>>>,
    projection_cache_builder: Option<Box<CachedProjectionBuilder<Row>>>,
    stateful_cache_builder: Option<Box<StatefulCachedBuilder<Row>>>,
    columns: Vec<CachedColumnDef<Row>>,
    filters: Vec<CachedFilterDef>,
    constraint_filters: Vec<CachedConstraintFilterDef>,
    indexes: Vec<CachedIndexDef<Row>>,
    rowid: Option<Box<CachedRowid<Row>>>,
    before_modify: Option<Box<ModifyHook>>,
    after_modify: Option<Box<ModifyHook>>,
    transaction_hooks: TransactionHooks,
    counts_as_write: bool,
    delete_row: Option<Box<CachedDelete<Row>>>,
    insert_row: Option<Box<InsertFn>>,
    row_lookup: Option<Box<CachedLookup<Row>>>,
    row_populator: Option<Box<CachedPopulator<Row>>>,
    update_from_column_values: bool,
    use_shared_cache: bool,
    shared_cache: RefCell<SharedCache<Row>>,
    /// Monotonic count of writes through this table (INSERT/UPDATE/DELETE). A
    /// query-scoped cursor-lived cache records the value it was built at and is
    /// reused across that cursor's xFilter calls only while it still matches.
    /// Mirrors the C++ `CachedTableDef::write_generation`.
    write_generation: Cell<u64>,
}

impl<Row> CachedInner<Row> {
    fn schema(&self) -> String {
        let columns = self
            .columns
            .iter()
            .map(|column| {
                format!(
                    "{} {}",
                    quote_identifier(&column.name),
                    column.column_type.sql()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("CREATE TABLE {}({columns})", quote_identifier(&self.name))
    }

    fn find_filter_by_column(&self, column_index: usize, op: c_int) -> Option<&CachedFilterDef> {
        self.filters
            .iter()
            .filter(|filter| filter.column_index == column_index && filter.op == op)
            .min_by(|a, b| a.estimated_cost.total_cmp(&b.estimated_cost))
    }

    fn find_filter_by_id(&self, filter_id: c_int) -> Option<&CachedFilterDef> {
        self.filters
            .iter()
            .find(|filter| filter.filter_id == filter_id)
    }

    fn find_index_by_column(&self, column_index: usize) -> Option<usize> {
        self.indexes
            .iter()
            .position(|index| index.column_index == column_index)
    }

    /// Whether `row` passes every `(index position, operator, operand)` test of
    /// an in-memory constraint filter.
    fn row_matches_constraints(
        &self,
        row: &Row,
        tests: &[(usize, ConstraintOp, KeyOperand)],
    ) -> bool {
        tests
            .iter()
            .all(|&(pos, op, operand)| key_satisfies((self.indexes[pos].key)(row), op, operand))
    }

    fn build_rows(&self, state: &TransactionState, rows: &mut Vec<Row>, col_used: u64) {
        rows.clear();
        if let Some(stateful_cache_builder) = self.stateful_cache_builder.as_ref() {
            stateful_cache_builder(state, rows);
        } else if let Some(projection_cache_builder) = self.projection_cache_builder.as_ref() {
            projection_cache_builder(rows, col_used);
        } else if let Some(cache_builder) = self.cache_builder.as_ref() {
            cache_builder(rows);
        }
    }

    fn has_cache_builder(&self) -> bool {
        self.cache_builder.is_some()
            || self.projection_cache_builder.is_some()
            || self.stateful_cache_builder.is_some()
    }

    fn ensure_shared_cache(&self) {
        if !self.use_shared_cache {
            return;
        }
        if self.shared_cache.borrow().built {
            return;
        }
        let mut cache = self.shared_cache.borrow_mut();
        cache.data.clear();
        cache.indexes.clear();
        // The parallel sorted (range) views are built lazily on the first range
        // query (see the xFilter range branch); we only size the slot vector here.
        cache.sorted_indexes.clear();
        if let Some(cache_builder) = self.cache_builder.as_ref() {
            cache_builder(&mut cache.data);
        }
        let mut indexes = Vec::with_capacity(self.indexes.len());
        for index in &self.indexes {
            let mut map: HashMap<i64, Vec<usize>> = HashMap::new();
            for (row_index, row) in cache.data.iter().enumerate() {
                map.entry((index.key)(row)).or_default().push(row_index);
            }
            indexes.push(map);
        }
        cache.indexes = indexes;
        cache
            .sorted_indexes
            .resize_with(self.indexes.len(), SortedIndex::default);
        cache.built = true;
        cache.mutation_snapshot = false;
    }

    /// Invalidate after an `UPDATE`/`DELETE`. Mirrors the C++
    /// `cached_table_invalidate_after_mutation`: indexes are dropped and `built`
    /// is cleared, but `data` is retained as a snapshot so the rest of an
    /// in-progress scan can keep resolving rows positionally.
    fn invalidate_shared_cache(&self) {
        self.write_generation
            .set(self.write_generation.get().wrapping_add(1));
        if !self.use_shared_cache {
            return;
        }
        let mut cache = self.shared_cache.borrow_mut();
        cache.indexes.clear();
        cache.sorted_indexes.clear();
        cache.built = false;
        cache.mutation_snapshot = !cache.data.is_empty();
    }

    /// Fully clear the shared cache after an `INSERT`. The new row is not present
    /// in any snapshot, so positional reconstruction would be invalid; force a
    /// full rebuild on next use. Mirrors the C++ `invalidate_cache`.
    fn clear_shared_cache(&self) {
        self.write_generation
            .set(self.write_generation.get().wrapping_add(1));
        if !self.use_shared_cache {
            return;
        }
        let mut cache = self.shared_cache.borrow_mut();
        cache.data.clear();
        cache.indexes.clear();
        cache.sorted_indexes.clear();
        cache.built = false;
        cache.mutation_snapshot = false;
    }

    /// A table supports scan-driven mutation when a `DELETE` callback or any
    /// writable column setter exists. Such tables must not use the count-only
    /// fast path. Mirrors the C++ `cached_table_has_scan_driven_mutation`.
    fn has_scan_driven_mutation(&self) -> bool {
        self.delete_row.is_some() || self.columns.iter().any(|column| column.setter.is_some())
    }

    /// Query-scoped analog of the shared-cache mutation snapshot: a NON-shared,
    /// full-scan (no filter), positional table (no `row_lookup` / `rowid`) whose
    /// scan-driven UPDATE/DELETE needs the pre-mutation cache frozen for the whole
    /// statement (positional rowids stay valid). Mirrors the C++
    /// `query_scoped_uses_mutation_snapshot` (filter tables are excluded: a filter
    /// iterator rowid may be iterator-local, not a global cache position).
    fn query_scoped_uses_mutation_snapshot(&self) -> bool {
        !self.use_shared_cache
            && self.row_lookup.is_none()
            && self.rowid.is_none()
            && self.filters.is_empty()
            && self.has_cache_builder()
            && self.has_scan_driven_mutation()
    }

    /// Build (once) a snapshot of the pre-mutation cache in `shared_cache` so a
    /// scan-driven multi-row UPDATE/DELETE on a query-scoped table keeps stable
    /// positional rowids for the statement.
    fn ensure_query_scoped_mutation_snapshot(&self, state: &TransactionState) -> Result<()> {
        if !self.query_scoped_uses_mutation_snapshot() {
            return Ok(());
        }
        let mut cache = self.shared_cache.borrow_mut();
        if !cache.mutation_snapshot {
            cache.data.clear();
            cache.indexes.clear();
            cache.sorted_indexes.clear();
            let build_result = catch_unwind(AssertUnwindSafe(|| {
                self.build_rows(state, &mut cache.data, u64::MAX);
            }));
            if build_result.is_err() {
                cache.data.clear();
                cache.indexes.clear();
                cache.sorted_indexes.clear();
                cache.mutation_snapshot = false;
                cache.built = false;
                return Err(Error::Message(
                    "cached table query-scoped snapshot builder panicked".to_string(),
                ));
            }
            cache.mutation_snapshot = true;
            cache.built = false;
        }
        Ok(())
    }

    /// Drop a query-scoped snapshot at the start of the next scan (= next
    /// statement), so reads rebuild fresh. No-op for shared tables.
    fn clear_query_scoped_mutation_snapshot(&self) {
        if self.use_shared_cache {
            return;
        }
        let mut cache = self.shared_cache.borrow_mut();
        if cache.mutation_snapshot {
            cache.data.clear();
            cache.mutation_snapshot = false;
        }
    }
}

/// Definition for a cache-backed virtual table.
#[derive(Clone)]
pub struct CachedTableDef<Row> {
    inner: Rc<CachedInner<Row>>,
}

impl<Row> CachedTableDef<Row> {
    /// Returns the table's name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Drops this table's cached rows so the next scan rebuilds them from the
    /// source, and retires every query-scoped cursor cache built before the call.
    ///
    /// Clones of a definition share one cache, so this reaches the copy a
    /// [`Database`](crate::Database) registered. Unlike a write through the
    /// table, it also discards the shared-cache data rather than keeping it as a
    /// snapshot. Mirrors the C++ `CachedTableDef::invalidate_cache`. To reach
    /// every cached table on a connection without holding their definitions, use
    /// [`FunctionContext::invalidate_cached_tables`] or
    /// [`Database::invalidate_cached_tables`](crate::Database::invalidate_cached_tables).
    pub fn invalidate_cache(&self) {
        self.inner.clear_shared_cache();
    }

    /// Compute write-surface capabilities for the prepare-time authorizer
    /// (mirrors the C++ cached `record_write_surface`): insertable/deletable iff
    /// the callback is present, updatable iff any column has a setter.
    pub(crate) fn write_caps(&self) -> super::WriteCaps {
        super::WriteCaps {
            insertable: self.inner.insert_row.is_some(),
            deletable: self.inner.delete_row.is_some(),
            writable_columns: self
                .inner
                .columns
                .iter()
                .filter(|column| column.setter.is_some())
                .map(|column| column.name.to_ascii_lowercase())
                .collect(),
        }
    }
}

/// Start a cache-backed virtual table definition.
pub fn cached_table<Row: 'static>(name: impl Into<String>) -> CachedTableBuilder<Row> {
    CachedTableBuilder {
        name: name.into(),
        estimate_rows: None,
        row_count: None,
        cache_builder: None,
        projection_cache_builder: None,
        stateful_cache_builder: None,
        columns: Vec::new(),
        filters: Vec::new(),
        constraint_filters: Vec::new(),
        indexes: Vec::new(),
        rowid: None,
        before_modify: None,
        after_modify: None,
        transaction_hooks: TransactionHooks::default(),
        counts_as_write: true,
        delete_row: None,
        insert_row: None,
        row_lookup: None,
        row_populator: None,
        update_from_column_values: false,
        use_shared_cache: true,
    }
}

/// Fluent builder for cache-backed virtual tables.
pub struct CachedTableBuilder<Row> {
    name: String,
    estimate_rows: Option<Box<CachedCount>>,
    row_count: Option<Box<CachedCount>>,
    cache_builder: Option<Box<CachedBuilder<Row>>>,
    projection_cache_builder: Option<Box<CachedProjectionBuilder<Row>>>,
    stateful_cache_builder: Option<Box<StatefulCachedBuilder<Row>>>,
    columns: Vec<CachedColumnDef<Row>>,
    filters: Vec<CachedFilterDef>,
    constraint_filters: Vec<CachedConstraintFilterDef>,
    indexes: Vec<CachedIndexDef<Row>>,
    rowid: Option<Box<CachedRowid<Row>>>,
    before_modify: Option<Box<ModifyHook>>,
    after_modify: Option<Box<ModifyHook>>,
    transaction_hooks: TransactionHooks,
    counts_as_write: bool,
    delete_row: Option<Box<CachedDelete<Row>>>,
    insert_row: Option<Box<InsertFn>>,
    row_lookup: Option<Box<CachedLookup<Row>>>,
    row_populator: Option<Box<CachedPopulator<Row>>>,
    update_from_column_values: bool,
    use_shared_cache: bool,
}

impl<Row: 'static> CachedTableBuilder<Row> {
    /// Set the callback returning an estimated row count for query planning.
    pub fn estimate_rows<F>(mut self, estimate_rows: F) -> Self
    where
        F: Fn() -> usize + 'static,
    {
        self.estimate_rows = Some(Box::new(estimate_rows));
        self
    }

    /// Set the callback returning the table's row count.
    pub fn count<F>(mut self, count: F) -> Self
    where
        F: Fn() -> usize + 'static,
    {
        self.row_count = Some(Box::new(count));
        self
    }

    /// Set the builder that materializes all rows into the cache.
    pub fn cache_builder<F>(mut self, cache_builder: F) -> Self
    where
        F: Fn(&mut Vec<Row>) + 'static,
    {
        self.cache_builder = Some(Box::new(cache_builder));
        self
    }

    /// Set a projection-aware builder that receives the `colUsed` bitmask so it
    /// can populate only the columns the query needs.
    pub fn projection_cache_builder<F>(mut self, projection_cache_builder: F) -> Self
    where
        F: Fn(&mut Vec<Row>, u64) + 'static,
    {
        self.projection_cache_builder = Some(Box::new(projection_cache_builder));
        self
    }

    /// Set a query-scoped builder that can read this registration's transaction
    /// state. Stateful builders never use the definition-wide shared cache.
    pub fn stateful_cache_builder<F>(mut self, stateful_cache_builder: F) -> Self
    where
        F: Fn(&TransactionState, &mut Vec<Row>) + 'static,
    {
        self.stateful_cache_builder = Some(Box::new(stateful_cache_builder));
        self.use_shared_cache = false;
        self
    }

    /// Disable the process-wide shared cache; each cursor builds its own local
    /// cache instead.
    pub fn no_shared_cache(mut self) -> Self {
        self.use_shared_cache = false;
        self
    }

    /// Set a hook invoked with the SQL operation string before any
    /// INSERT/UPDATE/DELETE.
    pub fn on_modify<F>(mut self, before_modify: F) -> Self
    where
        F: Fn(&str) + 'static,
    {
        self.before_modify = Some(Box::new(before_modify));
        self
    }

    /// Set a hook invoked with the SQL operation string after a successful
    /// INSERT/UPDATE/DELETE.
    pub fn after_modify<F>(mut self, after_modify: F) -> Self
    where
        F: Fn(&str) + 'static,
    {
        self.after_modify = Some(Box::new(after_modify));
        self
    }

    /// Install the complete SQLite transaction lifecycle for this table.
    pub fn transaction_hooks(mut self, transaction_hooks: TransactionHooks) -> Self {
        self.transaction_hooks = transaction_hooks;
        self
    }

    /// Whether this table's row writes bump the connection's
    /// [`Database::vtab_write_count`](crate::Database::vtab_write_count)
    /// (default `true`). A session or bookkeeping table (settings, checkpoints,
    /// previews) passes `false`, so writing it does not read as a host change.
    pub fn counts_as_write(mut self, counts_as_write: bool) -> Self {
        self.counts_as_write = counts_as_write;
        self
    }

    /// Add a column with a custom getter that writes the value for a row
    /// reference into the context.
    pub fn column<F>(self, name: impl Into<String>, column_type: ColumnType, getter: F) -> Self
    where
        F: for<'a> Fn(&mut FunctionContext<'a>, &Row) + 'static,
    {
        self.add_column(name, column_type, Box::new(getter), None)
    }

    /// Add an `INTEGER` column whose getter returns an `i32` for a row.
    pub fn column_int<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> i32 + 'static,
    {
        self.add_column(
            name,
            ColumnType::Integer,
            column_helpers::row_getter_int(getter),
            None,
        )
    }

    /// Add an `INTEGER` column whose getter returns an `i64` for a row.
    pub fn column_i64<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> i64 + 'static,
    {
        self.add_column(
            name,
            ColumnType::Integer,
            column_helpers::row_getter_i64(getter),
            None,
        )
    }

    /// Alias for [`column_i64`](Self::column_i64).
    pub fn column_int64<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> i64 + 'static,
    {
        self.column_i64(name, getter)
    }

    /// Add a `TEXT` column whose getter returns a `String` for a row.
    pub fn column_text<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> String + 'static,
    {
        self.add_column(
            name,
            ColumnType::Text,
            column_helpers::row_getter_text(getter),
            None,
        )
    }

    /// Add a `REAL` column whose getter returns an `f64` for a row.
    pub fn column_double<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> f64 + 'static,
    {
        self.add_column(
            name,
            ColumnType::Real,
            column_helpers::row_getter_double(getter),
            None,
        )
    }

    /// Add a `BLOB` column whose getter returns a `Vec<u8>` for a row.
    pub fn column_blob<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> Vec<u8> + 'static,
    {
        self.add_column(
            name,
            ColumnType::Blob,
            column_helpers::row_getter_blob(getter),
            None,
        )
    }

    /// Add a read-only `INTEGER` (`i32`) column whose getter may return `None`
    /// (emitting SQL NULL). Read-only sibling of
    /// [`column_int_nullable_rw`](Self::column_int_nullable_rw) (getter only).
    pub fn column_int_nullable<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> Option<i32> + 'static,
    {
        self.add_column(
            name,
            ColumnType::Integer,
            column_helpers::row_getter_nullable_int(getter),
            None,
        )
    }

    /// Add a read-only `TEXT` column whose getter may return `None` (emitting
    /// SQL NULL). Read-only sibling of
    /// [`column_text_nullable_rw`](Self::column_text_nullable_rw) (getter only).
    pub fn column_text_nullable<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> Option<String> + 'static,
    {
        self.add_column(
            name,
            ColumnType::Text,
            column_helpers::row_getter_nullable_text(getter),
            None,
        )
    }

    /// Add a writable column with a custom getter and setter operating on a row
    /// reference.
    pub fn column_rw<G, S>(
        self,
        name: impl Into<String>,
        column_type: ColumnType,
        getter: G,
        setter: S,
    ) -> Self
    where
        G: for<'a> Fn(&mut FunctionContext<'a>, &Row) + 'static,
        S: for<'a> Fn(&mut Row, &FunctionArg<'a>) -> bool + 'static,
    {
        self.add_column(name, column_type, Box::new(getter), Some(Box::new(setter)))
    }

    /// Add a writable `INTEGER` (`i32`) column; the setter returns false to
    /// reject the write.
    ///
    /// The setter receives only the integer the written value equals under
    /// SQLite's INTEGER comparison ([`exact_i64`](crate::exact_i64): `16`,
    /// `16.0` and `'16'` all arrive as 16). NULL, a blob, `16.5`, `'0x10'` or
    /// `'abc'` refuse the write with `column "<name>": expected an integer, got
    /// ...` and the setter does not run.
    /// An integer outside `i32` is refused too (`column "<name>": <value> does
    /// not fit a 32-bit integer`).
    pub fn column_int_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> i32 + 'static,
        S: Fn(&mut Row, i32) -> bool + 'static,
    {
        let name = name.into();
        self.add_column(
            name.clone(),
            ColumnType::Integer,
            column_helpers::row_getter_int(getter),
            Some(column_helpers::row_setter_int(name, setter)),
        )
    }

    /// Add a writable `INTEGER` (`i64`) column; the setter returns false to
    /// reject the write.
    ///
    /// The setter receives only the integer the written value equals under
    /// SQLite's INTEGER comparison ([`exact_i64`](crate::exact_i64): `16`,
    /// `16.0` and `'16'` all arrive as 16). NULL, a blob, `16.5`, `'0x10'` or
    /// `'abc'` refuse the write with `column "<name>": expected an integer, got
    /// ...` and the setter does not run.
    pub fn column_i64_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> i64 + 'static,
        S: Fn(&mut Row, i64) -> bool + 'static,
    {
        let name = name.into();
        self.add_column(
            name.clone(),
            ColumnType::Integer,
            column_helpers::row_getter_i64(getter),
            Some(column_helpers::row_setter_i64(name, setter)),
        )
    }

    /// Alias for [`column_i64_rw`](Self::column_i64_rw).
    pub fn column_int64_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> i64 + 'static,
        S: Fn(&mut Row, i64) -> bool + 'static,
    {
        self.column_i64_rw(name, getter, setter)
    }

    /// Add a writable `TEXT` column; the setter returns false to reject the
    /// write.
    pub fn column_text_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> String + 'static,
        S: Fn(&mut Row, &str) -> bool + 'static,
    {
        self.add_column(
            name,
            ColumnType::Text,
            column_helpers::row_getter_text(getter),
            Some(column_helpers::row_setter_text(setter)),
        )
    }

    /// Add a writable `TEXT` column whose getter may return `None` (emitting SQL
    /// NULL); the setter receives the raw argument.
    pub fn column_text_nullable_rw<G, S>(
        self,
        name: impl Into<String>,
        getter: G,
        setter: S,
    ) -> Self
    where
        G: Fn(&Row) -> Option<String> + 'static,
        S: for<'a> Fn(&mut Row, &FunctionArg<'a>) -> bool + 'static,
    {
        self.add_column(
            name,
            ColumnType::Text,
            column_helpers::row_getter_nullable_text(getter),
            Some(Box::new(setter)),
        )
    }

    /// Add a writable `INTEGER` (`i32`) column whose getter may return `None`
    /// (emitting SQL NULL); the setter receives the raw argument. Mirrors
    /// [`column_text_nullable_rw`](Self::column_text_nullable_rw) for the
    /// integer type — used where a mapped-but-unknown value must read back as
    /// NULL rather than 0.
    pub fn column_int_nullable_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> Option<i32> + 'static,
        S: for<'a> Fn(&mut Row, &FunctionArg<'a>) -> bool + 'static,
    {
        self.add_column(
            name,
            ColumnType::Integer,
            column_helpers::row_getter_nullable_int(getter),
            Some(Box::new(setter)),
        )
    }

    /// Add a writable `REAL` column; the setter returns false to reject the
    /// write.
    pub fn column_double_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> f64 + 'static,
        S: Fn(&mut Row, f64) -> bool + 'static,
    {
        self.add_column(
            name,
            ColumnType::Real,
            column_helpers::row_getter_double(getter),
            Some(column_helpers::row_setter_double(setter)),
        )
    }

    /// Add a writable `BLOB` column; the setter returns false to reject the
    /// write.
    pub fn column_blob_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> Vec<u8> + 'static,
        S: Fn(&mut Row, &[u8]) -> bool + 'static,
    {
        self.add_column(
            name,
            ColumnType::Blob,
            column_helpers::row_getter_blob(getter),
            Some(column_helpers::row_setter_blob(setter)),
        )
    }

    /// Register an equality-pushdown filter on `column_name` backed by a
    /// [`RowIterator`] factory keyed by an `i64` value.
    ///
    /// The constraint is omitted (SQLite trusts the rows), so the value is compared
    /// the way SQLite compares an INTEGER column: the factory is called only with the
    /// integer that equals it (`10`, `10.0`, `'10'`), and a value no integer equals
    /// (`10.5`, `'a'`, NULL, a blob) yields no rows without calling it.
    pub fn filter_eq<F>(
        self,
        column_name: impl AsRef<str>,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        F: Fn(i64) -> Box<dyn RowIterator> + 'static,
    {
        self.add_filter(
            column_name.as_ref(),
            ffi::SQLITE_INDEX_CONSTRAINT_EQ,
            true,
            estimated_cost,
            estimated_rows,
            move |value| match index_key::exact_i64(value) {
                Some(key) => factory(key),
                None => Box::new(index_key::NoRows),
            },
        )
    }

    /// Register an equality-pushdown filter on `column_name` backed by a
    /// [`RowIterator`] factory keyed by a text value.
    ///
    /// The constraint is omitted, so NULL (which equals nothing) and a blob (which
    /// never equals text) yield no rows without calling the factory.
    pub fn filter_eq_text<F>(
        self,
        column_name: impl AsRef<str>,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        F: Fn(&str) -> Box<dyn RowIterator> + 'static,
    {
        self.add_filter(
            column_name.as_ref(),
            ffi::SQLITE_INDEX_CONSTRAINT_EQ,
            true,
            estimated_cost,
            estimated_rows,
            move |value| match index_key::exact_text(value) {
                Some(text) => factory(&text),
                None => Box::new(index_key::NoRows),
            },
        )
    }

    /// Register a `LIKE`-prefix pushdown filter on `column_name`; applied only
    /// when the pattern has a usable literal prefix, and the full scan re-checks
    /// the pattern (the constraint is not omitted).
    pub fn filter_prefix<F>(
        self,
        column_name: impl AsRef<str>,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        F: Fn(&str) -> Box<dyn RowIterator> + 'static,
    {
        self.add_filter(
            column_name.as_ref(),
            ffi::SQLITE_INDEX_CONSTRAINT_LIKE,
            false,
            estimated_cost,
            estimated_rows,
            move |value| {
                let text = value.as_c_str().map(CStr::to_string_lossy);
                factory(text.as_deref().unwrap_or(""))
            },
        )
    }

    /// Build an in-memory hash index on `column_name` keyed by an `i64`, enabling
    /// O(1) equality lookups against the shared cache.
    pub fn index_on<F>(mut self, column_name: impl AsRef<str>, key: F) -> Self
    where
        F: Fn(&Row) -> i64 + 'static,
    {
        if let Some(column_index) = self.find_column(column_name.as_ref()) {
            self.indexes.push(CachedIndexDef {
                column_index,
                key: Box::new(key),
            });
        }
        self
    }

    /// Add an exact, factory-backed multi-column/operator pushdown. When every
    /// required constraint is present (and at least one constraint matches) and
    /// this is the cheapest plan, SQLite hands the matched values to `factory`
    /// and does not re-check them, so the iterator must return exactly the rows
    /// that satisfy them. A missing required constraint only disqualifies the
    /// filter: the table falls back to its other plans.
    ///
    /// Errors when a constraint names an unknown column, or when `constraints`
    /// is empty. Mirrors the C++ `CachedTableBuilder::constraint_filter`.
    pub fn constraint_filter<I, F>(
        self,
        constraints: I,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = ConstraintRequest>,
        F: for<'a> Fn(&[GeneratorConstraintArg<'a>]) -> Box<dyn RowIterator> + 'static,
    {
        self.add_constraint_filter(
            "constraint_filter",
            constraints,
            estimated_cost,
            estimated_rows,
            Some(Box::new(factory)),
        )
    }

    /// Add an exact multi-column/operator pushdown evaluated in memory over the
    /// table's rows: the shared cache for a shared table, the cursor-lived rows
    /// for a query-scoped one. Every constrained column needs an earlier
    /// [`index_on`](Self::index_on), whose integer key supplies the compared
    /// value (compared as SQLite does: NULL never matches, text sorts after every
    /// number).
    ///
    /// Errors when a constraint names an unknown column, a column without an
    /// index, or `LIKE` (not evaluable against an integer key), or when
    /// `constraints` is empty. Mirrors the C++
    /// `CachedTableBuilder::constraint_cache_filter`.
    pub fn constraint_cache_filter<I>(
        self,
        constraints: I,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = ConstraintRequest>,
    {
        self.add_constraint_filter(
            "constraint_cache_filter",
            constraints,
            estimated_cost,
            estimated_rows,
            None,
        )
    }

    fn add_constraint_filter<I>(
        mut self,
        builder: &str,
        constraints: I,
        estimated_cost: f64,
        estimated_rows: f64,
        factory: Option<Box<CachedConstraintFactory>>,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = ConstraintRequest>,
    {
        let fail = |detail: String| {
            Error::Message(format!("{builder} on table '{}': {detail}", self.name))
        };
        let mut specs = Vec::new();
        for request in constraints {
            let column_index = self
                .find_column(&request.column_name)
                .ok_or_else(|| fail(format!("unknown column '{}'", request.column_name)))?;
            if factory.is_none() {
                if self.find_index(column_index).is_none() {
                    return Err(fail(format!(
                        "column '{}' needs a matching index_on() to supply its comparison value",
                        request.column_name
                    )));
                }
                if request.op == ConstraintOp::Like {
                    return Err(fail(format!(
                        "LIKE is not evaluable against an integer index key (column '{}')",
                        request.column_name
                    )));
                }
            }
            specs.push(CachedConstraintSpec {
                column_index,
                op: request.op,
                required: request.required,
            });
        }
        if specs.is_empty() {
            return Err(fail("needs at least one constraint".to_string()));
        }
        let filter_id = CACHED_CONSTRAINT_FILTER_BASE + self.constraint_filters.len() as c_int;
        self.constraint_filters.push(CachedConstraintFilterDef {
            specs,
            filter_id,
            estimated_cost,
            estimated_rows: estimated_rows.max(1.0) as i64,
            factory,
        });
        Ok(self)
    }

    /// Set a callback computing a stable rowid (a KEY) from a row. Full and index
    /// scans then report `rowid(row)` instead of the cache position, and
    /// `WHERE rowid = ?` matches it by key.
    ///
    /// Mandatory with [`row_lookup`](Self::row_lookup): it must return the same key
    /// the table's filter iterators report and `row_lookup` resolves.
    pub fn rowid<F>(mut self, rowid: F) -> Self
    where
        F: Fn(&Row) -> i64 + 'static,
    {
        self.rowid = Some(Box::new(rowid));
        self
    }

    /// Set a callback that resolves a rowid (a KEY, never a cache position) back
    /// to its row. It becomes the SOLE authority for UPDATE, DELETE and
    /// `WHERE rowid = ?`: a rowid it cannot resolve (e.g. a row gone mid-statement)
    /// fails the write with `row <id> not found in <table>` (or the reason the
    /// callback set with [`set_vtab_error`](crate::set_vtab_error)) and matches no
    /// row on read -- never another row by cache position, and never a row
    /// rebuilt by [`row_populator`](Self::row_populator).
    ///
    /// Requires [`rowid`](Self::rowid): a table with `row_lookup` must report keys
    /// from its full and index scans, computed exactly as its filter iterators
    /// compute their rowids and as `row_lookup` decodes them.
    /// [`build`](Self::build) returns an error otherwise.
    pub fn row_lookup<F>(mut self, row_lookup: F) -> Self
    where
        F: Fn(i64) -> Option<Row> + 'static,
    {
        self.row_lookup = Some(Box::new(row_lookup));
        self
    }

    /// Set a callback that reconstructs a row from raw column values, used for
    /// UPDATE reconstruction when the supplied values are available. On a table
    /// with a [`row_lookup`](Self::row_lookup) it is used only when the table opts
    /// out via [`update_from_column_values`](Self::update_from_column_values).
    pub fn row_populator<F>(mut self, row_populator: F) -> Self
    where
        F: for<'a> Fn(&[FunctionArg<'a>]) -> Option<Row> + 'static,
    {
        self.row_populator = Some(Box::new(row_populator));
        self
    }

    /// Opt out of rowid-based UPDATE reconstruction and the `SQLITE_NOCHANGE`
    /// optimization: UPDATE reconstructs the row from the real column values via
    /// [`row_populator`](Self::row_populator), so it sees the new values of its
    /// identity columns and never interprets the rowid. DELETE still resolves
    /// through [`row_lookup`](Self::row_lookup) when the table has one.
    pub fn update_from_column_values(mut self) -> Self {
        self.update_from_column_values = true;
        self
    }

    /// Make the table deletable via a callback that deletes the given row.
    pub fn deletable<F>(mut self, delete_row: F) -> Self
    where
        F: Fn(&Row) -> bool + 'static,
    {
        self.delete_row = Some(Box::new(delete_row));
        self
    }

    /// Make the table insertable via a callback receiving the inserted column
    /// values.
    pub fn insertable<F>(mut self, insert_row: F) -> Self
    where
        F: for<'a> Fn(&[FunctionArg<'a>]) -> bool + 'static,
    {
        self.insert_row = Some(Box::new(insert_row));
        self
    }

    /// Finalize the definition; errors if neither a cache builder nor a count
    /// callback was set, or if [`row_lookup`](Self::row_lookup) is set without
    /// [`rowid`](Self::rowid).
    pub fn build(self) -> Result<CachedTableDef<Row>> {
        if self.cache_builder.is_none()
            && self.projection_cache_builder.is_none()
            && self.stateful_cache_builder.is_none()
            && self.row_count.is_none()
        {
            return Err(Error::Message(format!(
                "cached table '{}' needs a cache builder or count callback",
                self.name
            )));
        }
        if self.indexes.len() > CACHED_MAX_INDEXES {
            return Err(Error::Message(format!(
                "cached table '{}' registers {} indexes; at most {CACHED_MAX_INDEXES} fit its \
                 plan encoding",
                self.name,
                self.indexes.len()
            )));
        }
        // Row identity: every rowid a row_lookup table's scans report must be a
        // KEY. Without rowid() a full scan would report a cache POSITION, and a
        // position read as a key can name a different, existing row.
        if self.row_lookup.is_some() && self.rowid.is_none() {
            return Err(Error::Message(format!(
                "cached table '{}': row_lookup() requires rowid(): scans must report the \
                 same key row_lookup() resolves, not a cache position",
                self.name
            )));
        }
        Ok(CachedTableDef {
            inner: Rc::new(CachedInner {
                name: self.name,
                estimate_rows: self.estimate_rows,
                row_count: self.row_count,
                cache_builder: self.cache_builder,
                projection_cache_builder: self.projection_cache_builder,
                stateful_cache_builder: self.stateful_cache_builder,
                columns: self.columns,
                filters: self.filters,
                constraint_filters: self.constraint_filters,
                indexes: self.indexes,
                rowid: self.rowid,
                before_modify: self.before_modify,
                after_modify: self.after_modify,
                transaction_hooks: self.transaction_hooks,
                counts_as_write: self.counts_as_write,
                delete_row: self.delete_row,
                insert_row: self.insert_row,
                row_lookup: self.row_lookup,
                row_populator: self.row_populator,
                update_from_column_values: self.update_from_column_values,
                use_shared_cache: self.use_shared_cache,
                shared_cache: RefCell::new(SharedCache::default()),
                write_generation: Cell::new(0),
            }),
        })
    }

    fn add_filter<F>(
        mut self,
        column_name: &str,
        op: c_int,
        omit: bool,
        estimated_cost: f64,
        estimated_rows: f64,
        factory: F,
    ) -> Self
    where
        F: for<'a> Fn(&FunctionArg<'a>) -> Box<dyn RowIterator> + 'static,
    {
        if let Some(column_index) = self.find_column(column_name) {
            let filter_id = self.filters.len() as c_int + 1;
            self.filters.push(CachedFilterDef {
                column_index,
                op,
                omit,
                filter_id,
                estimated_cost,
                estimated_rows: estimated_rows.max(1.0) as i64,
                factory: Box::new(factory),
            });
        }
        self
    }

    fn add_column(
        mut self,
        name: impl Into<String>,
        column_type: ColumnType,
        getter: Box<CachedGetter<Row>>,
        setter: Option<Box<CachedSetter<Row>>>,
    ) -> Self {
        self.columns.push(CachedColumnDef {
            name: name.into(),
            column_type,
            getter,
            setter,
        });
        self
    }

    fn find_column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    fn find_index(&self, column_index: usize) -> Option<usize> {
        self.indexes
            .iter()
            .position(|index| index.column_index == column_index)
    }
}

#[repr(C)]
struct CachedModuleState<Row> {
    module: ffi::sqlite3_module,
    def: Rc<CachedInner<Row>>,
    registry: *const RefCell<WriteSurfaceRegistry>,
    caps: WriteCaps,
}

#[repr(C)]
struct CachedVtab<Row> {
    base: ffi::sqlite3_vtab,
    def: Rc<CachedInner<Row>>,
    transaction: TransactionLifecycle,
    registry: *const RefCell<WriteSurfaceRegistry>,
    schema_name: String,
    table_name: String,
}

#[repr(C)]
struct CachedCursor<Row> {
    base: ffi::sqlite3_vtab_cursor,
    def: Rc<CachedInner<Row>>,
    transaction_state: TransactionState,
    local_cache: Vec<Row>,
    /// Set when `local_cache` holds a full-scan build: the projection (colUsed)
    /// and write generation it was built at. Such a build is cursor-lived --
    /// reused across this cursor's xFilter calls (a JOIN's inner loop re-filters
    /// one cursor per outer row) while both still match. `None` whenever
    /// `local_cache` holds scratch rows (a rowid lookup) instead.
    local_full_scan: Option<(u64, u64)>,
    current_row: usize,
    iterator: Option<Box<dyn RowIterator>>,
    iterator_eof: bool,
    index_matches: Option<Vec<usize>>,
    /// When true, `index_matches` index into `local_cache` (a non-shared rowid
    /// rebuild or a single `row_lookup` result) rather than the shared cache.
    index_into_local: bool,
    /// When true, `index_matches` index into `cursor_index_cache` — a query-scoped
    /// per-cursor index cache built ONCE and reused across this cursor's xFilter
    /// calls (a JOIN's inner loop), so a query-scoped table stays fast in a JOIN
    /// without an engine-lifetime shared cache. Mirrors C++ 1a per-cursor index.
    index_into_cursor: bool,
    cursor_index_cache: Vec<Row>,
    cursor_indexes: Vec<HashMap<i64, Vec<usize>>>,
    cursor_index_built: bool,
    /// Write generation `cursor_index_cache` was built at.
    index_generation: u64,
    /// Parallel sorted views for range pushdown on a query-scoped table, one slot
    /// per index, built lazily on the first range query against this cursor and
    /// reused thereafter (like `cursor_indexes`). Dropped with the cursor. The
    /// carved match list is stored back into `index_matches` (with
    /// `index_into_cursor` set), reusing the existing index-match iteration.
    /// Mirrors the C++ `CachedCursor::cursor_sorted_indexes`.
    cursor_sorted_indexes: Vec<SortedIndex>,
    /// The rowid this cursor was opened to look up (a `CACHED_ROWID_SCAN`). The
    /// cursor returns exactly the row with this rowid, so `xRowid` reports it
    /// directly — independent of how the row was resolved (`row_lookup`, shared
    /// cache, or positional rebuild) and of whether a `rowid()` callback exists.
    rowid_lookup_id: Option<i64>,
    count_only_total: Option<usize>,
}

/// Connection-wide invalidation of cached tables (the C++ `cache_registry.hpp`).
///
/// A query-scoped cached table keeps its rows for the life of one SQLite cursor
/// and rebuilds only after a write THROUGH the table. A SQL function that changes
/// the engine behind a table is not such a write and usually holds no handle to
/// it, so this registry keeps, per connection, one invalidator per registered
/// cached module. Running one does exactly what a write through that table does
/// (`invalidate_shared_cache`): a scan already in flight keeps its rows as a
/// snapshot and every later probe or statement rebuilds.
///
/// Connections and definitions are `!Send` (`Rc`), so every module registered
/// on a connection lives on that connection's thread, and so does every SQL
/// function call on it: a thread-local registry is exact.
mod connection_registry {
    use super::{c_void, ffi};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    type Invalidator = Rc<dyn Fn()>;

    thread_local! {
        /// connection -> [(module state, invalidator)]
        static BY_DB: RefCell<HashMap<usize, Vec<(usize, Invalidator)>>> =
            RefCell::new(HashMap::new());
    }

    pub(super) fn register(db: *mut ffi::sqlite3, state: *const c_void, invalidate: Invalidator) {
        let _ = BY_DB.try_with(|map| {
            map.borrow_mut()
                .entry(db as usize)
                .or_default()
                .push((state as usize, invalidate));
        });
    }

    /// Called when SQLite destroys a cached module (connection close, re-register).
    pub(super) fn unregister(state: *const c_void) {
        let state = state as usize;
        // try_with: a module destroyed during thread-local teardown has nothing
        // left to unregister from.
        let _ = BY_DB.try_with(|map| {
            map.borrow_mut().retain(|_, entries| {
                entries.retain(|(entry, _)| *entry != state);
                !entries.is_empty()
            });
        });
    }

    /// Runs every invalidator registered on `db`. They are copied out first and
    /// run with the registry unborrowed, so one may itself reach the registry.
    pub(crate) fn invalidate(db: *mut ffi::sqlite3) {
        if db.is_null() {
            return;
        }
        let invalidators: Vec<Invalidator> = BY_DB
            .try_with(|map| {
                map.borrow()
                    .get(&(db as usize))
                    .map(|entries| entries.iter().map(|(_, f)| f.clone()).collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        for invalidate in invalidators {
            invalidate();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::BY_DB;

        /// Invalidators registered on this thread; each test runs on its own
        /// thread, so the count is exact.
        fn registered() -> usize {
            BY_DB.with(|map| map.borrow().values().map(Vec::len).sum())
        }

        #[test]
        fn registry_drops_closed_connections_and_reregistered_modules() {
            let def = crate::cached_table::<i64>("reg_conn")
                .no_shared_cache()
                .cache_builder(|rows| rows.push(1))
                .column_i64("id", |row| *row)
                .build()
                .expect("definition builds");
            let mut db = crate::Database::open_memory().expect("open");
            assert_eq!(registered(), 0);
            db.register_cached_table_as("reg_conn_mod", &def)
                .expect("register");
            assert_eq!(registered(), 1);
            // Re-registering the module name destroys the old module state.
            db.register_cached_table_as("reg_conn_mod", &def)
                .expect("re-register");
            assert_eq!(registered(), 1, "the replaced module must drop its entry");
            db.register_cached_table_as("reg_conn_mod2", &def)
                .expect("second module");
            assert_eq!(registered(), 2);
            db.invalidate_cached_tables();
            db.close();
            assert_eq!(registered(), 0, "closing must drop every entry");
            db.invalidate_cached_tables(); // a closed database is a no-op
        }
    }
}

pub(crate) use connection_registry::invalidate as invalidate_connection_cached_tables;

pub(crate) fn register_cached_table<Row: 'static>(
    db: *mut ffi::sqlite3,
    registry: *const RefCell<WriteSurfaceRegistry>,
    module_name: &str,
    def: &CachedTableDef<Row>,
) -> Result<()> {
    if db.is_null() {
        return Err(Error::DatabaseNotOpen);
    }
    let module_name = CString::new(module_name)?;
    let state = Box::new(CachedModuleState {
        module: create_cached_module::<Row>(),
        def: def.inner.clone(),
        registry,
        caps: def.write_caps(),
    });
    let invalidated = Rc::downgrade(&state.def);
    let state_ptr = Box::into_raw(state);
    let module_ptr = unsafe { &(*state_ptr).module as *const ffi::sqlite3_module };
    let rc = unsafe {
        ffi::sqlite3_create_module_v2(
            db,
            module_name.as_ptr(),
            module_ptr,
            state_ptr.cast::<c_void>(),
            Some(destroy_cached_module_state::<Row>),
        )
    };
    if sqlite_ok(rc) {
        // Let invalidate_cached_tables(db) reach this table -- what a SQL function
        // that mutates the engine calls, since it holds no table handle.
        // destroy_cached_module_state removes the entry when SQLite drops the module.
        connection_registry::register(
            db,
            state_ptr.cast_const().cast::<c_void>(),
            Rc::new(move || {
                if let Some(def) = invalidated.upgrade() {
                    def.invalidate_shared_cache();
                }
            }),
        );
        Ok(())
    } else {
        Err(Error::sqlite(rc, crate::function::sqlite_error(db)))
    }
}

fn create_cached_module<Row: 'static>() -> ffi::sqlite3_module {
    let mut module: ffi::sqlite3_module = unsafe { std::mem::zeroed() };
    module.iVersion = 3;
    module.xCreate = Some(cached_vtab_connect::<Row>);
    module.xConnect = Some(cached_vtab_connect::<Row>);
    module.xBestIndex = Some(cached_vtab_best_index::<Row>);
    module.xDisconnect = Some(cached_vtab_disconnect::<Row>);
    module.xDestroy = Some(cached_vtab_destroy::<Row>);
    module.xOpen = Some(cached_vtab_open::<Row>);
    module.xClose = Some(cached_vtab_close::<Row>);
    module.xFilter = Some(cached_vtab_filter::<Row>);
    module.xNext = Some(cached_vtab_next::<Row>);
    module.xEof = Some(cached_vtab_eof::<Row>);
    module.xColumn = Some(cached_vtab_column::<Row>);
    module.xRowid = Some(cached_vtab_rowid::<Row>);
    module.xUpdate = Some(cached_vtab_update::<Row>);
    // xBegin enrolls the vtab. Run the fallible hook in xSync, whose error
    // SQLite propagates; xCommit only clears state because its result is ignored.
    module.xBegin = Some(cached_vtab_begin::<Row>);
    module.xSync = Some(cached_vtab_sync::<Row>);
    module.xCommit = Some(cached_vtab_commit::<Row>);
    module.xRollback = Some(cached_vtab_rollback::<Row>);
    module.xSavepoint = Some(cached_vtab_savepoint::<Row>);
    module.xRelease = Some(cached_vtab_release::<Row>);
    module.xRollbackTo = Some(cached_vtab_rollback_to::<Row>);
    module
}

unsafe extern "C" fn destroy_cached_module_state<Row>(ptr: *mut c_void) {
    connection_registry::unregister(ptr.cast_const());
    unsafe {
        if !ptr.is_null() {
            drop(Box::from_raw(ptr.cast::<CachedModuleState<Row>>()));
        }
    }
}

unsafe extern "C" fn cached_vtab_connect<Row>(
    db: *mut ffi::sqlite3,
    p_aux: *mut c_void,
    argc: c_int,
    argv: *const *const c_char,
    pp_vtab: *mut *mut ffi::sqlite3_vtab,
    pz_err: *mut *mut c_char,
) -> c_int {
    unsafe {
        callback_status(pz_err, || {
            let state = p_aux
                .cast::<CachedModuleState<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cached module state is null".to_string()))?;
            let schema = CString::new(state.def.schema())?;
            let rc = ffi::sqlite3_declare_vtab(db, schema.as_ptr());
            if !sqlite_ok(rc) {
                return Err(Error::sqlite(rc, crate::function::sqlite_error(db)));
            }
            let transaction =
                TransactionLifecycle::new(&state.def.transaction_hooks)?.with_write_counter(
                    table_write_counter(state.registry, state.def.counts_as_write),
                );
            let (schema_name, table_name) =
                connect_write_surface(state.registry, argc, argv, state.caps.clone())?;
            let vtab = Box::new(CachedVtab {
                base: std::mem::zeroed(),
                def: state.def.clone(),
                transaction,
                registry: state.registry,
                schema_name,
                table_name,
            });
            let vtab_ptr = Box::into_raw(vtab);
            *pp_vtab = &mut (*vtab_ptr).base;
            Ok(ffi::SQLITE_OK)
        })
    }
}

unsafe extern "C" fn cached_vtab_destroy<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if !p_vtab.is_null() {
            let vtab = Box::from_raw(p_vtab.cast::<CachedVtab<Row>>());
            destroy_write_surface(vtab.registry, &vtab.schema_name, &vtab.table_name);
            drop(vtab);
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn cached_vtab_disconnect<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if !p_vtab.is_null() {
            drop(Box::from_raw(p_vtab.cast::<CachedVtab<Row>>()));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn cached_vtab_open<Row>(
    p_vtab: *mut ffi::sqlite3_vtab,
    pp_cursor: *mut *mut ffi::sqlite3_vtab_cursor,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<CachedVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cached vtab is null".to_string()))?;
            let cursor = Box::new(CachedCursor {
                base: std::mem::zeroed(),
                def: vtab.def.clone(),
                transaction_state: vtab.transaction.state(),
                local_cache: Vec::new(),
                local_full_scan: None,
                current_row: 0,
                iterator: None,
                iterator_eof: false,
                index_matches: None,
                index_into_local: false,
                index_into_cursor: false,
                cursor_index_cache: Vec::new(),
                cursor_indexes: Vec::new(),
                cursor_index_built: false,
                index_generation: 0,
                cursor_sorted_indexes: Vec::new(),
                rowid_lookup_id: None,
                count_only_total: None,
            });
            let cursor_ptr = Box::into_raw(cursor);
            *pp_cursor = &mut (*cursor_ptr).base;
            Ok::<_, Error>(())
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn cached_vtab_close<Row>(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        if !cursor.is_null() {
            drop(Box::from_raw(cursor.cast::<CachedCursor<Row>>()));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn cached_vtab_filter<Row: 'static>(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    idx_num: c_int,
    idx_str: *const c_char,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let cursor = cursor
                .cast::<CachedCursor<Row>>()
                .as_mut()
                .ok_or_else(|| Error::Message("xsql cached cursor is null".to_string()))?;
            // `local_cache` (when it holds a full-scan build) and the
            // cursor_index_* caches are cursor-lived: kept across this cursor's
            // xFilter calls and dropped only by a write through the table.
            let generation = cursor.def.write_generation.get();
            if cursor.cursor_index_built && cursor.index_generation != generation {
                cursor.cursor_index_cache.clear();
                cursor.cursor_indexes.clear();
                cursor.cursor_sorted_indexes.clear();
                cursor.cursor_index_built = false;
            }
            cursor.current_row = 0;
            cursor.iterator = None;
            cursor.iterator_eof = false;
            cursor.index_matches = None;
            cursor.index_into_local = false;
            cursor.index_into_cursor = false; // cursor_index_* persist (build-once)
            cursor.rowid_lookup_id = None;
            cursor.count_only_total = None;
            // A new scan begins a new statement: drop any query-scoped mutation
            // snapshot preserved from a prior scan-driven UPDATE/DELETE.
            cursor.def.clear_query_scoped_mutation_snapshot();

            if idx_num == CACHED_COUNT_ONLY_SCAN {
                cursor.count_only_total = Some(
                    cursor
                        .def
                        .row_count
                        .as_ref()
                        .map(|count| count())
                        .unwrap_or(0),
                );
                return Ok(());
            }

            if idx_num == CACHED_ROWID_SCAN && argc > 0 {
                // omit=1: only a value an integer rowid can equal finds a row
                // (`rowid = 1.5`, `rowid = 'a'` and `rowid = NULL` must not read
                // as 1, 0 and 0).
                let Some(target_rowid) = index_key::exact_i64(&FunctionArg::new(*argv)) else {
                    cursor.index_matches = Some(Vec::new());
                    return Ok(());
                };
                // This cursor yields the row with exactly this rowid; record it so
                // xRowid reports it regardless of how the row is resolved.
                cursor.rowid_lookup_id = Some(target_rowid);
                // `row_lookup` is the sole authority for a table that has one (a
                // single-row result, no cache build, any sign -- a key may be an
                // address >= 2^63): a key it cannot resolve is no row, never a
                // cache position to retry.
                if let Some(row_lookup) = cursor.def.row_lookup.as_ref() {
                    cursor.local_full_scan = None;
                    cursor.local_cache.clear();
                    cursor.local_cache.extend(row_lookup(target_rowid));
                    cursor.index_matches = Some((0..cursor.local_cache.len()).collect());
                    cursor.index_into_local = true;
                    return Ok(());
                }
                // Otherwise match the materialization: by KEY when the table has a
                // `rowid()` (any sign), else by position -- the shared cache, or a
                // freshly built local cache for a non-shared table.
                if cursor.def.use_shared_cache {
                    cursor.def.ensure_shared_cache();
                    let cache = cursor.def.shared_cache.borrow();
                    cursor.index_matches =
                        Some(cached_rowid_matches(&cursor.def, &cache.data, target_rowid));
                } else {
                    let def = cursor.def.clone();
                    cursor.local_full_scan = None;
                    def.build_rows(
                        &cursor.transaction_state,
                        &mut cursor.local_cache,
                        parse_col_used(idx_str),
                    );
                    cursor.index_matches = Some(cached_rowid_matches(
                        &def,
                        &cursor.local_cache,
                        target_rowid,
                    ));
                    cursor.index_into_local = true;
                }
                return Ok(());
            }

            // Multi-column/operator constraint filter. Tested BEFORE the range
            // decode, whose open-ended encoding would swallow this idxNum.
            if idx_num >= CACHED_CONSTRAINT_FILTER_BASE
                && let Some(filter) = cursor
                    .def
                    .constraint_filters
                    .iter()
                    .find(|filter| filter.filter_id == idx_num)
            {
                let def = cursor.def.clone();
                let spec_list = c_string_to_string(idx_str).unwrap_or_default();
                let spec_indices = resolve_constraint_specs(
                    &spec_list,
                    filter.specs.len(),
                    argc.max(0) as usize,
                    &def.name,
                )
                .map_err(Error::Message)?;
                let args = spec_indices
                    .into_iter()
                    .enumerate()
                    .map(|(arg_index, spec_index)| {
                        let spec = &filter.specs[spec_index];
                        GeneratorConstraintArg {
                            column_index: spec.column_index,
                            op: spec.op,
                            value: FunctionArg::new(*argv.add(arg_index)),
                        }
                    })
                    .collect::<Vec<_>>();

                // Factory-backed: the source resolves the predicate exactly and
                // streams only matching rows; nothing is materialized here.
                if let Some(factory) = filter.factory.as_ref() {
                    let mut iterator = factory(&args);
                    cursor.iterator_eof = !iterator.next()?;
                    cursor.iterator = Some(iterator);
                    return Ok(());
                }

                // Each column has an index_on key (checked when the filter was
                // built); the operands get the column's INTEGER affinity once.
                let tests = args
                    .iter()
                    .filter_map(|arg| {
                        def.find_index_by_column(arg.column_index)
                            .map(|pos| (pos, arg.op, key_operand(&arg.value)))
                    })
                    .collect::<Vec<_>>();

                // Cache-backed: evaluate in memory over the table's own
                // materialization -- the shared cache (built once for the engine
                // lifetime) or this cursor's rows (built once per cursor, reused
                // by a JOIN's inner loop, dropped by a write through the table).
                // Matches are positions, like the equality and range arms.
                if def.use_shared_cache {
                    def.ensure_shared_cache();
                    let cache = def.shared_cache.borrow();
                    let matches = cache
                        .data
                        .iter()
                        .enumerate()
                        .filter(|(_, row)| def.row_matches_constraints(row, &tests))
                        .map(|(position, _)| position)
                        .collect();
                    cursor.index_matches = Some(matches);
                } else {
                    cursor.ensure_cursor_index_cache();
                    let matches = cursor
                        .cursor_index_cache
                        .iter()
                        .enumerate()
                        .filter(|(_, row)| def.row_matches_constraints(row, &tests))
                        .map(|(position, _)| position)
                        .collect();
                    cursor.index_matches = Some(matches);
                    cursor.index_into_cursor = true;
                }
                return Ok(());
            }

            // Range (>=, >, <=, <, BETWEEN) plan on an indexed column. The range
            // base sits above CACHED_INDEX_BASE, so this must be tested BEFORE the
            // equality-index branch below. The sorted view is built lazily here
            // and cached (shared cache for shared tables, per-cursor otherwise).
            if let Some((index_pos, range_flags)) = decode_range_idx(idx_num) {
                let range_def = cursor.def.clone();
                let index_defs = &range_def.indexes;
                if index_pos < index_defs.len() {
                    // Decode bounds from argv: low first (if present), then high.
                    // Each becomes an inclusive integer bound under SQLite's
                    // comparison rules (the constraints are omitted, so a truncated
                    // 14.5 or a text bound read as 0 would return wrong rows).
                    let mut next_arg = 0;
                    let mut operand = || {
                        let value = if next_arg < argc {
                            key_operand(&FunctionArg::new(*argv.offset(next_arg as isize)))
                        } else {
                            KeyOperand::Null
                        };
                        next_arg += 1;
                        value
                    };
                    let mut key_flags = 0;
                    let (mut low, mut high) = (0i64, 0i64);
                    let mut empty = false;
                    if range_flags & RANGE_HAS_LOW != 0 {
                        match lower_bound(operand(), range_flags & RANGE_LOW_STRICT != 0) {
                            KeyBound::Empty => empty = true,
                            KeyBound::Unbounded => {}
                            KeyBound::Inclusive(bound) => {
                                key_flags |= RANGE_HAS_LOW;
                                low = bound;
                            }
                        }
                    }
                    if range_flags & RANGE_HAS_HIGH != 0 {
                        match upper_bound(operand(), range_flags & RANGE_HIGH_STRICT != 0) {
                            KeyBound::Empty => empty = true,
                            KeyBound::Unbounded => {}
                            KeyBound::Inclusive(bound) => {
                                key_flags |= RANGE_HAS_HIGH;
                                high = bound;
                            }
                        }
                    }
                    if empty {
                        cursor.index_matches = Some(Vec::new());
                        return Ok(());
                    }
                    if cursor.def.use_shared_cache {
                        cursor.def.ensure_shared_cache();
                        let mut cache = cursor.def.shared_cache.borrow_mut();
                        if cache.built && index_pos < cache.sorted_indexes.len() {
                            if !cache.sorted_indexes[index_pos].built {
                                // Build into a fresh view (reads `data`), then move
                                // it into the slot — avoids overlapping borrows.
                                let mut sorted = SortedIndex::default();
                                sorted.build(&cache.data, &index_defs[index_pos].key);
                                cache.sorted_indexes[index_pos] = sorted;
                            }
                            let mut matches = Vec::new();
                            cache.sorted_indexes[index_pos].collect_matches(
                                key_flags,
                                low,
                                high,
                                &mut matches,
                            );
                            cursor.index_matches = Some(matches);
                        } else {
                            cursor.index_matches = Some(Vec::new());
                        }
                        return Ok(());
                    } else if cursor.def.has_cache_builder() {
                        // Query-scoped: build the per-cursor cache once (reused
                        // across this cursor's xFilter calls), then the sorted
                        // view on demand.
                        cursor.ensure_cursor_index_cache();
                        if cursor.cursor_sorted_indexes.len() != index_defs.len() {
                            cursor.cursor_sorted_indexes.clear();
                            cursor
                                .cursor_sorted_indexes
                                .resize_with(index_defs.len(), SortedIndex::default);
                        }
                        if !cursor.cursor_sorted_indexes[index_pos].built {
                            cursor.cursor_sorted_indexes[index_pos]
                                .build(&cursor.cursor_index_cache, &index_defs[index_pos].key);
                        }
                        let mut matches = Vec::new();
                        cursor.cursor_sorted_indexes[index_pos].collect_matches(
                            key_flags,
                            low,
                            high,
                            &mut matches,
                        );
                        cursor.index_matches = Some(matches);
                        cursor.index_into_cursor = true;
                        return Ok(());
                    }
                }
                // Not resolvable as a range plan; fall through to a full scan below.
            }

            if idx_num >= CACHED_INDEX_BASE && argc > 0 {
                let index_pos = (idx_num - CACHED_INDEX_BASE) as usize;
                // Only an integer exactly equal to the value can match (a 10.5,
                // non-numeric text or NULL matches nothing; '10' is 10).
                let key = exact_key(key_operand(&FunctionArg::new(*argv)));
                if cursor.def.use_shared_cache {
                    cursor.def.ensure_shared_cache();
                    let cache = cursor.def.shared_cache.borrow();
                    // Own the matched positions rather than referencing the shared
                    // cache: the borrow is released when xFilter returns, and a
                    // mutation may rebuild the cache before the scan reads them, so a
                    // borrowed reference would go stale. The clone keeps this scan's
                    // snapshot consistent.
                    let matches = cache
                        .indexes
                        .get(index_pos)
                        .zip(key)
                        .and_then(|(index, key)| index.get(&key))
                        .cloned()
                        .unwrap_or_default();
                    cursor.index_matches = Some(matches);
                } else {
                    // Query-scoped: build the per-cursor cache + hash indexes ONCE,
                    // then reuse across this cursor's xFilter calls (JOIN inner loop).
                    cursor.ensure_cursor_index_cache();
                    let matches = cursor
                        .cursor_indexes
                        .get(index_pos)
                        .zip(key)
                        .and_then(|(index, key)| index.get(&key))
                        .cloned()
                        .unwrap_or_default();
                    cursor.index_matches = Some(matches);
                    cursor.index_into_cursor = true;
                }
                return Ok(());
            }

            if idx_num != FILTER_NONE
                && argc > 0
                && let Some(filter) = cursor.def.find_filter_by_id(idx_num)
            {
                let arg = FunctionArg::new(*argv);
                let mut iterator = (filter.factory)(&arg);
                cursor.iterator_eof = !iterator.next()?;
                cursor.iterator = Some(iterator);
                return Ok(());
            }

            if cursor.def.use_shared_cache {
                cursor.def.ensure_shared_cache();
            } else {
                // Reuse this cursor's rows when nothing could have changed them:
                // same projection, no write through the table since the build.
                // Without this a nested-loop JOIN rebuilt the whole table once per
                // OUTER row.
                let col_used = parse_col_used(idx_str);
                let key = (col_used, generation);
                if cursor.local_full_scan != Some(key) {
                    let def = cursor.def.clone();
                    cursor.local_full_scan = None;
                    def.build_rows(&cursor.transaction_state, &mut cursor.local_cache, col_used);
                    cursor.local_full_scan = Some(key);
                }
            }
            Ok(())
        }));
        finish_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn cached_vtab_next<Row>(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let cursor = cursor
                .cast::<CachedCursor<Row>>()
                .as_mut()
                .ok_or_else(|| Error::Message("xsql cached cursor is null".to_string()))?;
            if let Some(iterator) = cursor.iterator.as_mut() {
                cursor.iterator_eof = !iterator.next()?;
            } else {
                cursor.current_row = cursor.current_row.saturating_add(1);
            }
            Ok(())
        }));
        finish_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn cached_vtab_eof<Row>(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        let Some(cursor) = cursor.cast::<CachedCursor<Row>>().as_ref() else {
            return 1;
        };
        if let Some(iterator) = cursor.iterator.as_ref() {
            return i32::from(cursor.iterator_eof || iterator.eof());
        }
        if let Some(total) = cursor.count_only_total {
            return i32::from(cursor.current_row >= total);
        }
        if let Some(matches) = cursor.index_matches.as_ref() {
            return i32::from(cursor.current_row >= matches.len());
        }
        if cursor.def.use_shared_cache {
            let cache = cursor.def.shared_cache.borrow();
            return i32::from(cursor.current_row >= cache.data.len());
        }
        i32::from(cursor.current_row >= cursor.local_cache.len())
    }
}

unsafe extern "C" fn cached_vtab_column<Row>(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    ctx: *mut ffi::sqlite3_context,
    col: c_int,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let cursor = cursor
                .cast::<CachedCursor<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cached cursor is null".to_string()))?;
            // During UPDATE, SQLite asks for unchanged column values. Returning
            // without a value marks the column SQLITE_NOCHANGE in xUpdate, but only
            // when xUpdate reconstructs the row by rowid alone (see
            // cached_reconstruct_by_rowid, the one predicate both sides share).
            if sqlite3_vtab_nochange(ctx) != 0 && cached_reconstruct_by_rowid(&cursor.def) {
                return Ok(());
            }
            let mut fctx = FunctionContext::new(ctx);
            if col < 0
                || col as usize >= cursor.def.columns.len()
                || cursor.count_only_total.is_some()
            {
                fctx.result_null();
                return Ok(());
            }
            if let Some(iterator) = cursor.iterator.as_ref() {
                if cursor.iterator_eof {
                    fctx.result_null();
                } else {
                    iterator.column(&mut fctx, col)?;
                }
                return Ok(());
            }
            let getter = &cursor.def.columns[col as usize].getter;
            if cursor
                .with_current_row(|row| getter(&mut fctx, row))
                .is_none()
            {
                fctx.result_null();
            }
            Ok(())
        }));
        match result {
            Ok(Ok(())) => ffi::SQLITE_OK,
            Ok(Err(err)) => {
                FunctionContext::new(ctx).result_error(err.to_string());
                ffi::SQLITE_ERROR
            }
            Err(_) => {
                FunctionContext::new(ctx).result_error("xsql cached table column panicked");
                ffi::SQLITE_ERROR
            }
        }
    }
}

unsafe extern "C" fn cached_vtab_rowid<Row>(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let cursor = cursor
                .cast::<CachedCursor<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cached cursor is null".to_string()))?;
            if let Some(iterator) = cursor.iterator.as_ref() {
                *rowid = if cursor.iterator_eof {
                    0
                } else {
                    iterator.rowid()
                };
                return Ok(());
            }
            if let Some(rowid_fn) = cursor.def.rowid.as_ref()
                && let Some(stable_rowid) = cursor.with_current_row(|row| rowid_fn(row))
            {
                *rowid = stable_rowid;
                return Ok(());
            }
            // A rowid-lookup cursor yields the row with the requested rowid; report it
            // directly (a positional table's match, which has no rowid_fn).
            if let Some(target) = cursor.rowid_lookup_id {
                *rowid = target;
                return Ok(());
            }
            // On the index lookup path with no stable rowid_fn, the rowid is the
            // matched row's position in the cache, not the cursor's offset into the
            // match list.
            if let Some(matches) = cursor.index_matches.as_ref()
                && let Some(row_index) = matches.get(cursor.current_row)
            {
                *rowid = *row_index as i64;
                return Ok(());
            }
            *rowid = cursor.current_row as i64;
            Ok(())
        }));
        finish_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn cached_vtab_update<Row: 'static>(
    p_vtab: *mut ffi::sqlite3_vtab,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
    rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<c_int> {
            let vtab = p_vtab
                .cast::<CachedVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cached vtab is null".to_string()))?;
            if argc <= 0 || argv.is_null() {
                return Ok(ffi::SQLITE_READONLY);
            }

            let old_rowid = FunctionArg::new(*argv);
            if argc == 1 && !old_rowid.is_null() {
                let Some(delete_row) = vtab.def.delete_row.as_ref() else {
                    return Ok(unsupported_delete(p_vtab, &vtab.def.name));
                };
                let operation = format!("DELETE FROM {}", vtab.def.name);
                // Snapshot before the before_modify hook: a failed snapshot
                // aborts the statement before any observable side effect (e.g.
                // an undo point). Idempotent — the reconstruct helper's own
                // call becomes a no-op.
                let transaction_state = vtab.transaction.state();
                vtab.def
                    .ensure_query_scoped_mutation_snapshot(&transaction_state)?;
                vtab.transaction.touch();
                call_cached_modify_hook(&vtab.def.before_modify, &operation);
                cached_delete_reconstruct(
                    &vtab.def,
                    &transaction_state,
                    old_rowid.as_i64(),
                    delete_row,
                )?;
                vtab.def.invalidate_shared_cache();
                call_cached_modify_hook(&vtab.def.after_modify, &operation);
                vtab.transaction.mark_written();
                return Ok(ffi::SQLITE_OK);
            }

            if argc > 1 && !old_rowid.is_null() {
                if !vtab
                    .def
                    .columns
                    .iter()
                    .any(|column| column.setter.is_some())
                {
                    return Ok(unsupported_update(p_vtab, &vtab.def.name));
                }
                let args = build_args(argc - 2, argv.add(2));
                let operation = format!("UPDATE {}", vtab.def.name);
                // Snapshot before the before_modify hook — see the DELETE arm.
                let transaction_state = vtab.transaction.state();
                vtab.def
                    .ensure_query_scoped_mutation_snapshot(&transaction_state)?;
                vtab.transaction.touch();
                call_cached_modify_hook(&vtab.def.before_modify, &operation);

                let updated = cached_update_reconstruct_and_apply(
                    &vtab.def,
                    &transaction_state,
                    old_rowid.as_i64(),
                    &args,
                )?;
                if updated {
                    vtab.def.invalidate_shared_cache();
                    call_cached_modify_hook(&vtab.def.after_modify, &operation);
                    vtab.transaction.mark_written();
                    return Ok(ffi::SQLITE_OK);
                }
                // No reconstruction branch applies (e.g. a stable rowid_fn on a shared
                // cache with no row_lookup/row_populator) — read-only, per C++. (A
                // row_lookup table never gets here: a miss is an error.)
                return Ok(ffi::SQLITE_READONLY);
            }

            if argc > 1 && old_rowid.is_null() {
                let Some(insert_row) = vtab.def.insert_row.as_ref() else {
                    return Ok(unsupported_insert(p_vtab, &vtab.def.name));
                };
                let operation = format!("INSERT INTO {}", vtab.def.name);
                vtab.transaction.touch();
                call_cached_modify_hook(&vtab.def.before_modify, &operation);
                let args = build_args(argc - 2, argv.add(2));
                begin_vtab_callback();
                if insert_row(&args) {
                    if !rowid.is_null() {
                        *rowid = 0;
                    }
                    vtab.def.clear_shared_cache();
                    call_cached_modify_hook(&vtab.def.after_modify, &operation);
                    vtab.transaction.mark_written();
                    return Ok(ffi::SQLITE_OK);
                }
                return Err(vtab_callback_failure("cached table insert failed"));
            }

            Ok(ffi::SQLITE_READONLY)
        }));
        match result {
            Ok(Ok(status)) => status,
            Ok(Err(err)) => {
                set_vtab_error(p_vtab, &err.to_string());
                ffi::SQLITE_ERROR
            }
            Err(_) => {
                set_vtab_error(p_vtab, "xsql cached table update panicked");
                ffi::SQLITE_ERROR
            }
        }
    }
}

unsafe extern "C" fn cached_vtab_begin<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if let Some(vtab) = p_vtab.cast::<CachedVtab<Row>>().as_ref() {
            vtab.transaction.begin();
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn cached_vtab_sync<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<CachedVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cached vtab is null".to_string()))?;
            vtab.transaction.sync(&vtab.def.transaction_hooks)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn cached_vtab_commit<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if let Some(vtab) = p_vtab.cast::<CachedVtab<Row>>().as_ref() {
            // SQLite ignores xCommit/xRollback errors. Contain a panic without
            // leaving an ignored allocation in zErrMsg.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                vtab.transaction.commit(&vtab.def.transaction_hooks);
            }));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn cached_vtab_rollback<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if let Some(vtab) = p_vtab.cast::<CachedVtab<Row>>().as_ref() {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                vtab.transaction.rollback(&vtab.def.transaction_hooks);
            }));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn cached_vtab_savepoint<Row>(
    p_vtab: *mut ffi::sqlite3_vtab,
    id: c_int,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<CachedVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cached vtab is null".to_string()))?;
            vtab.transaction.savepoint(&vtab.def.transaction_hooks, id)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn cached_vtab_release<Row>(p_vtab: *mut ffi::sqlite3_vtab, id: c_int) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<CachedVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cached vtab is null".to_string()))?;
            vtab.transaction.release(&vtab.def.transaction_hooks, id)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn cached_vtab_rollback_to<Row>(
    p_vtab: *mut ffi::sqlite3_vtab,
    id: c_int,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<CachedVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cached vtab is null".to_string()))?;
            vtab.transaction
                .rollback_to(&vtab.def.transaction_hooks, id)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn cached_vtab_best_index<Row>(
    p_vtab: *mut ffi::sqlite3_vtab,
    index_info: *mut ffi::sqlite3_index_info,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let vtab = p_vtab
                .cast::<CachedVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cached vtab is null".to_string()))?;
            let info = index_info
                .as_mut()
                .ok_or_else(|| Error::Message("sqlite index_info is null".to_string()))?;
            if vtab.def.projection_cache_builder.is_some() && !vtab.def.use_shared_cache {
                let idx_str = sqlite_mprintf_string(&info.colUsed.to_string());
                if !idx_str.is_null() {
                    info.idxStr = idx_str;
                    info.needToFreeIdxStr = 1;
                }
            }
            let mut best_filter: Option<&CachedFilterDef> = None;
            let mut best_filter_constraint = -1;
            let mut best_index_pos: Option<usize> = None;
            let mut best_index_constraint = -1;
            let mut best_rowid_constraint = -1;
            // Cost-tracked filter selection mirroring the C++ planner; the hash
            // index is tracked independently and ranked against the best filter at
            // plan selection (shared cache: index wins; query-scoped: filter wins).
            let mut best_cost = 1.0e9_f64;

            // Range-pushdown candidate: the lower/upper bound constraints that
            // target a single indexed column. SQLite delivers `x BETWEEN a AND b`
            // as a GE + an LE constraint on the same column, so a lower and an
            // upper bound on the SAME indexed column are consumed together (two
            // argv slots). A range plan is only chosen when NO equality
            // index/filter won (equality is strictly cheaper). A range is only
            // served off a sorted view, which needs the cache: shared tables or
            // query-scoped tables with a cache_builder.
            let range_eligible = vtab.def.use_shared_cache || vtab.def.has_cache_builder();
            let mut range_index_pos: Option<usize> = None;
            let mut range_low_constraint = -1;
            let mut range_high_constraint = -1;
            let mut range_low_strict = false; // GT (>) vs GE (>=)
            let mut range_high_strict = false; // LT (<) vs LE (<=)

            // The cheapest constraint filter whose required specs are all present;
            // ties go to the one consuming more constraints (each is a predicate
            // SQLite no longer re-checks). It then competes by cost below.
            let mut best_constraint_filter: Option<(&CachedConstraintFilterDef, Vec<c_int>)> = None;
            let mut best_constraint_matched = 0;
            for filter in &vtab.def.constraint_filters {
                let mut matched = vec![-1; filter.specs.len()];
                let mut matched_count = 0;
                let mut all_required = true;
                for (spec_index, spec) in filter.specs.iter().enumerate() {
                    let found = (0..info.nConstraint).find(|&i| {
                        let constraint = &*info.aConstraint.add(i as usize);
                        constraint.usable != 0
                            && super::ffi::binary_constraint(info, i)
                            && constraint.iColumn >= 0
                            && constraint.iColumn as usize == spec.column_index
                            && constraint.op as c_int == spec.op.sqlite_op()
                    });
                    if let Some(i) = found {
                        matched[spec_index] = i;
                        matched_count += 1;
                    } else if spec.required {
                        all_required = false;
                        break;
                    }
                }
                let better = best_constraint_filter.as_ref().is_none_or(|(best, _)| {
                    filter.estimated_cost < best.estimated_cost
                        || (filter.estimated_cost == best.estimated_cost
                            && matched_count > best_constraint_matched)
                });
                if all_required && matched_count > 0 && better {
                    best_constraint_filter = Some((filter, matched));
                    best_constraint_matched = matched_count;
                }
            }

            for i in 0..info.nConstraint {
                let constraint = &*info.aConstraint.add(i as usize);
                if constraint.usable == 0 || !super::ffi::binary_constraint(info, i) {
                    continue;
                }
                let op = constraint.op as c_int;
                let is_eq = op == ffi::SQLITE_INDEX_CONSTRAINT_EQ;
                let is_like = op == ffi::SQLITE_INDEX_CONSTRAINT_LIKE
                    || op == ffi::SQLITE_INDEX_CONSTRAINT_GLOB;
                let is_low =
                    op == ffi::SQLITE_INDEX_CONSTRAINT_GE || op == ffi::SQLITE_INDEX_CONSTRAINT_GT;
                let is_high =
                    op == ffi::SQLITE_INDEX_CONSTRAINT_LE || op == ffi::SQLITE_INDEX_CONSTRAINT_LT;

                // Range (>=, >, <=, <) on an indexed column: record the tightest
                // bound(s) for a single indexed column. Only pursued if no
                // equality lookup wins (chosen at plan selection below).
                if (is_low || is_high) && range_eligible && constraint.iColumn >= 0 {
                    let column_index = constraint.iColumn as usize;
                    if let Some(idx_pos) = vtab.def.find_index_by_column(column_index)
                        && range_index_pos.is_none_or(|pos| pos == idx_pos)
                    {
                        range_index_pos = Some(idx_pos);
                        if is_low && range_low_constraint < 0 {
                            range_low_constraint = i;
                            range_low_strict = op == ffi::SQLITE_INDEX_CONSTRAINT_GT;
                        } else if is_high && range_high_constraint < 0 {
                            range_high_constraint = i;
                            range_high_strict = op == ffi::SQLITE_INDEX_CONSTRAINT_LT;
                        }
                    }
                    continue;
                }

                if !is_eq && !is_like {
                    continue;
                }
                if constraint.iColumn < 0 {
                    // rowid is addressable only via equality; chosen for any cached
                    // table (shared or not) and resolved positionally at filter time.
                    if is_eq {
                        best_rowid_constraint = i;
                    }
                    continue;
                }
                let column_index = constraint.iColumn as usize;
                // Only route a LIKE/GLOB constraint to a prefix filter when the
                // pattern has a usable literal prefix; a leading-wildcard pattern
                // would force the iterator to return everything, so leave it to the
                // full scan (which re-applies the real pattern). Matches the C++
                // like_constraint_has_usable_prefix gate.
                if (!is_like || like_constraint_has_usable_prefix(info, i))
                    && let Some(filter) = vtab.def.find_filter_by_column(column_index, op)
                    && filter.estimated_cost < best_cost
                {
                    best_filter = Some(filter);
                    best_filter_constraint = i;
                    best_cost = filter.estimated_cost;
                }
                // Hash index (equality only). Query-scoped tables also use it:
                // xFilter builds the hash index on a per-cursor cache (needs a
                // cache_builder to (re)build).
                if is_eq
                    && (vtab.def.use_shared_cache || vtab.def.has_cache_builder())
                    && let Some(index_pos) = vtab.def.find_index_by_column(column_index)
                {
                    if vtab.def.use_shared_cache {
                        // Hash lookup on the already-built shared cache is
                        // near-free: compete by cost, displacing a costlier filter.
                        let index_cost = 1.0;
                        if index_cost < best_cost {
                            best_index_pos = Some(index_pos);
                            best_index_constraint = i;
                            best_cost = index_cost;
                            best_filter = None;
                        }
                    } else if best_index_pos.is_none() {
                        // Query-scoped: the index scan must first BUILD the
                        // per-cursor cache (a full engine read), so it never
                        // outranks an explicit filter (a targeted engine read).
                        // Recorded as the fallback that beats a full scan; chosen
                        // below only when no filter matched.
                        best_index_pos = Some(index_pos);
                        best_index_constraint = i;
                    }
                }
            }

            // Plan selection priority mirrors the reference planner:
            // count-only -> rowid -> hash index -> explicit filter -> full scan. The
            // full-scan/count-only estimate uses estimate_rows (default 1000).
            let estimated_rows = vtab
                .def
                .estimate_rows
                .as_ref()
                .map(|estimate| estimate())
                .unwrap_or(1_000) as i64;

            if info.nConstraint == 0
                && info.colUsed == 0
                && vtab.def.row_count.is_some()
                && vtab.def.rowid.is_none()
                && !vtab.def.has_scan_driven_mutation()
            {
                info.idxNum = CACHED_COUNT_ONLY_SCAN;
                info.estimatedCost = estimated_rows as f64;
                info.estimatedRows = estimated_rows;
                return Ok(());
            }

            if best_rowid_constraint >= 0 {
                let usage = &mut *info.aConstraintUsage.add(best_rowid_constraint as usize);
                usage.argvIndex = 1;
                usage.omit = 1;
                info.idxNum = CACHED_ROWID_SCAN;
                info.estimatedCost = 1.0;
                info.estimatedRows = 1;
                return Ok(());
            }

            // A constraint filter must be strictly cheaper than the best
            // single-column plan (an equality filter, or a shared hash index at 1.0).
            if let Some((filter, matched)) = best_constraint_filter
                && filter.estimated_cost < best_cost
            {
                // Encode which spec each argv slot carries, in argv order. omit=1
                // means SQLite does not re-check these, which is why xFilter
                // treats an undecodable spec index as a hard error.
                let mut argv_index = 1;
                let mut idx_parts = Vec::new();
                for (spec_index, constraint_index) in matched.iter().enumerate() {
                    if *constraint_index < 0 {
                        continue;
                    }
                    let usage = &mut *info.aConstraintUsage.add(*constraint_index as usize);
                    usage.argvIndex = argv_index;
                    usage.omit = 1;
                    argv_index += 1;
                    idx_parts.push(spec_index.to_string());
                }
                // Replaces the projection colUsed idxStr set above: a constraint
                // plan never reaches the full-scan build that reads it.
                if info.needToFreeIdxStr != 0 && !info.idxStr.is_null() {
                    ffi::sqlite3_free(info.idxStr.cast());
                }
                info.idxStr = std::ptr::null_mut();
                info.needToFreeIdxStr = 0;
                let idx_str = sqlite_mprintf_string(&idx_parts.join(","));
                if !idx_str.is_null() {
                    info.idxStr = idx_str;
                    info.needToFreeIdxStr = 1;
                }
                info.idxNum = filter.filter_id;
                info.estimatedCost = filter.estimated_cost;
                info.estimatedRows = filter.estimated_rows;
                return Ok(());
            }

            if let Some(index_pos) = best_index_pos
                && best_filter.is_none()
            {
                let usage = &mut *info.aConstraintUsage.add(best_index_constraint as usize);
                usage.argvIndex = 1;
                usage.omit = 1;
                info.idxNum = CACHED_INDEX_BASE + index_pos as c_int;
                info.estimatedCost = 1.0;
                info.estimatedRows = 5;
                return Ok(());
            }

            if let Some(filter) = best_filter {
                let usage = &mut *info.aConstraintUsage.add(best_filter_constraint as usize);
                usage.argvIndex = 1;
                usage.omit = u8::from(filter.omit);
                info.idxNum = filter.filter_id;
                info.estimatedCost = filter.estimated_cost;
                info.estimatedRows = filter.estimated_rows;
                return Ok(());
            }

            // Range plan on an indexed column: cheaper than a full scan (a
            // sorted-view window), pricier than an EQ point lookup. A two-bound
            // (BETWEEN) window is tighter than a one-bound half-scan, so it costs
            // less. The bounds are handled EXACTLY by the sorted-view carve
            // (strict/inclusive honored), so the consumed constraints are omitted
            // (SQLite need not re-check them). argv order is [low?, high?].
            if let Some(index_pos) = range_index_pos
                && (range_low_constraint >= 0 || range_high_constraint >= 0)
            {
                let mut range_flags = 0;
                let mut argv_slot = 1;
                if range_low_constraint >= 0 {
                    range_flags |= RANGE_HAS_LOW;
                    if range_low_strict {
                        range_flags |= RANGE_LOW_STRICT;
                    }
                    let usage = &mut *info.aConstraintUsage.add(range_low_constraint as usize);
                    usage.argvIndex = argv_slot;
                    usage.omit = 1;
                    argv_slot += 1;
                }
                if range_high_constraint >= 0 {
                    range_flags |= RANGE_HAS_HIGH;
                    if range_high_strict {
                        range_flags |= RANGE_HIGH_STRICT;
                    }
                    let usage = &mut *info.aConstraintUsage.add(range_high_constraint as usize);
                    usage.argvIndex = argv_slot;
                    usage.omit = 1;
                }
                let two_bound = range_low_constraint >= 0 && range_high_constraint >= 0;
                // Fraction-of-scan heuristic: a two-sided window is assumed to
                // select ~1/4 of rows, a one-sided ~1/2. Always strictly below the
                // full-scan cost so a pushed range beats the scan, and above the EQ
                // cost of 1.0.
                let frac = if two_bound { 0.25 } else { 0.5 };
                let full_cost = estimated_rows as f64;
                let mut range_cost = 2.0 + full_cost * frac;
                // Keep the range strictly cheaper than a full scan even on tiny
                // tables so the planner prefers the pushed window (and reports a
                // virtual index, not INDEX 0). Never dip to/below the EQ cost (1.0).
                if range_cost >= full_cost {
                    range_cost = 1.5_f64.max(full_cost * 0.75);
                }
                info.idxNum =
                    CACHED_RANGE_BASE + index_pos as c_int * CACHED_RANGE_STRIDE + range_flags;
                info.estimatedCost = range_cost;
                info.estimatedRows = if estimated_rows > 0 {
                    (estimated_rows as f64 * frac) as i64 + 1
                } else {
                    1
                };
                return Ok(());
            }

            info.idxNum = FILTER_NONE;
            info.estimatedCost = estimated_rows as f64;
            info.estimatedRows = estimated_rows;
            Ok(())
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

impl<Row> CachedCursor<Row> {
    /// Build this cursor's rows and hash indexes once; the equality, range and
    /// constraint-filter arms of a query-scoped table all reuse them across the
    /// cursor's xFilter calls (a JOIN's inner loop) until a write through the
    /// table retires them.
    fn ensure_cursor_index_cache(&mut self) {
        if self.cursor_index_built {
            return;
        }
        let def = self.def.clone();
        self.index_generation = def.write_generation.get();
        def.build_rows(
            &self.transaction_state,
            &mut self.cursor_index_cache,
            u64::MAX,
        );
        self.cursor_indexes = def
            .indexes
            .iter()
            .map(|index| {
                let mut map: HashMap<i64, Vec<usize>> = HashMap::new();
                for (row_index, row) in self.cursor_index_cache.iter().enumerate() {
                    map.entry((index.key)(row)).or_default().push(row_index);
                }
                map
            })
            .collect();
        self.cursor_index_built = true;
    }

    fn with_current_row<T>(&self, f: impl FnOnce(&Row) -> T) -> Option<T> {
        if let Some(matches) = self.index_matches.as_ref() {
            let row_index = *matches.get(self.current_row)?;
            // index_matches index into the per-cursor index cache (query-scoped
            // JOIN), the local cache (non-shared rebuild / single row_lookup), or
            // otherwise the shared cache.
            if self.index_into_cursor {
                return self.cursor_index_cache.get(row_index).map(f);
            }
            if self.index_into_local {
                return self.local_cache.get(row_index).map(f);
            }
            let cache = self.def.shared_cache.borrow();
            return cache.data.get(row_index).map(f);
        }
        if self.def.use_shared_cache {
            let cache = self.def.shared_cache.borrow();
            let row = cache.data.get(self.current_row)?;
            return Some(f(row));
        }
        self.local_cache.get(self.current_row).map(f)
    }
}

/// Non-shared positional rebuild for UPDATE/DELETE reconstruction: for a
/// `no_shared_cache` table without a stable `rowid_fn` (and so without a
/// `row_lookup`, which requires one), the full-scan rowid IS the row's index in a
/// freshly rebuilt cache, so rebuild and return that row.
fn cached_rebuilt_positional_row<Row>(
    def: &CachedInner<Row>,
    state: &TransactionState,
    old_rowid: i64,
) -> Option<Row> {
    if def.use_shared_cache
        || def.rowid.is_some()
        || (def.cache_builder.is_none()
            && def.projection_cache_builder.is_none()
            && def.stateful_cache_builder.is_none())
    {
        return None;
    }
    let index = usize::try_from(old_rowid).ok()?;
    let mut rows = Vec::new();
    def.build_rows(state, &mut rows, u64::MAX);
    rows.into_iter().nth(index)
}

/// A row can be resolved from the rowid alone — a positional shared cache (no
/// stable `rowid_fn`), a `row_lookup` (the sole authority: a rowid it cannot
/// resolve fails the statement), or a query-scoped mutation snapshot — and the
/// table is not opted into argv reconstruction. This is also the cached `xColumn`
/// NOCHANGE gate. Mirrors the C++ `reconstruct_by_rowid` predicate.
fn cached_reconstruct_by_rowid<Row>(def: &CachedInner<Row>) -> bool {
    !def.update_from_column_values
        && ((def.rowid.is_none() && def.use_shared_cache)
            || def.row_lookup.is_some()
            || def.query_scoped_uses_mutation_snapshot())
}

/// Resolve a query-scoped mutation-snapshot row positionally by `old_rowid`. Only
/// a frozen snapshot qualifies; the rowid must be in range. Mirrors the C++
/// `!use_shared_cache && shared->mutation_snapshot` reconstruction branch.
fn cached_query_scoped_snapshot_index<Row>(
    def: &CachedInner<Row>,
    old_rowid: i64,
) -> Option<usize> {
    if def.use_shared_cache || !def.query_scoped_uses_mutation_snapshot() {
        return None;
    }
    let index = usize::try_from(old_rowid).ok()?;
    let cache = def.shared_cache.borrow();
    (cache.mutation_snapshot && index < cache.data.len()).then_some(index)
}

/// Resolve a shared-cache row positionally by `old_rowid` — valid only for a
/// no-`rowid_fn` shared cache (and so one without a `row_lookup`, which resolves
/// every rowid itself). The cache must be usable (built, or a mutation snapshot)
/// and the rowid in range.
fn cached_shared_positional_index<Row>(def: &CachedInner<Row>, old_rowid: i64) -> Option<usize> {
    let index = usize::try_from(old_rowid).ok()?;
    let cache = def.shared_cache.borrow();
    let usable = cache.built || cache.mutation_snapshot;
    (usable && index < cache.data.len()).then_some(index)
}

/// The error for an UPDATE/DELETE rowid that names no row: the reason the
/// resolving callback set with [`set_vtab_error`](crate::set_vtab_error), else
/// `row <id> not found in <table>`.
fn cached_row_not_found<Row>(def: &CachedInner<Row>, rowid: i64) -> Error {
    vtab_callback_failure(&format!("row {rowid} not found in {}", def.name))
}

/// Resolve an UPDATE/DELETE rowid through `row_lookup`, the sole authority for a
/// table that has one: every rowid its scans report is a KEY (`build` requires a
/// `rowid()`), so a key it cannot resolve is a row that is gone — never a cache
/// position to retry, and never a row rebuilt from argv.
fn cached_row_lookup_or_error<Row>(
    def: &CachedInner<Row>,
    row_lookup: &dyn Fn(i64) -> Option<Row>,
    rowid: i64,
) -> Result<Row> {
    begin_vtab_callback();
    row_lookup(rowid).ok_or_else(|| cached_row_not_found(def, rowid))
}

/// UPDATE row reconstruction, mirroring the C++ decision tree. Applies the
/// non-NOCHANGE writable setters to the resolved row; returns false when no
/// branch applies (caller maps that to read-only). A `row_lookup` table resolves
/// only through it and fails with `row <id> not found in <table>` on a miss.
fn cached_update_reconstruct_and_apply<'a, Row>(
    def: &CachedInner<Row>,
    state: &TransactionState,
    old_rowid: i64,
    args: &[FunctionArg<'a>],
) -> Result<bool> {
    // Query-scoped tables: freeze the pre-mutation cache for this statement.
    def.ensure_query_scoped_mutation_snapshot(state)?;
    let by_rowid = cached_reconstruct_by_rowid(def);

    if let Some(row_lookup) = def.row_lookup.as_ref() {
        let mut row = match def.row_populator.as_ref() {
            // Opted out (update_from_column_values): NOCHANGE is off, so every
            // column carries its real value and the row is rebuilt from them. The
            // rowid is not interpreted at all.
            Some(populate) if !by_rowid => {
                begin_vtab_callback();
                populate(args).ok_or_else(|| cached_row_not_found(def, old_rowid))?
            }
            // The sole authority: a key it cannot resolve fails the statement.
            _ => cached_row_lookup_or_error(def, row_lookup, old_rowid)?,
        };
        apply_cached_setters(def, &mut row, args)?;
        return Ok(true);
    }

    // Positional shared cache (no rowid_fn), mutated in place. Only an
    // ALREADY-built/snapshotted cache qualifies — like C++, never build the shared
    // cache during a mutation (a filter_eq cursor leaves it unbuilt, and its
    // iterator rowid is not a reliable cache position).
    if by_rowid
        && def.rowid.is_none()
        && def.use_shared_cache
        && let Some(index) = cached_shared_positional_index(def, old_rowid)
    {
        let mut cache = def.shared_cache.borrow_mut();
        apply_cached_setters(def, &mut cache.data[index], args)?;
        return Ok(true);
    }

    // NOCHANGE off — reconstruct from the real argv values.
    if !by_rowid && let Some(mut row) = def.row_populator.as_ref().and_then(|build| build(args)) {
        apply_cached_setters(def, &mut row, args)?;
        return Ok(true);
    }

    // Query-scoped mutation snapshot (positional, frozen for the statement) — used
    // instead of a rebuild-on-mutated-data for a multi-row UPDATE.
    if let Some(index) = cached_query_scoped_snapshot_index(def, old_rowid) {
        let mut cache = def.shared_cache.borrow_mut();
        apply_cached_setters(def, &mut cache.data[index], args)?;
        return Ok(true);
    }

    // Non-shared positional rebuild.
    if !def.update_from_column_values
        && let Some(mut row) = cached_rebuilt_positional_row(def, state, old_rowid)
    {
        apply_cached_setters(def, &mut row, args)?;
        return Ok(true);
    }

    // Last-resort populator (argv). CONTRACT: when this runs under NOCHANGE (a
    // positional shared cache not built yet, reached via filter_eq), the populator
    // must resolve the row from values that are real, not from unchanged columns.
    if let Some(mut row) = def.row_populator.as_ref().and_then(|build| build(args)) {
        apply_cached_setters(def, &mut row, args)?;
        return Ok(true);
    }

    Ok(false)
}

/// DELETE row reconstruction, mirroring the C++ decision tree: `row_lookup` (the
/// sole authority when set) -> positional shared -> query-scoped snapshot ->
/// non-shared positional rebuild. A rowid that resolves no row fails with
/// `row <id> not found in <table>`; a `delete_row` refusal fails with its reason.
fn cached_delete_reconstruct<Row>(
    def: &CachedInner<Row>,
    state: &TransactionState,
    old_rowid: i64,
    delete_row: &CachedDelete<Row>,
) -> Result<()> {
    // Query-scoped tables: freeze the pre-mutation cache for this statement.
    def.ensure_query_scoped_mutation_snapshot(state)?;
    let delete = |row: &Row| {
        begin_vtab_callback();
        if delete_row(row) {
            Ok(())
        } else {
            Err(vtab_callback_failure("cached table delete failed"))
        }
    };

    if let Some(row_lookup) = def.row_lookup.as_ref() {
        return delete(&cached_row_lookup_or_error(def, row_lookup, old_rowid)?);
    }

    // Positional shared cache (no rowid_fn). Only an already built/snapshotted
    // cache qualifies — like C++, never build during a mutation.
    if cached_reconstruct_by_rowid(def)
        && def.rowid.is_none()
        && def.use_shared_cache
        && let Some(index) = cached_shared_positional_index(def, old_rowid)
    {
        let cache = def.shared_cache.borrow();
        return delete(&cache.data[index]);
    }

    // Query-scoped mutation snapshot (positional, frozen for the statement).
    if let Some(index) = cached_query_scoped_snapshot_index(def, old_rowid) {
        let cache = def.shared_cache.borrow();
        return delete(&cache.data[index]);
    }

    // Non-shared positional rebuild.
    if let Some(row) = cached_rebuilt_positional_row(def, state, old_rowid) {
        return delete(&row);
    }

    begin_vtab_callback();
    Err(cached_row_not_found(def, old_rowid))
}

fn cached_shared_row_index<Row>(
    def: &CachedInner<Row>,
    rows: &[Row],
    old_rowid: i64,
) -> Option<usize> {
    if let Some(rowid) = def.rowid.as_ref() {
        rows.iter().position(|row| rowid(row) == old_rowid)
    } else {
        usize::try_from(old_rowid)
            .ok()
            .filter(|index| *index < rows.len())
    }
}

fn cached_rowid_matches<Row>(def: &CachedInner<Row>, rows: &[Row], rowid: i64) -> Vec<usize> {
    if let Some(rowid_fn) = def.rowid.as_ref() {
        rows.iter()
            .enumerate()
            .filter_map(|(index, row)| (rowid_fn(row) == rowid).then_some(index))
            .collect()
    } else {
        cached_shared_row_index(def, rows, rowid)
            .map(|index| vec![index])
            .unwrap_or_default()
    }
}

fn apply_cached_setters<'a, Row>(
    def: &CachedInner<Row>,
    row: &mut Row,
    args: &[FunctionArg<'a>],
) -> Result<()> {
    // NOCHANGE-eligible only when a row resolves from its rowid (the xColumn gate);
    // argv-reconstruct tables carry read-only identity columns as real values.
    apply_update_columns(
        args,
        def.columns.len(),
        cached_reconstruct_by_rowid(def),
        |i| def.columns[i].setter.is_some(),
        |i| def.columns[i].name.clone(),
        |i, value| {
            def.columns[i]
                .setter
                .as_ref()
                .expect("apply runs only for writable columns")(row, value)
        },
    )
}
