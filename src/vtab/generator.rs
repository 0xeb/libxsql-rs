// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::error::{Error, Result, sqlite_ok};
use crate::function::{FunctionArg, FunctionContext, build_args};
use libsqlite3_sys as ffi;
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use super::cached::{CachedCount, CachedFilterFactory};
use super::column_helpers;
use super::ffi::{
    c_string_to_string, call_cached_modify_hook, callback_status, finish_cursor_unit,
    finish_vtab_unit, like_constraint_has_usable_prefix, parse_col_used, quote_identifier,
    resolve_constraint_specs, set_vtab_error, sqlite_mprintf_string, sqlite3_vtab_nochange,
    unsupported_delete, unsupported_insert, unsupported_update,
};
use super::index::InsertFn;
use super::index_key;
use super::{
    ColumnType, ConstraintOp, ConstraintRequest, FILTER_NONE, Generator, GeneratorConstraintArg,
    ModifyHook, RowIterator, TransactionHooks, TransactionLifecycle, WriteCaps,
    WriteSurfaceRegistry, apply_update_columns, begin_vtab_callback, connect_write_surface,
    destroy_write_surface, table_write_counter, vtab_callback_failure,
};

const MISSING_REQUIRED_CONSTRAINT: c_int = -2;
const GENERATOR_COUNT_ONLY_SCAN: c_int = -1;
const CONSTRAINT_FILTER_BASE: c_int = 2_000;
const PARAMETRIC_FILTER_BASE: c_int = 500;

/// How a cursor callback that may refuse the scan failed. A table refusing to be
/// read without constraints (a required constraint is missing, or a full scan
/// is forbidden) reports `SQLITE_CONSTRAINT` -- SQLite's own code for an
/// unusable constraint set -- with the table's message, so a caller such as
/// [`crate::export_tables`] can tell "needs a WHERE" from a genuine failure,
/// which keeps `SQLITE_ERROR`.
enum CursorFailure {
    Refused(String),
    Failed(Error),
}

impl From<Error> for CursorFailure {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

type CursorResult = std::result::Result<(), CursorFailure>;

/// [`finish_cursor_unit`] for a callback that may refuse the scan.
unsafe fn finish_refusable_cursor_unit(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    result: std::thread::Result<CursorResult>,
) -> c_int {
    unsafe {
        match result {
            Ok(Err(CursorFailure::Refused(message))) => {
                if !cursor.is_null() {
                    set_vtab_error((*cursor).pVtab, &message);
                }
                ffi::SQLITE_CONSTRAINT_VTAB
            }
            Ok(Err(CursorFailure::Failed(error))) => finish_cursor_unit(cursor, Ok(Err(error))),
            Ok(Ok(())) => finish_cursor_unit(cursor, Ok(Ok(()))),
            Err(panic) => finish_cursor_unit(cursor, Err(panic)),
        }
    }
}

type GeneratorFactory<Row> = dyn Fn() -> Box<dyn Generator<Row>> + 'static;
// Projection-aware full-scan factory: receives SQLite's colUsed bitmask so it can skip
// materializing expensive unused columns. Used for the full-scan plan when set.
type ProjectionGeneratorFactory<Row> = dyn Fn(u64) -> Box<dyn Generator<Row>> + 'static;
type GeneratorGetter<Row> = dyn for<'a> Fn(&mut FunctionContext<'a>, &Row) + 'static;
type GeneratorSetter<Row> = dyn for<'a> Fn(&mut Row, &FunctionArg<'a>) -> bool + 'static;
type GeneratorParametricFactory<Row> =
    dyn for<'a> Fn(&[FunctionArg<'a>]) -> Box<dyn Generator<Row>> + 'static;
type GeneratorConstraintFactory<Row> =
    dyn for<'a> Fn(&[GeneratorConstraintArg<'a>]) -> Box<dyn Generator<Row>> + 'static;
// Projection-aware variants: the same inputs plus SQLite's colUsed bitmask, carried
// to xFilter through idxStr so the producer can skip unused expensive columns.
type ProjectionFilterFactory =
    dyn for<'a> Fn(&FunctionArg<'a>, u64) -> Box<dyn RowIterator> + 'static;
type ProjectionParametricFactory<Row> =
    dyn for<'a> Fn(&[FunctionArg<'a>], u64) -> Box<dyn Generator<Row>> + 'static;
type ProjectionConstraintFactory<Row> =
    dyn for<'a> Fn(&[GeneratorConstraintArg<'a>], u64) -> Box<dyn Generator<Row>> + 'static;

/// A filter factory, optionally projection-aware. Only a `Projection` factory
/// makes xBestIndex carry colUsed; a `Plain` one keeps idxStr exactly as it was.
enum FilterFactory<Plain: ?Sized, Projection: ?Sized> {
    Plain(Box<Plain>),
    Projection(Box<Projection>),
}

impl<Plain: ?Sized, Projection: ?Sized> FilterFactory<Plain, Projection> {
    fn is_projection(&self) -> bool {
        matches!(self, Self::Projection(_))
    }
}
type GeneratorDelete<Row> = dyn Fn(&Row) -> bool + 'static;
type GeneratorLookup<Row> = dyn Fn(i64) -> Option<Row> + 'static;

struct GeneratorColumnDef<Row> {
    name: String,
    column_type: ColumnType,
    getter: Box<GeneratorGetter<Row>>,
    setter: Option<Box<GeneratorSetter<Row>>>,
    hidden: bool,
}

struct GeneratorFilterDef {
    column_index: usize,
    filter_id: c_int,
    estimated_cost: f64,
    estimated_rows: i64,
    /// The SQLite constraint operator this filter answers: EQ (trusted, omit=1)
    /// or LIKE (a best-effort superset, omit=0 so SQLite re-applies the pattern).
    op: c_int,
    factory: FilterFactory<CachedFilterFactory, ProjectionFilterFactory>,
}

struct ParametricFilterDef<Row> {
    column_indices: Vec<usize>,
    filter_id: c_int,
    estimated_cost: f64,
    estimated_rows: i64,
    factory: FilterFactory<GeneratorParametricFactory<Row>, ProjectionParametricFactory<Row>>,
}

struct GeneratorConstraintSpec {
    column_index: usize,
    op: ConstraintOp,
    required: bool,
    missing_error: String,
}

struct ConstraintFilterDef<Row> {
    specs: Vec<GeneratorConstraintSpec>,
    filter_id: c_int,
    estimated_cost: f64,
    estimated_rows: i64,
    ordered_column: Option<usize>,
    ordered_desc: bool,
    factory: FilterFactory<GeneratorConstraintFactory<Row>, ProjectionConstraintFactory<Row>>,
}

struct GeneratorInner<Row> {
    name: String,
    estimate_rows: Option<Box<CachedCount>>,
    row_count: Option<Box<CachedCount>>,
    generator: Option<Box<GeneratorFactory<Row>>>,
    projection_generator: Option<Box<ProjectionGeneratorFactory<Row>>>,
    columns: Vec<GeneratorColumnDef<Row>>,
    filters: Vec<GeneratorFilterDef>,
    parametric_filters: Vec<ParametricFilterDef<Row>>,
    constraint_filters: Vec<ConstraintFilterDef<Row>>,
    full_scan_error: Option<String>,
    before_modify: Option<Box<ModifyHook>>,
    after_modify: Option<Box<ModifyHook>>,
    transaction_hooks: TransactionHooks,
    counts_as_write: bool,
    delete_row: Option<Box<GeneratorDelete<Row>>>,
    insert_row: Option<Box<InsertFn>>,
    row_lookup: Option<Box<GeneratorLookup<Row>>>,
}

impl<Row> GeneratorInner<Row> {
    fn schema(&self) -> String {
        let columns = self
            .columns
            .iter()
            .map(|column| {
                let mut declaration = format!(
                    "{} {}",
                    quote_identifier(&column.name),
                    column.column_type.sql()
                );
                if column.hidden {
                    declaration.push_str(" HIDDEN");
                }
                declaration
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("CREATE TABLE {}({columns})", quote_identifier(&self.name))
    }

    fn find_filter_by_column(&self, column_index: usize, op: c_int) -> Option<&GeneratorFilterDef> {
        self.filters
            .iter()
            .filter(|filter| filter.column_index == column_index && filter.op == op)
            .min_by(|a, b| a.estimated_cost.total_cmp(&b.estimated_cost))
    }

    fn find_filter_by_id(&self, filter_id: c_int) -> Option<&GeneratorFilterDef> {
        self.filters
            .iter()
            .find(|filter| filter.filter_id == filter_id)
    }
}

/// Definition for a generator-backed virtual table.
#[derive(Clone)]
pub struct GeneratorTableDef<Row> {
    inner: Rc<GeneratorInner<Row>>,
}

impl<Row> GeneratorTableDef<Row> {
    /// Returns the table's name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Compute write-surface capabilities for the prepare-time authorizer
    /// (mirrors the C++ generator `record_write_surface`): a generator
    /// DELETE/UPDATE additionally requires a `row_lookup` to resolve the target
    /// row, so deletable/updatable are gated on it.
    pub(crate) fn write_caps(&self) -> super::WriteCaps {
        let has_lookup = self.inner.row_lookup.is_some();
        super::WriteCaps {
            insertable: self.inner.insert_row.is_some(),
            deletable: self.inner.delete_row.is_some() && has_lookup,
            writable_columns: if has_lookup {
                self.inner
                    .columns
                    .iter()
                    .filter(|column| column.setter.is_some())
                    .map(|column| column.name.to_ascii_lowercase())
                    .collect()
            } else {
                Default::default()
            },
        }
    }
}

/// Start a generator-backed virtual table definition.
pub fn generator_table<Row: 'static>(name: impl Into<String>) -> GeneratorTableBuilder<Row> {
    GeneratorTableBuilder {
        name: name.into(),
        estimate_rows: None,
        row_count: None,
        generator: None,
        projection_generator: None,
        columns: Vec::new(),
        filters: Vec::new(),
        parametric_filters: Vec::new(),
        constraint_filters: Vec::new(),
        full_scan_error: None,
        before_modify: None,
        after_modify: None,
        transaction_hooks: TransactionHooks::default(),
        counts_as_write: true,
        delete_row: None,
        insert_row: None,
        row_lookup: None,
    }
}

/// Fluent builder for generator-backed virtual tables.
pub struct GeneratorTableBuilder<Row> {
    name: String,
    estimate_rows: Option<Box<CachedCount>>,
    row_count: Option<Box<CachedCount>>,
    generator: Option<Box<GeneratorFactory<Row>>>,
    projection_generator: Option<Box<ProjectionGeneratorFactory<Row>>>,
    columns: Vec<GeneratorColumnDef<Row>>,
    filters: Vec<GeneratorFilterDef>,
    parametric_filters: Vec<ParametricFilterDef<Row>>,
    constraint_filters: Vec<ConstraintFilterDef<Row>>,
    full_scan_error: Option<String>,
    before_modify: Option<Box<ModifyHook>>,
    after_modify: Option<Box<ModifyHook>>,
    transaction_hooks: TransactionHooks,
    counts_as_write: bool,
    delete_row: Option<Box<GeneratorDelete<Row>>>,
    insert_row: Option<Box<InsertFn>>,
    row_lookup: Option<Box<GeneratorLookup<Row>>>,
}

impl<Row: 'static> GeneratorTableBuilder<Row> {
    /// Set the callback returning an estimated row count for query planning.
    pub fn estimate_rows<F>(mut self, estimate_rows: F) -> Self
    where
        F: Fn() -> usize + 'static,
    {
        self.estimate_rows = Some(Box::new(estimate_rows));
        self
    }

    /// Set an exact cheap row count for bare `COUNT(*)` queries.
    pub fn row_count<F>(mut self, row_count: F) -> Self
    where
        F: Fn() -> usize + 'static,
    {
        self.row_count = Some(Box::new(row_count));
        self
    }

    /// Set the factory that produces a fresh [`Generator`] for a full scan.
    pub fn generator<F>(mut self, generator: F) -> Self
    where
        F: Fn() -> Box<dyn Generator<Row>> + 'static,
    {
        self.generator = Some(Box::new(generator));
        self
    }

    /// Set a projection-aware full-scan factory that receives SQLite's `colUsed`
    /// bitmask so it can skip materializing expensive unused columns. Used for the
    /// full-scan plan when set; otherwise [`generator`](Self::generator) is used.
    pub fn projection_generator<F>(mut self, generator: F) -> Self
    where
        F: Fn(u64) -> Box<dyn Generator<Row>> + 'static,
    {
        self.projection_generator = Some(Box::new(generator));
        self
    }

    /// Make an unconstrained full scan fail with `message`, forcing callers to
    /// supply a pushdown constraint.
    pub fn full_scan_error(mut self, message: impl Into<String>) -> Self {
        self.full_scan_error = Some(message.into());
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
        self.add_column(name, column_type, Box::new(getter), None, false)
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
        self.add_column(
            name,
            column_type,
            Box::new(getter),
            Some(Box::new(setter)),
            false,
        )
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
        )
    }

    /// Add a hidden `INTEGER` column (usable as a constraint input but not
    /// returned by `*`).
    pub fn hidden_column_i64(self, name: impl Into<String>) -> Self {
        self.hidden_column(name, ColumnType::Integer)
    }

    /// Alias for [`hidden_column_i64`](Self::hidden_column_i64).
    pub fn hidden_column_int64(self, name: impl Into<String>) -> Self {
        self.hidden_column_i64(name)
    }

    /// Add a hidden `INTEGER` column (usable as a constraint input but not
    /// returned by `*`).
    pub fn hidden_column_int(self, name: impl Into<String>) -> Self {
        self.hidden_column(name, ColumnType::Integer)
    }

    /// Add a hidden `TEXT` column (usable as a constraint input but not returned
    /// by `*`).
    pub fn hidden_column_text(self, name: impl Into<String>) -> Self {
        self.hidden_column(name, ColumnType::Text)
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
            estimated_cost,
            estimated_rows,
            move |value| match index_key::exact_text(value) {
                Some(text) => factory(&text),
                None => Box::new(index_key::NoRows),
            },
        )
    }

    /// Projection-aware [`filter_eq`](Self::filter_eq): the factory also
    /// receives SQLite's `colUsed` bitmask (bit `i` = column `i` is read; bit 63 =
    /// column 63 or later, assume all), so an equality lookup that does not
    /// select an expensive column can use a cheaper producer.
    pub fn filter_eq_projection<F>(
        self,
        column_name: impl AsRef<str>,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        F: Fn(i64, u64) -> Box<dyn RowIterator> + 'static,
    {
        self.add_filter_def(
            column_name.as_ref(),
            ffi::SQLITE_INDEX_CONSTRAINT_EQ,
            estimated_cost,
            estimated_rows,
            FilterFactory::Projection(Box::new(move |value: &FunctionArg<'_>, col_used| {
                match index_key::exact_i64(value) {
                    Some(key) => factory(key, col_used),
                    None => Box::new(index_key::NoRows),
                }
            })),
        )
    }

    /// `WHERE <column> LIKE 'prefix%'` pushdown for a text column. The factory
    /// receives the raw pattern and the `colUsed` bitmask. It is a best-effort
    /// superset: the plan is claimed only when the pattern starts with a literal
    /// (not `%`, `_` or an escape), and SQLite still re-applies the real pattern
    /// to every row, so the factory may return extra rows but must not miss one.
    pub fn filter_like_text<F>(
        self,
        column_name: impl AsRef<str>,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        F: Fn(&str, u64) -> Box<dyn RowIterator> + 'static,
    {
        self.add_filter_def(
            column_name.as_ref(),
            ffi::SQLITE_INDEX_CONSTRAINT_LIKE,
            estimated_cost,
            estimated_rows,
            FilterFactory::Projection(Box::new(move |value: &FunctionArg<'_>, col_used| {
                let text = value.as_c_str().map(CStr::to_string_lossy);
                factory(text.as_deref().unwrap_or(""), col_used)
            })),
        )
    }

    /// Register a filter requiring equality constraints on all `param_columns`,
    /// producing a [`Generator`] from the matched values.
    pub fn parametric_filter<I, S, F>(
        self,
        param_columns: I,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
        F: for<'a> Fn(&[FunctionArg<'a>]) -> Box<dyn Generator<Row>> + 'static,
    {
        self.add_parametric_filter(
            param_columns,
            estimated_cost,
            estimated_rows,
            FilterFactory::Plain(Box::new(factory)),
        )
    }

    /// Projection-aware [`parametric_filter`](Self::parametric_filter): the
    /// factory also receives the `colUsed` bitmask.
    pub fn parametric_filter_projection<I, S, F>(
        self,
        param_columns: I,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
        F: for<'a> Fn(&[FunctionArg<'a>], u64) -> Box<dyn Generator<Row>> + 'static,
    {
        self.add_parametric_filter(
            param_columns,
            estimated_cost,
            estimated_rows,
            FilterFactory::Projection(Box::new(factory)),
        )
    }

    /// Register a filter driven by a set of required/optional
    /// [`ConstraintRequest`]s; the factory receives the matched constraint values.
    pub fn constraint_filter<I, F>(
        self,
        constraints: I,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        I: IntoIterator<Item = ConstraintRequest>,
        F: for<'a> Fn(&[GeneratorConstraintArg<'a>]) -> Box<dyn Generator<Row>> + 'static,
    {
        self.add_constraint_filter(
            constraints,
            estimated_cost,
            estimated_rows,
            FilterFactory::Plain(Box::new(factory)),
        )
    }

    /// Projection-aware [`constraint_filter`](Self::constraint_filter): the
    /// factory also receives the `colUsed` bitmask, so a pushdown that does not
    /// select an expensive column can skip computing it. A plain
    /// `constraint_filter` is unaffected (its factory sees no mask and its plan
    /// encoding is unchanged).
    pub fn constraint_filter_projection<I, F>(
        self,
        constraints: I,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        I: IntoIterator<Item = ConstraintRequest>,
        F: for<'a> Fn(&[GeneratorConstraintArg<'a>], u64) -> Box<dyn Generator<Row>> + 'static,
    {
        self.add_constraint_filter(
            constraints,
            estimated_cost,
            estimated_rows,
            FilterFactory::Projection(Box::new(factory)),
        )
    }

    fn add_parametric_filter<I, S>(
        mut self,
        param_columns: I,
        estimated_cost: f64,
        estimated_rows: f64,
        factory: FilterFactory<GeneratorParametricFactory<Row>, ProjectionParametricFactory<Row>>,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut column_indices = Vec::new();
        for column_name in param_columns {
            let Some(column_index) = self.find_column(column_name.as_ref()) else {
                return self;
            };
            column_indices.push(column_index);
        }
        let filter_id = PARAMETRIC_FILTER_BASE + self.parametric_filters.len() as c_int;
        self.parametric_filters.push(ParametricFilterDef {
            column_indices,
            filter_id,
            estimated_cost,
            estimated_rows: estimated_rows.max(1.0) as i64,
            factory,
        });
        self
    }

    fn add_constraint_filter<I>(
        mut self,
        constraints: I,
        estimated_cost: f64,
        estimated_rows: f64,
        factory: FilterFactory<GeneratorConstraintFactory<Row>, ProjectionConstraintFactory<Row>>,
    ) -> Self
    where
        I: IntoIterator<Item = ConstraintRequest>,
    {
        let mut specs = Vec::new();
        for constraint in constraints {
            let Some(column_index) = self.find_column(&constraint.column_name) else {
                return self;
            };
            specs.push(GeneratorConstraintSpec {
                column_index,
                op: constraint.op,
                required: constraint.required,
                missing_error: constraint.missing_error,
            });
        }
        let filter_id = CONSTRAINT_FILTER_BASE + self.constraint_filters.len() as c_int;
        self.constraint_filters.push(ConstraintFilterDef {
            specs,
            filter_id,
            estimated_cost,
            estimated_rows: estimated_rows.max(1.0) as i64,
            ordered_column: None,
            ordered_desc: false,
            factory,
        });
        self
    }

    /// Mark the most recently added constraint filter as satisfying an `ORDER BY`
    /// on `column_name` (in `desc` direction), so SQLite skips its own sort.
    pub fn order_by_consumed(mut self, column_name: impl AsRef<str>, desc: bool) -> Self {
        let Some(column_index) = self.find_column(column_name.as_ref()) else {
            return self;
        };
        if let Some(filter) = self.constraint_filters.last_mut() {
            filter.ordered_column = Some(column_index);
            filter.ordered_desc = desc;
        }
        self
    }

    /// Set a callback that resolves a single row by rowid, used for rowid lookups
    /// and update/delete reconstruction.
    pub fn row_lookup<F>(mut self, row_lookup: F) -> Self
    where
        F: Fn(i64) -> Option<Row> + 'static,
    {
        self.row_lookup = Some(Box::new(row_lookup));
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

    /// Finalize the definition; errors if no generator callback was set.
    pub fn build(self) -> Result<GeneratorTableDef<Row>> {
        if self.generator.is_none() && self.projection_generator.is_none() {
            return Err(Error::Message(format!(
                "generator table '{}' is missing a generator callback",
                self.name
            )));
        }
        Ok(GeneratorTableDef {
            inner: Rc::new(GeneratorInner {
                name: self.name,
                estimate_rows: self.estimate_rows,
                row_count: self.row_count,
                generator: self.generator,
                projection_generator: self.projection_generator,
                columns: self.columns,
                filters: self.filters,
                parametric_filters: self.parametric_filters,
                constraint_filters: self.constraint_filters,
                full_scan_error: self.full_scan_error,
                before_modify: self.before_modify,
                after_modify: self.after_modify,
                transaction_hooks: self.transaction_hooks,
                counts_as_write: self.counts_as_write,
                delete_row: self.delete_row,
                insert_row: self.insert_row,
                row_lookup: self.row_lookup,
            }),
        })
    }

    fn hidden_column(self, name: impl Into<String>, column_type: ColumnType) -> Self {
        self.add_column(
            name,
            column_type,
            column_helpers::row_getter_null(),
            None,
            true,
        )
    }

    fn add_column(
        mut self,
        name: impl Into<String>,
        column_type: ColumnType,
        getter: Box<GeneratorGetter<Row>>,
        setter: Option<Box<GeneratorSetter<Row>>>,
        hidden: bool,
    ) -> Self {
        self.columns.push(GeneratorColumnDef {
            name: name.into(),
            column_type,
            getter,
            setter,
            hidden,
        });
        self
    }

    fn add_filter<F>(
        self,
        column_name: &str,
        estimated_cost: f64,
        estimated_rows: f64,
        factory: F,
    ) -> Self
    where
        F: for<'a> Fn(&FunctionArg<'a>) -> Box<dyn RowIterator> + 'static,
    {
        self.add_filter_def(
            column_name,
            ffi::SQLITE_INDEX_CONSTRAINT_EQ,
            estimated_cost,
            estimated_rows,
            FilterFactory::Plain(Box::new(factory)),
        )
    }

    fn add_filter_def(
        mut self,
        column_name: &str,
        op: c_int,
        estimated_cost: f64,
        estimated_rows: f64,
        factory: FilterFactory<CachedFilterFactory, ProjectionFilterFactory>,
    ) -> Self {
        if let Some(column_index) = self.find_column(column_name) {
            let filter_id = self.filters.len() as c_int + 1;
            self.filters.push(GeneratorFilterDef {
                column_index,
                filter_id,
                estimated_cost,
                estimated_rows: estimated_rows.max(1.0) as i64,
                op,
                factory,
            });
        }
        self
    }

    fn find_column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }
}

#[repr(C)]
struct GeneratorModuleState<Row> {
    module: ffi::sqlite3_module,
    def: Rc<GeneratorInner<Row>>,
    registry: *const RefCell<WriteSurfaceRegistry>,
    caps: WriteCaps,
}

#[repr(C)]
struct GeneratorVtab<Row> {
    base: ffi::sqlite3_vtab,
    def: Rc<GeneratorInner<Row>>,
    transaction: TransactionLifecycle,
    registry: *const RefCell<WriteSurfaceRegistry>,
    schema_name: String,
    table_name: String,
}

#[repr(C)]
struct GeneratorCursor<Row> {
    base: ffi::sqlite3_vtab_cursor,
    vtab: *mut ffi::sqlite3_vtab,
    def: Rc<GeneratorInner<Row>>,
    generator: Option<Box<dyn Generator<Row>>>,
    iterator: Option<Box<dyn RowIterator>>,
    eof: bool,
    count_only_total: Option<usize>,
    count_only_row: usize,
}

pub(crate) fn register_generator_table<Row: 'static>(
    db: *mut ffi::sqlite3,
    registry: *const RefCell<WriteSurfaceRegistry>,
    module_name: &str,
    def: &GeneratorTableDef<Row>,
) -> Result<()> {
    if db.is_null() {
        return Err(Error::DatabaseNotOpen);
    }
    let module_name = CString::new(module_name)?;
    let state = Box::new(GeneratorModuleState {
        module: create_generator_module::<Row>(),
        def: def.inner.clone(),
        registry,
        caps: def.write_caps(),
    });
    let state_ptr = Box::into_raw(state);
    let module_ptr = unsafe { &(*state_ptr).module as *const ffi::sqlite3_module };
    let rc = unsafe {
        ffi::sqlite3_create_module_v2(
            db,
            module_name.as_ptr(),
            module_ptr,
            state_ptr.cast::<c_void>(),
            Some(destroy_generator_module_state::<Row>),
        )
    };
    if sqlite_ok(rc) {
        Ok(())
    } else {
        Err(Error::sqlite(rc, crate::function::sqlite_error(db)))
    }
}

fn create_generator_module<Row: 'static>() -> ffi::sqlite3_module {
    let mut module: ffi::sqlite3_module = unsafe { std::mem::zeroed() };
    module.iVersion = 3;
    module.xCreate = Some(generator_vtab_connect::<Row>);
    module.xConnect = Some(generator_vtab_connect::<Row>);
    module.xBestIndex = Some(generator_vtab_best_index::<Row>);
    module.xDisconnect = Some(generator_vtab_disconnect::<Row>);
    module.xDestroy = Some(generator_vtab_destroy::<Row>);
    module.xOpen = Some(generator_vtab_open::<Row>);
    module.xClose = Some(generator_vtab_close::<Row>);
    module.xFilter = Some(generator_vtab_filter::<Row>);
    module.xNext = Some(generator_vtab_next::<Row>);
    module.xEof = Some(generator_vtab_eof::<Row>);
    module.xColumn = Some(generator_vtab_column::<Row>);
    module.xRowid = Some(generator_vtab_rowid::<Row>);
    module.xUpdate = Some(generator_vtab_update::<Row>);
    // xBegin enrolls the vtab. Run the fallible hook in xSync, whose error
    // SQLite propagates; xCommit only clears state.
    module.xBegin = Some(generator_vtab_begin::<Row>);
    module.xSync = Some(generator_vtab_sync::<Row>);
    module.xCommit = Some(generator_vtab_commit::<Row>);
    module.xRollback = Some(generator_vtab_rollback::<Row>);
    module.xSavepoint = Some(generator_vtab_savepoint::<Row>);
    module.xRelease = Some(generator_vtab_release::<Row>);
    module.xRollbackTo = Some(generator_vtab_rollback_to::<Row>);
    module
}

unsafe extern "C" fn destroy_generator_module_state<Row>(ptr: *mut c_void) {
    unsafe {
        if !ptr.is_null() {
            drop(Box::from_raw(ptr.cast::<GeneratorModuleState<Row>>()));
        }
    }
}

unsafe extern "C" fn generator_vtab_connect<Row>(
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
                .cast::<GeneratorModuleState<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator module state is null".to_string()))?;
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
            let vtab = Box::new(GeneratorVtab {
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

unsafe extern "C" fn generator_vtab_destroy<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if !p_vtab.is_null() {
            let vtab = Box::from_raw(p_vtab.cast::<GeneratorVtab<Row>>());
            destroy_write_surface(vtab.registry, &vtab.schema_name, &vtab.table_name);
            drop(vtab);
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn generator_vtab_disconnect<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if !p_vtab.is_null() {
            drop(Box::from_raw(p_vtab.cast::<GeneratorVtab<Row>>()));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn generator_vtab_open<Row>(
    p_vtab: *mut ffi::sqlite3_vtab,
    pp_cursor: *mut *mut ffi::sqlite3_vtab_cursor,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<GeneratorVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator vtab is null".to_string()))?;
            let cursor = Box::new(GeneratorCursor {
                base: std::mem::zeroed(),
                vtab: p_vtab,
                def: vtab.def.clone(),
                generator: None,
                iterator: None,
                eof: false,
                count_only_total: None,
                count_only_row: 0,
            });
            let cursor_ptr = Box::into_raw(cursor);
            *pp_cursor = &mut (*cursor_ptr).base;
            Ok::<_, Error>(())
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn generator_vtab_close<Row>(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        if !cursor.is_null() {
            drop(Box::from_raw(cursor.cast::<GeneratorCursor<Row>>()));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn generator_vtab_filter<Row>(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    idx_num: c_int,
    idx_str: *const c_char,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> CursorResult {
            let cursor = cursor
                .cast::<GeneratorCursor<Row>>()
                .as_mut()
                .ok_or_else(|| Error::Message("xsql generator cursor is null".to_string()))?;
            cursor.generator = None;
            cursor.iterator = None;
            cursor.eof = false;
            cursor.count_only_total = None;
            cursor.count_only_row = 0;

            if idx_num == GENERATOR_COUNT_ONLY_SCAN && argc == 0 {
                let row_count =
                    cursor.def.row_count.as_ref().ok_or_else(|| {
                        Error::Message("generator row count is unavailable".into())
                    })?;
                cursor.count_only_total = Some(row_count());
                return Ok(());
            }

            if idx_num == MISSING_REQUIRED_CONSTRAINT {
                let message = c_string_to_string(idx_str)
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| "required constraint missing".to_string());
                return Err(CursorFailure::Refused(message));
            }

            if idx_num >= CONSTRAINT_FILTER_BASE
                && argc > 0
                && let Some(filter) = cursor
                    .def
                    .constraint_filters
                    .iter()
                    .find(|filter| filter.filter_id == idx_num)
            {
                // A projection-aware filter's idxStr is "<colUsed>;<spec list>";
                // every other filter's is the bare spec list.
                let idx_text = c_string_to_string(idx_str).unwrap_or_default();
                let (col_used, spec_list) = match (&filter.factory, idx_text.split_once(';')) {
                    (FilterFactory::Projection(_), Some((mask, list))) => {
                        (mask.parse::<u64>().unwrap_or(u64::MAX), list)
                    }
                    _ => (u64::MAX, idx_text.as_str()),
                };
                let spec_indices = resolve_constraint_specs(
                    spec_list,
                    filter.specs.len(),
                    argc as usize,
                    &cursor.def.name,
                )
                .map_err(|message| {
                    set_vtab_error(cursor.vtab, &message);
                    Error::Message(message)
                })?;
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
                let mut generator = match &filter.factory {
                    FilterFactory::Plain(factory) => factory(&args),
                    FilterFactory::Projection(factory) => factory(&args, col_used),
                };
                cursor.eof = !generator.next()?;
                cursor.generator = Some(generator);
                return Ok(());
            }

            if idx_num >= PARAMETRIC_FILTER_BASE
                && argc > 0
                && let Some(filter) = cursor
                    .def
                    .parametric_filters
                    .iter()
                    .find(|filter| filter.filter_id == idx_num)
            {
                let args = build_args(argc, argv);
                let mut generator = match &filter.factory {
                    FilterFactory::Plain(factory) => factory(&args),
                    FilterFactory::Projection(factory) => factory(&args, parse_col_used(idx_str)),
                };
                cursor.eof = !generator.next()?;
                cursor.generator = Some(generator);
                return Ok(());
            }

            if idx_num != FILTER_NONE
                && argc > 0
                && let Some(filter) = cursor.def.find_filter_by_id(idx_num)
            {
                let arg = FunctionArg::new(*argv);
                let mut iterator = match &filter.factory {
                    FilterFactory::Plain(factory) => factory(&arg),
                    FilterFactory::Projection(factory) => factory(&arg, parse_col_used(idx_str)),
                };
                cursor.eof = !iterator.next()?;
                cursor.iterator = Some(iterator);
                return Ok(());
            }

            if let Some(message) = cursor.def.full_scan_error.as_ref() {
                return Err(CursorFailure::Refused(message.clone()));
            }

            let mut generator = if let Some(projection) = cursor.def.projection_generator.as_ref() {
                projection(parse_col_used(idx_str))
            } else {
                // build() guarantees a plain generator when there is no projection factory.
                (cursor
                    .def
                    .generator
                    .as_ref()
                    .expect("generator or projection_generator"))()
            };
            cursor.eof = !generator.next()?;
            cursor.generator = Some(generator);
            Ok(())
        }));
        finish_refusable_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn generator_vtab_next<Row>(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let cursor = cursor
                .cast::<GeneratorCursor<Row>>()
                .as_mut()
                .ok_or_else(|| Error::Message("xsql generator cursor is null".to_string()))?;
            if cursor.count_only_total.is_some() {
                cursor.count_only_row += 1;
            } else if let Some(iterator) = cursor.iterator.as_mut() {
                cursor.eof = !iterator.next()?;
            } else if let Some(generator) = cursor.generator.as_mut() {
                cursor.eof = !generator.next()?;
            } else {
                cursor.eof = true;
            }
            Ok(())
        }));
        finish_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn generator_vtab_eof<Row>(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        let Some(cursor) = cursor.cast::<GeneratorCursor<Row>>().as_ref() else {
            return 1;
        };
        if let Some(total) = cursor.count_only_total {
            return i32::from(cursor.count_only_row >= total);
        }
        i32::from(cursor.eof || (cursor.generator.is_none() && cursor.iterator.is_none()))
    }
}

fn materialize_count_only_generator<Row>(
    cursor: &mut GeneratorCursor<Row>,
    col_used: u64,
) -> CursorResult {
    if cursor.count_only_total.is_none() {
        return Ok(());
    }
    // SQLite plans COUNT(*) and a rowid-only scan alike (colUsed == 0). Turning
    // the count-only cursor into a real generator IS a full scan, so a table
    // that forbids one refuses here exactly as xFilter's full-scan branch does;
    // only the count itself, which enumerates nothing, is exempt.
    if let Some(message) = cursor.def.full_scan_error.as_ref() {
        cursor.count_only_total = None;
        cursor.eof = true;
        return Err(CursorFailure::Refused(message.clone()));
    }
    let position = cursor.count_only_row;
    let mut generator = if let Some(projection) = cursor.def.projection_generator.as_ref() {
        projection(col_used)
    } else {
        (cursor
            .def
            .generator
            .as_ref()
            .expect("generator or projection_generator"))()
    };
    let mut available = generator.next()?;
    for _ in 0..position {
        if !available {
            break;
        }
        available = generator.next()?;
    }
    cursor.count_only_total = None;
    cursor.eof = !available;
    cursor.generator = Some(generator);
    if !available {
        return Err(Error::Message(
            "generator row_count() exceeded the rows produced by generator()".to_string(),
        )
        .into());
    }
    Ok(())
}

unsafe extern "C" fn generator_vtab_column<Row>(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    ctx: *mut ffi::sqlite3_context,
    col: c_int,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> CursorResult {
            // During UPDATE, SQLite asks for unchanged column values. Generator
            // updates reconstruct via row_lookup (rowid key), so reporting NOCHANGE
            // is always safe. Returning without a value marks the column
            // SQLITE_NOCHANGE in xUpdate.
            if sqlite3_vtab_nochange(ctx) != 0 {
                return Ok(());
            }
            let cursor = cursor
                .cast::<GeneratorCursor<Row>>()
                .as_mut()
                .ok_or_else(|| Error::Message("xsql generator cursor is null".to_string()))?;
            let mut fctx = FunctionContext::new(ctx);
            if cursor.count_only_total.is_some() {
                let col_used = if (0..63).contains(&col) {
                    1_u64 << col
                } else {
                    1_u64 << 63
                };
                materialize_count_only_generator(cursor, col_used)?;
            }
            if cursor.eof || col < 0 || col as usize >= cursor.def.columns.len() {
                fctx.result_null();
                return Ok(());
            }
            if let Some(iterator) = cursor.iterator.as_ref() {
                iterator.column(&mut fctx, col)?;
                return Ok(());
            }
            let Some(generator) = cursor.generator.as_ref() else {
                fctx.result_null();
                return Ok(());
            };
            (cursor.def.columns[col as usize].getter)(&mut fctx, generator.current());
            Ok(())
        }));
        match result {
            Ok(Ok(())) => ffi::SQLITE_OK,
            Ok(Err(CursorFailure::Refused(message))) => {
                FunctionContext::new(ctx).result_error(message);
                ffi::sqlite3_result_error_code(ctx, ffi::SQLITE_CONSTRAINT_VTAB);
                ffi::SQLITE_CONSTRAINT_VTAB
            }
            Ok(Err(CursorFailure::Failed(err))) => {
                FunctionContext::new(ctx).result_error(err.to_string());
                ffi::SQLITE_ERROR
            }
            Err(_) => {
                FunctionContext::new(ctx).result_error("xsql generator table column panicked");
                ffi::SQLITE_ERROR
            }
        }
    }
}

unsafe extern "C" fn generator_vtab_rowid<Row>(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> CursorResult {
            let cursor = cursor
                .cast::<GeneratorCursor<Row>>()
                .as_mut()
                .ok_or_else(|| Error::Message("xsql generator cursor is null".to_string()))?;
            if cursor.count_only_total.is_some() {
                materialize_count_only_generator(cursor, 0)?;
            }
            if let Some(iterator) = cursor.iterator.as_ref() {
                *rowid = if cursor.eof { 0 } else { iterator.rowid() };
                return Ok(());
            }
            *rowid = cursor
                .generator
                .as_ref()
                .filter(|_| !cursor.eof)
                .map(|generator| generator.rowid())
                .unwrap_or(0);
            Ok(())
        }));
        finish_refusable_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn generator_vtab_update<Row: 'static>(
    p_vtab: *mut ffi::sqlite3_vtab,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
    rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<c_int> {
            let vtab = p_vtab
                .cast::<GeneratorVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator vtab is null".to_string()))?;
            if argc <= 0 || argv.is_null() {
                return Ok(ffi::SQLITE_READONLY);
            }

            let old_rowid = FunctionArg::new(*argv);
            if argc == 1 && !old_rowid.is_null() {
                // A generator DELETE needs both a delete callback AND a row_lookup
                // to resolve the target row; missing either makes the surface
                // unsupported (matches the recorded caps and the C++ port).
                let Some(delete_row) = vtab.def.delete_row.as_ref() else {
                    return Ok(unsupported_delete(p_vtab, &vtab.def.name));
                };
                let Some(row_lookup) = vtab.def.row_lookup.as_ref() else {
                    return Ok(unsupported_delete(p_vtab, &vtab.def.name));
                };
                let Some(row) = row_lookup(old_rowid.as_i64()) else {
                    return Err(Error::Message(
                        "generator table row lookup failed".to_string(),
                    ));
                };
                let operation = format!("DELETE FROM {}", vtab.def.name);
                vtab.transaction.touch();
                call_cached_modify_hook(&vtab.def.before_modify, &operation);
                begin_vtab_callback();
                if delete_row(&row) {
                    call_cached_modify_hook(&vtab.def.after_modify, &operation);
                    vtab.transaction.mark_written();
                    return Ok(ffi::SQLITE_OK);
                }
                return Err(vtab_callback_failure("generator table delete failed"));
            }

            if argc > 1 && !old_rowid.is_null() {
                // A generator UPDATE needs both a writable column AND a row_lookup;
                // missing either makes the surface unsupported (matches the
                // recorded caps and the C++ port).
                if !vtab
                    .def
                    .columns
                    .iter()
                    .any(|column| column.setter.is_some())
                {
                    return Ok(unsupported_update(p_vtab, &vtab.def.name));
                }
                let Some(row_lookup) = vtab.def.row_lookup.as_ref() else {
                    return Ok(unsupported_update(p_vtab, &vtab.def.name));
                };
                let Some(mut row) = row_lookup(old_rowid.as_i64()) else {
                    return Err(Error::Message(
                        "generator table row lookup failed".to_string(),
                    ));
                };
                let operation = format!("UPDATE {}", vtab.def.name);
                vtab.transaction.touch();
                call_cached_modify_hook(&vtab.def.before_modify, &operation);
                let args = build_args(argc - 2, argv.add(2));
                apply_generator_setters(&vtab.def, &mut row, &args)?;
                call_cached_modify_hook(&vtab.def.after_modify, &operation);
                vtab.transaction.mark_written();
                return Ok(ffi::SQLITE_OK);
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
                    call_cached_modify_hook(&vtab.def.after_modify, &operation);
                    vtab.transaction.mark_written();
                    return Ok(ffi::SQLITE_OK);
                }
                return Err(vtab_callback_failure("generator table insert failed"));
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
                set_vtab_error(p_vtab, "xsql generator table update panicked");
                ffi::SQLITE_ERROR
            }
        }
    }
}

/// Which filter kind a generator plan uses; see `generator_vtab_best_index`.
#[derive(Clone, Copy)]
enum FilterPick {
    Single,
    Constraint,
    Parametric,
}

unsafe extern "C" fn generator_vtab_best_index<Row>(
    p_vtab: *mut ffi::sqlite3_vtab,
    index_info: *mut ffi::sqlite3_index_info,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let vtab = p_vtab
                .cast::<GeneratorVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator vtab is null".to_string()))?;
            let info = index_info
                .as_mut()
                .ok_or_else(|| Error::Message("sqlite index_info is null".to_string()))?;
            if info.nConstraint == 0
                && info.colUsed == 0
                && let Some(row_count) = vtab.def.row_count.as_ref()
            {
                let estimated_rows = vtab
                    .def
                    .estimate_rows
                    .as_ref()
                    .map(|estimate| estimate())
                    .unwrap_or_else(row_count);
                info.idxNum = GENERATOR_COUNT_ONLY_SCAN;
                info.estimatedCost = estimated_rows as f64;
                info.estimatedRows = estimated_rows as i64;
                return Ok(());
            }
            // Every filter kind competes by estimated_cost: the cheapest matching
            // constraint filter (multi-column, multi-operator), the cheapest
            // parametric filter (all of its columns EQ-constrained) and the
            // cheapest single-column filter (EQ, or LIKE/GLOB with a usable literal
            // prefix). The cheapest plan wins; on a cost tie the single-column
            // filter wins (the same rule the cached planner applies to its
            // constraint filters), then the constraint filter, then the parametric
            // filter. Mirrors the C++ generator_vtab_best_index.
            let mut best_constraint_filter: Option<&ConstraintFilterDef<Row>> = None;
            let mut best_matched_constraints = Vec::new();
            let mut best_consumes_order = false;
            let mut best_matched_count = -1;
            let mut missing_required_error = String::new();

            for filter in &vtab.def.constraint_filters {
                let mut matched_constraints = vec![-1; filter.specs.len()];
                let mut all_required_matched = true;
                let mut matched_count = 0;

                for (spec_index, spec) in filter.specs.iter().enumerate() {
                    let mut matched = None;
                    for i in 0..info.nConstraint {
                        let constraint = &*info.aConstraint.add(i as usize);
                        if constraint.usable != 0
                            && super::ffi::binary_constraint(info, i)
                            && constraint.op as c_int == spec.op.sqlite_op()
                            && constraint.iColumn >= 0
                            && constraint.iColumn as usize == spec.column_index
                        {
                            matched = Some(i);
                            break;
                        }
                    }
                    if let Some(matched) = matched {
                        matched_constraints[spec_index] = matched;
                        matched_count += 1;
                    } else if spec.required {
                        all_required_matched = false;
                        if missing_required_error.is_empty() && !spec.missing_error.is_empty() {
                            missing_required_error = spec.missing_error.clone();
                        }
                        break;
                    }
                }

                if all_required_matched && matched_count > 0 {
                    let consumes_order = match filter.ordered_column {
                        Some(ordered_column) if info.nOrderBy == 1 => {
                            let order = &*info.aOrderBy;
                            order.iColumn >= 0
                                && order.iColumn as usize == ordered_column
                                && (order.desc != 0) == filter.ordered_desc
                        }
                        _ => false,
                    };

                    let replace = best_constraint_filter
                        .map(|best| {
                            filter.estimated_cost < best.estimated_cost
                                || (filter.estimated_cost == best.estimated_cost
                                    && consumes_order
                                    && !best_consumes_order)
                                || (filter.estimated_cost == best.estimated_cost
                                    && consumes_order == best_consumes_order
                                    && matched_count > best_matched_count)
                        })
                        .unwrap_or(true);
                    if replace {
                        best_constraint_filter = Some(filter);
                        best_matched_constraints = matched_constraints;
                        best_consumes_order = consumes_order;
                        best_matched_count = matched_count;
                    }
                }
            }

            // Parametric filters: one matches when ALL its columns are
            // EQ-constrained; the cheapest matching one (the first registered on a
            // tie) is the candidate.
            let mut best_parametric_filter: Option<&ParametricFilterDef<Row>> = None;
            let mut best_parametric_constraints = Vec::new();
            for filter in &vtab.def.parametric_filters {
                let mut matched_constraints = Vec::with_capacity(filter.column_indices.len());
                let mut all_matched = true;
                for column_index in &filter.column_indices {
                    let mut matched = None;
                    for i in 0..info.nConstraint {
                        let constraint = &*info.aConstraint.add(i as usize);
                        if constraint.usable != 0
                            && super::ffi::binary_constraint(info, i)
                            && constraint.op as i32 == ffi::SQLITE_INDEX_CONSTRAINT_EQ
                            && constraint.iColumn >= 0
                            && constraint.iColumn as usize == *column_index
                        {
                            matched = Some(i);
                            break;
                        }
                    }
                    if let Some(matched) = matched {
                        matched_constraints.push(matched);
                    } else {
                        all_matched = false;
                        break;
                    }
                }
                let replace = best_parametric_filter
                    .map(|best| filter.estimated_cost < best.estimated_cost)
                    .unwrap_or(true);
                if all_matched && replace {
                    best_parametric_filter = Some(filter);
                    best_parametric_constraints = matched_constraints;
                }
            }

            // Single-column filters. EQ is trusted (omit=1); LIKE/GLOB is a
            // best-effort superset (omit=0, SQLite re-applies the pattern), claimed
            // only when the pattern has a usable literal prefix.
            let mut best_filter: Option<&GeneratorFilterDef> = None;
            let mut best_constraint_index = -1;
            for i in 0..info.nConstraint {
                let constraint = &*info.aConstraint.add(i as usize);
                let op = constraint.op as c_int;
                let is_eq = op == ffi::SQLITE_INDEX_CONSTRAINT_EQ;
                let is_like = op == ffi::SQLITE_INDEX_CONSTRAINT_LIKE
                    || op == ffi::SQLITE_INDEX_CONSTRAINT_GLOB;
                if constraint.usable == 0
                    || !super::ffi::binary_constraint(info, i)
                    || constraint.iColumn < 0
                    || !(is_eq || is_like)
                {
                    continue;
                }
                if is_like && !like_constraint_has_usable_prefix(info, i) {
                    continue;
                }
                if let Some(filter) = vtab
                    .def
                    .find_filter_by_column(constraint.iColumn as usize, op)
                {
                    let replace = best_filter
                        .map(|best| filter.estimated_cost < best.estimated_cost)
                        .unwrap_or(true);
                    if replace {
                        best_filter = Some(filter);
                        best_constraint_index = i;
                    }
                }
            }

            // Considered in tie-break order: a later candidate must be strictly
            // cheaper.
            let mut pick: Option<(FilterPick, f64)> = None;
            let mut consider = |kind: FilterPick, cost: f64| {
                if pick.is_none_or(|(_, best)| cost < best) {
                    pick = Some((kind, cost));
                }
            };
            if let Some(filter) = best_filter {
                consider(FilterPick::Single, filter.estimated_cost);
            }
            if let Some(filter) = best_constraint_filter {
                consider(FilterPick::Constraint, filter.estimated_cost);
            }
            if let Some(filter) = best_parametric_filter {
                consider(FilterPick::Parametric, filter.estimated_cost);
            }

            match pick.map(|(kind, _)| kind) {
                Some(FilterPick::Constraint) => {
                    let filter = best_constraint_filter.expect("picked a constraint filter");
                    let mut argv_index = 1;
                    let mut idx_parts = Vec::new();
                    // Only a projection-aware filter prefixes colUsed ("<n>;"), so a
                    // plain constraint_filter's idxStr stays the bare spec list.
                    let prefix = if filter.factory.is_projection() {
                        format!("{};", info.colUsed)
                    } else {
                        String::new()
                    };
                    for (spec_index, constraint_index) in
                        best_matched_constraints.iter().enumerate()
                    {
                        if *constraint_index < 0 {
                            continue;
                        }
                        let usage = &mut *info.aConstraintUsage.add(*constraint_index as usize);
                        usage.argvIndex = argv_index;
                        usage.omit = 1;
                        argv_index += 1;
                        idx_parts.push(spec_index.to_string());
                    }
                    if best_consumes_order {
                        info.orderByConsumed = 1;
                    }
                    let idx_str =
                        sqlite_mprintf_string(&format!("{prefix}{}", idx_parts.join(",")));
                    if !idx_str.is_null() {
                        info.idxStr = idx_str;
                        info.needToFreeIdxStr = 1;
                    }
                    info.idxNum = filter.filter_id;
                    info.estimatedCost = filter.estimated_cost;
                    info.estimatedRows = filter.estimated_rows;
                    return Ok(());
                }
                Some(FilterPick::Parametric) => {
                    let filter = best_parametric_filter.expect("picked a parametric filter");
                    for (arg_index, constraint_index) in
                        best_parametric_constraints.iter().enumerate()
                    {
                        let usage = &mut *info.aConstraintUsage.add(*constraint_index as usize);
                        usage.argvIndex = (arg_index + 1) as c_int;
                        usage.omit = 1;
                    }
                    info.idxNum = filter.filter_id;
                    info.estimatedCost = filter.estimated_cost;
                    info.estimatedRows = filter.estimated_rows;
                    if filter.factory.is_projection() {
                        set_col_used_idx_str(info);
                    }
                    return Ok(());
                }
                Some(FilterPick::Single) => {
                    let filter = best_filter.expect("picked a single-column filter");
                    let usage = &mut *info.aConstraintUsage.add(best_constraint_index as usize);
                    usage.argvIndex = 1;
                    usage.omit = u8::from(filter.op == ffi::SQLITE_INDEX_CONSTRAINT_EQ);
                    info.idxNum = filter.filter_id;
                    info.estimatedCost = filter.estimated_cost;
                    info.estimatedRows = filter.estimated_rows;
                    if filter.factory.is_projection() {
                        set_col_used_idx_str(info);
                    }
                    return Ok(());
                }
                None => {}
            }

            if !missing_required_error.is_empty() {
                let idx_str = sqlite_mprintf_string(&missing_required_error);
                if !idx_str.is_null() {
                    info.idxStr = idx_str;
                    info.needToFreeIdxStr = 1;
                }
                // This plan can only fail, so SQLite must pick it last: in a join it
                // competes with plans where the other table runs outer and feeds the
                // required value in per row. Priced cheap, it won and a valid join
                // failed. Alone, it is still the only plan and the error stands.
                // (Same pricing as the C++ core and libxsql-c.)
                info.idxNum = MISSING_REQUIRED_CONSTRAINT;
                info.estimatedCost = 1e9;
                info.estimatedRows = 1;
                return Ok(());
            }

            let estimated_rows = vtab
                .def
                .estimate_rows
                .as_ref()
                .map(|estimate| estimate())
                .unwrap_or(1_000);
            info.idxNum = FILTER_NONE;
            info.estimatedCost = estimated_rows as f64;
            info.estimatedRows = estimated_rows as i64;
            // Carry colUsed to xFilter via idxStr (like the cached table) so a
            // projection-aware generator can skip expensive unused columns. Only the
            // FILTER_NONE plan uses idxStr here (filter plans returned above).
            if vtab.def.projection_generator.is_some() {
                set_col_used_idx_str(info);
            }
            Ok(())
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

/// Carry `colUsed` to xFilter as the plan's idxStr (decoded by `parse_col_used`).
fn set_col_used_idx_str(info: &mut ffi::sqlite3_index_info) {
    let idx_str = sqlite_mprintf_string(&info.colUsed.to_string());
    if !idx_str.is_null() {
        info.idxStr = idx_str;
        info.needToFreeIdxStr = 1;
    }
}

unsafe extern "C" fn generator_vtab_begin<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if let Some(vtab) = p_vtab.cast::<GeneratorVtab<Row>>().as_ref() {
            vtab.transaction.begin();
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn generator_vtab_sync<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<GeneratorVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator vtab is null".to_string()))?;
            vtab.transaction.sync(&vtab.def.transaction_hooks)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn generator_vtab_commit<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if let Some(vtab) = p_vtab.cast::<GeneratorVtab<Row>>().as_ref() {
            // SQLite ignores xCommit/xRollback errors. Contain a panic without
            // leaving an ignored allocation in zErrMsg.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                vtab.transaction.commit(&vtab.def.transaction_hooks);
            }));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn generator_vtab_rollback<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if let Some(vtab) = p_vtab.cast::<GeneratorVtab<Row>>().as_ref() {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                vtab.transaction.rollback(&vtab.def.transaction_hooks);
            }));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn generator_vtab_savepoint<Row>(
    p_vtab: *mut ffi::sqlite3_vtab,
    id: c_int,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<GeneratorVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator vtab is null".to_string()))?;
            vtab.transaction.savepoint(&vtab.def.transaction_hooks, id)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn generator_vtab_release<Row>(
    p_vtab: *mut ffi::sqlite3_vtab,
    id: c_int,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<GeneratorVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator vtab is null".to_string()))?;
            vtab.transaction.release(&vtab.def.transaction_hooks, id)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn generator_vtab_rollback_to<Row>(
    p_vtab: *mut ffi::sqlite3_vtab,
    id: c_int,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<GeneratorVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator vtab is null".to_string()))?;
            vtab.transaction
                .rollback_to(&vtab.def.transaction_hooks, id)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

fn apply_generator_setters<'a, Row>(
    def: &GeneratorInner<Row>,
    row: &mut Row,
    args: &[FunctionArg<'a>],
) -> Result<()> {
    // Generator updates always resolve the row from its rowid via row_lookup, so
    // unchanged columns arrive as NOCHANGE -- always NOCHANGE-eligible.
    apply_update_columns(
        args,
        def.columns.len(),
        true,
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
